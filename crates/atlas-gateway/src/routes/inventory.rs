// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_api_types::{Capabilities, StorageClassInfo};
use atlas_common::{AppError, AppResult};

use super::util::CEPH_BACKEND_ID;
use crate::auth::Actor;
use crate::state::AppState;

// ---- clusters / inventory ----

pub(crate) async fn list_clusters(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(atlas_inventory::list_clusters(&s.pool).await?)))
}

pub(crate) async fn cluster_health(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let h = atlas_inventory::cluster_health(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("cluster {id}")))?;
    Ok(Json(json!(h)))
}

pub(crate) async fn cluster_capabilities(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Capabilities>> {
    let cluster = atlas_inventory::get_cluster(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("cluster {id}")))?;
    let backends = atlas_inventory::list_backends(&s.pool).await?;
    let caps = backends
        .into_iter()
        .find(|b| b.id == cluster.backend_id)
        .map(|b| b.capabilities)
        .unwrap_or_default();
    Ok(Json(caps))
}

/// Storage nodes are derived from distinct OSD hosts in the MVP.
pub(crate) async fn list_nodes(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let osds = atlas_inventory::list_osds(&s.pool).await?;
    let mut hosts: Vec<String> = osds.into_iter().filter_map(|o| o.host).collect();
    hosts.sort();
    hosts.dedup();
    let nodes: Vec<Value> = hosts.into_iter().map(|h| json!({ "host": h })).collect();
    Ok(Json(json!(nodes)))
}

pub(crate) async fn list_osds(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(atlas_inventory::list_osds(&s.pool).await?)))
}

/// `GET /ceph/status` — live `ceph status` from the cluster (health, quorum, osdmap, pgmap, I/O).
pub(crate) async fn get_ceph_status(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    Ok(Json(
        d.ceph_status()
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    ))
}

/// `GET /ceph/osd-tree` — the CRUSH hierarchy (roots → hosts → OSDs).
pub(crate) async fn get_ceph_osd_tree(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    Ok(Json(
        d.ceph_osd_tree()
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    ))
}

/// `GET /ceph/df` — cluster + per-pool capacity/usage/objects (`ceph df detail`).
pub(crate) async fn get_ceph_df(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    Ok(Json(
        d.ceph_df()
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    ))
}

/// `GET /ceph/osd-df` — per-OSD utilization (`ceph osd df`).
pub(crate) async fn get_ceph_osd_df(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    Ok(Json(
        d.ceph_osd_df()
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    ))
}

/// `CephCluster.status.ceph.health` (`"HEALTH_OK"`/`_WARN`/`_ERR`), when a k8s driver is attached
/// and the CR exists — `None` on any absence/failure (non-Rook Ceph, no cluster attached, CR not
/// yet reconciled), never an error, since this is only ever a cross-check/fallback input.
async fn rook_cluster_health(s: &AppState) -> Option<String> {
    let k8s = s.k8s.as_ref()?;
    match k8s
        .get_ceph_cluster_status(&s.config.rook_namespace, &s.config.rook_cluster_name)
        .await
    {
        Ok(Some(status)) => status
            .pointer("/ceph/health")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Ok(None) => None,
        Err(e) => {
            tracing::debug!("get_ceph_cluster_status failed: {e}");
            None
        }
    }
}

/// `GET /ceph/health-rollup` — Atlas's own Healthy/Degraded/Rebuilding/At-Risk/Critical severity,
/// synthesized from status/osd-tree/osd-df so a caller doesn't have to parse Ceph's own health
/// vocabulary. See `atlas_driver_ceph::health_rollup` for the classification rules.
///
/// Cross-checked against `CephCluster.status.ceph.health` (Rook's own view) when a k8s driver is
/// attached — a disagreement is surfaced via `rook_disagreement` rather than silently picked. When
/// the `ceph` CLI path itself fails (no keyring/socket) but the Rook CR is reachable, this falls
/// back to a coarser Rook-CRD-only rollup instead of erroring out.
pub(crate) async fn get_ceph_health_rollup(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let rook_health = rook_cluster_health(&s).await;
    let rollup = match s.driver_for(CEPH_BACKEND_ID) {
        Some(d) => match atlas_driver_ceph::health_rollup::compute(d.as_ref()).await {
            Ok(mut r) => {
                atlas_driver_ceph::health_rollup::merge_rook_health(&mut r, rook_health.as_deref());
                r
            }
            Err(e) => match &rook_health {
                Some(h) => {
                    tracing::warn!(
                        "ceph CLI health-rollup failed ({e}); falling back to Rook CRD-only health"
                    );
                    atlas_driver_ceph::health_rollup::from_rook_only(h)
                }
                None => return Err(AppError::Driver(e.to_string())),
            },
        },
        None => match &rook_health {
            Some(h) => atlas_driver_ceph::health_rollup::from_rook_only(h),
            None => return Err(AppError::Driver("no ceph driver".into())),
        },
    };
    Ok(Json(json!(rollup)))
}

