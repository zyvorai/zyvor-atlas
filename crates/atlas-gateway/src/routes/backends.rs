// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_api_types::{BackendMode, BackendType, StorageBackend};
use atlas_common::{AppError, AppResult};

use super::util::ceph_default_caps;
use crate::auth::Actor;
use crate::state::AppState;

// ---- backends ----

pub(crate) async fn list_backends(
    State(s): State<AppState>,
) -> AppResult<Json<Vec<StorageBackend>>> {
    Ok(Json(atlas_inventory::list_backends(&s.pool).await?))
}

#[derive(serde::Deserialize)]
pub(crate) struct DeleteBackendQuery {
    /// Also purge the backend's leftover inventory volume rows (no real storage is touched) — for a
    /// decommissioned/fixture backend whose discovered volumes have no live driver to delete through.
    #[serde(default)]
    purge: bool,
}

/// `DELETE /backends/{id}[?purge=true]` — remove a backend inventory row (e.g. a decommissioned or
/// fixture backend). Refused while volumes still reference it (so live volumes aren't orphaned)
/// unless `?purge=true`, which first drops the backend's orphaned inventory volume rows.
pub(crate) async fn delete_backend(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<DeleteBackendQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let volumes = atlas_inventory::backend_volume_count(&s.pool, &id).await?;
    let mut purged = 0u64;
    if volumes > 0 {
        if !q.purge {
            return Err(AppError::Validation(format!(
                "backend {id} still has {volumes} volume(s); remove them first or pass ?purge=true \
                 to drop the orphaned inventory rows"
            )));
        }
        purged = atlas_inventory::delete_volumes_by_backend(&s.pool, &id).await?;
    }
    atlas_inventory::delete_backend(&s.pool, &id).await?;
    Ok(Json(
        json!({ "backend_id": id, "deleted": true, "volumes_purged": purged }),
    ))
}

/// `GET /backends/summary` — per-backend inventory breakdown (type, clusters/volumes, capacity).
pub(crate) async fn backends_summary(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::backend_breakdown(&s.pool).await?
    )))
}

#[derive(Debug, Deserialize)]

pub(crate) struct CreateBackendBody {
    name: String,
    #[serde(default)]
    backend_type: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    /// Connection host: NFS server or ZFS host (used when instantiating an nfs/zfs backend live).
    #[serde(default)]
    server: Option<String>,
    /// NFS exports or ZFS zpools to surface.
    #[serde(default)]
    targets: Option<Vec<String>>,
}

