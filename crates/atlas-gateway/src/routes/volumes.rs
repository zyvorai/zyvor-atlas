// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_api_types::{
    CloneSnapshotRequest, CreateSnapshotRequest, CreateVolumeRequest, DeleteSnapshotRequest,
    DeleteVolumeRequest, ExpandVolumeRequest, Owner, Placement,
};
use atlas_common::{ids, AppError, AppResult};
use atlas_driver_core::{DriverError, StorageDriver};
use atlas_jobs::{JobSpec, OwnerRef};

use super::util::{accepted, validate_k8s_name, CEPH_BACKEND_ID};
use crate::auth::Actor;
use crate::startup::{LONGHORN_BACKEND_ID, NATIVE_BACKEND_ID};
use crate::state::AppState;

// ---- snapshots (read) ----

pub(crate) async fn list_snapshots(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::snapshots::list_snapshots(&s.pool, None).await?
    )))
}

/// `POST /volumes` — create a PVC as an async job (Ceph by default, Longhorn when selected).
pub(crate) async fn create_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateVolumeRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    // A tenant-scoped operator must not create a volume attributed to (and billed against the
    // quota of) a DIFFERENT tenant by simply naming it in the request body — `tenant_id` here is
    // caller-supplied, unlike the read/single-resource paths where it's looked up from inventory.
    if let Some(scope) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        if body.tenant_id != scope {
            return Err(AppError::Forbidden(format!(
                "actor '{}' may only create volumes for tenant '{scope}'",
                actor.id
            )));
        }
    }
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    validate_k8s_name(&body.name)?;
    if body.size_bytes <= 0 {
        return Err(AppError::Validation("size_bytes must be > 0".into()));
    }

    // Tenant quota admission (PDF §14): reject a create that would exceed the tenant's limits.
    match atlas_inventory::tenants::check_admission(&s.pool, &body.tenant_id, body.size_bytes)
        .await?
    {
        atlas_inventory::tenants::QuotaCheck::Ok => {}
        atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
            return Err(AppError::Conflict(format!(
                "tenant {} byte quota exceeded: {would_be} > {limit}",
                body.tenant_id
            )));
        }
        atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
            return Err(AppError::Conflict(format!(
                "tenant {} volume-count quota exceeded: {current} already at limit {limit}",
                body.tenant_id
            )));
        }
    }

    if body
        .kubernetes
        .as_ref()
        .and_then(|k| k.backend_id.as_deref())
        == Some(NATIVE_BACKEND_ID)
    {
        return create_native_volume(&s, &actor, body).await;
    }

    let k8s_opts = body.kubernetes.clone();
    let namespace = k8s_opts
        .as_ref()
        .and_then(|k| k.namespace.clone())
        .unwrap_or_else(|| "default".into());
    let sc_override = k8s_opts.as_ref().and_then(|k| k.storage_class.clone());
    // Per-tenant policy override (PDF §14): unless the request pins an explicit StorageClass, a
    // tenant's override for this intent wins over the built-in catalog — and, since
    // `PUT /tenants/{id}/policies/{intent}` isn't restricted to the built-in catalog, this can
    // resolve an `intent` that `atlas_policy::resolve` doesn't recognize at all.
    let tenant_override = if sc_override.is_none() {
        match body.policy.as_deref() {
            Some(intent) => {
                atlas_inventory::tenants::get_policy(&s.pool, &body.tenant_id, intent).await?
            }
            None => None,
        }
    } else {
        None
    };
    let mut placement =
        match atlas_policy::resolve(body.policy.as_deref(), body.kind, sc_override.as_deref()) {
            Ok(p) => p,
            // Unrecognized by the built-in catalog, but the tenant has its own override for this
            // intent — the fields below get overwritten from `tenant_override` immediately after.
            Err(_) if tenant_override.is_some() => Placement {
                intent: body.policy.clone().unwrap_or_default(),
                storage_class: String::new(),
                access_mode: String::new(),
                volume_mode: String::new(),
                kind: body.kind,
            },
            Err(e) => return Err(AppError::Validation(e)),
        };
    if let Some(tp) = tenant_override {
        placement.storage_class = tp.storage_class;
        placement.access_mode = tp.access_mode;
        placement.volume_mode = tp.volume_mode;
        // `tenant_policies` has no `kind` column, so `placement.kind` (already set from the
        // built-in policy's kind, or the request's kind if the intent isn't a known built-in) is
        // the best available signal here — still correct for the common case of a tenant
        // overriding just the StorageClass for an existing named intent.
    }
    let backend_id = k8s_opts
        .as_ref()
        .and_then(|k| k.backend_id.as_deref())
        .unwrap_or(CEPH_BACKEND_ID);
    if backend_id != CEPH_BACKEND_ID {
        if backend_id != LONGHORN_BACKEND_ID || s.driver_for(backend_id).is_none() {
            return Err(AppError::Validation(format!(
                "backend {backend_id} is not available for PVC provisioning"
            )));
        }
        let k8s = s
            .k8s
            .as_ref()
            .ok_or_else(|| AppError::Unavailable("Kubernetes unavailable".into()))?;
        let classes = k8s
            .list_storage_classes()
            .await
            .map_err(|e| AppError::Unavailable(e.to_string()))?;
        if !classes
            .iter()
            .any(|sc| sc.name == placement.storage_class && sc.provisioner == "driver.longhorn.io")
        {
            return Err(AppError::Validation(format!(
                "storage class {} must use driver.longhorn.io for backend {backend_id}",
                placement.storage_class
            )));
        }
    }
    if atlas_inventory::is_backend_cordoned(&s.pool, backend_id).await? {
        return Err(AppError::Unavailable(format!(
            "backend {backend_id} is cordoned for maintenance"
        )));
    }
    let access_modes = k8s_opts
        .as_ref()
        .map(|k| k.access_modes.clone())
        .filter(|m| !m.is_empty());
    let access_mode = access_modes
        .and_then(|m| m.into_iter().next())
        .unwrap_or_else(|| placement.access_mode.clone());
    let volume_mode = k8s_opts
        .as_ref()
        .and_then(|k| k.volume_mode.clone())
        .unwrap_or_else(|| placement.volume_mode.clone());

    let volume_id = ids::volume_id();
    let job_id = ids::job_id();
    let owner = body.owner.clone().map(|o| OwnerRef {
        product: o.product,
        resource_type: o.resource_type,
        resource_id: o.resource_id,
        role: o.role,
    });
    // Idempotency: a repeat of the same tenant/name/size create returns the same job (PDF §17.4).
    let idem = ids::stable_id(
        "idem",
        &format!(
            "{}|{}|{}|{}|volume.create",
            body.tenant_id, body.name, body.size_bytes, backend_id
        ),
    );

    let spec = JobSpec::VolumeCreate {
        volume_id: volume_id.clone(),
        backend_id: backend_id.into(),
        name: body.name.clone(),
        namespace: namespace.clone(),
        storage_class: placement.storage_class.clone(),
        access_mode,
        volume_mode,
        size_bytes: body.size_bytes,
        // Use the *resolved* kind, not the raw request's — a named policy (e.g. `shared`) always
        // implies a specific kind (CephFS/`Filesystem`) regardless of what the client passed or
        // defaulted to; trusting `body.kind` here mis-tags the volume and it gets silently pruned
        // by discovery on drivers that only enumerate one storage kind.
        kind: format!("{:?}", placement.kind).to_lowercase(),
        policy: Some(placement.intent.clone()),
        owner,
    };

    let job = s
        .jobs
        .enqueue(&job_id, &body.tenant_id, &actor.id, spec, Some(&idem))
        .await
        .map_err(AppError::from)?;

    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&body.tenant_id),
        &actor.id,
        "volume.create.requested",
        "volume",
        &volume_id,
        "accepted",
        Some(json!({ "name": body.name, "policy": placement.intent, "storage_class": placement.storage_class })),
        None,
    )
    .await;

    Ok(accepted(
        &job,
        json!({ "volume_id": volume_id, "storage_class": placement.storage_class,
                "namespace": namespace, "pvc": body.name }),
    ))
}