/// `GET /ceph/rook-status` — the raw Rook CR view, additive alongside `/ceph/status` (the CLI
/// path): `CephCluster` phase/health plus every `CephBlockPool`/`CephFilesystem`/`CephObjectStore`
/// CR's name and phase. `404` when no k8s driver is attached (non-Kubernetes deployments).
pub(crate) async fn get_rook_status(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let k8s = s
        .k8s
        .as_ref()
        .ok_or_else(|| AppError::NotFound("no kubernetes driver attached".into()))?;
    let ns = &s.config.rook_namespace;
    let cluster = k8s
        .get_ceph_cluster_status(ns, &s.config.rook_cluster_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let block_pools = k8s
        .list_ceph_block_pools(ns)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let filesystems = k8s
        .list_ceph_filesystems(ns)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    let object_stores = k8s
        .list_ceph_object_stores(ns)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!({
        "namespace": ns,
        "cluster": {
            "name": s.config.rook_cluster_name,
            "phase": cluster.as_ref().and_then(|c| c.get("phase")).and_then(|v| v.as_str()),
            "health": cluster.as_ref().and_then(|c| c.pointer("/ceph/health")).and_then(|v| v.as_str()),
        },
        "block_pools": block_pools,
        "filesystems": filesystems,
        "object_stores": object_stores,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PoolQuery {
    backend: Option<String>,
    kind: Option<String>,
}

pub(crate) async fn list_pools(
    State(s): State<AppState>,
    Query(q): Query<PoolQuery>,
) -> AppResult<Json<Value>> {
    let pools = if q.backend.is_some() || q.kind.is_some() {
        atlas_inventory::list_pools_filtered(&s.pool, q.backend.as_deref(), q.kind.as_deref())
            .await?
    } else {
        atlas_inventory::list_pools(&s.pool).await?
    };
    Ok(Json(json!(pools)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct VolumeQuery {
    state: Option<String>,
    tenant: Option<String>,
    backend: Option<String>,
    kind: Option<String>,
}

pub(crate) async fn list_volumes(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(mut q): Query<VolumeQuery>,
) -> AppResult<Json<Value>> {
    // Tenant isolation: a non-admin actor can only ever see their own tenant's volumes, even if
    // they pass a different `?tenant=` explicitly.
    if let Some(t) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        q.tenant = Some(t.to_string());
    }
    let vols = if q.state.is_some() || q.tenant.is_some() || q.backend.is_some() || q.kind.is_some()
    {
        atlas_inventory::list_volumes_filtered(
            &s.pool,
            q.state.as_deref(),
            q.tenant.as_deref(),
            q.backend.as_deref(),
            q.kind.as_deref(),
        )
        .await?
    } else {
        atlas_inventory::list_volumes(&s.pool).await?
    };
    Ok(Json(json!(vols)))
}

/// `GET /volumes.csv[?state=&tenant=&backend=&kind=]` — the volume inventory as a downloadable CSV
/// (honors the same filters as `/volumes`), for spreadsheets / capacity reporting.
pub(crate) async fn volumes_csv(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(mut q): Query<VolumeQuery>,
) -> impl axum::response::IntoResponse {
    use std::fmt::Write;
    if let Some(t) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        q.tenant = Some(t.to_string());
    }
    let vols = if q.state.is_some() || q.tenant.is_some() || q.backend.is_some() || q.kind.is_some()
    {
        atlas_inventory::list_volumes_filtered(
            &s.pool,
            q.state.as_deref(),
            q.tenant.as_deref(),
            q.backend.as_deref(),
            q.kind.as_deref(),
        )
        .await
        .unwrap_or_default()
    } else {
        atlas_inventory::list_volumes(&s.pool)
            .await
            .unwrap_or_default()
    };

    let mut out = String::from(
        "id,name,kind,state,size_bytes,used_bytes,cluster_id,pool_id,namespace,pvc,storage_class\n",
    );
    let cell = |o: &mut String, v: &str, last: bool| {
        // Neutralize CSV formula injection: a cell starting with =/+/-/@ can be interpreted as a
        // formula by spreadsheet apps (Excel/Sheets) when this export is opened.
        let guarded = if matches!(v.as_bytes().first(), Some(b'=' | b'+' | b'-' | b'@')) {
            format!("'{v}")
        } else {
            v.to_string()
        };
        if guarded.contains([',', '"', '\n']) {
            let _ = write!(o, "\"{}\"", guarded.replace('"', "\"\""));
        } else {
            let _ = write!(o, "{guarded}");
        }
        o.push(if last { '\n' } else { ',' });
    };
    for vol in &vols {
        let val = serde_json::to_value(vol).unwrap_or(Value::Null);
        let g = |k: &str| match val.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        cell(&mut out, &g("id"), false);
        cell(&mut out, &g("name"), false);
        cell(&mut out, &g("kind"), false);
        cell(&mut out, &g("state"), false);
        cell(&mut out, &g("size_bytes"), false);
        cell(&mut out, &g("used_bytes"), false);
        cell(&mut out, &g("cluster_id"), false);
        cell(&mut out, &g("pool_id"), false);
        cell(&mut out, &g("kubernetes_namespace"), false);
        cell(&mut out, &g("pvc_name"), false);
        cell(&mut out, &g("storage_class_name"), true);
    }
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"atlas-volumes.csv\"",
            ),
        ],
        out,
    )
}

pub(crate) async fn get_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let v = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("volume {id}"),
    )?;
    Ok(Json(json!(v)))
}
// ---- live Kubernetes ----

pub(crate) async fn list_storage_classes(
    State(s): State<AppState>,
) -> AppResult<Json<Vec<StorageClassInfo>>> {
    let k8s = s
        .k8s
        .as_ref()
        .ok_or_else(|| AppError::Driver("no reachable Kubernetes cluster".into()))?;
    let classes = k8s
        .list_storage_classes()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(classes))
}

pub(crate) async fn list_pvcs(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let k8s = s
        .k8s
        .as_ref()
        .ok_or_else(|| AppError::Driver("no reachable Kubernetes cluster".into()))?;
    let pvcs = k8s
        .list_pvcs(None)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(pvcs)))
}

pub(crate) async fn list_pvs(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let k8s = s
        .k8s
        .as_ref()
        .ok_or_else(|| AppError::Driver("no reachable Kubernetes cluster".into()))?;
    let pvs = k8s
        .list_pvs()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(pvs)))
}
