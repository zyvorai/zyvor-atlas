// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Volume lifecycle for a Mac's external disks (`docs/MACOS.md`): erase, add/delete an APFS volume,
//! eject. Admin only; erase and delete need the target typed back; each runs as a job and is audited.
//! The refusals (internal disk, running system, system volumes, disk images) are checked here for a
//! fast 409 and again inside the job. Local to the Mac the gateway runs on.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_driver_core::DriverError;
use atlas_jobs::JobSpec;

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

fn ops() -> AppResult<atlas_driver_macos::MacosOps> {
    let settings = atlas_driver_macos::EnvSettings::from_env();
    if !settings.enabled {
        return Err(AppError::Unavailable(
            "the macOS backend is not enabled (set ATLAS_MACOS_ENABLE=1 and restart the gateway)"
                .into(),
        ));
    }
    Ok(settings.ops())
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
pub(crate) async fn list_macos_devices(State(_s): State<AppState>) -> AppResult<Json<Value>> {
    let rows = ops()?.devices().await.map_err(map_check)?;
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
    Json(b): Json<EraseBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != b.device {
        return Err(AppError::Validation("confirm_device must equal device".into()));
    }
    ops()?.check_erase(&b.device, &b.name).await.map_err(map_check)?;
    let spec = JobSpec::MacosDiskErase {
        backend_id: crate::startup::MACOS_BACKEND_ID.into(),
        device: b.device.clone(),
        confirmed_device: b.confirm_device,
        name: b.name.clone(),
    };
    enqueue(&s, &actor, spec, "macos.disk.erase.requested", "disk", &b.device, json!({ "name": b.name }))
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
    Json(b): Json<AddVolumeBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if let (Some(q), Some(r)) = (b.quota_bytes, b.reserve_bytes) {
        if r > q {
            return Err(AppError::Validation("reserve_bytes cannot exceed quota_bytes".into()));
        }
    }
    ops()?.check_add_volume(&container, &b.name).await.map_err(map_check)?;
    let spec = JobSpec::MacosVolumeAdd {
        backend_id: crate::startup::MACOS_BACKEND_ID.into(),
        container: container.clone(),
        name: b.name.clone(),
        quota_bytes: b.quota_bytes,
        reserve_bytes: b.reserve_bytes,
    };
    enqueue(
        &s, &actor, spec, "macos.volume.add.requested", "container", &container,
        json!({ "name": b.name, "quota_bytes": b.quota_bytes, "reserve_bytes": b.reserve_bytes }),
    )
    .await
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
    Json(b): Json<ConfirmDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    if b.confirm_device != device {
        return Err(AppError::Validation("confirm_device must equal the volume device".into()));
    }
    ops()?.check_delete_volume(&device).await.map_err(map_check)?;
    let spec = JobSpec::MacosVolumeDelete {
        backend_id: crate::startup::MACOS_BACKEND_ID.into(),
        device: device.clone(),
        confirmed_device: b.confirm_device,
    };
    enqueue(&s, &actor, spec, "macos.volume.delete.requested", "volume", &device, json!({})).await
}

/// `POST /macos/disks/{device}/eject`.
pub(crate) async fn eject_macos_disk(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(device): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    admin(&s, &actor)?;
    ops()?.check_eject(&device).await.map_err(map_check)?;
    let spec = JobSpec::MacosDiskEject {
        backend_id: crate::startup::MACOS_BACKEND_ID.into(),
        device: device.clone(),
    };
    enqueue(&s, &actor, spec, "macos.disk.eject.requested", "disk", &device, json!({})).await
}
