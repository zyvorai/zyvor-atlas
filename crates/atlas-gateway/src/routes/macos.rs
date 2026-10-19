// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Volume lifecycle for a Mac's external disks (`docs/MACOS.md`): erase, add/delete an APFS volume,
//! eject. Admin only; erase and delete need the target typed back; each runs as a job and is audited.
//! The refusals (internal disk, running system, system volumes, disk images) are checked here for a
//! fast 409 and again inside the job. Local to the Mac the gateway runs on by default; with
//! `?backend=bkd_macos_<name>` the operation runs on that Mac through its agent, which runs the same
//! refusals against its own live layout (and must have been started with `ATLAS_MAC_ALLOW_WRITES=1`).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_driver_core::DriverError;
use atlas_driver_macos::Lifecycle;
use atlas_jobs::JobSpec;

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct BackendQuery {
    /// `bkd_macos` (default, this Mac) or `bkd_macos_<name>` for an agent in `ATLAS_MACOS_AGENTS`.
    backend: Option<String>,
}

/// The backend id the operation targets and its lifecycle client.
fn ops(q: &BackendQuery) -> AppResult<(String, Box<dyn Lifecycle>)> {
    let local = crate::startup::MACOS_BACKEND_ID;
    match q.backend.as_deref().filter(|b| !b.is_empty()) {
        None => local_ops().map(|o| (local.to_string(), o)),
        Some(b) if b == local => local_ops().map(|o| (local.to_string(), o)),
        Some(b) if b.starts_with("bkd_macos_") => {
            let remote = atlas_driver_macos::remote_ops_from_env(b)
                .map_err(AppError::Validation)?
                .ok_or_else(|| AppError::NotFound(format!("macOS agent backend {b}")))?;
            Ok((b.to_string(), Box::new(remote)))
        }
        Some(b) => Err(AppError::Validation(format!("{b:?} is not a macOS backend"))),
    }
}

fn local_ops() -> AppResult<Box<dyn Lifecycle>> {
    let settings = atlas_driver_macos::EnvSettings::from_env();
    if !settings.enabled {
        return Err(AppError::Unavailable(
            "the macOS backend is not enabled (set ATLAS_MACOS_ENABLE=1 and restart the gateway)"
                .into(),
        ));
    }
    Ok(Box::new(settings.ops()))
}

/// A refusal is the operator's conflict; anything else is a driver problem.
fn map_check(e: DriverError) -> AppError {
    match e {
        DriverError::Backend(m) if m.starts_with("refused: ") => {
            AppError::Conflict(m.trim_start_matches("refused: ").to_string())
        }
        DriverError::Backend(m) => AppError::Validation(m),
        other => AppError::Driver(other.to_string()),
    }
}

async fn enqueue(
    s: &AppState,
    actor: &Actor,
    spec: JobSpec,
    action: &str,
    kind: &str,
    target: &str,
    detail: Value,
) -> AppResult<(StatusCode, Json<Value>)> {
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool, None, &actor.id, action, kind, target, "accepted", Some(detail.clone()), None,
    )
    .await;
    Ok(accepted(&job, json!({ "target": target, "detail": detail })))
}

fn admin(s: &AppState, actor: &Actor) -> AppResult<()> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_ADMIN)
}

