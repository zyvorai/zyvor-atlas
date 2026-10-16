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
use crate::auth::{require_role, require_tenant, ROLE_OPERATOR};
use crate::routes::volumes::{self, SnapshotBody};

const TOKEN_TTL_SECS: u64 = 300;
const CREATE_SNAPSHOT: &str = "create_snapshot";

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
        if !write_enabled() {
            return Err(McpError::invalid_request(
                "write tools are disabled on this server",
                None,
            ));
        }
        let s = &self.state;
        require_role(s.config.auth_required, &actor, ROLE_OPERATOR)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
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
        let audit = |status: &'static str, detail: Value| {
            let pool = s.pool.clone();
            let tenant_id = actor.tenant_id.clone();
            let actor_id = actor.id.clone();
            let rid = args.volume_id.clone();
            async move {
                let _ = atlas_inventory::audit::record(
                    &pool,
                    Some(&tenant_id),
                    &actor_id,
                    "mcp.create_snapshot",
                    "volume",
                    &rid,
                    status,
                    None,
                    Some(detail),
                )
                .await;
            }
        };

        let Some(token) = args.confirmation_token.as_deref() else {
            let token = issue(&actor.id, CREATE_SNAPSHOT, &params, now_secs());
            audit("proposed", json!({ "via": "mcp", "confirmed": false })).await;
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
        };

        if let Err(why) = redeem(token, &actor.id, CREATE_SNAPSHOT, &params) {
            audit("rejected", json!({ "via": "mcp", "reason": why })).await;
            return Err(McpError::invalid_params(why, None));
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
            Ok(Json(v)) => audit("success", json!({ "via": "mcp", "confirmed": true, "result": v })).await,
            Err(e) => audit("failure", json!({ "via": "mcp", "confirmed": true, "error": e.to_string() })).await,
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