/// The atlas-native driver when `volume_id` lives on that backend.
pub(super) async fn native_driver_for_volume(
    s: &AppState,
    volume_id: &str,
) -> AppResult<Option<std::sync::Arc<dyn StorageDriver>>> {
    if atlas_inventory::volume_backend_id(&s.pool, volume_id)
        .await?
        .as_deref()
        != Some(NATIVE_BACKEND_ID)
    {
        return Ok(None);
    }
    s.drivers
        .get(NATIVE_BACKEND_ID)
        .map(Some)
        .ok_or_else(|| AppError::Unavailable("the atlas-native driver is not enabled".into()))
}

pub(super) fn driver_error(e: DriverError) -> AppError {
    match e {
        DriverError::Unreachable(m) => AppError::Unavailable(m),
        DriverError::Backend(m) if m.starts_with("not found") => AppError::NotFound(m),
        DriverError::Backend(m) if m.starts_with("invalid") => AppError::Validation(m),
        DriverError::Backend(m) => AppError::Conflict(m),
        e => AppError::Driver(e.to_string()),
    }
}

pub(super) async fn audit(
    s: &AppState,
    tenant: Option<&str>,
    actor: &Actor,
    action: &str,
    kind: &str,
    id: &str,
    detail: Option<Value>,
) {
    let _ = atlas_inventory::audit::record(
        &s.pool,
        tenant,
        &actor.id,
        action,
        kind,
        id,
        "succeeded",
        detail,
        None,
    )
    .await;
}

