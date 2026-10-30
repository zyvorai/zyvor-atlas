// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};

use super::object_store::ListBackupsQuery;
use crate::auth::Actor;
use crate::state::AppState;

// ---- policies ----

pub(crate) async fn list_policies() -> Json<Value> {
    let items: Vec<Value> = atlas_policy::POLICIES
        .iter()
        .map(|p| {
            json!({
                "intent": p.intent, "storage_class": p.storage_class,
                "access_mode": p.access_mode, "volume_mode": p.volume_mode,
                "description": p.description
            })
        })
        .collect();
    Json(json!(items))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScheduleBody {
    /// Cadence in seconds.
    interval_secs: i64,
    /// Retain the newest N (0 = keep all).
    #[serde(default)]
    keep: i64,
    /// "snapshot" (default) or "backup".
    #[serde(default)]
    kind: Option<String>,
    /// Target bucket id (required for backup schedules).
    #[serde(default)]
    bucket_id: Option<String>,
    /// Backup mode ("manifest" default, or "data"); ignored for snapshot schedules.
    #[serde(default)]
    mode: Option<String>,
}

/// `POST /volumes/{id}/schedule` — create a protection schedule for a volume (operator).
pub(crate) async fn create_schedule(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<ScheduleBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.interval_secs <= 0 {
        return Err(AppError::Validation("interval_secs must be > 0".into()));
    }
    if body.keep < 0 {
        return Err(AppError::Validation("keep must be >= 0".into()));
    }
    let kind = body.kind.as_deref().unwrap_or("snapshot");
    if kind != "snapshot" && kind != "backup" {
        return Err(AppError::Validation(
            "kind must be snapshot or backup".into(),
        ));
    }
    let vol = atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    // Both snapshot and backup schedules dispatch a CSI VolumeSnapshot job under the hood, which
    // needs a PVC-backed (k8s) volume. Without this check a schedule against a raw NFS/ZFS volume
    // creates successfully and its "next run" timer keeps advancing forever, but no job is ever
    // enqueued for it — a silent, permanent no-op that looks like a healthy active schedule.
    if vol.pvc_name.is_none() {
        return Err(AppError::Validation(format!(
            "volume {id} has no PVC — schedules require a PVC-backed (CSI) volume"
        )));
    }
    let mode = body.mode.as_deref().unwrap_or("manifest");
    if kind == "backup" {
        let bucket_id = body
            .bucket_id
            .as_deref()
            .ok_or_else(|| AppError::Validation("backup schedule requires bucket_id".into()))?;
        let bucket = atlas_inventory::buckets::get_bucket(&s.pool, bucket_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("bucket {bucket_id}")))?;
        if bucket.state != "bound" {
            return Err(AppError::Validation(format!(
                "bucket {bucket_id} is not bound yet"
            )));
        }
    }
    let tenant_id = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    let sched = atlas_inventory::schedules::insert(
        &s.pool,
        &ids::schedule_id(),
        &tenant_id,
        &id,
        kind,
        body.bucket_id.as_deref(),
        mode,
        body.interval_secs,
        body.keep,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(sched))))
}

/// `GET /schedules` (optionally `?volume_id=`).
pub(crate) async fn list_schedules(
    State(s): State<AppState>,
    Query(q): Query<ListBackupsQuery>,
) -> AppResult<Json<Value>> {
    let items = atlas_inventory::schedules::list(&s.pool, q.volume_id.as_deref()).await?;
    Ok(Json(json!(items)))
}

