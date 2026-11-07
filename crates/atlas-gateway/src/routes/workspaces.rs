// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent workspaces: an instant, isolated, disposable copy of an atlas-native volume or filesystem
//! for one agent run (docs/NATIVE_FS.md#agent-workspaces).
//!
//! A workspace is a snapshot (taken here, or an existing one you name) plus a clone of it, a TTL and
//! an optional size cap. Everything goes through the existing snapshot/clone handlers, so role,
//! tenant and quota checks are the ones the rest of the API applies. `reset` re-clones from the base
//! snapshot (rollback), `discard` deletes the clone and the snapshot Atlas took, and the GC discards
//! expired workspaces. The source volume is never written. Only atlas-native sources are supported.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};
use atlas_driver_core::StorageDriver;
use atlas_inventory::workspaces::{self as store, Workspace};

use super::volumes::{self, audit, driver_error, native_driver_for_volume};
use crate::auth::{require_role, require_tenant, tenant_scope, Actor, ROLE_OPERATOR};
use crate::startup::NATIVE_BACKEND_ID;
use crate::state::AppState;

const DEFAULT_TTL_SECS: i64 = 3600;
const MIN_TTL_SECS: i64 = 60;
const DEFAULT_MAX_TTL_SECS: i64 = 7 * 24 * 3600;
const DEFAULT_MAX_PER_TENANT: i64 = 10;
/// Default seconds between GC passes (`ATLAS_WORKSPACE_GC_SECS`; 0 turns the worker off).
const DEFAULT_GC_SECS: u64 = 60;

fn env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(default)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CreateWorkspaceBody {
    /// Volume or filesystem to copy (a snapshot of it is taken now). Give this or `source_snapshot_id`.
    #[serde(default)]
    pub(crate) source_volume_id: Option<String>,
    /// An existing snapshot to clone instead; it is left alone when the workspace goes away.
    #[serde(default)]
    pub(crate) source_snapshot_id: Option<String>,
    /// Human label, for example the agent run id.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Lifetime in seconds (60 to `ATLAS_WORKSPACE_MAX_TTL_SECS`, default 3600).
    #[serde(default)]
    pub(crate) ttl_secs: Option<i64>,
    /// Refuse a source larger than this many bytes (0 or unset: `ATLAS_WORKSPACE_DEFAULT_MAX_BYTES`, else none).
    #[serde(default)]
    pub(crate) max_bytes: Option<i64>,
}

/// The workspace as the API shows it: the row plus what it uses now and how long it has left.
async fn view(s: &AppState, w: &Workspace) -> AppResult<Value> {
    let used = if w.volume_id.is_empty() {
        0
    } else {
        atlas_inventory::get_volume(&s.pool, &w.volume_id)
            .await?
            .map_or(0, |v| v.size_bytes)
    };
    let mut v = serde_json::to_value(w).map_err(|e| AppError::Internal(e.to_string()))?;
    v["used_bytes"] = json!(used);
    v["over_cap"] = json!(w.max_bytes > 0 && used > w.max_bytes);
    v["remaining_secs"] = json!((w.expires_at_unix - now_unix()).max(0));
    Ok(v)
}

async fn load(s: &AppState, actor: &Actor, id: &str) -> AppResult<Workspace> {
    let w = store::get(&s.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("workspace {id}")))?;
    require_tenant(
        s.config.auth_required,
        actor,
        &w.tenant_id,
        format!("workspace {id}"),
    )?;
    Ok(w)
}

fn native_driver(s: &AppState) -> AppResult<std::sync::Arc<dyn StorageDriver>> {
    s.drivers
        .get(NATIVE_BACKEND_ID)
        .ok_or_else(|| AppError::Unavailable("the atlas-native driver is not enabled".into()))
}