/// `POST /volumes` on the atlas-native backend: synchronous (no Kubernetes involved).
async fn create_native_volume(
    s: &AppState,
    actor: &Actor,
    body: CreateVolumeRequest,
) -> AppResult<(StatusCode, Json<Value>)> {
    let driver = s.drivers.get(NATIVE_BACKEND_ID).ok_or_else(|| {
        AppError::Validation(format!("backend {NATIVE_BACKEND_ID} is not enabled"))
    })?;
    if atlas_inventory::is_backend_cordoned(&s.pool, NATIVE_BACKEND_ID).await? {
        return Err(AppError::Unavailable(format!(
            "backend {NATIVE_BACKEND_ID} is cordoned for maintenance"
        )));
    }
    let res = driver
        .create_volume(body.clone())
        .await
        .map_err(driver_error)?;
    // Discovery records the cluster/pool rows and the volume; then attribute it to the tenant.
    atlas_discovery::run_discovery(&s.pool, driver, None, None)
        .await
        .map_err(|e| AppError::Unavailable(format!("discovery after create: {e:#}")))?;
    let vol = atlas_inventory::get_volume(&s.pool, &res.volume_id)
        .await?
        .ok_or_else(|| AppError::Internal(format!("{} missing after discovery", res.volume_id)))?;
    atlas_inventory::upsert_volume(&s.pool, NATIVE_BACKEND_ID, &body.tenant_id, &vol, None).await?;
    if let Some(o) = &body.owner {
        atlas_inventory::insert_binding(
            &s.pool,
            &format!("bind_{}", res.volume_id),
            &body.tenant_id,
            &o.product,
            &o.resource_type,
            &o.resource_id,
            "volume",
            &res.volume_id,
            &o.role,
        )
        .await?;
    }
    audit(
        s,
        Some(&body.tenant_id),
        actor,
        "volume.create",
        "volume",
        &res.volume_id,
        Some(json!({ "name": body.name, "backend_id": NATIVE_BACKEND_ID })),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "volume_id": res.volume_id, "backend_id": NATIVE_BACKEND_ID,
                     "backend_native_id": res.backend_native_id, "state": "available" }),
        ),
    ))
}