/// `DELETE /schedules/{id}` (operator).
pub(crate) async fn delete_schedule(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let removed = atlas_inventory::schedules::delete(&s.pool, &id).await?;
    if !removed {
        return Err(AppError::NotFound(format!("schedule {id}")));
    }
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueTokenBody {
    /// The token subject — typically the product/service-account name (goes into audit logs).
    subject: String,
    /// Role: `viewer`, `operator`, `admin`, or a `product.service.<name>` (→ operator). Default viewer.
    #[serde(default)]
    role: Option<String>,
    /// Tenant this token is scoped to on read endpoints (default `"global"`).
    #[serde(default)]
    tenant_id: Option<String>,
    /// MCP tool allowlist for an agent token. Omitted or empty mints an ordinary token. A
    /// non-empty list makes the token MCP-only: it can call just these tools (the role still
    /// applies on top) and is refused on REST, gRPC and `/metrics`.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Lifetime in seconds (default 3600, capped at 90 days).
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginBody {
    username: String,
    password: String,
    /// Session lifetime in seconds (default 24h, capped at 90 days).
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// `POST /auth/login` — exchange console username/password for a JWT.
/// Checks admin-created `console_users` first, then the bootstrap env admin.
pub(crate) async fn login(
    State(s): State<AppState>,
    Json(body): Json<LoginBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    if body.username.trim().is_empty() || body.password.is_empty() {
        return Err(AppError::Validation(
            "username and password are required".into(),
        ));
    }
    const MAX_TTL: u64 = 90 * 24 * 3600;
    let ttl_secs = body.ttl_secs.unwrap_or(86_400).clamp(60, MAX_TTL);
    let user = body.username.trim();

    // 1) DB console users (admin-created, with privilege level).
    if let Some((hash, role, tenant_id)) =
        atlas_inventory::users::credentials_for_login(&s.pool, user)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?
    {
        if !crate::auth::verify_password_hash(&body.password, &hash) {
            return Err(AppError::Auth("invalid username or password".into()));
        }
        // Transparently upgrade legacy `sha256$...` hashes to Argon2 on successful login — no
        // forced password reset, but the weak legacy format is retired the next time each user
        // authenticates rather than persisting indefinitely.
        if !hash.starts_with('$') {
            let upgraded = crate::auth::hash_password(&body.password);
            if let Err(e) =
                atlas_inventory::users::update(&s.pool, user, None, Some(&upgraded), None).await
            {
                tracing::warn!("failed to upgrade legacy password hash for '{user}': {e}");
            }
        }
        return mint_login_response(&s, user, &role, &tenant_id, ttl_secs).await;
    }

    // 2) Bootstrap env admin (default admin / Admin@321).
    let ok = crate::auth::verify_console_credentials(
        user,
        &body.password,
        &s.config.admin_username,
        &s.config.admin_password,
    );
    if !ok {
        return Err(AppError::Auth("invalid username or password".into()));
    }
    // Bootstrap admin is platform-wide (admin role bypasses tenant scoping regardless).
    mint_login_response(&s, &s.config.admin_username, "admin", "global", ttl_secs).await
}

pub(crate) async fn mint_login_response(
    s: &AppState,
    subject: &str,
    role: &str,
    tenant_id: &str,
    ttl_secs: u64,
) -> AppResult<(StatusCode, Json<Value>)> {
    let (token, exp, jti) =
        crate::auth::mint_token(&s.config.jwt_secret, subject, role, tenant_id, ttl_secs)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        subject,
        "auth.login",
        "console",
        subject,
        "ok",
        Some(json!({ "role": role, "tenant_id": tenant_id, "ttl_secs": ttl_secs, "jti": jti })),
        None,
    )
    .await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "token": token,
            "jti": jti,
            "subject": subject,
            "role": role,
            "tenant_id": tenant_id,
            "level": crate::auth::role_level(role),
            "expires_at": exp,
            "ttl_secs": ttl_secs,
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateUserBody {
    username: String,
    password: String,
    /// Privilege level: `viewer`, `operator`, or `admin` (default viewer).
    #[serde(default)]
    role: Option<String>,
    /// Tenant this user is scoped to on read endpoints (default `"global"`); ignored in practice
    /// for `role: "admin"`, which stays cross-tenant regardless of this value.
    #[serde(default)]
    tenant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateUserBody {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// `GET /auth/users` — list console users (admin).
pub(crate) async fn list_users(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let users = atlas_inventory::users::list(&s.pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    // Surface the bootstrap env admin as a virtual entry so operators see it in the UI.
    let mut items: Vec<Value> = users
        .into_iter()
        .map(|u| {
            json!({
                "username": u.username,
                "role": u.role,
                "tenant_id": u.tenant_id,
                "level": crate::auth::role_level(&u.role),
                "disabled": u.disabled,
                "created_by": u.created_by,
                "created_at": u.created_at,
                "updated_at": u.updated_at,
                "kind": "user",
            })
        })
        .collect();
    items.insert(
        0,
        json!({
            "username": s.config.admin_username,
            "role": "admin",
            "tenant_id": "global",
            "level": crate::auth::ROLE_ADMIN,
            "disabled": false,
            "created_by": null,
            "created_at": null,
            "updated_at": null,
            "kind": "bootstrap",
        }),
    );
    Ok(Json(json!(items)))
}

/// `POST /auth/users` — create a console user with a privilege level (admin).
pub(crate) async fn create_user(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateUserBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let username = body.username.trim();
    if username.is_empty() {
        return Err(AppError::Validation("username is required".into()));
    }
    if username.len() > 64
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(AppError::Validation(
            "username must be 1–64 chars of [A-Za-z0-9._-]".into(),
        ));
    }
    if username.eq_ignore_ascii_case(&s.config.admin_username) {
        return Err(AppError::Conflict(format!(
            "username '{}' is reserved for the bootstrap admin",
            s.config.admin_username
        )));
    }
    if body.password.len() < 8 {
        return Err(AppError::Validation(
            "password must be at least 8 characters".into(),
        ));
    }
    let role = crate::auth::normalize_console_role(body.role.as_deref().unwrap_or("viewer"))
        .map_err(AppError::Validation)?;
    let tenant_id = body
        .tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("global");
    let hash = crate::auth::hash_password(&body.password);
    let user = atlas_inventory::users::create(&s.pool, username, &hash, role, tenant_id, &actor.id)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("already exists") {
                AppError::Conflict(msg)
            } else {
                AppError::Database(msg)
            }
        })?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "auth.user.created",
        "console_user",
        &user.username,
        "ok",
        Some(json!({ "role": user.role, "tenant_id": user.tenant_id })),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "username": user.username,
            "role": user.role,
            "tenant_id": user.tenant_id,
            "level": crate::auth::role_level(&user.role),
            "disabled": user.disabled,
            "created_by": user.created_by,
            "created_at": user.created_at,
            "kind": "user",
        })),
    ))
}

