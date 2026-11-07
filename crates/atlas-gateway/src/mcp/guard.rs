// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Guardrails at the one `call_tool` chokepoint: the agent's run id, its resource scope, and the call
//! budgets. Kept out of `mcp.rs` so every tool, present and future, passes the same checks without
//! carrying any of them.
//!
//! * **Run id.** An agent may send `X-Atlas-Run-Id` (1 to 64 characters of `[A-Za-z0-9_.:-]`). It is
//!   stored on every audit row the call causes ([`atlas_inventory::audit::RUN_ID`]), so one run can be
//!   joined to the atlas_data audit trail, which accepts the same id.
//! * **Resource scope.** A token carrying a [`ResourceScope`] may name only the volumes, snapshots,
//!   workspaces and paths it lists. Any tool argument called `volume_id`, `source_volume_id`,
//!   `snapshot_id`, `source_snapshot_id`, `workspace_id`, `path` or `paths` is checked, and so is `id`
//!   on the volume tools; tools that enumerate inventory cannot be narrowed per resource and are
//!   refused outright under a scope.
//! * **Budgets.** Calls per minute per token and per tool, on the in-process limiter that syncs its
//!   counts through the database (`rate_limit_counters`), so on Postgres the budget holds across
//!   replicas within one sync interval (`ATLAS_RATE_LIMIT_SYNC_SECS`, default 2 s). On SQLite each pod
//!   has its own database, so it stays per pod, as `docs/HA.md` says for the request limiter.

use std::sync::OnceLock;

use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, ErrorData as McpError},
    service::{RequestContext, RoleServer},
};
use serde_json::{json, Value};

use super::{actor_from, AtlasMcp};
use crate::auth::{Actor, ResourceKind, ResourceScope};
use crate::state::RateLimiter;

/// Request header carrying the agent run id.
pub(super) const RUN_ID_HEADER: &str = "x-atlas-run-id";

/// Longest tool name written to the audit trail; a caller can send any string as a tool name.
const MAX_AUDITED_TOOL_NAME: usize = 64;

/// Tools whose result is a list of volumes, snapshots, workspaces or their audit trail across the
/// whole inventory. They take no resource to check and cannot be filtered to a scope, so a token with
/// a resource scope may not call them (unless the call names an in-scope resource, as `list_backups`
/// can with `volume_id`).
const ENUMERATING_TOOLS: &[&str] = &[
    "list_volumes",
    "list_snapshots",
    "list_workspaces",
    "list_volume_groups",
    "list_backups",
    "list_schedules",
    "protection_status",
    "orphan_backups",
    "list_events",
    "list_audit",
];

/// Tools whose generic `id` argument is a volume id.
const VOLUME_ID_TOOLS: &[&str] = &["get_volume", "volume_protection", "volume_ransomware"];

/// Per-token budget of tool calls per minute (`ATLAS_MCP_CALLS_PER_MIN`, 0 or unset = off), on top of the
/// per-actor request limit at the edge. A token with a `jti` has its own budget; callers without one (the
/// bootstrap secret, legacy tokens) share their actor id's. Read once at first use.
fn call_budget() -> &'static RateLimiter {
    static BUDGET: OnceLock<RateLimiter> = OnceLock::new();
    BUDGET.get_or_init(|| RateLimiter::new(env_u32("ATLAS_MCP_CALLS_PER_MIN")))
}

/// Per-token, per-tool budget (`ATLAS_MCP_TOOL_CALLS_PER_MIN`, 0 or unset = off): each tool may be called this
/// many times a minute by one token, so an agent looping on one expensive tool (`ceph_status`, `list_audit`)
/// cannot use up its whole `ATLAS_MCP_CALLS_PER_MIN` allowance on it. Only names that are real tools are
/// counted, so an arbitrary tool name cannot grow the table. Read once at first use.
fn tool_budget() -> &'static RateLimiter {
    static BUDGET: OnceLock<RateLimiter> = OnceLock::new();
    BUDGET.get_or_init(|| RateLimiter::new(env_u32("ATLAS_MCP_TOOL_CALLS_PER_MIN")))
}

fn env_u32(name: &str) -> u32 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Start (once per process) the background sync that shares both budgets' counts through the
/// database. The budget keys carry an `mcp:` prefix so they never mix with the request limiter's
/// per-actor rows in the same table. A no-op for a budget that is off.
fn ensure_budget_sync(pool: &sqlx::AnyPool) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let secs = std::env::var("ATLAS_RATE_LIMIT_SYNC_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        let replica = format!(
            "{}-{}-mcp",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
            std::process::id()
        );
        crate::state::spawn_rate_limit_sync(pool.clone(), call_budget().clone(), replica.clone(), secs);
        crate::state::spawn_rate_limit_sync(pool.clone(), tool_budget().clone(), format!("{replica}-tool"), secs);
    });
}

