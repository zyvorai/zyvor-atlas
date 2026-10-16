// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Guarded action tools for the MCP edge. Off unless `ATLAS_MCP_WRITE_ENABLE=1` (when off the tools
//! are not even listed). Every write is two-phase: a call without a `confirmation_token` changes
//! nothing and returns the plan plus a short-lived, single-use token bound to the caller, the tool
//! and the exact arguments; only a second call with that token executes. Role and tenant checks are
//! the wrapped REST handler's own. Only low-blast-radius, reversible actions are exposed — failover,
//! delete, restore and disk operations stay human REST/CLI actions.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Path, State};
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
use crate::routes::observability;
use crate::routes::volumes::{self, SnapshotBody};
use crate::state::AppState;

const TOKEN_TTL_SECS: u64 = 300;
const CREATE_SNAPSHOT: &str = "create_snapshot";
const ACK_ALERT: &str = "ack_alert";

pub(super) fn write_enabled() -> bool {
    matches!(
        std::env::var("ATLAS_MCP_WRITE_ENABLE").ok().as_deref().map(str::trim),
        Some("1" | "true" | "TRUE" | "yes")
    )
}

/// Per-process random signing key: a restart invalidates every outstanding token.
fn key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    })
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

fn issue(actor: &str, tool: &str, params: &str, now: u64) -> String {
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
        &EncodingKey::from_secret(key().as_bytes()),
    )
    .expect("HS256 encode of plain claims cannot fail")
}

/// Tokens already redeemed (jti → exp), pruned lazily, so a token cannot be replayed in its TTL.
fn used() -> &'static Mutex<HashMap<String, u64>> {
    static USED: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    USED.get_or_init(Default::default)
}

fn redeem(token: &str, actor: &str, tool: &str, params: &str) -> Result<(), &'static str> {
    let mut v = Validation::new(Algorithm::HS256);
    v.leeway = 0;
    v.set_required_spec_claims(&["exp"]);
    let data = decode::<Claims>(token, &DecodingKey::from_secret(key().as_bytes()), &v)
        .map_err(|_| "confirmation token is invalid or expired")?;
    let c = data.claims;
    if c.sub != actor || c.tool != tool || c.params != params {
        return Err("confirmation token was issued for a different caller or arguments");
    }
    let now = now_secs();
    let mut map = used().lock().expect("used-token map");
    map.retain(|_, exp| *exp > now);
    if map.insert(c.jti, c.exp).is_some() {
        return Err("confirmation token was already used");
    }
    Ok(())
}

/// Outcome of the shared two-phase gate.
enum Gate {
    /// No token was given: change nothing, hand back this token with the plan.
    Propose(String),
    /// A valid, unused token bound to this caller, tool and exact arguments: go ahead.
    Execute,
}

/// The two-phase confirm shared by every write tool. `params` must be the canonical JSON of the
/// arguments (everything except the token) so a token cannot be replayed with different arguments.
fn gate(actor_id: &str, tool: &str, params: &str, token: Option<&str>) -> Result<Gate, &'static str> {
    match token {
        None => Ok(Gate::Propose(issue(actor_id, tool, params, now_secs()))),
        Some(t) => redeem(t, actor_id, tool, params).map(|()| Gate::Execute),
    }
}

/// Audit record `mcp.<tool>` (proposed | rejected | success | failure). Failures to audit never block.
async fn audit(
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

fn write_guard(s: &AppState, actor: &Actor) -> Result<(), McpError> {
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

        match gate(&actor.id, CREATE_SNAPSHOT, &params, args.confirmation_token.as_deref()) {
            Ok(Gate::Propose(token)) => {
                audit(s, &actor, CREATE_SNAPSHOT, "volume", rid, "proposed", json!({ "via": "mcp", "confirmed": false })).await;
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
                audit(s, &actor, CREATE_SNAPSHOT, "volume", rid, "rejected", json!({ "via": "mcp", "reason": why })).await;
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
            Ok(Json(v)) => audit(s, &actor, CREATE_SNAPSHOT, "volume", rid, "success", json!({ "via": "mcp", "confirmed": true, "result": v })).await,
            Err(e) => audit(s, &actor, CREATE_SNAPSHOT, "volume", rid, "failure", json!({ "via": "mcp", "confirmed": true, "error": e.to_string() })).await,
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
            .ok_or_else(|| McpError::invalid_params(format!("unknown alert {}", args.alert_id), None))?;
        let params = json!({ "alert_id": args.alert_id }).to_string();
        let rid = args.alert_id.as_str();

        match gate(&actor.id, ACK_ALERT, &params, args.confirmation_token.as_deref()) {
            Ok(Gate::Propose(token)) => {
                audit(s, &actor, ACK_ALERT, "alert", rid, "proposed", json!({ "via": "mcp", "confirmed": false })).await;
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
                audit(s, &actor, ACK_ALERT, "alert", rid, "rejected", json!({ "via": "mcp", "reason": why })).await;
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
            Ok(Json(v)) => audit(s, &actor, ACK_ALERT, "alert", rid, "success", json!({ "via": "mcp", "confirmed": true, "result": v })).await,
            Err(e) => audit(s, &actor, ACK_ALERT, "alert", rid, "failure", json!({ "via": "mcp", "confirmed": true, "error": e.to_string() })).await,
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_proposes_without_a_token_and_executes_with_a_matching_one() {
        let params = r#"{"alert_id":"alert_a"}"#;
        let Ok(Gate::Propose(t)) = gate("alice", ACK_ALERT, params, None) else {
            panic!("no token must propose");
        };
        // A token for another tool or other arguments never opens the gate.
        assert!(gate("alice", CREATE_SNAPSHOT, params, Some(&t)).is_err());
        assert!(gate("alice", ACK_ALERT, r#"{"alert_id":"alert_b"}"#, Some(&t)).is_err());
        assert!(matches!(gate("alice", ACK_ALERT, params, Some(&t)), Ok(Gate::Execute)));
        assert!(gate("alice", ACK_ALERT, params, Some(&t)).is_err(), "single use");
    }

    #[test]
    fn token_is_bound_single_use_and_expires() {
        let params = r#"{"volume_id":"vol_a","name":null}"#;
        let now = now_secs();
        let t = issue("alice", CREATE_SNAPSHOT, params, now);
        // Wrong caller / tool / arguments are rejected without consuming the token.
        assert!(redeem(&t, "bob", CREATE_SNAPSHOT, params).is_err());
        assert!(redeem(&t, "alice", "delete_volume", params).is_err());
        assert!(redeem(&t, "alice", CREATE_SNAPSHOT, r#"{"volume_id":"vol_b","name":null}"#).is_err());
        // The right caller redeems exactly once.
        assert!(redeem(&t, "alice", CREATE_SNAPSHOT, params).is_ok());
        assert_eq!(
            redeem(&t, "alice", CREATE_SNAPSHOT, params),
            Err("confirmation token was already used")
        );
        // Tampered and expired tokens fail.
        let mut bad = t.clone();
        bad.push('x');
        assert!(redeem(&bad, "alice", CREATE_SNAPSHOT, params).is_err());
        let old = issue("alice", CREATE_SNAPSHOT, params, now.saturating_sub(TOKEN_TTL_SECS + 120));
        assert_eq!(
            redeem(&old, "alice", CREATE_SNAPSHOT, params),
            Err("confirmation token is invalid or expired")
        );
    }
}
