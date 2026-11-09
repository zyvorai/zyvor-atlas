// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Provision a raw, unformatted local block device into a brand-new ZFS pool. Mirrors
//! `routes::rook`'s create-pool shape (async job, `202 Accepted`), but this mutation is genuinely
//! destructive (formats a disk), so it additionally requires an explicit `confirm: true`.

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

#[derive(Debug, Deserialize)]
pub(crate) struct CreateZfsPoolFromDeviceBody {
    pool_name: String,
    device_path: String,
    confirm: bool,
    /// Explicit second opt-in to clear a residual partition table/filesystem/RAID/LVM signature
    /// before formatting — e.g. a disk that previously backed a Ceph OSD. Without this, a device
    /// that isn't genuinely empty is refused (see `docs/DISKS.md`). Never overrides the
    /// unconditional refusals (root/boot disk, mounted, read-only, not a whole disk).
    #[serde(default)]
    wipe_existing: bool,
}

/// `POST /zfs/pools/from-device` — provision a brand-new zpool on a raw, unformatted local disk
/// (async job). Local-host-only: the gateway process must be running on the host that owns
/// `device_path` (same limitation `RealZfsDriver` itself already has — ZFS has no remote query
/// protocol, unlike Ceph).
pub(crate) async fn create_zfs_pool_from_device(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateZfsPoolFromDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required to format a raw device".into(),
        ));
    }
    atlas_common::device::validate_raw_device_path(&body.device_path)
        .map_err(AppError::Validation)?;
    super::util::validate_k8s_name(&body.pool_name)?;
    // The synthesized pool/volume rows this job writes reference `ZFS_BACKEND_ID` as their
    // backend_id (a NOT NULL foreign key into storage_backends) — that row only exists once the
    // static ZFS backend was actually registered at startup (`ATLAS_ZFS_ENABLE=1`). Fail fast here
    // with a clear message instead of a cryptic FK-violation surfacing deep in job dispatch.
    if !s.config.zfs_enable {
        return Err(AppError::Unavailable(
            "the ZFS backend is not enabled (set ATLAS_ZFS_ENABLE=1 and restart the gateway)"
                .into(),
        ));
    }

    let host = s
        .config
        .zfs_host
        .clone()
        .unwrap_or_else(|| "zfs01.zyvor.lab".into());
    let spec = JobSpec::ZfsPoolCreateFromDevice {
        backend_id: crate::startup::ZFS_BACKEND_ID.into(),
        pool_name: body.pool_name.clone(),
        device_path: body.device_path.clone(),
        confirmed_device_path: body.device_path.clone(),
        host,
        wipe_existing: body.wipe_existing,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "zfs.pool.create_from_device.requested",
        "pool",
        &body.pool_name,
        "accepted",
        Some(json!({ "device": body.device_path, "wipe_existing": body.wipe_existing })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "pool_name": body.pool_name, "device": body.device_path }),
    ))
}

/// The only device fake mode ever reports — mirrors `atlas_jobs::dispatch::zfs::FAKE_FIXTURE_DEVICE`
/// (kept as a separate literal: that constant is private to the job-dispatch crate, and this is
/// display-only data, not a safety-relevant check).
const FAKE_FIXTURE_DEVICE: &str = "/dev/vdz";

/// `GET /zfs/devices` — every whole disk the gateway's own host currently sees, with its current
/// signature state (empty / has data / mounted / root-boot disk), so an operator can pick a device
/// instead of guessing a path blind. Advisory only: `POST /zfs/pools/from-device` re-runs the
/// authoritative safety check itself against whatever's chosen. Local-host-only, same limitation
/// as the rest of this driver — this can never see a disk on a different node.
pub(crate) async fn list_zfs_devices(State(s): State<AppState>) -> AppResult<Json<Value>> {
    if s.config.zfs_driver_mode == atlas_common::config::DriverMode::Fake {
        return Ok(Json(json!([{
            "path": FAKE_FIXTURE_DEVICE,
            "size_bytes": 10_737_418_240u64,
            "read_only": false,
            "has_children": false,
            "fstype": null,
            "pttype": null,
            "mounted_at": null,
            "wipefs_signatures": [],
            "member_of_zpool": null,
            "is_root_or_boot_disk": false,
            "status": "empty",
        }])));
    }
    let devices = atlas_driver_zfs::list_whole_disks()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let rows: Vec<Value> = devices
        .iter()
        .map(|d| {
            let mut v = serde_json::to_value(d).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("status".into(), json!(d.status()));
            }
            v
        })
        .collect();
    Ok(Json(json!(rows)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DestroyZfsPoolBody {
    /// Must equal the pool name in the URL — typed by the operator as the explicit confirmation.
    confirm_pool_name: String,
}

/// `POST /zfs/pools/{name}/destroy` — `zpool destroy` a pool Atlas knows about (async job). Refused
/// while inventory or the host still shows any dataset/volume inside it; the member disk is left
/// carrying ZFS labels and reappears in the Disks picker as a wipeable device.
pub(crate) async fn destroy_zfs_pool(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Json(body): Json<DestroyZfsPoolBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    super::util::validate_k8s_name(&name)?;
    if body.confirm_pool_name != name {
        return Err(AppError::Validation(
            "confirm_pool_name must equal the pool name".into(),
        ));
    }
    if !s.config.zfs_enable {
        return Err(AppError::Unavailable(
            "the ZFS backend is not enabled (set ATLAS_ZFS_ENABLE=1 and restart the gateway)"
                .into(),
        ));
    }
    let pool_row_id = atlas_driver_zfs::pool_id(&name);
    if !atlas_inventory::pool_row_exists(&s.pool, &pool_row_id).await? {
        return Err(AppError::NotFound(format!(
            "zfs pool {name} is not in inventory"
        )));
    }
    let root_volume = atlas_driver_zfs::root_volume_id(&name);
    let extra: Vec<String> = atlas_inventory::volume_ids_in_pool(&s.pool, &pool_row_id)
        .await?
        .into_iter()
        .filter(|v| *v != root_volume)
        .collect();
    if !extra.is_empty() {
        return Err(AppError::Conflict(format!(
            "pool {name} still holds {} volume(s): {}",
            extra.len(),
            extra.join(", ")
        )));
    }
    let spec = JobSpec::ZfsPoolDestroy {
        backend_id: crate::startup::ZFS_BACKEND_ID.into(),
        pool_name: name.clone(),
        confirmed_pool_name: body.confirm_pool_name.clone(),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "zfs.pool.destroy.requested",
        "pool",
        &name,
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(&job, json!({ "pool_name": name })))
}