/// The validated run id of this request: `Ok(None)` without the header, `Err` with a reason when it
/// is present but not a valid id.
pub(super) fn run_id_from(context: &RequestContext<RoleServer>) -> Result<Option<String>, &'static str> {
    let Some(parts) = context.extensions.get::<axum::http::request::Parts>() else {
        return Ok(None);
    };
    let Some(raw) = parts.headers.get(RUN_ID_HEADER) else {
        return Ok(None);
    };
    match raw.to_str() {
        Ok(v) if atlas_inventory::audit::valid_run_id(v) => Ok(Some(v.to_string())),
        _ => Err("X-Atlas-Run-Id must be 1 to 64 characters of letters, digits, '_', '.', ':' or '-'"),
    }
}

/// Every resource a call names, by the argument names listed in the module docs. A value of the wrong
/// type is returned as an empty name, which no scope allows.
pub(super) fn named_resources(tool: &str, args: Option<&serde_json::Map<String, Value>>) -> Vec<(ResourceKind, String)> {
    let Some(args) = args else { return Vec::new() };
    let mut out = Vec::new();
    let mut one = |kind: ResourceKind, v: &Value| match v {
        Value::Null => {}
        Value::String(s) => out.push((kind, s.clone())),
        _ => out.push((kind, String::new())),
    };
    for (key, v) in args {
        match key.as_str() {
            "volume_id" | "source_volume_id" => one(ResourceKind::Volume, v),
            "snapshot_id" | "source_snapshot_id" => one(ResourceKind::Snapshot, v),
            "workspace_id" => one(ResourceKind::Workspace, v),
            "path" => one(ResourceKind::Path, v),
            "paths" => match v {
                Value::Array(items) => items.iter().for_each(|i| one(ResourceKind::Path, i)),
                Value::Null => {}
                _ => one(ResourceKind::Path, &json!(0)),
            },
            "id" if VOLUME_ID_TOOLS.contains(&tool) => one(ResourceKind::Volume, v),
            _ => {}
        }
    }
    out
}

/// Check a call against a resource scope. `Err` carries the reason and the first offending resource.
pub(super) fn check_scope(
    scope: &ResourceScope,
    tool: &str,
    args: Option<&serde_json::Map<String, Value>>,
) -> Result<(), (String, Option<(ResourceKind, String)>)> {
    let named = named_resources(tool, args);
    if named.is_empty() && ENUMERATING_TOOLS.contains(&tool) {
        return Err((
            format!("tool '{tool}' lists resources across the inventory and cannot be limited to this token's resource scope"),
            None,
        ));
    }
    for (kind, name) in named {
        if !scope.allows(kind, &name) {
            return Err((
                format!("this token's scope does not allow {} '{}'", kind.as_str(), name.chars().take(128).collect::<String>()),
                Some((kind, name)),
            ));
        }
    }
    Ok(())
}

impl AtlasMcp {
    /// Record a refusal. The tool name and the reason are the only things stored; arguments never are.
    async fn audit_refusal(&self, actor: &Actor, run_id: Option<&str>, action: &str, tool: &str, detail: Value) {
        let _ = atlas_inventory::audit::record_with_run(
            &self.state.pool,
            run_id,
            Some(&actor.tenant_id),
            &actor.id,
            action,
            "mcp_tool",
            tool,
            "denied",
            None,
            Some(detail),
        )
        .await;
    }

