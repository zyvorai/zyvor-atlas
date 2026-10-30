// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Rook lifecycle routes: create/list/delete `CephBlockPool`/`CephFilesystem`/`CephObjectStore`
//! CRs (+ their StorageClass) from the API instead of hand-edited YAML manifests. Mirrors
//! `routes::object_store`'s bucket create/delete shape (async job, dependent-delete guard).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::accepted;
use super::volumes::ForceParams;
use crate::auth::Actor;
use crate::state::AppState;

fn require_k8s(s: &AppState) -> AppResult<()> {
    if s.k8s.is_none() {
        return Err(AppError::Unavailable(
            "no kubernetes cluster attached; cannot manage Rook resources".into(),
        ));
    }
    Ok(())
}

// ---- pools ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePoolBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    replicated_size: Option<i64>,
    failure_domain: Option<String>,
    device_class: Option<String>,
}

/// `GET /ceph/pools` — every live `CephBlockPool` CR (name + phase).
pub(crate) async fn list_ceph_pools(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let pools = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_block_pools(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(pools)))
}

/// `POST /ceph/pools` — create a `CephBlockPool` + matching StorageClass (async job).
pub(crate) async fn create_ceph_pool(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreatePoolBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}", body.name));
    let spec = JobSpec::CephPoolCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
        failure_domain: body.failure_domain.unwrap_or_else(|| "host".into()),
        device_class: body.device_class,
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
        "ceph.pool.create.requested",
        "pool",
        &body.name,
        "accepted",
        Some(json!({ "storage_class": storage_class })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

/// `DELETE /ceph/pools/{name}[?force=true]` — delete the CR + StorageClass; blocked (409) while
/// any volume still references the StorageClass, unless `force=true`.
pub(crate) async fn delete_ceph_pool(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}");
    let deps = atlas_inventory::count_volumes_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "pool {name}'s StorageClass {storage_class} still has {deps} volume(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephPoolDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}

// ---- filesystems ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreateFilesystemBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    data_pool_name: Option<String>,
    replicated_size: Option<i64>,
}

/// `GET /ceph/filesystems` — every live `CephFilesystem` CR (name + phase).
pub(crate) async fn list_ceph_filesystems(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let fs = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_filesystems(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(fs)))
}

/// `POST /ceph/filesystems` — create a `CephFilesystem` (RWX CephFS) + StorageClass (async job).
pub(crate) async fn create_ceph_filesystem(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateFilesystemBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}-shared", body.name));
    let spec = JobSpec::CephFilesystemCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        data_pool_name: body.data_pool_name.unwrap_or_else(|| "data0".into()),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

/// `DELETE /ceph/filesystems/{name}[?force=true]`.
pub(crate) async fn delete_ceph_filesystem(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}-shared");
    let deps = atlas_inventory::count_volumes_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "filesystem {name}'s StorageClass {storage_class} still has {deps} volume(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephFilesystemDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}

// ---- object stores ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreateObjectStoreBody {
    name: String,
    namespace: Option<String>,
    storage_class: Option<String>,
    replicated_size: Option<i64>,
    gateway_port: Option<i64>,
    gateway_instances: Option<i64>,
}

/// `GET /ceph/object-stores` — every live `CephObjectStore` CR (name + phase).
pub(crate) async fn list_ceph_object_stores(State(s): State<AppState>) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let stores = s
        .k8s
        .as_ref()
        .unwrap()
        .list_ceph_object_stores(&s.config.rook_namespace)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!(stores)))
}

/// `POST /ceph/object-stores` — create a `CephObjectStore` (RGW) + bucket StorageClass (async job).
pub(crate) async fn create_ceph_object_store(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateObjectStoreBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    require_k8s(&s)?;
    super::util::validate_k8s_name(&body.name)?;
    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let storage_class = body
        .storage_class
        .unwrap_or_else(|| format!("zyvor-{}-bucket", body.name));
    let spec = JobSpec::CephObjectStoreCreate {
        name: body.name.clone(),
        namespace,
        storage_class: storage_class.clone(),
        replicated_size: body.replicated_size.unwrap_or(3).clamp(1, 9),
        gateway_port: body.gateway_port.unwrap_or(80),
        gateway_instances: body.gateway_instances.unwrap_or(1).max(1),
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "name": body.name, "storage_class": storage_class }),
    ))
}

// ---- raw disk -> OSD provisioning ----

#[derive(Debug, Deserialize)]
pub(crate) struct ListNodeDevicesParams {
    namespace: Option<String>,
}