/// `DELETE /volumes/{id}[?confirm=true]` — delete the PVC + inventory row as a job.
/// Safe-by-default (PDF §14 Rule 2): production-class volumes require confirm=true
/// (legacy force=true also accepted — matches the console delete path).
pub(crate) async fn delete_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<ConfirmParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let vol = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    if volume_requires_delete_confirm(&vol) && !q.confirmed() {
        return Err(AppError::Validation(
            "confirm=true is required to delete this volume (production / protected class)".into(),
        ));
    }
    if let Some(driver) = native_driver_for_volume(&s, &id).await? {
        let native = vol.backend_native_id.clone().unwrap_or_else(|| id.clone());
        driver
            .delete_volume(DeleteVolumeRequest { volume_id: native })
            .await
            .map_err(driver_error)?;
        atlas_inventory::delete_volume_row(&s.pool, &id).await?;
        audit(&s, None, &actor, "volume.delete", "volume", &id, None).await;
        return Ok((
            StatusCode::OK,
            Json(json!({ "volume_id": id, "deleted": true })),
        ));
    }
    let namespace = vol
        .kubernetes_namespace
        .ok_or_else(|| AppError::Validation("volume has no kubernetes namespace".into()))?;
    let pvc_name = vol
        .pvc_name
        .ok_or_else(|| AppError::Validation("volume has no pvc".into()))?;

    let job_id = ids::job_id();
    let spec = JobSpec::VolumeDelete {
        volume_id: id.clone(),
        namespace,
        pvc_name,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;

    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "volume.delete.requested",
        "volume",
        &id,
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(&job, json!({ "volume_id": id })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExpandBody {
    new_size_bytes: i64,
}

/// `POST /volumes/{id}/expand`
pub(crate) async fn expand_volume(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<ExpandBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let vol = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("volume {id}"),
    )?;
    if body.new_size_bytes <= vol.size_bytes {
        return Err(AppError::Validation(
            "new_size_bytes must be larger than the current size".into(),
        ));
    }
    if let Some(driver) = native_driver_for_volume(&s, &id).await? {
        let native = vol.backend_native_id.clone().unwrap_or_else(|| id.clone());
        driver
            .expand_volume(ExpandVolumeRequest {
                volume_id: native,
                new_size_bytes: body.new_size_bytes,
            })
            .await
            .map_err(driver_error)?;
        atlas_inventory::set_volume_size(&s.pool, &id, body.new_size_bytes).await?;
        audit(
            &s,
            Some(&resource_tenant),
            &actor,
            "volume.expand",
            "volume",
            &id,
            Some(json!({ "new_size_bytes": body.new_size_bytes })),
        )
        .await;
        return Ok((
            StatusCode::OK,
            Json(json!({ "volume_id": id, "size_bytes": body.new_size_bytes })),
        ));
    }
    let namespace = vol
        .kubernetes_namespace
        .ok_or_else(|| AppError::Validation("volume has no kubernetes namespace".into()))?;
    let pvc_name = vol
        .pvc_name
        .ok_or_else(|| AppError::Validation("volume has no pvc".into()))?;

    let job_id = ids::job_id();
    let spec = JobSpec::VolumeExpand {
        volume_id: id.clone(),
        namespace,
        pvc_name,
        new_size_bytes: body.new_size_bytes,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "volume_id": id, "new_size_bytes": body.new_size_bytes }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotBody {
    name: Option<String>,
    snapshot_class: Option<String>,
}

/// `POST /volumes/{id}/snapshots`
pub(crate) async fn create_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<SnapshotBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let vol = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("volume {id}"),
    )?;
    if let Some(driver) = native_driver_for_volume(&s, &id).await? {
        let name = body
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("{}-snap", vol.name));
        let native = vol.backend_native_id.clone().unwrap_or_else(|| id.clone());
        let res = driver
            .create_snapshot(CreateSnapshotRequest {
                volume_id: native,
                name: name.clone(),
            })
            .await
            .map_err(driver_error)?;
        atlas_inventory::snapshots::insert_snapshot(
            &s.pool,
            &res.snapshot_id,
            &resource_tenant,
            &id,
            &name,
            Some(&res.backend_native_id),
            "crash",
            "ready",
        )
        .await?;
        audit(
            &s,
            Some(&resource_tenant),
            &actor,
            "snapshot.create",
            "volume",
            &id,
            Some(json!({ "snapshot_id": res.snapshot_id, "name": name })),
        )
        .await;
        return Ok((
            StatusCode::CREATED,
            Json(
                json!({ "snapshot_id": res.snapshot_id, "volume_id": id, "name": name,
                         "state": "ready" }),
            ),
        ));
    }
    let namespace = vol
        .kubernetes_namespace
        .ok_or_else(|| AppError::Validation("volume has no kubernetes namespace".into()))?;
    let pvc_name = vol
        .pvc_name
        .ok_or_else(|| AppError::Validation("volume has no pvc".into()))?;

    let snapshot_id = ids::snapshot_id();
    let snap_name = body
        .name
        .unwrap_or_else(|| format!("{}-{}", vol.name, &snapshot_id[5..]));
    let snapshot_class = body
        .snapshot_class
        .unwrap_or_else(|| "zyvor-rbd-snapclass".into());

    let job_id = ids::job_id();
    let spec = JobSpec::SnapshotCreate {
        snapshot_id: snapshot_id.clone(),
        volume_id: id.clone(),
        name: snap_name.clone(),
        namespace,
        pvc_name,
        snapshot_class,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;

    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "snapshot.create.requested",
        "volume",
        &id,
        "accepted",
        Some(json!({ "snapshot_id": snapshot_id, "name": snap_name })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "snapshot_id": snapshot_id, "volume_id": id, "name": snap_name }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForceParams {
    #[serde(default)]
    pub(crate) force: bool,
}

/// Explicit destructive confirm for volume delete (PDF §14 Rule 2).
#[derive(Debug, Deserialize, Default)]
pub(crate) struct ConfirmParams {
    #[serde(default)]
    pub(crate) confirm: bool,
    /// Legacy alias used by the console (`?force=true`).
    #[serde(default)]
    pub(crate) force: bool,
}

impl ConfirmParams {
    pub(crate) fn confirmed(&self) -> bool {
        self.confirm || self.force
    }
}

fn volume_requires_delete_confirm(vol: &atlas_api_types::StorageVolume) -> bool {
    let sc = vol
        .storage_class_name
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    // Production / protected classes (zyvor-rbd-prod, *-production*, etc.).
    sc.contains("prod") || sc.contains("production") || sc.contains("database")
}

/// `DELETE /snapshots/{id}[?force=true]` — blocked if the snapshot has dependent clones (PDF §8.3).
pub(crate) async fn delete_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<ForceParams>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    // Force-deleting past the dependency guard is an admin action.
    if q.force {
        crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    }
    let snap = atlas_inventory::snapshots::get_snapshot(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("snapshot {id}")))?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &snap.tenant_id,
        format!("snapshot {id}"),
    )?;

    // Safe-by-default: refuse to delete a snapshot that still has clones/restores derived from it.
    let deps = atlas_inventory::count_snapshot_dependents(&s.pool, &id).await?;
    if deps > 0 && !q.force {
        return Err(AppError::Conflict(format!(
            "snapshot {id} has {deps} dependent volume(s); delete them first or pass ?force=true"
        )));
    }

    if let Some(driver) = native_driver_for_volume(&s, &snap.volume_id).await? {
        let native = snap.backend_native_id.clone().unwrap_or_else(|| id.clone());
        driver
            .delete_snapshot(DeleteSnapshotRequest {
                snapshot_id: native,
            })
            .await
            .map_err(driver_error)?;
        atlas_inventory::snapshots::delete_snapshot_row(&s.pool, &id).await?;
        audit(
            &s,
            Some(&snap.tenant_id),
            &actor,
            "snapshot.delete",
            "snapshot",
            &id,
            None,
        )
        .await;
        return Ok((
            StatusCode::OK,
            Json(json!({ "snapshot_id": id, "deleted": true })),
        ));
    }
    // The VolumeSnapshot lives in the source volume's namespace.
    let vol = atlas_inventory::get_volume(&s.pool, &snap.volume_id).await?;
    let namespace = vol
        .and_then(|v| v.kubernetes_namespace)
        .unwrap_or_else(|| "default".into());

    let job_id = ids::job_id();
    let spec = JobSpec::SnapshotDelete {
        snapshot_id: id.clone(),
        namespace,
        name: snap.name,
    };
    let job = s
        .jobs
        .enqueue(&job_id, &snap.tenant_id, &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(
        &job,
        json!({ "snapshot_id": id, "forced": q.force }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LockBody {
    retain_secs: u64,
}

/// Longest retention one call may set (10 years): a lock is irreversible, so it is bounded.
const MAX_LOCK_SECS: u64 = 10 * 365 * 24 * 3600;

/// `POST /snapshots/{id}/lock {retain_secs}` — retention-lock (WORM) a snapshot (admin). Delete is
/// refused until the lock expires; it can be extended later, never shortened. atlas-native only.
pub(crate) async fn lock_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<LockBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.retain_secs == 0 || body.retain_secs > MAX_LOCK_SECS {
        return Err(AppError::Validation(format!(
            "retain_secs must be between 1 and {MAX_LOCK_SECS}"
        )));
    }
    let snap = atlas_inventory::snapshots::get_snapshot(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("snapshot {id}")))?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &snap.tenant_id,
        format!("snapshot {id}"),
    )?;
    let driver = native_driver_for_volume(&s, &snap.volume_id)
        .await?
        .ok_or_else(|| {
            AppError::Validation("snapshot locking is only supported on atlas-native".into())
        })?;
    let native = snap.backend_native_id.clone().unwrap_or_else(|| id.clone());
    let until = driver
        .lock_snapshot(&native, body.retain_secs)
        .await
        .map_err(driver_error)?;
    audit(
        &s,
        Some(&snap.tenant_id),
        &actor,
        "snapshot.lock",
        "snapshot",
        &id,
        Some(json!({ "retain_secs": body.retain_secs, "retain_until_unix": until })),
    )
    .await;
    Ok(Json(json!({ "snapshot_id": id, "retain_until_unix": until })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CloneBody {
    /// New volume/PVC name (required for clone; optional for restore).
    name: Option<String>,
    namespace: Option<String>,
    storage_class: Option<String>,
    size_bytes: Option<i64>,
    #[serde(default)]
    owner: Option<Owner>,
}

/// `POST /snapshots/{id}/clone` — provision a new independent volume from a snapshot (PDF §8.3).
pub(crate) async fn clone_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let name = body
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| AppError::Validation("name is required for clone".into()))?;
    enqueue_clone(&s, &actor, &id, "clone", name, body).await
}

/// `POST /snapshots/{id}/restore` — provision a point-in-time copy of the source volume (PDF §8.3).
pub(crate) async fn restore_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    // Default restore name derives from the snapshot id when not supplied. `id` is client-supplied
    // (URL path), so strip the "snap_" prefix defensively instead of slicing — a short/malformed id
    // would otherwise panic before the not-found check below runs.
    let name = body
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| format!("restore-{}", id.strip_prefix("snap_").unwrap_or(&id)));
    enqueue_clone(&s, &actor, &id, "restore", name, body).await
}

