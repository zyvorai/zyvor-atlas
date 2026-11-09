// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Guarded action tools for the MCP edge. Off unless `ATLAS_MCP_WRITE_ENABLE=1` (when off the tools
//! are not even listed). Every write is two-phase: a call without a `confirmation_token` changes
//! nothing and returns the plan plus a short-lived, single-use token bound to the caller, the tool
//! and the exact arguments; only a second call with that token executes. The token's signing key is
//! derived from `ATLAS_JWT_SECRET` and its single use is recorded in the database
//! (`mcp_confirm_used`), so a token survives a restart and is honoured by any replica sharing the
//! database, exactly once. On a 2026-07-28 client that can elicit, `mrtr.rs` carries the same token
//! as the MRTR `requestState` instead of making the agent echo it. Role and tenant checks are
//! the wrapped REST handler's own. Only low-blast-radius, reversible actions are exposed — failover,
//! delete, restore and disk operations stay human REST/CLI actions. The one exception is the agent
//! workspace tools (`workspaces.rs`), which reset or discard only a disposable clone the agent made
//! itself, never the volume it was copied from.

use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rmcp::{
    handler::server::wrapper::{Json as ToolJson, Parameters},
    model::ErrorData as McpError,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{actor_from, internal_error, rest, AtlasMcp};
use crate::auth::{require_role, require_tenant, Actor, ROLE_OPERATOR};
use crate::routes::observability::{self, SilenceQuery};
use crate::routes::volumes::{self, SnapshotBody};
use crate::state::AppState;

pub(super) const TOKEN_TTL_SECS: u64 = 300;
const CREATE_SNAPSHOT: &str = "create_snapshot";
const ACK_ALERT: &str = "ack_alert";
const SILENCE_ALERT: &str = "silence_alert";
/// Longest silence an agent can request. REST allows 30 days; a mistaken or manipulated agent should not
/// be able to mute an alert for that long, and a human can still use REST for a longer window.
const MAX_MCP_SILENCE_SECS: i64 = 24 * 3600;
const DEFAULT_MCP_SILENCE_SECS: i64 = 3600;

pub(super) fn write_enabled() -> bool {
    matches!(
        std::env::var("ATLAS_MCP_WRITE_ENABLE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "TRUE" | "yes")
    )
}

/// Signing key for confirmation tokens, derived from the gateway's JWT secret with a domain label so a
/// confirmation token can never verify as a login/API token (those are signed with the raw secret) and the
/// other way round. Every replica with the same `ATLAS_JWT_SECRET` derives the same key; rotating the
/// secret invalidates tokens still outstanding (at most `TOKEN_TTL_SECS` old).
fn key(secret: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"atlas.mcp.confirmation-token.v1\0");
    h.update(secret.as_bytes());
    h.finalize().into()
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    jti: String,
    sub: String,
    tool: String,
    params: String,
    exp: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn issue(secret: &str, actor: &str, tool: &str, params: &str, now: u64) -> String {
    let claims = Claims {
        jti: uuid::Uuid::new_v4().to_string(),
        sub: actor.into(),
        tool: tool.into(),
        params: params.into(),
        exp: now + TOKEN_TTL_SECS,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(&key(secret)),
    )
    .expect("HS256 encode of plain claims cannot fail")
}

/// Everything about a token except whether it was already used: signature, expiry, and that it was issued
/// to this caller for this tool and these exact arguments. Consumes nothing.
fn verify(
    secret: &str,
    token: &str,
    actor: &str,
    tool: &str,
    params: &str,
) -> Result<Claims, &'static str> {
    let mut v = Validation::new(Algorithm::HS256);
    v.leeway = 0;
    v.set_required_spec_claims(&["exp"]);
    let data = decode::<Claims>(token, &DecodingKey::from_secret(&key(secret)), &v)
        .map_err(|_| "confirmation token is invalid or expired")?;
    let c = data.claims;
    if c.sub != actor || c.tool != tool || c.params != params {
        return Err("confirmation token was issued for a different caller or arguments");
    }
    Ok(c)
}

/// Rows of `mcp_confirm_used` are kept this long past the token's own expiry, so a replica whose clock runs
/// behind cannot see a pruned row as a fresh token.
const USED_RETENTION_SECS: i64 = 900;

/// Last time (unix seconds) this process pruned `mcp_confirm_used`; pruning runs at most once a minute.
static LAST_PRUNE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Redeem a token: verify it, then record its `jti` in the database. The primary key makes the first
/// redemption win on every replica sharing the database, so a replay (also after a restart or on another
/// replica) is refused. A database error is not a reason to let the write through: it fails closed.
async fn redeem(
    s: &AppState,
    token: &str,
    actor: &str,
    tool: &str,
    params: &str,
) -> Result<(), &'static str> {
    let c = verify(&s.config.jwt_secret, token, actor, tool, params)?;
    match atlas_inventory::mcp_confirm::mark_used(&s.pool, &c.jti, c.exp as i64).await {
        Ok(true) => {}
        Ok(false) => return Err("confirmation token was already used"),
        Err(e) => {
            tracing::error!(error = %e, "could not record an MCP confirmation token as used; refusing the write");
            return Err("confirmation could not be recorded, nothing was executed; try again");
        }
    }
    let now = now_secs();
    let last = LAST_PRUNE.load(std::sync::atomic::Ordering::Relaxed);
    if now >= last + 60
        && LAST_PRUNE
            .compare_exchange(
                last,
                now,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    {
        // Best effort: a failed prune only leaves old rows for the next one.
        let _ =
            atlas_inventory::mcp_confirm::prune_expired(&s.pool, now as i64 - USED_RETENTION_SECS)
                .await;
    }
    Ok(())
}

/// Outcome of the shared two-phase gate.
pub(super) enum Gate {
    /// No token was given: change nothing, hand back this token with the plan.
    Propose(String),
    /// A valid, unused token bound to this caller, tool and exact arguments: go ahead.
    Execute,
}

/// The two-phase confirm shared by every write tool. `params` must be the canonical JSON of the
/// arguments (everything except the token) so a token cannot be replayed with different arguments.
pub(super) async fn gate(
    s: &AppState,
    actor_id: &str,
    tool: &str,
    params: &str,
    token: Option<&str>,
) -> Result<Gate, &'static str> {
    match token {
        None => Ok(Gate::Propose(issue(
            &s.config.jwt_secret,
            actor_id,
            tool,
            params,
            now_secs(),
        ))),
        Some(t) => redeem(s, t, actor_id, tool, params)
            .await
            .map(|()| Gate::Execute),
    }
}