/// `GET /ceph/nodes/{node_name}/devices` — Rook's own device-discovery data for one node (the same
/// oracle `add_ceph_device` checks against, see below), surfaced read-only so an operator can see
/// what's actually there before picking a `device_path` instead of guessing. Returns an empty list
/// (not an error) when Rook's discovery ConfigMap doesn't exist yet — the mutating path below still
/// fails closed on its own if that data is genuinely required for a claim.
pub(crate) async fn list_ceph_node_devices(
    State(s): State<AppState>,
    Path(node_name): Path<String>,
    Query(q): Query<ListNodeDevicesParams>,
) -> AppResult<Json<Value>> {
    require_k8s(&s)?;
    let namespace = q
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let devices = s
        .k8s
        .as_ref()
        .unwrap()
        .get_local_devices_classified(&namespace, &node_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?
        .unwrap_or_default();
    Ok(Json(json!(devices)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddCephDeviceBody {
    node_name: String,
    device_path: String,
    confirm: bool,
    namespace: Option<String>,
    cluster_name: Option<String>,
    /// Put this OSD's RocksDB (and WAL) on a separate, faster device, e.g. `/dev/nvme1n1`.
    db_device: Option<String>,
    db_size_mb: Option<u64>,
    wal_size_mb: Option<u64>,
    osds_per_device: Option<u32>,
}

/// Largest DB/WAL size accepted, in MB (4 TiB): a typo guard, not a Ceph limit.
const MAX_DB_WAL_MB: u64 = 4 * 1024 * 1024;
const MAX_OSDS_PER_DEVICE: u32 = 16;

/// Shape checks on the BlueStore layout fields, independent of any cluster state.
fn validate_layout(body: &AddCephDeviceBody) -> Result<(), String> {
    if body.db_device.is_none() && (body.db_size_mb.is_some() || body.wal_size_mb.is_some()) {
        return Err("db_size_mb and wal_size_mb need db_device".into());
    }
    for (name, v) in [("db_size_mb", body.db_size_mb), ("wal_size_mb", body.wal_size_mb)] {
        if v.is_some_and(|v| v == 0 || v > MAX_DB_WAL_MB) {
            return Err(format!("{name} must be between 1 and {MAX_DB_WAL_MB}"));
        }
    }
    if body
        .osds_per_device
        .is_some_and(|n| n == 0 || n > MAX_OSDS_PER_DEVICE)
    {
        return Err(format!("osds_per_device must be between 1 and {MAX_OSDS_PER_DEVICE}"));
    }
    if let Some(db) = &body.db_device {
        atlas_common::device::validate_raw_device_path(db)
            .map_err(|e| format!("db_device: {e}"))?;
        if db == &body.device_path {
            return Err("db_device must differ from device_path".into());
        }
    }
    Ok(())
}

/// Whether `db` (bare name) may hold the DB of a new OSD on this node, given Rook's discovery data
/// and the node's current entry in the CephCluster CR. A shared DB device is by design not empty
/// once one OSD uses it, so it is accepted when it is either reported empty, or is already the
/// `metadataDevice` of another device on this node. It is refused when it is a rotational disk
/// (defeats the purpose), unknown to discovery, or itself listed as an OSD data device.
fn check_db_device(
    db: &str,
    node: Option<&atlas_driver_k8s::rook::CephClusterNodeSpec>,
    discovery: &[Value],
) -> Result<(), String> {
    let entry = discovery
        .iter()
        .find(|d| d.get("name").and_then(Value::as_str) == Some(db))
        .ok_or_else(|| format!("db_device {db} is not visible to Rook's discovery on this node"))?;
    if atlas_driver_k8s::rook::classify_device(entry) == "hdd" {
        return Err(format!("db_device {db} is a rotational disk; refusing to put an OSD DB on it"));
    }
    if node.is_some_and(|n| n.devices.iter().any(|d| d.name == db)) {
        return Err(format!("db_device {db} is already an OSD data device on this node"));
    }
    let empty = entry.get("empty").and_then(Value::as_bool).unwrap_or(false)
        && !entry
            .get("filesystem")
            .and_then(Value::as_str)
            .is_some_and(|f| !f.is_empty());
    let already_shared = node.is_some_and(|n| {
        n.devices
            .iter()
            .any(|d| d.config.get("metadataDevice").map(String::as_str) == Some(db))
    });
    if !empty && !already_shared {
        return Err(format!(
            "db_device {db} is not reported empty by Rook's discovery and is not already a shared \
             metadataDevice on this node"
        ));
    }
    Ok(())
}

/// `POST /ceph/devices` — claim a raw, unformatted disk on a specific Kubernetes node as a new
/// Ceph OSD via Rook (async job: patches the `CephCluster` CR's device list, then polls — via the
/// job engine's own retry machinery, not a blocking wait — until Rook actually produces the OSD).
pub(crate) async fn add_ceph_device(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<AddCephDeviceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required to claim a raw device as an OSD".into(),
        ));
    }
    atlas_common::device::validate_raw_device_path(&body.device_path)
        .map_err(AppError::Validation)?;
    validate_layout(&body).map_err(AppError::Validation)?;
    require_k8s(&s)?;

    let namespace = body
        .namespace
        .unwrap_or_else(|| s.config.rook_namespace.clone());
    let cluster_name = body
        .cluster_name
        .unwrap_or_else(|| s.config.rook_cluster_name.clone());
    let k8s = s.k8s.as_ref().unwrap();

    if k8s
        .get_node(&body.node_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?
        .is_none()
    {
        return Err(AppError::Validation(format!(
            "no such Kubernetes node: {}",
            body.node_name
        )));
    }
    let storage = k8s
        .get_ceph_cluster_storage(&namespace, &cluster_name)
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?
        .ok_or_else(|| AppError::NotFound(format!("CephCluster {namespace}/{cluster_name}")))?;
    if storage.use_all_devices {
        return Err(AppError::Validation(
            "this cluster has useAllDevices=true — it already auto-claims every empty device; \
             this API only applies when devices are pinned explicitly per node"
                .into(),
        ));
    }

    // Safety oracle: the gateway has no remote-exec mechanism to shell `lsblk`/`findmnt` against
    // an arbitrary Kubernetes node the way it can for a local ZFS host, so this reads Rook's own
    // device-discovery ConfigMap instead. Fail closed (503) if that data isn't there at all —
    // never assume a device is safe to claim when this oracle is unavailable.
    let device_basename = body.device_path.trim_start_matches("/dev/");
    match k8s
        .ceph_device_reports_empty(&namespace, &body.node_name, device_basename)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(AppError::Validation(format!(
                "device {} on node {} is not reported empty by Rook's discovery (already has a \
                 filesystem or is in use)",
                body.device_path, body.node_name
            )))
        }
        Err(atlas_driver_k8s::K8sError::NotFound(msg)) => {
            return Err(AppError::Unavailable(format!(
                "cannot safety-verify {}: {msg} — enable Rook's discovery daemon \
                 (ROOK_ENABLE_DISCOVERY_DAEMON) before using this API",
                body.device_path
            )))
        }
        Err(e) => return Err(AppError::Driver(e.to_string())),
    }

    if let Some(db) = &body.db_device {
        let discovery = k8s
            .get_local_devices(&namespace, &body.node_name)
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?
            .ok_or_else(|| {
                AppError::Unavailable(format!(
                    "cannot verify db_device {db}: Rook device discovery has no data for node {}",
                    body.node_name
                ))
            })?;
        let node = storage.nodes.iter().find(|n| n.name == body.node_name);
        check_db_device(db.trim_start_matches("/dev/"), node, &discovery)
            .map_err(AppError::Validation)?;
    }

    let spec = JobSpec::CephOsdAddDevice {
        namespace,
        cluster_name,
        node_name: body.node_name.clone(),
        device_path: body.device_path.clone(),
        confirmed_device_path: body.device_path.clone(),
        db_device: body.db_device.clone(),
        db_size_mb: body.db_size_mb,
        wal_size_mb: body.wal_size_mb,
        osds_per_device: body.osds_per_device,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    // Generous retry budget: Rook's reconciliation can take several minutes. Backoff is capped at
    // 64s/attempt (engine.rs), so 30 retries gives roughly 25-30 minutes of polling before the job
    // is finally marked failed.
    const CEPH_OSD_ADD_MAX_RETRIES: i64 = 30;
    let _ = atlas_inventory::jobs::set_max_retries(&s.pool, &job_id, CEPH_OSD_ADD_MAX_RETRIES).await;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "ceph.osd.add_device.requested",
        "node",
        &body.node_name,
        "accepted",
        Some(json!({
            "device": body.device_path,
            "db_device": body.db_device,
            "db_size_mb": body.db_size_mb,
            "wal_size_mb": body.wal_size_mb,
            "osds_per_device": body.osds_per_device,
        })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "node": body.node_name, "device": body.device_path, "db_device": body.db_device }),
    ))
}