pub(crate) async fn enqueue_clone(
    s: &AppState,
    actor: &Actor,
    snapshot_id: &str,
    mode: &str,
    new_name: String,
    body: CloneBody,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_OPERATOR)?;
    let snap = atlas_inventory::snapshots::get_snapshot(&s.pool, snapshot_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("snapshot {snapshot_id}")))?;
    crate::auth::require_tenant(
        s.config.auth_required,
        actor,
        &snap.tenant_id,
        format!("snapshot {snapshot_id}"),
    )?;
    // Defaults come from the source volume.
    let src = atlas_inventory::get_volume(&s.pool, &snap.volume_id).await?;
    let namespace = body
        .namespace
        .or_else(|| src.as_ref().and_then(|v| v.kubernetes_namespace.clone()))
        .unwrap_or_else(|| "default".into());
    let storage_class = body
        .storage_class
        .or_else(|| src.as_ref().and_then(|v| v.storage_class_name.clone()))
        .unwrap_or_else(|| atlas_policy::DEFAULT_BLOCK_SC.to_string());
    let size_bytes = body
        .size_bytes
        .or_else(|| src.as_ref().map(|v| v.size_bytes))
        .ok_or_else(|| {
            AppError::Validation("size_bytes is required (source volume unknown)".into())
        })?;
    // An atlas-native filesystem has no size limit; its size is the bytes in it, possibly 0.
    let unsized_source = src
        .as_ref()
        .is_some_and(|v| v.kind == atlas_api_types::VolumeKind::Filesystem)
        && atlas_inventory::volume_backend_id(&s.pool, &snap.volume_id)
            .await?
            .as_deref()
            == Some(NATIVE_BACKEND_ID);
    if size_bytes < 0 || (size_bytes == 0 && !unsized_source) {
        return Err(AppError::Validation("size_bytes must be > 0".into()));
    }

    // Tenant quota admission (PDF §14): a clone/restore provisions a new volume just like
    // `POST /volumes` does, so it must be admission-checked the same way.
    match atlas_inventory::tenants::check_admission(&s.pool, &snap.tenant_id, size_bytes).await? {
        atlas_inventory::tenants::QuotaCheck::Ok => {}
        atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
            return Err(AppError::Conflict(format!(
                "tenant {} byte quota exceeded: {would_be} > {limit}",
                snap.tenant_id
            )));
        }
        atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
            return Err(AppError::Conflict(format!(
                "tenant {} volume-count quota exceeded: {current} already at limit {limit}",
                snap.tenant_id
            )));
        }
    }

    if let Some(driver) = native_driver_for_volume(s, &snap.volume_id).await? {
        return clone_native_snapshot(
            s, actor, driver, &snap, mode, new_name, size_bytes, body.owner,
        )
        .await;
    }

    let new_volume_id = ids::volume_id();
    let job_id = ids::job_id();
    let owner = body.owner.map(|o| OwnerRef {
        product: o.product,
        resource_type: o.resource_type,
        resource_id: o.resource_id,
        role: o.role,
    });

    let spec = JobSpec::SnapshotClone {
        mode: mode.into(),
        new_volume_id: new_volume_id.clone(),
        backend_id: CEPH_BACKEND_ID.into(),
        snapshot_id: snapshot_id.to_string(),
        snapshot_k8s_name: snap.name.clone(),
        new_name: new_name.clone(),
        namespace: namespace.clone(),
        storage_class: storage_class.clone(),
        size_bytes,
        access_mode: "ReadWriteOnce".into(),
        volume_mode: "Filesystem".into(),
        owner,
    };
    let job = s
        .jobs
        .enqueue(&job_id, &snap.tenant_id, &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;

    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        &format!("snapshot.{mode}.requested"),
        "snapshot",
        snapshot_id,
        "accepted",
        Some(json!({ "new_volume_id": new_volume_id, "new_name": new_name })),
        None,
    )
    .await;

    Ok(accepted(
        &job,
        json!({ "volume_id": new_volume_id, "from_snapshot": snapshot_id,
                "mode": mode, "namespace": namespace, "pvc": new_name,
                "storage_class": storage_class }),
    ))
}