/// Audit record `mcp.<tool>` (proposed | rejected | success | failure). Failures to audit never block.
pub(super) async fn audit(
    s: &AppState,
    actor: &Actor,
    tool: &str,
    resource_type: &str,
    resource_id: &str,
    status: &'static str,
    detail: Value,
) {
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&actor.tenant_id),
        &actor.id,
        &format!("mcp.{tool}"),
        resource_type,
        resource_id,
        status,
        None,
        Some(detail),
    )
    .await;
}

pub(super) fn write_guard(s: &AppState, actor: &Actor) -> Result<(), McpError> {
    if !write_enabled() {
        return Err(McpError::invalid_request(
            "write tools are disabled on this server",
            None,
        ));
    }
    require_role(s.config.auth_required, actor, ROLE_OPERATOR)
        .map_err(|e| McpError::invalid_request(e.to_string(), None))
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSnapshotArgs {
    /// Volume id, as returned by `list_volumes`.
    volume_id: String,
    /// Optional snapshot name (the REST default applies when omitted).
    #[serde(default)]
    name: Option<String>,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AckAlertArgs {
    /// Alert id, as returned by `list_alerts`.
    alert_id: String,
    /// Omit to get the alert and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SilenceAlertArgs {
    /// Alert id, as returned by `list_alerts`.
    alert_id: String,
    /// Silence window in seconds, 60 to 86400 (default 3600).
    #[serde(default)]
    secs: Option<i64>,
    /// Omit to get the alert and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[tool_router(router = write_router, vis = "pub(super)")]
impl AtlasMcp {
    #[tool(
        description = "Create a snapshot of one volume (reversible). Two-phase: call without \
                        confirmation_token to receive the plan and a 5-minute single-use token, then \
                        call again with the same arguments plus that token to execute. Requires an \
                        operator-or-higher token and the caller's tenant must own the volume."
    )]
    async fn create_snapshot(
        &self,
        Parameters(args): Parameters<CreateSnapshotArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let s = &self.state;
        write_guard(s, &actor)?;
        let vol = atlas_inventory::get_volume(&s.pool, &args.volume_id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| {
                McpError::invalid_params(format!("unknown volume {}", args.volume_id), None)
            })?;
        let tenant = atlas_inventory::volume_tenant(&s.pool, &args.volume_id)
            .await
            .map_err(internal_error)?;
        require_tenant(
            s.config.auth_required,
            &actor,
            &tenant,
            format!("volume {}", args.volume_id),
        )
        .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let name = args.name.clone().filter(|n| !n.trim().is_empty());
        let params = json!({ "volume_id": args.volume_id, "name": name }).to_string();
        let rid = args.volume_id.as_str();

        match gate(
            s,
            &actor.id,
            CREATE_SNAPSHOT,
            &params,
            args.confirmation_token.as_deref(),
        )
        .await
        {
            Ok(Gate::Propose(token)) => {
                audit(
                    s,
                    &actor,
                    CREATE_SNAPSHOT,
                    "volume",
                    rid,
                    "proposed",
                    json!({ "via": "mcp", "confirmed": false }),
                )
                .await;
                return Ok(ToolJson(json!({
                    "executed": false,
                    "plan": {
                        "action": "create_snapshot",
                        "volume_id": vol.id,
                        "volume_name": vol.name,
                        "size_bytes": vol.size_bytes,
                        "snapshot_name": name,
                    },
                    "confirmation_token": token,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": "call create_snapshot again with the same arguments plus confirmation_token",
                })));
            }
            Ok(Gate::Execute) => {}
            Err(why) => {
                audit(
                    s,
                    &actor,
                    CREATE_SNAPSHOT,
                    "volume",
                    rid,
                    "rejected",
                    json!({ "via": "mcp", "reason": why }),
                )
                .await;
                return Err(McpError::invalid_params(why, None));
            }
        }

        let body: SnapshotBody =
            serde_json::from_value(json!({ "name": name })).map_err(internal_error)?;
        let result = volumes::create_snapshot(
            State(s.clone()),
            Extension(actor.clone()),
            Path(args.volume_id.clone()),
            Json(body),
        )
        .await
        .map(|(_, j)| j);
        match &result {
            Ok(Json(v)) => {
                audit(
                    s,
                    &actor,
                    CREATE_SNAPSHOT,
                    "volume",
                    rid,
                    "success",
                    json!({ "via": "mcp", "confirmed": true, "result": v }),
                )
                .await
            }
            Err(e) => {
                audit(
                    s,
                    &actor,
                    CREATE_SNAPSHOT,
                    "volume",
                    rid,
                    "failure",
                    json!({ "via": "mcp", "confirmed": true, "error": e.to_string() }),
                )
                .await
            }
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }

    #[tool(
        description = "Acknowledge one alert (records who saw it; does not resolve it or silence \
                        notifications). Two-phase: call without confirmation_token to receive the \
                        alert and a 5-minute single-use token, then call again with the same \
                        arguments plus that token to execute. Requires an operator-or-higher token."
    )]
    async fn ack_alert(
        &self,
        Parameters(args): Parameters<AckAlertArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let s = &self.state;
        write_guard(s, &actor)?;
        let alert = atlas_inventory::alerts::list(&s.pool, None)
            .await
            .map_err(internal_error)?
            .into_iter()
            .find(|a| a.id == args.alert_id)
            .ok_or_else(|| {
                McpError::invalid_params(format!("unknown alert {}", args.alert_id), None)
            })?;
        let params = json!({ "alert_id": args.alert_id }).to_string();
        let rid = args.alert_id.as_str();

        match gate(
            s,
            &actor.id,
            ACK_ALERT,
            &params,
            args.confirmation_token.as_deref(),
        )
        .await
        {
            Ok(Gate::Propose(token)) => {
                audit(
                    s,
                    &actor,
                    ACK_ALERT,
                    "alert",
                    rid,
                    "proposed",
                    json!({ "via": "mcp", "confirmed": false }),
                )
                .await;
                return Ok(ToolJson(json!({
                    "executed": false,
                    "plan": {
                        "action": "ack_alert",
                        "alert_id": alert.id,
                        "severity": alert.severity,
                        "title": alert.title,
                        "state": alert.state,
                        "already_acknowledged_by": alert.acknowledged_by,
                    },
                    "confirmation_token": token,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": "call ack_alert again with the same arguments plus confirmation_token",
                })));
            }
            Ok(Gate::Execute) => {}
            Err(why) => {
                audit(
                    s,
                    &actor,
                    ACK_ALERT,
                    "alert",
                    rid,
                    "rejected",
                    json!({ "via": "mcp", "reason": why }),
                )
                .await;
                return Err(McpError::invalid_params(why, None));
            }
        }

        let result = observability::ack_alert(
            State(s.clone()),
            Extension(actor.clone()),
            Path(args.alert_id.clone()),
        )
        .await;
        match &result {
            Ok(Json(v)) => {
                audit(
                    s,
                    &actor,
                    ACK_ALERT,
                    "alert",
                    rid,
                    "success",
                    json!({ "via": "mcp", "confirmed": true, "result": v }),
                )
                .await
            }
            Err(e) => {
                audit(
                    s,
                    &actor,
                    ACK_ALERT,
                    "alert",
                    rid,
                    "failure",
                    json!({ "via": "mcp", "confirmed": true, "error": e.to_string() }),
                )
                .await
            }
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }

    #[tool(
        description = "Silence webhook notifications for one alert for a bounded window (60 s to 24 h, \
                        default 1 h). The alert keeps being tracked and still shows in list_alerts; it is \
                        not resolved or acknowledged. Two-phase: call without confirmation_token to \
                        receive the alert and a 5-minute single-use token, show it to a human, then call \
                        again with the same arguments plus that token to execute. Requires an \
                        operator-or-higher token."
    )]
    async fn silence_alert(
        &self,
        Parameters(args): Parameters<SilenceAlertArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let s = &self.state;
        write_guard(s, &actor)?;
        let secs = args.secs.unwrap_or(DEFAULT_MCP_SILENCE_SECS);
        if !(60..=MAX_MCP_SILENCE_SECS).contains(&secs) {
            return Err(McpError::invalid_params(
                format!("secs must be between 60 and {MAX_MCP_SILENCE_SECS}"),
                None,
            ));
        }
        let alert = atlas_inventory::alerts::list(&s.pool, None)
            .await
            .map_err(internal_error)?
            .into_iter()
            .find(|a| a.id == args.alert_id)
            .ok_or_else(|| {
                McpError::invalid_params(format!("unknown alert {}", args.alert_id), None)
            })?;
        let params = json!({ "alert_id": args.alert_id, "secs": secs }).to_string();
        let rid = args.alert_id.as_str();

        match gate(
            s,
            &actor.id,
            SILENCE_ALERT,
            &params,
            args.confirmation_token.as_deref(),
        )
        .await
        {
            Ok(Gate::Propose(token)) => {
                audit(
                    s,
                    &actor,
                    SILENCE_ALERT,
                    "alert",
                    rid,
                    "proposed",
                    json!({ "via": "mcp", "confirmed": false, "secs": secs }),
                )
                .await;
                return Ok(ToolJson(json!({
                    "executed": false,
                    "plan": {
                        "action": "silence_alert",
                        "alert_id": alert.id,
                        "severity": alert.severity,
                        "title": alert.title,
                        "state": alert.state,
                        "silence_secs": secs,
                        "already_silenced_until": alert.silenced_until,
                    },
                    "confirmation_token": token,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": "call silence_alert again with the same arguments plus confirmation_token",
                })));
            }
            Ok(Gate::Execute) => {}
            Err(why) => {
                audit(
                    s,
                    &actor,
                    SILENCE_ALERT,
                    "alert",
                    rid,
                    "rejected",
                    json!({ "via": "mcp", "reason": why }),
                )
                .await;
                return Err(McpError::invalid_params(why, None));
            }
        }

        let result = observability::silence_alert(
            State(s.clone()),
            Extension(actor.clone()),
            Path(args.alert_id.clone()),
            Query(SilenceQuery { secs: Some(secs) }),
        )
        .await;
        match &result {
            Ok(Json(v)) => {
                audit(
                    s,
                    &actor,
                    SILENCE_ALERT,
                    "alert",
                    rid,
                    "success",
                    json!({ "via": "mcp", "confirmed": true, "result": v }),
                )
                .await
            }
            Err(e) => {
                audit(
                    s,
                    &actor,
                    SILENCE_ALERT,
                    "alert",
                    rid,
                    "failure",
                    json!({ "via": "mcp", "confirmed": true, "error": e.to_string() }),
                )
                .await
            }
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "unit-test-secret-at-least-32-bytes!!";

    #[test]
    fn token_is_bound_to_caller_tool_and_arguments() {
        let params = r#"{"volume_id":"vol_a","name":null}"#;
        let t = issue(SECRET, "alice", CREATE_SNAPSHOT, params, now_secs());
        // Wrong caller / tool / arguments are rejected.
        assert!(verify(SECRET, &t, "bob", CREATE_SNAPSHOT, params).is_err());
        assert!(verify(SECRET, &t, "alice", "delete_volume", params).is_err());
        assert!(verify(
            SECRET,
            &t,
            "alice",
            CREATE_SNAPSHOT,
            r#"{"volume_id":"vol_b","name":null}"#
        )
        .is_err());
        // Verifying consumes nothing: the right caller still verifies, as often as asked.
        assert!(verify(SECRET, &t, "alice", CREATE_SNAPSHOT, params).is_ok());
        assert!(verify(SECRET, &t, "alice", CREATE_SNAPSHOT, params).is_ok());
    }

    #[test]
    fn tampered_expired_and_foreign_tokens_fail() {
        let params = r#"{"alert_id":"alert_a"}"#;
        let now = now_secs();
        let t = issue(SECRET, "alice", ACK_ALERT, params, now);
        let mut bad = t.clone();
        bad.push('x');
        assert_eq!(
            verify(SECRET, &bad, "alice", ACK_ALERT, params).unwrap_err(),
            "confirmation token is invalid or expired"
        );
        let old = issue(
            SECRET,
            "alice",
            ACK_ALERT,
            params,
            now.saturating_sub(TOKEN_TTL_SECS + 120),
        );
        assert_eq!(
            verify(SECRET, &old, "alice", ACK_ALERT, params).unwrap_err(),
            "confirmation token is invalid or expired"
        );
        // Another secret does not verify it (a replica with a different ATLAS_JWT_SECRET)...
        assert!(verify(
            "another-secret-at-least-32-bytes!!!!",
            &t,
            "alice",
            ACK_ALERT,
            params
        )
        .is_err());
        // ...and the same secret does, which is what makes the token portable across restarts and replicas.
        assert!(verify(SECRET, &t, "alice", ACK_ALERT, params).is_ok());
    }

    #[test]
    fn the_signing_key_is_not_the_login_secret() {
        // A confirmation token is not a login token and the other way round: the keys differ.
        assert_ne!(key(SECRET).as_slice(), SECRET.as_bytes());
        let t = issue(SECRET, "alice", ACK_ALERT, "{}", now_secs());
        assert!(
            crate::auth::decode_token(&t, SECRET, None).is_err(),
            "a confirmation token must not pass as an API token"
        );
    }
}