/// Register a backend row. No cluster lifecycle happens here (PDF §8.1 keeps it 'pending').
pub(crate) async fn create_backend(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateBackendBody>,
) -> AppResult<Json<StorageBackend>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    let backend_type = match body.backend_type.as_deref() {
        None | Some("ceph") => BackendType::Ceph,
        Some("nfs") => BackendType::Nfs,
        Some("zfs") => BackendType::Zfs,
        Some("longhorn") => BackendType::Longhorn,
        Some("rustfs") => BackendType::Rustfs,
        Some("san") => BackendType::San,
        Some("cloud_block") => BackendType::CloudBlock,
        Some("kubernetes") => BackendType::Kubernetes,
        Some("native") => BackendType::Native,
        Some("weka") => BackendType::Weka,
        Some("macos") => BackendType::Macos,
        Some(other) => {
            return Err(AppError::Validation(format!(
                "unknown backend_type: {other}"
            )))
        }
    };
    let mode = match body.mode.as_deref() {
        Some("managed_rook") => BackendMode::ManagedRook,
        Some("read_only") => BackendMode::ReadOnly,
        _ => BackendMode::External,
    };
    let id = atlas_common::ids::backend_id();
    // NFS/ZFS drivers can be instantiated live and registered into the running driver registry, so
    // the backend actually discovers + serves — not just a catalog row. Others stay a `pending` row.
    let (status, live): (
        &str,
        Option<std::sync::Arc<dyn atlas_driver_core::StorageDriver>>,
    ) = match backend_type {
        BackendType::Nfs => {
            let server = body
                .server
                .clone()
                .unwrap_or_else(|| "nfs01.zyvor.lab".into());
            let exports = body
                .targets
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| vec!["/exports/vmstore".into(), "/exports/backups".into()]);
            let driver: std::sync::Arc<dyn atlas_driver_core::StorageDriver> =
                match s.config.nfs_driver_mode {
                    atlas_common::config::DriverMode::Real => std::sync::Arc::new(
                        atlas_driver_nfs::RealNfsDriver::new(&id, server, exports),
                    ),
                    atlas_common::config::DriverMode::Fake => std::sync::Arc::new(
                        atlas_driver_nfs::FakeNfsDriver::new(&id, server, exports),
                    ),
                };
            ("active", Some(driver))
        }
        BackendType::Zfs => {
            let host = body
                .server
                .clone()
                .unwrap_or_else(|| "zfs01.zyvor.lab".into());
            let pools = body
                .targets
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| vec!["tank".into(), "vault".into()]);
            let driver: std::sync::Arc<dyn atlas_driver_core::StorageDriver> =
                match s.config.zfs_driver_mode {
                    atlas_common::config::DriverMode::Real => std::sync::Arc::new(
                        atlas_driver_zfs::RealZfsDriver::new(&id, host, pools),
                    ),
                    atlas_common::config::DriverMode::Fake => std::sync::Arc::new(
                        atlas_driver_zfs::FakeZfsDriver::new(&id, host, pools),
                    ),
                };
            ("active", Some(driver))
        }
        BackendType::Rustfs => {
            return Err(AppError::Validation(
                "the RustFS product driver was removed; use Ceph RGW (backend_type=ceph) \
                 or point state-backup / DataBridge at any S3-compatible endpoint"
                    .into(),
            ));
        }
        // Longhorn is registered at startup with an in-cluster Kubernetes client.
        // Avoid persisting a pending row which would silently lack a live driver.
        BackendType::Longhorn => return Err(AppError::Validation(
            "enable Longhorn with ATLAS_LONGHORN_ENABLE=1 and restart the gateway".into(),
        )),
        BackendType::Native => return Err(AppError::Validation(
            "enable atlas-native with ATLAS_NATIVE_ENABLE=1 and ATLAS_NATIVE_ENDPOINTS, then restart the gateway".into(),
        )),
        BackendType::Macos => return Err(AppError::Validation(
            "enable macOS disk discovery with ATLAS_MACOS_ENABLE=1 on a Mac (ATLAS_MACOS_DRIVER_MODE=real), then restart the gateway".into(),
        )),
        BackendType::Weka => return Err(AppError::Validation(
            "enable WEKA discovery with ATLAS_WEKA_ENABLE=1 and ATLAS_WEKA_ENDPOINT, then restart the gateway".into(),
        )),
        _ => ("pending", None),
    };
    let backend = StorageBackend {
        id: id.clone(),
        name: body.name,
        backend_type,
        mode,
        status: status.into(),
        capabilities: ceph_default_caps(backend_type),
        connection_ref: None,
        cordoned: false,
    };
    atlas_inventory::upsert_backend(&s.pool, &backend).await?;
    if let Some(driver) = live {
        s.drivers.register(driver.clone());
        // Discover immediately so the new backend's pools/volumes appear.
        if let Err(e) = atlas_discovery::run_discovery(&s.pool, driver, None, None).await {
            tracing::warn!("discovery for new backend {id} failed: {e:#}");
        }
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "backend.create.succeeded",
        "backend",
        &backend.id,
        "success",
        Some(json!({ "name": backend.name, "type": backend.backend_type, "status": status })),
        None,
    )
    .await;
    Ok(Json(backend))
}

/// Trigger a discovery pass for a registered backend (PDF §8.1).
pub(crate) async fn discover_backend(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let driver = s
        .driver_for(&id)
        .ok_or_else(|| AppError::NotFound(format!("no driver registered for backend {id}")))?;

    let rbd_owners = s.rbd_owners().await;
    let rook_pool_kinds = s.rook_pool_kinds().await;
    let result = atlas_discovery::run_discovery(
        &s.pool,
        driver,
        rbd_owners.as_ref(),
        rook_pool_kinds.as_ref(),
    )
    .await;
    // Discovery only ever touches clusters/pools/osds/volumes — snapshots stuck in "creating"
    // need their own reconcile pass so a manual resync can actually resolve one instead of being
    // a no-op (see atlas_monitor::reconcile_snapshots doc comment).
    if let Err(e) = atlas_monitor::reconcile_snapshots(&s.pool, &s.k8s).await {
        tracing::warn!("snapshot reconcile during backend discover failed: {e:#}");
    }
    let (status, payload) = match &result {
        Ok(sum) => ("success", serde_json::to_value(sum).unwrap_or(Value::Null)),
        Err(e) => ("failed", json!({ "error": e.to_string() })),
    };
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "backend.discover.requested",
        "backend",
        &id,
        status,
        None,
        Some(payload.clone()),
    )
    .await;

    let summary = result.map_err(AppError::from)?;
    Ok(Json(json!({ "state": "succeeded", "summary": summary })))
}
