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

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

// ---- cross-cluster DR (RBD mirroring; real ops drilled two-way on a lab, see docs/DR.md) ----

#[derive(Debug, Deserialize)]
pub(crate) struct PeerBody {
    name: String,
    cluster_fsid: Option<String>,
    direction: Option<String>,
    /// k8s Secret holding the peer bootstrap token (never the token itself).
    secret_ref: Option<String>,
}

/// `POST /dr/peers` — register a mirroring peer cluster (admin).
/// `DELETE /dr/peers/{id}` — remove a mirroring peer (and any mirrors that referenced it).
pub(crate) async fn delete_dr_peer(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    atlas_inventory::dr::delete_peer(&s.pool, &id).await?;
    Ok(Json(json!({ "peer_id": id, "deleted": true })))
}

pub(crate) async fn register_dr_peer(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<PeerBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    let id = ids::stable_id("drp", &body.name);
    let direction = body.direction.as_deref().unwrap_or("rx-tx");
    atlas_inventory::dr::register_peer(
        &s.pool,
        &id,
        &body.name,
        body.cluster_fsid.as_deref(),
        direction,
        body.secret_ref.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "name": body.name, "direction": direction })),
    ))
}

pub(crate) async fn list_dr_peers(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(json!(atlas_inventory::dr::list_peers(&s.pool).await?)))
}

pub(crate) async fn list_dr_mirrors(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(json!(
        atlas_inventory::dr::list_mirrors(&s.pool).await?
    )))
}

/// `GET /dr/status` — DR posture: mirror counts by role/state + worst RPO.
pub(crate) async fn dr_status(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let mirrors = atlas_inventory::dr::list_mirrors(&s.pool).await?;
    let primaries = mirrors.iter().filter(|m| m["role"] == "primary").count();
    let secondaries = mirrors.iter().filter(|m| m["role"] == "secondary").count();
    let errored = mirrors.iter().filter(|m| m["state"] == "error").count();
    let worst_rpo = mirrors
        .iter()
        .filter_map(|m| m["rpo_seconds"].as_i64())
        .max();
    let peers = atlas_inventory::dr::list_peers(&s.pool).await?.len();
    let control_plane_ready = peers > 0 && errored == 0;
    Ok(Json(json!({
        "peers": peers, "mirrors": mirrors.len(),
        "primary": primaries, "secondary": secondaries,
        "error": errored, "worst_rpo_seconds": worst_rpo,
        "control_plane_ready": control_plane_ready,
        "dataplane_verified": s.config.dr_dataplane_verified,
        "verified": s.config.dr_dataplane_verified,
        "note": if s.config.dr_dataplane_verified {
            "control-plane catalog ready; this deployment has completed the live two-site rbd mirror drill — see docs/DR.md"
        } else {
            "control-plane catalog ready; live rbd mirror needs a second Ceph cluster — see docs/DR.md"
        },
    })))
}

/// `GET /dr/preflight` — control-plane checklist before a failover drill (operator).
pub(crate) async fn dr_preflight(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(
        atlas_inventory::dr::preflight(&s.pool, s.config.dr_dataplane_verified).await?,
    ))
}

/// Resolve a volume id to its `(pool, image)` (direct-RBD `rbd:<pool>/<image>` native id).
pub(crate) async fn rbd_of_volume(s: &AppState, volume_id: &str) -> AppResult<(String, String)> {
    let vols = atlas_inventory::list_volumes(&s.pool).await?;
    let v = vols
        .iter()
        .find(|v| v.id == volume_id)
        .ok_or_else(|| AppError::NotFound(format!("volume {volume_id}")))?;
    let native = v.backend_native_id.as_deref().unwrap_or("");
    let rest = native
        .strip_prefix("rbd:")
        .ok_or_else(|| AppError::Validation("volume is not a direct RBD image".into()))?;
    let (pool, image) = rest
        .split_once('/')
        .ok_or_else(|| AppError::Validation("malformed rbd native id".into()))?;
    Ok((pool.to_string(), image.to_string()))
}

fn real_ceph(s: &AppState) -> bool {
    matches!(
        s.config.ceph_driver_mode,
        atlas_common::config::CephDriverMode::Real
    )
}