    /// The body of `call_tool` after the run id is known: tool allowlist, resource scope, budgets,
    /// then the tool itself, then the audit row for the call.
    pub(super) async fn guarded_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
        run_id: Option<String>,
    ) -> Result<CallToolResponse, McpError> {
        let actor = actor_from(&context)?;
        let tool: String = request.name.chars().take(MAX_AUDITED_TOOL_NAME).collect();
        if !actor.may_call_tool(&request.name) {
            self.audit_refusal(&actor, run_id.as_deref(), "mcp.tool.denied", &tool, json!({ "reason": "tool not in token allowlist" }))
                .await;
            return Err(McpError::invalid_request(
                format!("tool '{}' is not permitted for this token", request.name),
                None,
            ));
        }
        if let Some(scope) = &actor.scope {
            if let Err((reason, named)) = check_scope(scope, &request.name, request.arguments.as_ref()) {
                let detail = match named {
                    Some((kind, name)) => json!({ "reason": "resource outside token scope", "kind": kind.as_str(),
                                                  "resource": name.chars().take(128).collect::<String>() }),
                    None => json!({ "reason": "enumerating tool under a resource scope" }),
                };
                self.audit_refusal(&actor, run_id.as_deref(), "mcp.tool.denied", &tool, detail).await;
                return Err(McpError::invalid_request(reason, None));
            }
        }
        ensure_budget_sync(&self.state.pool);
        let budget_key = if actor.jti.is_empty() { actor.id.as_str() } else { actor.jti.as_str() };
        if !call_budget().allow(&format!("mcp:call:{budget_key}")) {
            self.audit_refusal(&actor, run_id.as_deref(), "mcp.tool.rate_limited", &tool, json!({ "jti": actor.jti }))
                .await;
            return Err(McpError::invalid_request(
                "tool-call budget for this token is used up for this minute; retry later",
                None,
            ));
        }
        if self.tool_router.has_route(&request.name)
            && !tool_budget().allow(&format!("mcp:tool:{budget_key}\u{1f}{}", request.name))
        {
            self.audit_refusal(&actor, run_id.as_deref(), "mcp.tool.rate_limited", &tool, json!({ "jti": actor.jti, "scope": "tool" }))
                .await;
            return Err(McpError::invalid_request(
                format!("call budget for tool '{}' is used up for this minute; retry later", request.name),
                None,
            ));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        if super::audit_calls_enabled() {
            let status = match &result {
                Ok(CallToolResponse::Complete(r)) if r.is_error != Some(true) => "success",
                _ => "error",
            };
            // Off the request path: a slow or failing audit write never delays or fails a tool call.
            let pool = self.state.pool.clone();
            tokio::spawn(async move {
                let _ = atlas_inventory::audit::record_with_run(
                    &pool,
                    run_id.as_deref(),
                    Some(&actor.tenant_id),
                    &actor.id,
                    "mcp.tool.call",
                    "mcp_tool",
                    &tool,
                    status,
                    None,
                    Some(json!({ "jti": actor.jti, "scoped": actor.is_scoped(), "role": actor.role })),
                )
                .await;
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Option<serde_json::Map<String, Value>> {
        v.as_object().cloned()
    }

    fn scope(volumes: &[&str], workspaces: &[&str]) -> ResourceScope {
        ResourceScope {
            volumes: volumes.iter().map(|s| s.to_string()).collect(),
            workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn resources_are_found_by_argument_name() {
        let n = named_resources(
            "create_workspace",
            args(json!({ "source_volume_id": "v1", "source_snapshot_id": null, "name": "x" })).as_ref(),
        );
        assert_eq!(n, vec![(ResourceKind::Volume, "v1".to_string())]);
        let n = named_resources("get_volume", args(json!({ "id": "v2" })).as_ref());
        assert_eq!(n, vec![(ResourceKind::Volume, "v2".to_string())]);
        // `id` is a resource only for the volume tools.
        assert!(named_resources("get_job", args(json!({ "id": "j1" })).as_ref()).is_empty());
        let n = named_resources("x", args(json!({ "paths": ["/a", "/b"], "workspace_id": "ws_1" })).as_ref());
        assert_eq!(n.len(), 3);
        // A value of the wrong type becomes an empty name, which nothing allows.
        let n = named_resources("x", args(json!({ "volume_id": 5 })).as_ref());
        assert_eq!(n, vec![(ResourceKind::Volume, String::new())]);
    }

    #[test]
    fn a_scope_allows_listed_resources_and_denies_the_rest_by_default() {
        let s = scope(&["vol_a", "agent-*"], &[]);
        assert!(check_scope(&s, "get_volume", args(json!({ "id": "vol_a" })).as_ref()).is_ok());
        assert!(check_scope(&s, "get_volume", args(json!({ "id": "agent-7" })).as_ref()).is_ok());
        assert!(check_scope(&s, "get_volume", args(json!({ "id": "vol_b" })).as_ref()).is_err());
        // No workspace list at all: naming any workspace is denied.
        assert!(check_scope(&s, "get_workspace", args(json!({ "workspace_id": "ws_1" })).as_ref()).is_err());
        // A tool naming nothing is not restricted by the scope.
        assert!(check_scope(&s, "ceph_status", None).is_ok());
        // One bad resource among several denies the call.
        let two = args(json!({ "source_volume_id": "vol_a", "source_snapshot_id": "snap_x" }));
        assert!(check_scope(&s, "create_workspace", two.as_ref()).is_err());
    }

    #[test]
    fn enumerating_tools_are_refused_unless_they_name_an_allowed_resource() {
        let s = scope(&["vol_a"], &[]);
        assert!(check_scope(&s, "list_volumes", None).is_err());
        assert!(check_scope(&s, "list_backups", args(json!({})).as_ref()).is_err());
        assert!(check_scope(&s, "list_backups", args(json!({ "volume_id": "vol_a" })).as_ref()).is_ok());
        assert!(check_scope(&s, "list_backups", args(json!({ "volume_id": "vol_b" })).as_ref()).is_err());
    }

    #[test]
    fn paths_cannot_escape_their_pattern() {
        let s = ResourceScope { paths: vec!["/data/run1/*".into()], ..Default::default() };
        assert!(check_scope(&s, "x", args(json!({ "path": "/data/run1/out.bin" })).as_ref()).is_ok());
        assert!(check_scope(&s, "x", args(json!({ "path": "/data/run2/out.bin" })).as_ref()).is_err());
        assert!(check_scope(&s, "x", args(json!({ "path": "/data/run1/../run2/x" })).as_ref()).is_err());
        assert!(check_scope(&s, "x", args(json!({ "paths": ["/data/run1/a", "/etc/passwd"] })).as_ref()).is_err());
    }
}