/// Delete a clone at the backend and in the inventory; already gone counts as done.
async fn delete_clone(s: &AppState, volume_id: &str) -> AppResult<()> {
    if volume_id.is_empty() {
        return Ok(());
    }
    let Some(vol) = atlas_inventory::get_volume(&s.pool, volume_id).await? else {
        return Ok(());
    };
    let native = vol
        .backend_native_id
        .clone()
        .unwrap_or_else(|| volume_id.to_string());
    match native_driver(s)?
        .delete_volume(atlas_api_types::DeleteVolumeRequest { volume_id: native })
        .await
        .map_err(driver_error)
    {
        Ok(()) | Err(AppError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    atlas_inventory::delete_volume_row(&s.pool, volume_id).await?;
    Ok(())
}

/// Delete the snapshot Atlas took for a workspace. Returns why it was kept, if it was: another
/// volume was cloned from it, so deleting it is not this workspace's call.
async fn delete_base_snapshot(s: &AppState, snapshot_id: &str) -> AppResult<Option<&'static str>> {
    let Some(snap) = atlas_inventory::snapshots::get_snapshot(&s.pool, snapshot_id).await? else {
        return Ok(None);
    };
    if atlas_inventory::count_snapshot_dependents(&s.pool, snapshot_id).await? > 0 {
        return Ok(Some("other volumes were cloned from it"));
    }
    let native = snap
        .backend_native_id
        .clone()
        .unwrap_or_else(|| snapshot_id.to_string());
    match native_driver(s)?
        .delete_snapshot(atlas_api_types::DeleteSnapshotRequest { snapshot_id: native })
        .await
        .map_err(driver_error)
    {
        Ok(()) | Err(AppError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    atlas_inventory::snapshots::delete_snapshot_row(&s.pool, snapshot_id).await?;
    Ok(None)
}

/// Clone `snapshot_id` as a new volume through the regular clone handler (tenant quota applies).
async fn clone_base(
    s: &AppState,
    actor: &Actor,
    snapshot_id: &str,
    volume_name: &str,
) -> AppResult<String> {
    let body = serde_json::from_value(json!({ "name": volume_name }))
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let (_, Json(out)) = volumes::enqueue_clone(s, actor, snapshot_id, "clone", volume_name.into(), body).await?;
    out["volume_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| AppError::Internal(format!("clone returned no volume id: {out}")))
}

/// The workspace size cap for a request: the given one, else `ATLAS_WORKSPACE_DEFAULT_MAX_BYTES`, else 0
/// (none).
pub(crate) fn resolve_max_bytes(requested: Option<i64>) -> AppResult<i64> {
    match requested {
        Some(b) if b < 0 => Err(AppError::Validation("max_bytes must not be negative".into())),
        Some(b) if b > 0 => Ok(b),
        _ => Ok(env_i64("ATLAS_WORKSPACE_DEFAULT_MAX_BYTES", 0)),
    }
}

/// The byte budget on the workspaces `actor` may hold at once: the token's `workspace_bytes` claim,
/// else `ATLAS_AGENT_WORKSPACE_BYTES`; 0 = no budget.
fn byte_budget(actor: &Actor) -> i64 {
    actor
        .workspace_bytes
        .filter(|b| *b > 0)
        .unwrap_or_else(|| env_i64("ATLAS_AGENT_WORKSPACE_BYTES", 0))
}

/// Everything that can refuse a new workspace before anything is created: the per-tenant count, the
/// source against its size cap, and the caller's byte budget. The MCP plan phase calls this too, so a
/// plan can say up front that the execute would be refused. Not atomic with the insert: two creates
/// racing, on one pod or two, can both pass (as with the per-tenant count).
pub(crate) async fn preflight_create(
    s: &AppState,
    actor: &Actor,
    tenant: &str,
    source_size_bytes: i64,
    max_bytes: i64,
) -> AppResult<()> {
    let per_tenant = env_i64("ATLAS_WORKSPACE_MAX_PER_TENANT", DEFAULT_MAX_PER_TENANT);
    if per_tenant > 0 && store::count_for_tenant(&s.pool, tenant).await? >= per_tenant {
        return Err(AppError::Conflict(format!(
            "tenant {tenant} already has {per_tenant} workspaces (ATLAS_WORKSPACE_MAX_PER_TENANT); discard one first"
        )));
    }
    if max_bytes > 0 && source_size_bytes > max_bytes {
        return Err(AppError::Conflict(format!(
            "source is {source_size_bytes} bytes, over the workspace cap of {max_bytes}"
        )));
    }
    let budget = byte_budget(actor);
    if budget > 0 {
        if max_bytes == 0 {
            return Err(AppError::Validation(
                "a workspace byte budget applies to this caller, so max_bytes must be set".into(),
            ));
        }
        let held = store::live_max_bytes_for_owner(&s.pool, tenant, &actor.id).await?;
        if held.saturating_add(max_bytes) > budget {
            return Err(AppError::Conflict(format!(
                "{} already holds {held} bytes of workspaces; {max_bytes} more would pass its budget of {budget}",
                actor.id
            )));
        }
    }
    Ok(())
}

/// `POST /workspaces`
pub(crate) async fn create_workspace(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateWorkspaceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    require_role(s.config.auth_required, &actor, ROLE_OPERATOR)?;
    let ttl = body.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
    let max_ttl = env_i64("ATLAS_WORKSPACE_MAX_TTL_SECS", DEFAULT_MAX_TTL_SECS).max(MIN_TTL_SECS);
    if !(MIN_TTL_SECS..=max_ttl).contains(&ttl) {
        return Err(AppError::Validation(format!(
            "ttl_secs must be between {MIN_TTL_SECS} and {max_ttl}"
        )));
    }
    let max_bytes = resolve_max_bytes(body.max_bytes)?;

    // Resolve the source and the tenant it belongs to; a caller from another tenant gets a 404.
    let (source_volume_id, existing_snapshot) = match (&body.source_volume_id, &body.source_snapshot_id) {
        (Some(v), None) => (v.clone(), None),
        (None, Some(sn)) => {
            let snap = atlas_inventory::snapshots::get_snapshot(&s.pool, sn)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("snapshot {sn}")))?;
            require_tenant(s.config.auth_required, &actor, &snap.tenant_id, format!("snapshot {sn}"))?;
            (snap.volume_id.clone(), Some(snap))
        }
        _ => {
            return Err(AppError::Validation(
                "give exactly one of source_volume_id and source_snapshot_id".into(),
            ))
        }
    };
    let src = atlas_inventory::get_volume(&s.pool, &source_volume_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {source_volume_id}")))?;
    let tenant = atlas_inventory::volume_tenant(&s.pool, &source_volume_id).await?;
    require_tenant(
        s.config.auth_required,
        &actor,
        &tenant,
        format!("volume {source_volume_id}"),
    )?;
    if native_driver_for_volume(&s, &source_volume_id).await?.is_none() {
        return Err(AppError::Validation(
            "workspaces are only available for atlas-native volumes and filesystems".into(),
        ));
    }

    // Every refusal that needs no side effect comes before the snapshot and clone are made.
    preflight_create(&s, &actor, &tenant, src.size_bytes, max_bytes).await?;

    let id = format!("ws_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let (snapshot_id, owns_snapshot) = match existing_snapshot {
        Some(snap) => (snap.id, false),
        None => {
            let snap_body = serde_json::from_value(json!({ "name": format!("{id}-base") }))
                .map_err(|e| AppError::Internal(e.to_string()))?;
            let (_, Json(out)) = volumes::create_snapshot(
                State(s.clone()),
                Extension(actor.clone()),
                Path(source_volume_id.clone()),
                Json(snap_body),
            )
            .await?;
            let sid = out["snapshot_id"]
                .as_str()
                .ok_or_else(|| AppError::Internal(format!("snapshot returned no id: {out}")))?
                .to_string();
            (sid, true)
        }
    };
    let undo_snapshot = |s: AppState, sid: String| async move {
        if owns_snapshot {
            let _ = delete_base_snapshot(&s, &sid).await;
        }
    };

    let volume_id = match clone_base(&s, &actor, &snapshot_id, &format!("ws-{id}")).await {
        Ok(v) => v,
        Err(e) => {
            undo_snapshot(s.clone(), snapshot_id).await;
            return Err(e);
        }
    };
    let now = now_unix();
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(128).collect::<String>())
        .unwrap_or_else(|| id.clone());
    let ws = Workspace {
        id: id.clone(),
        tenant_id: tenant.clone(),
        owner: actor.id.clone(),
        name,
        source_volume_id,
        base_snapshot_id: snapshot_id.clone(),
        owns_base_snapshot: owns_snapshot,
        volume_id: volume_id.clone(),
        max_bytes,
        state: store::ACTIVE.into(),
        reset_count: 0,
        last_error: None,
        created_at_unix: now,
        expires_at_unix: now + ttl,
    };
    if let Err(e) = store::insert(&s.pool, &ws).await {
        let _ = delete_clone(&s, &volume_id).await;
        undo_snapshot(s.clone(), snapshot_id).await;
        return Err(e.into());
    }
    audit(
        &s,
        Some(&tenant),
        &actor,
        "workspace.create",
        "workspace",
        &id,
        Some(json!({ "source_volume_id": ws.source_volume_id, "volume_id": volume_id,
                     "base_snapshot_id": ws.base_snapshot_id, "ttl_secs": ttl, "max_bytes": max_bytes })),
    )
    .await;
    Ok((StatusCode::CREATED, Json(view(&s, &ws).await?)))
}

/// `GET /workspaces` (tenant-scoped for non-admins)
pub(crate) async fn list_workspaces(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    let rows = store::list(&s.pool, tenant_scope(s.config.auth_required, &actor)).await?;
    let mut out = Vec::with_capacity(rows.len());
    for w in &rows {
        out.push(view(&s, w).await?);
    }
    Ok(Json(json!(out)))
}

/// `GET /workspaces/{id}`
pub(crate) async fn get_workspace(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let w = load(&s, &actor, &id).await?;
    Ok(Json(view(&s, &w).await?))
}

/// `POST /workspaces/{id}/reset` — roll the workspace back to its base snapshot: the current clone
/// (and everything written to it) is deleted and a fresh one is cloned from the same snapshot. The
/// workspace keeps its id and expiry; its volume id changes.
pub(crate) async fn reset_workspace(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    require_role(s.config.auth_required, &actor, ROLE_OPERATOR)?;
    let w = load(&s, &actor, &id).await?;
    if now_unix() >= w.expires_at_unix {
        return Err(AppError::Conflict(format!("workspace {id} has expired")));
    }
    if !store::transition(&s.pool, &id, &[store::ACTIVE, store::RESET_FAILED], store::RESETTING).await? {
        return Err(AppError::Conflict(format!(
            "workspace {id} is {}; only an active workspace can be reset",
            store::get(&s.pool, &id).await?.map_or("gone".into(), |w| w.state)
        )));
    }
    if let Err(e) = delete_clone(&s, &w.volume_id).await {
        // The old clone is still there: put the workspace back as it was.
        let _ = store::transition(&s.pool, &id, &[store::RESETTING], &w.state).await;
        let _ = store::set_error(&s.pool, &id, &e.to_string()).await;
        return Err(e);
    }
    let name = format!("ws-{id}-r{}", w.reset_count + 1);
    match clone_base(&s, &actor, &w.base_snapshot_id, &name).await {
        Ok(volume_id) => {
            if !store::set_reset_done(&s.pool, &id, &volume_id).await? {
                // Torn down underneath us (expiry): do not leave the new clone behind.
                let _ = delete_clone(&s, &volume_id).await;
                return Err(AppError::Conflict(format!("workspace {id} was discarded during the reset")));
            }
            audit(
                &s,
                Some(&w.tenant_id),
                &actor,
                "workspace.reset",
                "workspace",
                &id,
                Some(json!({ "volume_id": volume_id, "base_snapshot_id": w.base_snapshot_id })),
            )
            .await;
            let w = store::get(&s.pool, &id).await?.ok_or_else(|| AppError::NotFound(format!("workspace {id}")))?;
            Ok(Json(view(&s, &w).await?))
        }
        Err(e) => {
            let _ = store::set_reset_failed(&s.pool, &id, &e.to_string()).await;
            Err(e)
        }
    }
}

/// Tear a workspace down: clone, then the snapshot Atlas took, then the row. The caller has already
/// moved it to `discarding`. Safe to run twice (the GC retries a half-done teardown).
async fn teardown(s: &AppState, w: &Workspace) -> AppResult<Option<&'static str>> {
    delete_clone(s, &w.volume_id).await?;
    let kept = if w.owns_base_snapshot {
        delete_base_snapshot(s, &w.base_snapshot_id).await?
    } else {
        None
    };
    store::delete(&s.pool, &w.id).await?;
    Ok(kept)
}

async fn discard(s: &AppState, actor: &Actor, w: &Workspace, action: &str) -> AppResult<Value> {
    match teardown(s, w).await {
        Ok(kept) => {
            audit(
                s,
                Some(&w.tenant_id),
                actor,
                action,
                "workspace",
                &w.id,
                Some(json!({ "volume_id": w.volume_id, "base_snapshot_retained": kept })),
            )
            .await;
            Ok(json!({ "workspace_id": w.id, "discarded": true, "base_snapshot_retained": kept }))
        }
        Err(e) => {
            let _ = store::set_error(&s.pool, &w.id, &e.to_string()).await;
            Err(e)
        }
    }
}

/// `DELETE /workspaces/{id}` — discard the workspace and everything written to it.
pub(crate) async fn discard_workspace(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    require_role(s.config.auth_required, &actor, ROLE_OPERATOR)?;
    let w = load(&s, &actor, &id).await?;
    if !store::transition(&s.pool, &id, &[store::ACTIVE, store::RESET_FAILED], store::DISCARDING).await?
        && w.state != store::DISCARDING
    {
        return Err(AppError::Conflict(format!(
            "workspace {id} is {}; try again when the reset has finished",
            w.state
        )));
    }
    Ok(Json(discard(&s, &actor, &w, "workspace.discard").await?))
}

/// Result of one GC pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub discarded: usize,
    pub failed: usize,
}