fn valid_pool_name(pool: &str) -> bool {
    !pool.is_empty()
        && pool.len() <= 128
        && pool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A pool's mirroring mode: live from `rbd mirror pool info` in real mode, else what Atlas last
/// recorded (default `image`, the mode Rook's CephBlockPool mirroring usually sets).
async fn pool_mirror_mode(s: &AppState, rbd_pool: &str) -> AppResult<String> {
    if real_ceph(s) {
        let info = atlas_driver_ceph::rbd_mirror_pool_info(rbd_pool)
            .await
            .map_err(|e| AppError::Unavailable(format!("rbd mirror pool info {rbd_pool}: {e}")))?;
        return Ok(info
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("disabled")
            .to_string());
    }
    Ok(atlas_inventory::dr::pool_mode(&s.pool, rbd_pool)
        .await?
        .unwrap_or_else(|| "image".into()))
}

/// `GET /dr/pools/{pool}/mirroring` — the pool's mirroring mode and peers (operator).
pub(crate) async fn get_pool_mirroring(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(rbd_pool): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if !valid_pool_name(&rbd_pool) {
        return Err(AppError::Validation("invalid pool name".into()));
    }
    let live = if real_ceph(&s) {
        Some(
            atlas_driver_ceph::rbd_mirror_pool_info(&rbd_pool)
                .await
                .map_err(|e| {
                    AppError::Unavailable(format!("rbd mirror pool info {rbd_pool}: {e}"))
                })?,
        )
    } else {
        None
    };
    let mode = match &live {
        Some(info) => info
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("disabled")
            .to_string(),
        None => pool_mirror_mode(&s, &rbd_pool).await?,
    };
    let peers = live
        .as_ref()
        .and_then(|i| i.get("peers"))
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .map(
                    |p| json!({ "site_name": p.get("site_name"), "direction": p.get("direction") }),
                )
                .collect::<Vec<_>>()
        });
    Ok(Json(json!({
        "pool": rbd_pool, "mode": mode, "peers": peers,
        "recorded_mode": atlas_inventory::dr::pool_mode(&s.pool, &rbd_pool).await?,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PoolMirroringBody {
    mode: String,
}

/// `PUT /dr/pools/{pool}/mirroring` `{"mode":"image"|"pool"}` — set the pool's mirroring mode
/// (admin). In `pool` mode every image with the `journaling` feature is mirrored, with no
/// per-image enable. On Rook, set the CephBlockPool's `spec.mirroring.mode` to match, or the
/// operator puts its own value back on its next reconcile.
pub(crate) async fn set_pool_mirroring(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(rbd_pool): Path<String>,
    Json(body): Json<PoolMirroringBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !valid_pool_name(&rbd_pool) {
        return Err(AppError::Validation("invalid pool name".into()));
    }
    if !matches!(body.mode.as_str(), "image" | "pool") {
        return Err(AppError::Validation("mode must be image or pool".into()));
    }
    if real_ceph(&s) {
        atlas_driver_ceph::rbd_mirror_pool_enable(&rbd_pool, &body.mode)
            .await
            .map_err(|e| {
                AppError::Unavailable(format!("rbd mirror pool enable {rbd_pool}: {e}"))
            })?;
    }
    atlas_inventory::dr::set_pool_mode(&s.pool, &rbd_pool, &body.mode).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "dr.pool.mirroring",
        "rbd_pool",
        &rbd_pool,
        "ok",
        Some(json!({ "mode": body.mode })),
        None,
    )
    .await;
    Ok(Json(json!({ "pool": rbd_pool, "mode": body.mode })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MirrorQuery {
    mode: Option<String>,
    peer: Option<String>,
    /// `secondary`: record this site's copy of an image the peer mirrors to us instead of enabling
    /// mirroring here. Defaults to `primary`.
    role: Option<String>,
}

/// `POST /volumes/{id}/mirror?mode=snapshot&peer=<id>[&role=secondary]` — enable RBD mirroring for
/// a volume, or register the peer site's non-primary copy so it can later be promoted (admin).
pub(crate) async fn enable_mirror(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(volume_id): Path<String>,
    Query(q): Query<MirrorQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let (rbd_pool, image) = rbd_of_volume(&s, &volume_id).await?;
    let mode = q.mode.as_deref().unwrap_or("snapshot");
    if !matches!(mode, "snapshot" | "journal") {
        return Err(AppError::Validation(
            "mode must be snapshot or journal".into(),
        ));
    }
    let peer = if let Some(peer) = q.peer.as_deref() {
        if !atlas_inventory::dr::peer_exists(&s.pool, peer).await? {
            return Err(AppError::Validation(format!(
                "unknown DR peer '{peer}' — register it via POST /dr/peers first"
            )));
        }
        Some(peer.to_string())
    } else {
        let peers = atlas_inventory::dr::list_peers(&s.pool).await?;
        if peers.is_empty() {
            return Err(AppError::Validation(
                "no DR peers registered — POST /dr/peers before enabling mirroring".into(),
            ));
        }
        None
    };
    let role = q.role.as_deref().unwrap_or("primary");
    if !matches!(role, "primary" | "secondary") {
        return Err(AppError::Validation(
            "role must be primary or secondary".into(),
        ));
    }
    let mirror_id = ids::stable_id("drm", &format!("{rbd_pool}/{image}"));
    let real = real_ceph(&s);
    // In a pool-mode pool `rbd mirror image enable` is refused: an image is mirrored by having
    // the journaling feature, so the enable job turns that on instead.
    let pool_mode = pool_mirror_mode(&s, &rbd_pool).await?;
    match pool_mode.as_str() {
        "disabled" => {
            return Err(AppError::Conflict(format!(
                "mirroring is disabled on pool {rbd_pool} — PUT /dr/pools/{rbd_pool}/mirroring first"
            )))
        }
        "pool" if mode != "journal" => {
            return Err(AppError::Validation(format!(
                "pool {rbd_pool} is in pool mirroring mode, which mirrors journaling images only — use mode=journal"
            )))
        }
        _ => {}
    }
    let enable_action = if pool_mode == "pool" {
        "join-pool"
    } else {
        "enable"
    };
    let primary_here = if real {
        atlas_driver_ceph::rbd_mirror_primary(&rbd_pool, &image)
            .await
            .map_err(|e| AppError::Unavailable(format!("rbd info {rbd_pool}/{image}: {e}")))?
    } else {
        None
    };
    // `rbd mirror image enable` on the peer's replicated copy is a successful no-op, so without
    // this check that copy would be catalogued as an enabled primary.
    if role == "primary" && primary_here == Some(false) {
        return Err(AppError::Conflict(format!(
            "{rbd_pool}/{image} is the peer's non-primary copy — register it with role=secondary"
        )));
    }
    if role == "secondary" {
        if real {
            match primary_here {
                Some(false) => {}
                Some(true) => {
                    return Err(AppError::Conflict(format!(
                        "{rbd_pool}/{image} is primary on this cluster — enable it as primary instead"
                    )))
                }
                None => {
                    return Err(AppError::Conflict(format!(
                        "{rbd_pool}/{image} is not mirrored here yet — enable it on the peer and wait for rbd-mirror to create it"
                    )))
                }
            }
        }
        atlas_inventory::dr::upsert_mirror(
            &s.pool,
            &mirror_id,
            "global",
            Some(&volume_id),
            &rbd_pool,
            &image,
            peer.as_deref(),
            mode,
            "secondary",
            "enabled",
        )
        .await?;
        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "mirror_id": mirror_id, "rbd": format!("{rbd_pool}/{image}"),
                "role": "secondary", "state": "enabled",
            })),
        ));
    }
    let state = if real { "enabling" } else { "enabled" };
    atlas_inventory::dr::upsert_mirror(
        &s.pool,
        &mirror_id,
        "global",
        Some(&volume_id),
        &rbd_pool,
        &image,
        peer.as_deref(),
        mode,
        "primary",
        state,
    )
    .await?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdMirror {
        mirror_id: mirror_id.clone(),
        pool: rbd_pool.clone(),
        image: image.clone(),
        action: enable_action.into(),
        mode: mode.to_string(),
        force: false,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    Ok(accepted(
        &job,
        json!({
            "mirror_id": mirror_id, "rbd": format!("{rbd_pool}/{image}"), "state": state,
            "pool_mode": pool_mode,
        }),
    ))
}

/// `DELETE /volumes/{id}/mirror` — disable RBD mirroring for a volume (admin).
pub(crate) async fn disable_mirror(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(volume_id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let (rbd_pool, image) = rbd_of_volume(&s, &volume_id).await?;
    let mirror_id = ids::stable_id("drm", &format!("{rbd_pool}/{image}"));
    let real = real_ceph(&s);
    // A pool-mode pool refuses `rbd mirror image disable`; dropping journaling takes the image out.
    let action = if pool_mirror_mode(&s, &rbd_pool).await? == "pool" {
        "leave-pool"
    } else {
        "disable"
    };
    atlas_inventory::dr::set_mirror(
        &s.pool,
        &mirror_id,
        "primary",
        if real { "disabling" } else { "disabled" },
    )
    .await?;
    let job_id = ids::job_id();
    let spec = JobSpec::RbdMirror {
        mirror_id: mirror_id.clone(),
        pool: rbd_pool.clone(),
        image: image.clone(),
        action: action.into(),
        mode: "snapshot".into(),
        force: false,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    Ok(accepted(
        &job,
        json!({ "mirror_id": mirror_id, "state": "disabling" }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PromoteQuery {
    /// Split-brain / non-clean failover: pass to `rbd mirror image promote --force`.
    #[serde(default, deserialize_with = "query_flag")]
    force: Option<bool>,
    /// Journal mode: the operator checked that the peer reports `journal_peers_replayed: true`.
    #[serde(default, deserialize_with = "query_flag")]
    peer_replayed: Option<bool>,
}

/// A query-string boolean written `0`/`1` or `false`/`true`.
fn query_flag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    match Option::<String>::deserialize(d)?.as_deref() {
        None => Ok(None),
        Some("1" | "true") => Ok(Some(true)),
        Some("0" | "false") => Ok(Some(false)),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected 0, 1, true or false, got '{other}'"
        ))),
    }
}

/// `POST /dr/mirrors/{id}/promote?force=0|1` — failover: promote this cluster's copy to primary.
pub(crate) async fn promote_mirror(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<PromoteQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    mirror_role_op(
        &s,
        &actor,
        &id,
        "promote",
        q.force.unwrap_or(false),
        q.peer_replayed.unwrap_or(false),
    )
    .await
}

/// `GET /dr/mirrors/{id}/status` — live `rbd mirror image status` of this site's copy and whether a
/// non-forced promote would be a clean failback (operator).
pub(crate) async fn mirror_status(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let (rbd_pool, image, role, state) = atlas_inventory::dr::mirror_detail(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("mirror {id}")))?;
    let real = matches!(
        s.config.ceph_driver_mode,
        atlas_common::config::CephDriverMode::Real
    );
    let (live, blocker) = if real {
        let status = atlas_driver_ceph::rbd_mirror_image_status(&rbd_pool, &image)
            .await
            .map_err(|e| {
                AppError::Unavailable(format!("rbd mirror image status {rbd_pool}/{image}: {e}"))
            })?;
        let mode = atlas_driver_ceph::rbd_mirror_mode(&rbd_pool, &image)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let journal_peers_replayed = if mode == "journal" {
            atlas_driver_ceph::rbd_journal_peers_replayed(&rbd_pool, &image)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let blocker = atlas_driver_ceph::mirror_promote_blocker(&status, &mode, false);
        let live = json!({
            "mode": mode,
            "state": status.get("state"),
            "description": status.get("description"),
            "last_update": status.get("last_update"),
            "peer_sites": status.get("peer_sites"),
            "journal_peers_replayed": journal_peers_replayed,
        });
        (Some(live), blocker)
    } else {
        (None, None)
    };
    let blocker = if role != "secondary" {
        Some(format!("this site's copy is {role}"))
    } else {
        blocker
    };
    Ok(Json(json!({
        "mirror_id": id, "rbd": format!("{rbd_pool}/{image}"),
        "role": role, "state": state, "live": live,
        "promote_ready": blocker.is_none(), "promote_blocker": blocker,
    })))
}

/// `POST /dr/mirrors/{id}/demote` — demote this cluster's copy to secondary (admin).
pub(crate) async fn demote_mirror(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    mirror_role_op(&s, &actor, &id, "demote", false, false).await
}

/// `POST /dr/mirrors/{id}/resync` — discard this cluster's non-primary copy and pull a full copy
/// from the peer's primary; the way out of split-brain after a forced or one-way failback (admin).
pub(crate) async fn resync_mirror(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    mirror_role_op(&s, &actor, &id, "resync", false, false).await
}

pub(crate) async fn mirror_role_op(
    s: &AppState,
    actor: &Actor,
    id: &str,
    action: &str,
    force: bool,
    peer_replayed: bool,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_ADMIN)?;
    let (rbd_pool, image, role, state) = atlas_inventory::dr::mirror_detail(&s.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("mirror {id}")))?;
    if matches!(state.as_str(), "disabled" | "disabling") {
        return Err(AppError::Conflict(format!(
            "mirror {id} is {state} — re-enable mirroring before {action}"
        )));
    }
    match action {
        "promote" if role == "primary" && !force => {
            return Err(AppError::Conflict(format!(
                "mirror {id} is already primary — pass ?force=1 only for split-brain recovery"
            )));
        }
        "demote" if role == "secondary" => {
            return Err(AppError::Conflict(format!(
                "mirror {id} is already secondary"
            )));
        }
        "resync" if role != "secondary" => {
            return Err(AppError::Conflict(format!(
                "resync discards this copy and needs role=secondary (have {role}); demote first"
            )));
        }
        "promote" if role != "secondary" && !force => {
            return Err(AppError::Conflict(format!(
                "promote requires role=secondary (have {role}); use ?force=1 for unclean failover"
            )));
        }
        _ => {}
    }
    let real = matches!(
        s.config.ceph_driver_mode,
        atlas_common::config::CephDriverMode::Real
    );
    if real && action == "promote" && !force {
        let status = atlas_driver_ceph::rbd_mirror_image_status(&rbd_pool, &image)
            .await
            .map_err(|e| {
                AppError::Unavailable(format!("rbd mirror image status {rbd_pool}/{image}: {e}"))
            })?;
        let mode = atlas_driver_ceph::rbd_mirror_mode(&rbd_pool, &image)
            .await
            .map_err(|e| AppError::Unavailable(format!("rbd info {rbd_pool}/{image}: {e}")))?
            .unwrap_or_default();
        if let Some(blocker) =
            atlas_driver_ceph::mirror_promote_blocker(&status, &mode, peer_replayed)
        {
            return Err(AppError::Conflict(format!(
                "promoting {rbd_pool}/{image} would not be a clean failback: {blocker}; demote \
                 the peer and wait for GET /dr/mirrors/{id}/status to report promote_ready, or \
                 pass ?force=1 and resync the peer afterwards"
            )));
        }
    }
    if !real {
        let new_role = if action == "promote" {
            "primary"
        } else {
            "secondary"
        };
        atlas_inventory::dr::set_mirror(&s.pool, id, new_role, "enabled").await?;
        if action == "promote" {
            let _ = atlas_inventory::dr::record_failover(&s.pool, id, force).await;
        }
    } else {
        let pending = match action {
            "promote" => "promoting",
            "resync" => "resyncing",
            _ => "demoting",
        };
        atlas_inventory::dr::set_mirror(&s.pool, id, &role, pending).await?;
    }
    let job_id = ids::job_id();
    let spec = JobSpec::RbdMirror {
        mirror_id: id.to_string(),
        pool: rbd_pool,
        image,
        action: action.to_string(),
        mode: "snapshot".into(),
        force,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        &format!("dr.mirror.{action}"),
        "dr_mirror",
        id,
        "ok",
        Some(json!({ "force": force })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "mirror_id": id, "action": action, "force": force }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct FailoverBody {
    mirror_id: String,
    confirm: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    peer_replayed: bool,
}

/// `POST /dr/failover` — one-click failover runbook: promote a secondary (admin, confirm required).
pub(crate) async fn dr_failover(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<FailoverBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if !body.confirm {
        return Err(AppError::Validation(
            "confirm=true is required for failover (destructive)".into(),
        ));
    }
    let pre = atlas_inventory::dr::preflight(&s.pool, s.config.dr_dataplane_verified).await?;
    if pre["ready"] == false && !body.force {
        return Err(AppError::Conflict(format!(
            "DR preflight not ready: {}; pass force=true to override",
            pre["blockers"]
        )));
    }
    let result = mirror_role_op(
        &s,
        &actor,
        &body.mirror_id,
        "promote",
        body.force,
        body.peer_replayed,
    )
    .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "dr.failover",
        "dr_mirror",
        &body.mirror_id,
        "ok",
        Some(json!({ "force": body.force, "preflight": pre })),
        None,
    )
    .await;
    Ok(result)
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpoBody {
    rpo_seconds: Option<i64>,
}

/// `POST /dr/mirrors/{id}/rpo` — record observed RPO seconds (operator).
pub(crate) async fn set_mirror_rpo(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<RpoBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if !atlas_inventory::dr::set_rpo(&s.pool, &id, body.rpo_seconds).await? {
        return Err(AppError::NotFound(format!("mirror {id}")));
    }
    Ok(Json(
        json!({ "mirror_id": id, "rpo_seconds": body.rpo_seconds }),
    ))
}
