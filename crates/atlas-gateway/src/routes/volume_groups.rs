// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Multi-image consistency groups (`rbd group`): one crash-consistent snapshot across several
//! direct-RBD volumes, and rollback of all of them to it. Group snapshots stay on this cluster —
//! rbd-mirror does not replicate them and no released Ceph has `rbd mirror group` (docs/DR.md).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::dr::rbd_of_volume;
use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

fn real_ceph(s: &AppState) -> bool {
    matches!(
        s.config.ceph_driver_mode,
        atlas_common::config::CephDriverMode::Real
    )
}

/// RBD group and snapshot names become part of CLI arguments; keep them to a safe charset.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> AppError {
    AppError::Unavailable(format!("{what}: {e}"))
}

#[derive(Debug, Deserialize)]
pub(crate) struct GroupBody {
    name: String,
    volume_ids: Vec<String>,
}

/// Upper bound on a group's size, so a request cannot make the handler allocate or loop without limit.
const MAX_GROUP_MEMBERS: usize = 256;

/// `POST /volume-groups` — create an RBD group from direct-RBD volumes (admin). The group lives
/// in the first volume's pool; members may be in other pools of the same cluster.
pub(crate) async fn create_volume_group(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<GroupBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !valid_name(&body.name) {
        return Err(AppError::Validation(
            "name must be 1-64 characters of [A-Za-z0-9._-], not starting with '.'".into(),
        ));
    }
    if body.volume_ids.is_empty() {
        return Err(AppError::Validation("volume_ids must not be empty".into()));
    }
    if body.volume_ids.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::Validation(format!(
            "a group takes at most {MAX_GROUP_MEMBERS} volumes"
        )));
    }
    let mut members = Vec::with_capacity(body.volume_ids.len());
    for vid in &body.volume_ids {
        if members
            .iter()
            .any(|(v, _, _): &(String, String, String)| v == vid)
        {
            return Err(AppError::Validation(format!(
                "volume {vid} is listed twice"
            )));
        }
        if let Some(g) = atlas_inventory::volume_groups::group_of_volume(&s.pool, vid).await? {
            return Err(AppError::Conflict(format!(
                "volume {vid} is already in group {g} (an RBD image can be in one group)"
            )));
        }
        let (pool, image) = rbd_of_volume(&s, vid).await?;
        members.push((vid.clone(), pool, image));
    }
    let pool = members[0].1.clone();
    if atlas_inventory::volume_groups::exists(&s.pool, &pool, &body.name).await? {
        return Err(AppError::Conflict(format!(
            "group {pool}/{} already exists",
            body.name
        )));
    }
    if real_ceph(&s) {
        atlas_driver_ceph::rbd_group_create(&pool, &body.name)
            .await
            .map_err(|e| {
                if e.to_string().contains("(17) File exists") {
                    AppError::Conflict(format!(
                        "rbd group {pool}/{} already exists in Ceph but not in Atlas's catalog",
                        body.name
                    ))
                } else {
                    unavailable("rbd group create", e)
                }
            })?;
        for (i, (_, mpool, image)) in members.iter().enumerate() {
            if let Err(e) =
                atlas_driver_ceph::rbd_group_image_add(&pool, &body.name, mpool, image).await
            {
                for (_, p, img) in &members[..i] {
                    let _ =
                        atlas_driver_ceph::rbd_group_image_remove(&pool, &body.name, p, img).await;
                }
                let _ = atlas_driver_ceph::rbd_group_remove(&pool, &body.name).await;
                return Err(unavailable(
                    &format!("rbd group image add {mpool}/{image}"),
                    e,
                ));
            }
        }
    }
    let id = ids::stable_id("vgr", &format!("{pool}/{}", body.name));
    atlas_inventory::volume_groups::create(&s.pool, &id, "global", &body.name, &pool, &members)
        .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "volume_group.create",
        "volume_group",
        &id,
        "ok",
        Some(json!({ "volume_ids": body.volume_ids })),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id, "name": body.name, "pool": pool,
            "volume_ids": body.volume_ids,
        })),
    ))
}

pub(crate) async fn list_volume_groups(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(json!(
        atlas_inventory::volume_groups::list(&s.pool).await?
    )))
}

