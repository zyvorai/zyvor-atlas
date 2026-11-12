// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `POST /volumes/{id}/import`: queue a job that streams a disk image from an HTTP(S) server
//! into a block volume and verifies it by reading the volume back. Raw images are written as
//! they arrive. qcow2 is converted with `qemu-img convert -O raw` first. The volume's own
//! backend is used (atlas-native or Ceph RBD). The progress, result and cancel surfaces are the
//! ordinary job API. See `docs/IMAGE_IMPORT.md`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

/// Comma-separated hosts (names or IP literals) an import may fetch from. Empty or unset turns
/// the feature off: the gateway would otherwise fetch any URL a caller names.
pub(crate) const ALLOWED_HOSTS_ENV: &str = "ATLAS_IMPORT_ALLOWED_HOSTS";

/// Retries (each resumes from the recorded progress) before the job is failed; overridable with
/// `ATLAS_IMPORT_MAX_RETRIES`.
const DEFAULT_IMPORT_RETRIES: i64 = 5;

fn import_retries() -> i64 {
    std::env::var("ATLAS_IMPORT_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(DEFAULT_IMPORT_RETRIES)
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImportBody {
    /// `http://` or `https://` URL of a raw disk image.
    url: String,
    /// Lower-case or upper-case hex SHA-256 of the whole image.
    sha256: String,
    /// Image length in bytes, if known. Checked against what the server sends.
    #[serde(default)]
    size_bytes: Option<u64>,
    /// A failed or cancelled earlier import job for this volume whose progress this one continues.
    #[serde(default)]
    resume_from_job: Option<String>,
    /// `raw` (default) or `qcow2`.
    #[serde(default)]
    format: String,
}

fn normalize_format(raw: &str) -> AppResult<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "raw" => Ok("raw".into()),
        "qcow2" => Ok("qcow2".into()),
        other => Err(AppError::Validation(format!(
            "format must be raw or qcow2, not {other}"
        ))),
    }
}

fn allowed_hosts() -> Vec<String> {
    std::env::var(ALLOWED_HOSTS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

/// The URL, if it is acceptable: http(s), no embedded credentials (they would be stored in the
/// job), and a host on the allow-list.
fn check_url(raw: &str, allowed: &[String]) -> AppResult<reqwest::Url> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| AppError::Validation(format!("url is not valid: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::Validation("url must be http or https".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::Validation(
            "url must not contain credentials; the job record stores the url".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| AppError::Validation("url has no host".into()))?
        .to_ascii_lowercase();
    if allowed.is_empty() {
        return Err(AppError::Forbidden(format!(
            "image import is off; set {ALLOWED_HOSTS_ENV} to the hosts it may fetch from"
        )));
    }
    if !allowed.contains(&host) {
        return Err(AppError::Forbidden(format!(
            "host {host} is not in {ALLOWED_HOSTS_ENV}"
        )));
    }
    Ok(url)
}

fn check_sha256(raw: &str) -> AppResult<String> {
    let h = raw.trim().to_ascii_lowercase();
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::Validation(
            "sha256 must be 64 hexadecimal characters".into(),
        ));
    }
    Ok(h)
}

