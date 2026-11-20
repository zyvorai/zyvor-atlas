// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::{accepted, CEPH_BACKEND_ID, DEFAULT_RBD_POOL};
use crate::auth::Actor;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRbdBody {
    /// Image name.
    name: String,
    size_bytes: i64,
    /// RBD pool (defaults to the platform block pool).
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
}

/// `POST /rbd-usage/refresh` — recompute `used_bytes` for every RBD-backed volume via `rbd du`
/// (operator). Resolves each volume's image (direct `rbd:` id, or the PVC's PV for CSI volumes).
pub(crate) async fn refresh_rbd_usage(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let volumes = atlas_inventory::list_volumes(&s.pool).await?;
    let mut updated = 0usize;
    let mut total_used: i64 = 0;
    for v in volumes {
        // Determine the (pool, image) for this volume.
        let target = if let Some(native) = v
            .backend_native_id
            .as_deref()
            .and_then(|n| n.strip_prefix("rbd:"))
        {
            native
                .split_once('/')
                .map(|(p, i)| (p.to_string(), i.to_string()))
        } else if let (Some(ns), Some(pvc)) = (&v.kubernetes_namespace, &v.pvc_name) {
            match &s.k8s {
                Some(k8s) => k8s.resolve_rbd(ns, pvc).await.ok().flatten(),
                None => None,
            }
        } else {
            None
        };
        let Some((rbd_pool, image)) = target else {
            continue;
        };
        if let Ok(used) = atlas_driver_ceph::rbd_du_image(&rbd_pool, &image).await {
            if atlas_inventory::set_volume_used(&s.pool, &v.id, used)
                .await
                .is_ok()
            {
                updated += 1;
                total_used += used;
            }
        }
    }
    Ok(Json(
        json!({ "updated": updated, "total_used_bytes": total_used }),
    ))
}

/// `POST /rbd-images` — provision a raw RBD image directly (bypassing CSI) for non-K8s consumers.
pub(crate) async fn create_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateRbdBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    if body.size_bytes <= 0 {
        return Err(AppError::Validation("size_bytes must be > 0".into()));
    }

    // Maintenance: a cordoned backend rejects new provisioning (existing images are untouched) —
    // this direct-RBD path bypasses CSI, so it needs the same guard the PVC path has.
    if atlas_inventory::is_backend_cordoned(&s.pool, CEPH_BACKEND_ID).await? {
        return Err(AppError::Unavailable(format!(
            "backend {CEPH_BACKEND_ID} is cordoned for maintenance"
        )));
    }

    let pool_name = body.pool.unwrap_or_else(|| DEFAULT_RBD_POOL.into());
    let tenant_id = body.tenant_id.unwrap_or_else(|| "global".into());
    // A tenant-scoped operator must not create an RBD image attributed to a DIFFERENT tenant by
    // naming it in the request body — same class of gap as `POST /volumes` (see there for detail).
    if let Some(scope) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        if tenant_id != scope {
            return Err(AppError::Forbidden(format!(
                "actor '{}' may only create RBD images for tenant '{scope}'",
                actor.id
            )));
        }
    }

    // Reject a name collision up front: nothing else in the create path checks this (job dispatch
    // always upserts under a fresh volume_id), so without this guard two create calls for the same
    // pool/name — concurrent or just sequential — silently produce two distinct catalog rows both
    // claiming the same rbd:pool/image identity instead of one erroring like a real `rbd create`
    // would on an existing name.
    let native = format!("rbd:{pool_name}/{}", body.name);
    if atlas_inventory::list_volumes(&s.pool)
        .await?
        .iter()
        .any(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
    {
        return Err(AppError::Conflict(format!(
            "rbd image {pool_name}/{} already exists",
            body.name
        )));
    }

    // Tenant quota admission (PDF §14): this direct-RBD path bypasses CSI, so it needs the same
    // guard `POST /volumes` has — otherwise a tenant's byte/volume-count quota is pure decoration.
    match atlas_inventory::tenants::check_admission(&s.pool, &tenant_id, body.size_bytes).await? {
        atlas_inventory::tenants::QuotaCheck::Ok => {}
        atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
            return Err(AppError::Conflict(format!(
                "tenant {tenant_id} byte quota exceeded: {would_be} > {limit}"
            )));
        }
        atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
            return Err(AppError::Conflict(format!(
                "tenant {tenant_id} volume-count quota exceeded: {current} already at limit {limit}"
            )));
        }
    }

    // Deterministic, not ids::volume_id(): must match the id a discovery pass would derive for
    // this same pool/image (atlas-driver-ceph real.rs/fake.rs, `vol_{pool}_{name}`). Otherwise the
    // next discovery cycle inserts a *second* row under its own id for the same physical image,
    // and the create's original row — now the older, untouched one — gets pruned as stale on that
    // same pass: the volume_id returned by this response silently stops resolving.
    let volume_id = format!("vol_{pool_name}_{}", body.name);
    let job_id = ids::job_id();
    let spec = JobSpec::RbdCreate {
        volume_id: volume_id.clone(),
        backend_id: CEPH_BACKEND_ID.into(),
        pool: pool_name.clone(),
        image: body.name.clone(),
        size_bytes: body.size_bytes,
    };
    let job = s
        .jobs
        .enqueue(&job_id, &tenant_id, &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&tenant_id),
        &actor.id,
        "rbd_image.create.requested",
        "rbd_image",
        &volume_id,
        "accepted",
        Some(json!({ "rbd": format!("{pool_name}/{}", body.name), "size_bytes": body.size_bytes })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "volume_id": volume_id, "rbd": format!("{pool_name}/{}", body.name) }),
    ))
}