/// Clone/restore on the atlas-native backend: a new volume sharing the snapshot's extents,
/// created synchronously.
#[allow(clippy::too_many_arguments)]
async fn clone_native_snapshot(
    s: &AppState,
    actor: &Actor,
    driver: std::sync::Arc<dyn StorageDriver>,
    snap: &atlas_api_types::StorageSnapshot,
    mode: &str,
    new_name: String,
    size_bytes: i64,
    owner: Option<Owner>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let native = snap
        .backend_native_id
        .clone()
        .unwrap_or_else(|| snap.id.clone());
    let res = driver
        .clone_snapshot(CloneSnapshotRequest {
            snapshot_id: native,
            new_volume_name: new_name.clone(),
            size_bytes: Some(size_bytes),
        })
        .await
        .map_err(driver_error)?;
    atlas_discovery::run_discovery(&s.pool, driver, None, None)
        .await
        .map_err(|e| AppError::Unavailable(format!("discovery after {mode}: {e:#}")))?;
    let vol = atlas_inventory::get_volume(&s.pool, &res.volume_id)
        .await?
        .ok_or_else(|| AppError::Internal(format!("{} missing after discovery", res.volume_id)))?;
    atlas_inventory::upsert_volume(&s.pool, NATIVE_BACKEND_ID, &snap.tenant_id, &vol, None).await?;
    atlas_inventory::set_volume_source_snapshot(&s.pool, &res.volume_id, &snap.id).await?;
    if let Some(o) = &owner {
        atlas_inventory::insert_binding(
            &s.pool,
            &format!("bind_{}", res.volume_id),
            &snap.tenant_id,
            &o.product,
            &o.resource_type,
            &o.resource_id,
            "volume",
            &res.volume_id,
            &o.role,
        )
        .await?;
    }
    audit(
        s,
        Some(&snap.tenant_id),
        actor,
        &format!("snapshot.{mode}"),
        "snapshot",
        &snap.id,
        Some(json!({ "new_volume_id": res.volume_id, "new_name": new_name })),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "volume_id": res.volume_id, "from_snapshot": snap.id, "mode": mode,
                     "backend_id": NATIVE_BACKEND_ID, "name": new_name,
                     "size_bytes": vol.size_bytes, "state": "available" }),
        ),
    ))
}