/// `PUT /auth/users/{username}` — update role, password, or disabled flag (admin).
pub(crate) async fn update_user(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(username): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if username.eq_ignore_ascii_case(&s.config.admin_username) {
        return Err(AppError::Validation(
            "bootstrap admin is managed via ATLAS_ADMIN_* env, not this API".into(),
        ));
    }
    let role = match body.role.as_deref() {
        Some(r) => Some(crate::auth::normalize_console_role(r).map_err(AppError::Validation)?),
        None => None,
    };
    if let Some(pw) = body.password.as_deref() {
        if pw.len() < 8 {
            return Err(AppError::Validation(
                "password must be at least 8 characters".into(),
            ));
        }
    }
    // Refuse demoting/disabling the last enabled admin in the DB (bootstrap still works, but
    // keep at least one named admin when present).
    if body.disabled == Some(true) || role == Some("viewer") || role == Some("operator") {
        if let Some(existing) = atlas_inventory::users::get(&s.pool, &username)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?
        {
            if existing.role == "admin" && !existing.disabled {
                let admins = atlas_inventory::users::count_admins(&s.pool)
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                if admins <= 1
                    && (body.disabled == Some(true) || role.is_some_and(|r| r != "admin"))
                {
                    return Err(AppError::Validation(
                        "cannot disable or demote the last admin user".into(),
                    ));
                }
            }
        }
    }
    let hash = body.password.as_deref().map(crate::auth::hash_password);
    let user =
        atlas_inventory::users::update(&s.pool, &username, role, hash.as_deref(), body.disabled)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?
            .ok_or_else(|| AppError::NotFound(format!("user {username}")))?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "auth.user.updated",
        "console_user",
        &user.username,
        "ok",
        Some(json!({ "role": user.role, "disabled": user.disabled })),
        None,
    )
    .await;
    Ok(Json(json!({
        "username": user.username,
        "role": user.role,
        "level": crate::auth::role_level(&user.role),
        "disabled": user.disabled,
        "created_by": user.created_by,
        "created_at": user.created_at,
        "updated_at": user.updated_at,
        "kind": "user",
    })))
}

