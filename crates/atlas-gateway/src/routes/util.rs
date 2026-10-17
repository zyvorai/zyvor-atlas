// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use atlas_api_types::{BackendType, Capabilities};
use atlas_common::AppError;
use axum::{http::StatusCode, Json};
use serde_json::{json, Value};

pub(crate) const CEPH_BACKEND_ID: &str = "bkd_ceph_lab";
pub(crate) const DEFAULT_RBD_POOL: &str = "rbd-nvme-prod";

pub(crate) fn accepted(
    job: &atlas_api_types::JobRecord,
    resource: Value,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job.id,
            "state": job.state,
            "resource": resource,
            "links": { "job": format!("/api/atlas/v1/jobs/{}", job.id) }
        })),
    )
}

pub(crate) fn csv_field(v: &Value, key: &str) -> String {
    let s = v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    // Neutralize CSV formula injection: a cell starting with =/+/-/@ can be interpreted as a
    // formula by spreadsheet apps (Excel/Sheets) when this export is opened.
    let needs_guard = matches!(s.as_bytes().first(), Some(b'=' | b'+' | b'-' | b'@'));
    let guarded = if needs_guard {
        format!("'{s}")
    } else {
        s.to_string()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

/// Validate `name` as a Kubernetes-safe RFC 1123 name — lowercase alphanumeric and `-`/`.` only,
/// each `.`-separated label must start and end with an alphanumeric character, max 253 chars.
/// Without this, a name like `RT-Upper_Invalid!!` (uppercase, underscore, `!`) sails through
/// Atlas's own validation with a `202 Accepted` and only fails minutes later, deep in a PVC/OBC
/// create call, surfacing a raw Kubernetes API error straight to the operator instead of a clean
/// upfront `400`.
pub(crate) fn validate_k8s_name(name: &str) -> Result<(), AppError> {
    let valid_label = |s: &str| {
        !s.is_empty()
            && s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
    };
    if name.len() > 253 || !name.split('.').all(valid_label) {
        return Err(AppError::Validation(format!(
            "name {name:?} must be a valid Kubernetes resource name: lowercase alphanumeric \
             characters, '-', or '.', and must start and end with an alphanumeric character"
        )));
    }
    Ok(())
}

pub(crate) fn ceph_default_caps(t: BackendType) -> Capabilities {
    match t {
        BackendType::Ceph => Capabilities {
            block: true,
            file: true,
            object: true,
            snapshots: true,
            clone: true,
            expansion: true,
            replication: true,
        },
        // atlas-driver-nfs / atlas-driver-zfs only implement discover/health/list_pools/
        // list_volumes/metrics (read-only) — every write-path StorageDriver method falls back to
        // the trait default (NotImplemented), and both classify their volumes as
        // VolumeKind::Filesystem. `Capabilities::default()` (all false, including `file`) was
        // reporting a live, functioning NFS/ZFS backend as supporting nothing at all.
        BackendType::Nfs | BackendType::Zfs | BackendType::Weka | BackendType::Macos => Capabilities {
            file: true,
            ..Capabilities::default()
        },
        // Native driver is read-only; PVC provisioning is served by the Kubernetes path.
        BackendType::Longhorn => Capabilities {
            block: true,
            ..Capabilities::default()
        },
        // atlas-driver-rustfs only implements discover/health/list_pools/list_volumes/metrics
        // (read-only) and classifies its volumes as VolumeKind::Object.
        BackendType::Rustfs => Capabilities {
            object: true,
            ..Capabilities::default()
        },
        // atlas-driver-native: block volumes (resizable) and filesystems, both with snapshots
        // and clones.
        BackendType::Native => Capabilities {
            block: true,
            file: true,
            snapshots: true,
            clone: true,
            expansion: true,
            ..Capabilities::default()
        },
        _ => Capabilities::default(),
    }
}