/// Largest block read or write the gateway proxies in one request.
pub(crate) const MAX_VOLUME_IO_BYTES: usize = 4 << 20;

#[derive(Debug, Deserialize)]
pub(crate) struct VolumeIoParams {
    offset: u64,
    len: Option<u64>,
}

/// The native driver and native id for block I/O on `id`, after the role and tenant checks.
async fn volume_for_io(
    s: &AppState,
    actor: &Actor,
    id: &str,
) -> AppResult<(std::sync::Arc<dyn StorageDriver>, String)> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_OPERATOR)?;
    let vol = atlas_inventory::get_volume(&s.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let tenant = atlas_inventory::volume_tenant(&s.pool, id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        actor,
        &tenant,
        format!("volume {id}"),
    )?;
    let driver = native_driver_for_volume(s, id).await?.ok_or_else(|| {
        AppError::Validation("block data access is only available for atlas-native volumes".into())
    })?;
    Ok((
        driver,
        vol.backend_native_id.unwrap_or_else(|| id.to_string()),
    ))
}

/// `GET /volumes/{id}/data?offset=N&len=M` — raw bytes of an atlas-native volume.
pub(crate) async fn read_volume_data(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<VolumeIoParams>,
) -> AppResult<Response> {
    let len = q
        .len
        .ok_or_else(|| AppError::Validation("query parameter len is required".into()))?;
    if len > MAX_VOLUME_IO_BYTES as u64 {
        return Err(AppError::Validation(format!(
            "len exceeds {MAX_VOLUME_IO_BYTES} bytes"
        )));
    }
    let (driver, native) = volume_for_io(&s, &actor, &id).await?;
    let data = driver
        .read_volume(&native, q.offset, len)
        .await
        .map_err(driver_error)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}

/// `PUT /volumes/{id}/data?offset=N` — writes the raw body into an atlas-native volume.
pub(crate) async fn write_volume_data(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<VolumeIoParams>,
    body: Bytes,
) -> AppResult<StatusCode> {
    let (driver, native) = volume_for_io(&s, &actor, &id).await?;
    driver
        .write_volume(&native, q.offset, body.to_vec())
        .await
        .map_err(driver_error)?;
    Ok(StatusCode::NO_CONTENT)
}