async fn group_or_404(s: &AppState, id: &str) -> AppResult<(String, String)> {
    atlas_inventory::volume_groups::get(&s.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume group {id}")))
}

/// `GET /volume-groups/{id}` — members and snapshots; in real mode also Ceph's own view (operator).
pub(crate) async fn get_volume_group(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let (name, pool) = group_or_404(&s, &id).await?;
    let members: Vec<Value> = atlas_inventory::volume_groups::members(&s.pool, &id)
        .await?
        .into_iter()
        .map(|(v, p, i)| json!({ "volume_id": v, "rbd": format!("{p}/{i}") }))
        .collect();
    let snapshots = atlas_inventory::volume_groups::snapshots(&s.pool, &id).await?;
    let live = if real_ceph(&s) {
        let images = atlas_driver_ceph::rbd_group_image_list(&pool, &name)
            .await
            .map_err(|e| unavailable("rbd group image list", e))?;
        let snaps = atlas_driver_ceph::rbd_group_snap_list(&pool, &name)
            .await
            .map_err(|e| unavailable("rbd group snap list", e))?;
        Some(json!({ "images": images, "snapshots": snaps }))
    } else {
        None
    };
    Ok(Json(json!({
        "id": id, "name": name, "pool": pool,
        "members": members, "snapshots": snapshots, "live": live,
    })))
}

/// `DELETE /volume-groups/{id}` — remove the group (never its volumes) and its group snapshots
/// (admin).
pub(crate) async fn delete_volume_group(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let (name, pool) = group_or_404(&s, &id).await?;
    if real_ceph(&s) {
        atlas_driver_ceph::rbd_group_remove(&pool, &name)
            .await
            .map_err(|e| unavailable("rbd group remove", e))?;
    }
    atlas_inventory::volume_groups::delete(&s.pool, &id).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "volume_group.delete",
        "volume_group",
        &id,
        "ok",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct GroupSnapBody {
    name: String,
}

/// `POST /volume-groups/{id}/snapshots` — one crash-consistent snapshot of every member (admin).
pub(crate) async fn create_group_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<GroupSnapBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !valid_name(&body.name) {
        return Err(AppError::Validation(
            "name must be 1-64 characters of [A-Za-z0-9._-], not starting with '.'".into(),
        ));
    }
    let (name, pool) = group_or_404(&s, &id).await?;
    if atlas_inventory::volume_groups::snapshot_exists(&s.pool, &id, &body.name).await? {
        return Err(AppError::Conflict(format!(
            "group snapshot {} already exists",
            body.name
        )));
    }
    if real_ceph(&s) {
        atlas_driver_ceph::rbd_group_snap_create(&pool, &name, &body.name)
            .await
            .map_err(|e| unavailable("rbd group snap create", e))?;
    }
    let snap_id = ids::stable_id("vgs", &format!("{id}@{}", body.name));
    atlas_inventory::volume_groups::add_snapshot(&s.pool, &snap_id, &id, &body.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": snap_id, "group_id": id, "name": body.name,
            "snapshot": format!("{pool}/{name}@{}", body.name),
        })),
    ))
}

pub(crate) async fn list_group_snapshots(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    group_or_404(&s, &id).await?;
    Ok(Json(json!(
        atlas_inventory::volume_groups::snapshots(&s.pool, &id).await?
    )))
}

/// `DELETE /volume-groups/{id}/snapshots/{snap}` (admin).
pub(crate) async fn delete_group_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, snap)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let (name, pool) = group_or_404(&s, &id).await?;
    if !atlas_inventory::volume_groups::snapshot_exists(&s.pool, &id, &snap).await? {
        return Err(AppError::NotFound(format!("group snapshot {snap}")));
    }
    if real_ceph(&s) {
        atlas_driver_ceph::rbd_group_snap_remove(&pool, &name, &snap)
            .await
            .map_err(|e| unavailable("rbd group snap remove", e))?;
    }
    atlas_inventory::volume_groups::delete_snapshot(&s.pool, &id, &snap).await?;
    Ok(Json(
        json!({ "group_id": id, "snapshot": snap, "deleted": true }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RollbackBody {
    #[serde(default)]
    confirm: bool,
}

/// `POST /volume-groups/{id}/snapshots/{snap}/rollback` — every member back to the group snapshot
/// (admin, `confirm: true`; destructive, the volumes must not be in use).
pub(crate) async fn rollback_group_snapshot(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, snap)): Path<(String, String)>,
    Json(body): Json<RollbackBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required: rollback discards every member's writes since the snapshot"
                .into(),
        ));
    }
    let (name, pool) = group_or_404(&s, &id).await?;
    if !atlas_inventory::volume_groups::snapshot_exists(&s.pool, &id, &snap).await? {
        return Err(AppError::NotFound(format!("group snapshot {snap}")));
    }
    let job_id = ids::job_id();
    let spec = JobSpec::RbdGroupRollback {
        pool: pool.clone(),
        group: name.clone(),
        snap: snap.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "volume_group.rollback.requested",
        "volume_group",
        &id,
        "accepted",
        Some(json!({ "rollback_to": snap })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "group_id": id, "group": format!("{pool}/{name}"), "rollback_to": snap }),
    ))
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn names_are_cli_safe() {
        assert!(valid_name("app-db_1.cp"));
        assert!(!valid_name(""));
        assert!(!valid_name(".mirror"));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("a@b"));
        assert!(!valid_name(&"x".repeat(65)));
    }
}