/// Discard every workspace past its expiry at `now_unix`, and finish any teardown left half-done.
/// Several replicas may run this at once: the state change is compare-and-set and every step is
/// idempotent.
pub async fn gc_once(s: &AppState, now_unix: i64) -> anyhow::Result<GcReport> {
    let actor = Actor {
        id: "system:workspace-gc".into(),
        role: "admin".into(),
        tenant_id: "global".into(),
        tools: Vec::new(),
        scope: None,
        workspace_bytes: None,
        jti: String::new(),
    };
    let mut report = GcReport::default();
    for w in store::due_for_gc(&s.pool, now_unix).await? {
        let claimed = store::transition(
            &s.pool,
            &w.id,
            &[store::ACTIVE, store::RESET_FAILED, store::RESETTING],
            store::DISCARDING,
        )
        .await?;
        if !claimed && w.state != store::DISCARDING {
            continue; // another replica (or a user) got there first
        }
        match discard(s, &actor, &w, "workspace.expire").await {
            Ok(_) => report.discarded += 1,
            Err(e) => {
                report.failed += 1;
                tracing::warn!(workspace = %w.id, error = %e, "workspace teardown failed; will retry");
            }
        }
    }
    Ok(report)
}

/// Background GC for expired workspaces (`ATLAS_WORKSPACE_GC_SECS`, default 60; 0 disables it).
pub fn spawn_gc(s: AppState) {
    let every = std::env::var("ATLAS_WORKSPACE_GC_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_GC_SECS);
    if every == 0 {
        return;
    }
    let every = every.max(5);
    tracing::info!(every_secs = every, "agent workspace GC enabled");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(every));
        loop {
            tick.tick().await;
            match gc_once(&s, now_unix()).await {
                Ok(r) if r.discarded + r.failed > 0 => {
                    tracing::info!(discarded = r.discarded, failed = r.failed, "workspace GC pass");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "workspace GC pass failed"),
            }
        }
    });
}