/// `DELETE /rbd-images/{pool}/{image}` — delete a raw RBD image (admin).
pub(crate) async fn delete_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    // Best-effort: resolve the recorded volume row (by native id) so we clean up inventory too.
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let job_id = ids::job_id();
    let spec = JobSpec::RbdDelete {
        volume_id,
        pool: pool_name.clone(),
        image: image.clone(),
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
        "rbd_image.delete.requested",
        "rbd_image",
        &format!("{pool_name}/{image}"),
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}") }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CloneRbdBody {
    /// New clone image name.
    name: String,
    /// Snapshot name to create + protect on the parent (defaults to `<clone>-base`).
    #[serde(default)]
    snap: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
}

/// `POST /rbd-images/{pool}/{image}/clone` — snapshot+protect the parent and create a COW clone.
pub(crate) async fn clone_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Json(body): Json<CloneRbdBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }

    // Maintenance: a cordoned backend rejects new provisioning — a clone is a new image.
    if atlas_inventory::is_backend_cordoned(&s.pool, CEPH_BACKEND_ID).await? {
        return Err(AppError::Unavailable(format!(
            "backend {CEPH_BACKEND_ID} is cordoned for maintenance"
        )));
    }

    let snap = body.snap.unwrap_or_else(|| format!("{}-base", body.name));
    let tenant_id = body.tenant_id.unwrap_or_else(|| "global".into());
    // Same tenant-spoof guard as create_rbd_image / POST /volumes.
    if let Some(scope) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        if tenant_id != scope {
            return Err(AppError::Forbidden(format!(
                "actor '{}' may only create RBD images for tenant '{scope}'",
                actor.id
            )));
        }
    }

    // Reject a clone-target name collision up front — same reasoning as create_rbd_image: nothing
    // downstream checks this, so cloning onto an existing name would silently produce two catalog
    // rows for one identity instead of erroring like a real `rbd clone` would.
    let clone_native = format!("rbd:{pool_name}/{}", body.name);
    let existing = atlas_inventory::list_volumes(&s.pool).await?;
    if existing
        .iter()
        .any(|v| v.backend_native_id.as_deref() == Some(clone_native.as_str()))
    {
        return Err(AppError::Conflict(format!(
            "rbd image {pool_name}/{} already exists",
            body.name
        )));
    }

    // Tenant quota admission (PDF §14): a clone starts at the parent's virtual size — resolve it
    // from the catalog (populated by discovery/create in both real and fake mode) so quotas apply
    // here the same as they do to `POST /volumes`.
    let parent_native = format!("rbd:{pool_name}/{image}");
    let parent = existing
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(parent_native.as_str()));
    // A tenant-scoped operator must not clone a parent image they don't own, regardless of which
    // tenant they attribute the new clone to.
    if let Some(p) = &parent {
        let parent_tenant = atlas_inventory::volume_tenant(&s.pool, &p.id).await?;
        crate::auth::require_tenant(
            s.config.auth_required,
            &actor,
            &parent_tenant,
            format!("rbd image {pool_name}/{image}"),
        )?;
    }
    let parent_size = parent.map(|v| v.size_bytes).unwrap_or(0);
    match atlas_inventory::tenants::check_admission(&s.pool, &tenant_id, parent_size).await? {
        atlas_inventory::tenants::QuotaCheck::Ok => {}
        atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
            return Err(AppError::Conflict(format!(
                "tenant {tenant_id} byte quota exceeded: {would_be} > {limit}"
            )));
        }
        atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
            return Err(AppError::Conflict(format!(
                "tenant {tenant_id} volume-count quota exceeded: {current} already at limit {limit}"
            )));
        }
    }

    // Deterministic, matching create_rbd_image's reasoning: must be the same id a discovery pass
    // would derive for this clone's pool/image, or the clone's own row gets orphaned/pruned the
    // next time discovery runs.
    let volume_id = format!("vol_{pool_name}_{}", body.name);
    let job_id = ids::job_id();
    let spec = JobSpec::RbdClone {
        volume_id: volume_id.clone(),
        backend_id: CEPH_BACKEND_ID.into(),
        pool: pool_name.clone(),
        image: image.clone(),
        snap: snap.clone(),
        clone_image: body.name.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, &tenant_id, &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&tenant_id),
        &actor.id,
        "rbd_image.clone.requested",
        "rbd_image",
        &volume_id,
        "accepted",
        Some(json!({ "clone": format!("{pool_name}/{}", body.name),
                     "parent": format!("{pool_name}/{image}@{snap}") })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "volume_id": volume_id, "clone": format!("{pool_name}/{}", body.name),
                "parent": format!("{pool_name}/{image}@{snap}") }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResizeRbdBody {
    size_bytes: i64,
    /// Day-2: permit a shrink (guarded — a shrink can lose data past the new size). Default false.
    #[serde(default)]
    allow_shrink: bool,
}