/// `DELETE /ceph/object-stores/{name}[?force=true]` — blocked (409) while any bucket still
/// references the store's StorageClass.
pub(crate) async fn delete_ceph_object_store(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    require_k8s(&s)?;
    let storage_class = format!("zyvor-{name}-bucket");
    let deps = atlas_inventory::buckets::count_by_storage_class(&s.pool, &storage_class).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "object store {name}'s StorageClass {storage_class} still has {deps} bucket(s); delete them first or pass ?force=true"
        )));
    }
    let spec = JobSpec::CephObjectStoreDelete {
        name: name.clone(),
        namespace: s.config.rook_namespace.clone(),
        storage_class,
    };
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "name": name })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_driver_k8s::rook::{CephClusterDeviceSpec, CephClusterNodeSpec};

    fn body() -> AddCephDeviceBody {
        AddCephDeviceBody {
            node_name: "n1".into(),
            device_path: "/dev/sdb".into(),
            confirm: true,
            namespace: None,
            cluster_name: None,
            db_device: None,
            db_size_mb: None,
            wal_size_mb: None,
            osds_per_device: None,
        }
    }

    fn node(devices: Vec<CephClusterDeviceSpec>) -> CephClusterNodeSpec {
        CephClusterNodeSpec { name: "n1".into(), devices, extra: Default::default() }
    }

    fn dev(name: &str, db: Option<&str>) -> CephClusterDeviceSpec {
        let mut config = std::collections::BTreeMap::new();
        if let Some(db) = db {
            config.insert("metadataDevice".to_string(), db.to_string());
        }
        CephClusterDeviceSpec { name: name.into(), config, extra: Default::default() }
    }

    #[test]
    fn layout_fields_are_bounded_and_need_a_db_device() {
        assert!(validate_layout(&body()).is_ok());
        let sizes_alone = AddCephDeviceBody { db_size_mb: Some(1024), ..body() };
        assert!(validate_layout(&sizes_alone).is_err());
        let zero = AddCephDeviceBody { db_device: Some("/dev/nvme1n1".into()), db_size_mb: Some(0), ..body() };
        assert!(validate_layout(&zero).is_err());
        let huge = AddCephDeviceBody { osds_per_device: Some(99), ..body() };
        assert!(validate_layout(&huge).is_err());
        let ok = AddCephDeviceBody {
            db_device: Some("/dev/nvme1n1".into()),
            db_size_mb: Some(61440),
            wal_size_mb: Some(2048),
            osds_per_device: Some(2),
            ..body()
        };
        assert!(validate_layout(&ok).is_ok());
    }

    #[test]
    fn db_device_must_be_a_safe_distinct_whole_disk() {
        let same = AddCephDeviceBody { db_device: Some("/dev/sdb".into()), ..body() };
        assert!(validate_layout(&same).is_err());
        let boot = AddCephDeviceBody { db_device: Some("/dev/sda".into()), ..body() };
        assert!(validate_layout(&boot).is_err(), "the first-disk refusal applies to the DB device too");
        let part = AddCephDeviceBody { db_device: Some("/dev/nvme1n1p1".into()), ..body() };
        assert!(validate_layout(&part).is_err());
    }

    #[test]
    fn db_device_checks_against_discovery_and_the_cr() {
        let disc = vec![
            json!({"name": "nvme1n1", "empty": true, "rotational": false}),
            json!({"name": "nvme2n1", "empty": false, "filesystem": "LVM2_member", "rotational": false}),
            json!({"name": "sdc", "empty": true, "rotational": true}),
            json!({"name": "sdd", "empty": true, "rotational": false}),
        ];
        assert!(check_db_device("nvme1n1", None, &disc).is_ok(), "empty flash device");
        assert!(check_db_device("nvme9n1", None, &disc).is_err(), "unknown to discovery");
        assert!(check_db_device("sdc", None, &disc).is_err(), "rotational");
        assert!(check_db_device("nvme2n1", None, &disc).is_err(), "in use and not a shared DB device");
        let shared = node(vec![dev("sde", Some("nvme2n1"))]);
        assert!(check_db_device("nvme2n1", Some(&shared), &disc).is_ok(), "already a shared metadataDevice");
        let as_data = node(vec![dev("sdd", None)]);
        assert!(check_db_device("sdd", Some(&as_data), &disc).is_err(), "already an OSD data device");
    }
}