/// `GET /macos/devices` — whole disks with `erase_refusal` (why Atlas would not erase it).
pub(crate) async fn list_macos_devices(
    State(_s): State<AppState>,
    Query(q): Query<BackendQuery>,
) -> AppResult<Json<Value>> {
    let rows = ops(&q)?.1.devices().await.map_err(map_check)?;
    Ok(Json(json!(rows)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct EraseBody {
    device: String,
    /// The device typed again by the operator; must equal `device`.
    confirm_device: String,
    name: String,
}

/// `POST /macos/disks/erase` — `diskutil eraseDisk APFS <name> GPT <device>` on an external disk.
pub(crate) async fn erase_macos_disk(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<BackendQuery>,
    Json(b): Json<EraseBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != b.device {
        return Err(AppError::Validation("confirm_device must equal device".into()));
    }
    let (backend, ops) = ops(&q)?;
    ops.check_erase(&b.device, &b.name).await.map_err(map_check)?;
    let spec = JobSpec::MacosDiskErase {
        backend_id: backend.clone(),
        device: b.device.clone(),
        confirmed_device: b.confirm_device,
        name: b.name.clone(),
    };
    enqueue(&s, &actor, spec, "macos.disk.erase.requested", "disk", &b.device, json!({ "name": b.name, "backend": backend }))
        .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddVolumeBody {
    name: String,
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    reserve_bytes: Option<u64>,
}

/// `POST /macos/containers/{id}/volumes` — add an APFS volume to an external container.
pub(crate) async fn add_macos_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(container): Path<String>,
    Query(q): Query<BackendQuery>,
    Json(b): Json<AddVolumeBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if let (Some(q), Some(r)) = (b.quota_bytes, b.reserve_bytes) {
        if r > q {
            return Err(AppError::Validation("reserve_bytes cannot exceed quota_bytes".into()));
        }
    }
    let (backend, ops) = ops(&q)?;
    ops.check_add_volume(&container, &b.name).await.map_err(map_check)?;
    let spec = JobSpec::MacosVolumeAdd {
        backend_id: backend.clone(),
        container: container.clone(),
        name: b.name.clone(),
        quota_bytes: b.quota_bytes,
        reserve_bytes: b.reserve_bytes,
    };
    enqueue(
        &s, &actor, spec, "macos.volume.add.requested", "container", &container,
        json!({ "name": b.name, "quota_bytes": b.quota_bytes, "reserve_bytes": b.reserve_bytes, "backend": backend }),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResizeBody {
    size_bytes: u64,
    /// The container typed again by the operator; must equal the one in the path.
    confirm_device: String,
}

/// `POST /macos/containers/{container}/resize` — `diskutil apfs resizeContainer` on an external
/// container. Shrinking cannot go below what the container holds.
pub(crate) async fn resize_macos_container(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(container): Path<String>,
    Query(q): Query<BackendQuery>,
    Json(b): Json<ResizeBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != container {
        return Err(AppError::Validation("confirm_device must equal the container".into()));
    }
    let (backend, ops) = ops(&q)?;
    ops.check_resize_container(&container, b.size_bytes).await.map_err(map_check)?;
    let spec = JobSpec::MacosContainerResize {
        backend_id: backend.clone(),
        container: container.clone(),
        confirmed_container: b.confirm_device,
        size_bytes: b.size_bytes,
    };
    enqueue(
        &s, &actor, spec, "macos.container.resize.requested", "container", &container,
        json!({ "size_bytes": b.size_bytes, "backend": backend }),
    )
    .await
}

/// `GET /macos/snapshots` — local snapshots of the Mac (dates). Not per volume: macOS cannot make those.
pub(crate) async fn list_macos_snapshots(
    State(_s): State<AppState>,
    Query(q): Query<BackendQuery>,
) -> AppResult<Json<Value>> {
    let dates = ops(&q)?.1.list_local_snapshots().await.map_err(map_check)?;
    Ok(Json(json!(dates)))
}

/// `POST /macos/snapshots` — `tmutil localsnapshot`. Purgeable: macOS may remove it when space is needed.
pub(crate) async fn create_macos_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<BackendQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    let (backend, _) = ops(&q)?;
    let spec = JobSpec::MacosSnapshotCreate { backend_id: backend.clone() };
    enqueue(&s, &actor, spec, "macos.snapshot.create.requested", "mac", &backend, json!({ "backend": backend })).await
}

/// `POST /macos/snapshots/{date}/delete` — `tmutil deletelocalsnapshots`; the date is typed back and
/// must be one `tmutil` currently lists.
pub(crate) async fn delete_macos_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(date): Path<String>,
    Query(q): Query<BackendQuery>,
    Json(b): Json<ConfirmDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != date {
        return Err(AppError::Validation("confirm_device must equal the snapshot date".into()));
    }
    let (backend, ops) = ops(&q)?;
    ops.check_delete_local_snapshot(&date).await.map_err(map_check)?;
    let spec = JobSpec::MacosSnapshotDelete {
        backend_id: backend.clone(),
        date: date.clone(),
        confirmed_date: b.confirm_device,
    };
    enqueue(&s, &actor, spec, "macos.snapshot.delete.requested", "snapshot", &date, json!({ "backend": backend })).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConfirmDeviceBody {
    confirm_device: String,
}

/// `POST /macos/volumes/{device}/delete` — `diskutil apfs deleteVolume`; the device is typed back.
pub(crate) async fn delete_macos_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(device): Path<String>,
    Query(q): Query<BackendQuery>,
    Json(b): Json<ConfirmDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != device {
        return Err(AppError::Validation("confirm_device must equal the volume device".into()));
    }
    let (backend, ops) = ops(&q)?;
    ops.check_delete_volume(&device).await.map_err(map_check)?;
    let spec = JobSpec::MacosVolumeDelete {
        backend_id: backend.clone(),
        device: device.clone(),
        confirmed_device: b.confirm_device,
    };
    enqueue(&s, &actor, spec, "macos.volume.delete.requested", "volume", &device, json!({ "backend": backend })).await
}

/// `POST /macos/disks/{device}/eject`.
pub(crate) async fn eject_macos_disk(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(device): Path<String>,
    Query(q): Query<BackendQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    let (backend, ops) = ops(&q)?;
    ops.check_eject(&device).await.map_err(map_check)?;
    let spec = JobSpec::MacosDiskEject {
        backend_id: backend.clone(),
        device: device.clone(),
    };
    enqueue(&s, &actor, spec, "macos.disk.eject.requested", "disk", &device, json!({ "backend": backend })).await
}