/// `DELETE /auth/users/{username}` — remove a console user (admin).
pub(crate) async fn delete_user(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(username): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if username.eq_ignore_ascii_case(&s.config.admin_username) {
        return Err(AppError::Validation(
            "cannot delete the bootstrap admin".into(),
        ));
    }
    if let Some(existing) = atlas_inventory::users::get(&s.pool, &username)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?
    {
        if existing.role == "admin" && !existing.disabled {
            let admins = atlas_inventory::users::count_admins(&s.pool)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            if admins <= 1 {
                return Err(AppError::Validation(
                    "cannot delete the last admin user".into(),
                ));
            }
        }
    }
    let removed = atlas_inventory::users::delete(&s.pool, &username)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    if !removed {
        return Err(AppError::NotFound(format!("user {username}")));
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "auth.user.deleted",
        "console_user",
        &username,
        "ok",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "deleted": username })))
}

/// At most this many tools in one scope, each a plausible MCP tool name.
const MAX_SCOPE_TOOLS: usize = 64;

/// Checks a requested tool scope: bounded, `[a-z0-9_]` names only, duplicates dropped. It does not
/// check the names against the live tool list: an unknown name simply grants nothing.
fn validate_tool_scope(tools: &[String]) -> AppResult<Vec<String>> {
    if tools.len() > MAX_SCOPE_TOOLS {
        return Err(AppError::Validation(format!(
            "at most {MAX_SCOPE_TOOLS} tools per scoped token"
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for t in tools {
        let ok = !t.is_empty()
            && t.len() <= 64
            && t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err(AppError::Validation(format!("invalid tool name '{t}'")));
        }
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    Ok(out)
}

/// `POST /auth/tokens` — mint a scoped service-account JWT for a product (admin). The shared secret
/// never leaves Atlas; the caller receives only the signed token + its claims.
pub(crate) async fn issue_token(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<IssueTokenBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.subject.trim().is_empty() {
        return Err(AppError::Validation("subject is required".into()));
    }
    let role = body.role.unwrap_or_else(|| "viewer".into());
    let tenant_id = body
        .tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("global");
    // Cap the lifetime so a leaked token has a bounded blast radius.
    const MAX_TTL: u64 = 90 * 24 * 3600;
    let ttl_secs = body.ttl_secs.unwrap_or(3600).clamp(60, MAX_TTL);
    let tools = validate_tool_scope(body.tools.as_deref().unwrap_or(&[]))?;
    let (token, exp, jti) = crate::auth::mint_scoped_token(
        &s.config.jwt_secret,
        &body.subject,
        &role,
        tenant_id,
        ttl_secs,
        &tools,
    )?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "auth.token.issued",
        "service_account",
        &body.subject,
        "ok",
        Some(json!({ "role": role, "tenant_id": tenant_id, "ttl_secs": ttl_secs, "jti": jti,
                     "tools": tools })),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "token": token, "jti": jti, "subject": body.subject, "role": role, "tenant_id": tenant_id,
            "level": crate::auth::role_level(&role), "expires_at": exp, "ttl_secs": ttl_secs,
            "tools": tools
        })),
    ))
}

/// `GET /auth/tokens/revoked` — the current token deny-list (admin).
pub(crate) async fn list_revoked_tokens(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    Ok(Json(json!(atlas_inventory::tokens::list(&s.pool).await?)))
}

/// `POST /auth/tokens/{jti}/revoke` — kill a minted token by its id before it expires (admin).
pub(crate) async fn revoke_token(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(jti): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    atlas_inventory::tokens::revoke(&s.pool, &jti, &actor.id).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "auth.token.revoked",
        "service_account",
        &jti,
        "ok",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "jti": jti, "revoked": true })))
}