/// `POST /rbd-images/{pool}/{image}/resize` — resize a raw RBD image (operator). Grow by default;
/// shrink requires `allow_shrink: true`.
pub(crate) async fn resize_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Json(body): Json<ResizeRbdBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.size_bytes <= 0 {
        return Err(AppError::Validation("size_bytes must be > 0".into()));
    }
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &volume_id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("rbd image {pool_name}/{image}"),
    )?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdResize {
        volume_id,
        pool: pool_name.clone(),
        image: image.clone(),
        new_size_bytes: body.size_bytes,
        allow_shrink: body.allow_shrink,
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
        "rbd_image.resize.requested",
        "rbd_image",
        &format!("{pool_name}/{image}"),
        "accepted",
        Some(json!({ "new_size_bytes": body.size_bytes, "allow_shrink": body.allow_shrink })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}"), "new_size_bytes": body.size_bytes, "allow_shrink": body.allow_shrink }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MigrateQuery {
    dest_pool: String,
}

/// `POST /rbd-images/{pool}/{image}/migrate?dest_pool=<pool>` — live-migrate an RBD image to another
/// pool (operator; `rbd migration prepare→execute→commit`).
pub(crate) async fn migrate_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Query(q): Query<MigrateQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if q.dest_pool.trim().is_empty() || q.dest_pool == pool_name {
        return Err(AppError::Validation(
            "dest_pool must be a different, non-empty pool".into(),
        ));
    }
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &volume_id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("rbd image {pool_name}/{image}"),
    )?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdMigrate {
        volume_id,
        pool: pool_name.clone(),
        image: image.clone(),
        dest_pool: q.dest_pool.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "rbd_image.migrate.requested",
        "rbd_image",
        &format!("{pool_name}/{image}"),
        "accepted",
        Some(json!({ "dest_pool": q.dest_pool })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "from": format!("{pool_name}/{image}"), "to": format!("{}/{image}", q.dest_pool) }),
    ))
}

/// `POST /rbd-images/{pool}/{image}/flatten` — detach a COW clone from its parent (operator).
pub(crate) async fn flatten_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &volume_id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("rbd image {pool_name}/{image}"),
    )?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdFlatten {
        pool: pool_name.clone(),
        image: image.clone(),
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
        "rbd_image.flatten.requested",
        "rbd_image",
        &format!("{pool_name}/{image}"),
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}") }),
    ))
}