/// `POST /volumes/{id}/import` (operator): import a raw or qcow2 image into a block volume.
pub(crate) async fn import_volume_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<ImportBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let vol = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let tenant_id = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &tenant_id,
        format!("volume {id}"),
    )?;
    if vol.kind != atlas_api_types::VolumeKind::Block {
        return Err(AppError::Validation(
            "image import writes a block volume".into(),
        ));
    }
    let backend_id = atlas_inventory::volume_backend_id(&s.pool, &id)
        .await?
        .filter(|b| !b.is_empty())
        .ok_or_else(|| AppError::Validation(format!("volume {id} has no storage backend")))?;
    if s.drivers.get(&backend_id).is_none() {
        return Err(AppError::Unavailable(format!(
            "storage backend {backend_id} is not available"
        )));
    }
    let native_id = vol
        .backend_native_id
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| id.clone());
    let url = check_url(&body.url, &allowed_hosts())?;
    let sha256 = check_sha256(&body.sha256)?;
    let format = normalize_format(&body.format)?;
    if let Some(n) = body.size_bytes {
        if n == 0 || i64::try_from(n).map_or(true, |n| n > vol.size_bytes) {
            return Err(AppError::Validation(format!(
                "size_bytes {n} does not fit the {}-byte volume",
                vol.size_bytes
            )));
        }
    }

    let resume_from = match &body.resume_from_job {
        None => 0,
        Some(prev) => {
            let job = atlas_inventory::jobs::get_job(&s.pool, prev)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("job {prev}")))?;
            if job.job_type != "image.import"
                || job.result.get("volume_id").and_then(Value::as_str) != Some(id.as_str())
            {
                return Err(AppError::Validation(format!(
                    "job {prev} is not an image import into volume {id}"
                )));
            }
            if matches!(
                job.state.as_str(),
                "pending" | "queued" | "running" | "verifying"
            ) {
                return Err(AppError::Conflict(format!(
                    "job {prev} is still {}",
                    job.state
                )));
            }
            job.result
                .get("bytes_done")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        }
    };

    let job_id = ids::job_id();
    let spec = JobSpec::ImageImport {
        volume_id: id.clone(),
        backend_id: backend_id.clone(),
        native_id,
        url: url.to_string(),
        sha256: sha256.clone(),
        size_bytes: body.size_bytes,
        resume_from,
        format: format.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, &tenant_id, &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    atlas_inventory::jobs::set_max_retries(&s.pool, &job_id, import_retries()).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&tenant_id),
        &actor.id,
        "volume.import.requested",
        "volume",
        &id,
        "accepted",
        Some(json!({
            "host": url.host_str(),
            "sha256": sha256,
            "size_bytes": body.size_bytes,
            "resume_from": resume_from,
            "format": format,
        })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "volume_id": id, "resume_from": resume_from }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn format_is_raw_or_qcow2() {
        assert_eq!(normalize_format("").unwrap(), "raw");
        assert_eq!(normalize_format("QCOW2").unwrap(), "qcow2");
        assert!(matches!(
            normalize_format("vmdk"),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn rejects_everything_while_the_allow_list_is_empty() {
        let err = check_url("http://images.internal/a.raw", &[]).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "{err:?}");
    }

    #[test]
    fn accepts_a_listed_host_and_matches_case_insensitively() {
        assert!(check_url(
            "https://Images.Internal:8443/a.raw",
            &allow(&["images.internal"])
        )
        .is_ok());
        assert!(check_url("http://10.0.0.5:8099/a.raw", &allow(&["10.0.0.5"])).is_ok());
    }

    #[test]
    fn rejects_unlisted_hosts_including_lookalikes() {
        let a = allow(&["images.internal"]);
        for u in [
            "http://evil.example/a.raw",
            "http://images.internal.evil.example/a.raw",
            "http://169.254.169.254/latest/meta-data",
        ] {
            assert!(
                matches!(check_url(u, &a), Err(AppError::Forbidden(_))),
                "{u}"
            );
        }
    }

    #[test]
    fn rejects_other_schemes_and_embedded_credentials() {
        let a = allow(&["images.internal"]);
        assert!(matches!(
            check_url("file:///etc/passwd", &a),
            Err(AppError::Validation(_))
        ));
        assert!(matches!(
            check_url("ftp://images.internal/a.raw", &a),
            Err(AppError::Validation(_))
        ));
        assert!(matches!(
            check_url("https://user:pw@images.internal/a.raw", &a),
            Err(AppError::Validation(_))
        ));
        assert!(matches!(
            check_url("not a url", &a),
            Err(AppError::Validation(_))
        ));
    }

    #[test]
    fn sha256_must_be_64_hex_and_is_lowercased() {
        let ok = "AB".repeat(32);
        assert_eq!(check_sha256(&ok).unwrap(), "ab".repeat(32));
        assert!(check_sha256("abc").is_err());
        assert!(check_sha256(&"g".repeat(64)).is_err());
    }
}