pub(crate) async fn list_volume_bindings(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let rows = atlas_inventory::list_bindings_for(&s.pool, "volume", &id).await?;
    Ok(Json(json!(rows)))
}

/// `GET /volumes/{id}/labels` — the volume's user labels.
pub(crate) async fn get_volume_labels(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        atlas_inventory::get_volume_labels(&s.pool, &id).await?,
    ))
}

/// `PUT /volumes/{id}/labels` — merge labels into the volume (operator). Body is a JSON object.
pub(crate) async fn put_volume_labels(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let labels = body
        .as_object()
        .ok_or_else(|| AppError::Validation("body must be a JSON object of labels".into()))?;
    atlas_inventory::get_volume(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    let merged = atlas_inventory::set_volume_labels(&s.pool, &id, labels).await?;
    Ok(Json(merged))
}

/// `GET /tenants` — overview of every tenant with volumes or a quota (usage + limits).
pub(crate) async fn list_tenants(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::tenants::list_overview(&s.pool).await?
    )))
}
pub(crate) async fn list_tenant_policies(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let items = atlas_inventory::tenants::list_policies(&s.pool, &id).await?;
    Ok(Json(json!(items)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TenantPolicyBody {
    storage_class: String,
    #[serde(default)]
    access_mode: Option<String>,
    #[serde(default)]
    volume_mode: Option<String>,
}

/// `PUT /tenants/{id}/policies/{intent}` — override an intent's placement for a tenant (admin).
pub(crate) async fn put_tenant_policy(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, intent)): Path<(String, String)>,
    Json(body): Json<TenantPolicyBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.storage_class.trim().is_empty() {
        return Err(AppError::Validation("storage_class is required".into()));
    }
    let access_mode = body.access_mode.unwrap_or_else(|| "ReadWriteOnce".into());
    let volume_mode = body.volume_mode.unwrap_or_else(|| "Filesystem".into());
    atlas_inventory::tenants::set_policy(
        &s.pool,
        &id,
        &intent,
        &body.storage_class,
        &access_mode,
        &volume_mode,
    )
    .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "tenant.policy.set",
        "tenant",
        &id,
        "ok",
        Some(json!({ "intent": intent, "storage_class": body.storage_class })),
        None,
    )
    .await;
    let p = atlas_inventory::tenants::get_policy(&s.pool, &id, &intent).await?;
    Ok(Json(json!(p)))
}

/// `DELETE /tenants/{id}/policies/{intent}` (admin).
pub(crate) async fn delete_tenant_policy(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((id, intent)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let removed = atlas_inventory::tenants::delete_policy(&s.pool, &id, &intent).await?;
    if !removed {
        return Err(AppError::NotFound(format!(
            "tenant {id} has no override for intent {intent}"
        )));
    }
    Ok(Json(
        json!({ "deleted": { "tenant_id": id, "intent": intent } }),
    ))
}

/// `GET /tenants/{id}/quota` — the tenant's quota + current usage (unlimited 0/0 if unset).
pub(crate) async fn get_tenant_quota(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let q = atlas_inventory::tenants::get_quota(&s.pool, &id).await?;
    Ok(Json(json!(q)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TenantQuotaBody {
    /// Max total provisioned volume bytes (0 = unlimited).
    #[serde(default)]
    max_bytes: i64,
    /// Max number of volumes (0 = unlimited).
    #[serde(default)]
    max_volumes: i64,
}

/// `PUT /tenants/{id}/quota` — set the tenant's quota (admin).
pub(crate) async fn put_tenant_quota(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<TenantQuotaBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    if body.max_bytes < 0 || body.max_volumes < 0 {
        return Err(AppError::Validation("quota limits must be >= 0".into()));
    }
    atlas_inventory::tenants::set_quota(&s.pool, &id, body.max_bytes, body.max_volumes).await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "tenant.quota.set",
        "tenant",
        &id,
        "ok",
        Some(json!({ "max_bytes": body.max_bytes, "max_volumes": body.max_volumes })),
        None,
    )
    .await;
    let q = atlas_inventory::tenants::get_quota(&s.pool, &id).await?;
    Ok(Json(json!(q)))
}