/// `GET /rbd-images/{pool}/{image}/snapshots` — list a raw image's snapshots.
/// Real mode: live `rbd snap ls`. Fake/dev mode: no `rbd` binary in the image — read back the
/// catalog that create/clone/delete jobs maintain instead.
pub(crate) async fn list_rbd_snaps(
    State(s): State<AppState>,
    Path((pool_name, image)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    use atlas_common::config::CephDriverMode;

    let snaps = match s.config.ceph_driver_mode {
        CephDriverMode::Fake => {
            atlas_inventory::rbd_snapshots::list(&s.pool, &pool_name, &image).await?
        }
        CephDriverMode::Real => atlas_driver_ceph::rbd_snap_list(&pool_name, &image)
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    };
    Ok(Json(
        json!({ "rbd": format!("{pool_name}/{image}"), "snapshots": snaps }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RbdSnapBody {
    name: String,
}

/// `POST /rbd-images/{pool}/{image}/snapshots` — snapshot a raw RBD image (operator).
pub(crate) async fn create_rbd_snap(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Json(body): Json<RbdSnapBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &volume_id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("rbd image {pool_name}/{image}"),
    )?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdSnapshot {
        pool: pool_name.clone(),
        image: image.clone(),
        snap: body.name.clone(),
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
        "rbd_snapshot.create.requested",
        "rbd_snapshot",
        &format!("{pool_name}/{image}@{}", body.name),
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "snapshot": format!("{pool_name}/{image}@{}", body.name) }),
    ))
}

/// `POST /rbd-images/{pool}/{image}/rollback` — roll a raw image back to a snapshot (admin).
pub(crate) async fn rollback_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Json(body): Json<RbdSnapBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    let job_id = ids::job_id();
    let spec = JobSpec::RbdRollback {
        pool: pool_name.clone(),
        image: image.clone(),
        snap: body.name.clone(),
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
        "rbd_image.rollback.requested",
        "rbd_image",
        &format!("{pool_name}/{image}"),
        "accepted",
        Some(json!({ "rollback_to": body.name })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}"), "rollback_to": body.name }),
    ))
}

/// `DELETE /rbd-images/{pool}/{image}/snapshots/{snap}` — delete a raw image's snapshot (admin).
/// Without this, a snapshot created through the UI has no path back except deleting the whole
/// parent image, and `rbd rm` on an image with any snapshots fails outright.
pub(crate) async fn delete_rbd_snap(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image, snap)): Path<(String, String, String)>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdSnapDelete {
        pool: pool_name.clone(),
        image: image.clone(),
        snap: snap.clone(),
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
        "rbd_snapshot.delete.requested",
        "rbd_snapshot",
        &format!("{pool_name}/{image}@{snap}"),
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}"), "deleted_snapshot": snap }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RbdListQuery {
    pool: Option<String>,
}

/// `GET /rbd-images?pool=` — list RBD image names in a pool.
/// Real mode: live `rbd ls`. Fake/dev mode: no `rbd` binary in the image — derive names from the
/// inventory catalog (the same source of truth the create/delete/clone jobs write to) so newly
/// created or removed images show up immediately instead of a static fixture snapshot.
pub(crate) async fn list_rbd_images(
    State(s): State<AppState>,
    Query(q): Query<RbdListQuery>,
) -> AppResult<Json<Value>> {
    use atlas_common::config::CephDriverMode;

    let pool_name = q.pool.unwrap_or_else(|| DEFAULT_RBD_POOL.into());

    let images = match s.config.ceph_driver_mode {
        CephDriverMode::Fake => {
            let vols = atlas_inventory::list_volumes(&s.pool).await?;
            vols.into_iter()
                .filter_map(|v| {
                    let native = v.backend_native_id.as_deref()?;
                    let path = native.strip_prefix("rbd:").unwrap_or(native);
                    let (p, img) = path.split_once('/')?;
                    (p == pool_name).then(|| img.to_string())
                })
                .collect::<Vec<_>>()
        }
        CephDriverMode::Real => atlas_driver_ceph::rbd_list(&pool_name)
            .await
            .map_err(|e| AppError::Driver(e.to_string()))?,
    };
    Ok(Json(json!({ "pool": pool_name, "images": images })))
}
