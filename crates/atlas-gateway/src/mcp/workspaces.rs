// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent workspace tools: an instant, isolated, disposable copy of an atlas-native volume or
//! filesystem for one agent run (docs/NATIVE_FS.md#agent-workspaces).
//!
//! `list_workspaces` and `get_workspace` are read-only. `create_workspace`, `reset_workspace` and
//! `discard_workspace` are guarded writes: listed only with `ATLAS_MCP_WRITE_ENABLE=1`, two-phase with
//! a single-use token bound to the caller and the exact arguments (see `write.rs`), and executed
//! through the REST handlers so role, tenant and quota checks are theirs. Reset and discard only ever
//! touch a workspace's own clone (and the snapshot Atlas took for it), never the source volume, and a
//! non-admin may only reset or discard a workspace they created themselves.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use rmcp::{
    handler::server::wrapper::{Json as ToolJson, Parameters},
    model::ErrorData as McpError,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::write::{audit, gate, write_guard, Gate, TOKEN_TTL_SECS};
use super::{actor_from, internal_error, rest, AtlasMcp};
use crate::auth::{require_tenant, Actor, ROLE_ADMIN};
use crate::routes::workspaces::{self, CreateWorkspaceBody};

const CREATE_WORKSPACE: &str = "create_workspace";
const RESET_WORKSPACE: &str = "reset_workspace";
const DISCARD_WORKSPACE: &str = "discard_workspace";

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceIdArgs {
    /// Workspace id, as returned by `create_workspace` or `list_workspaces`.
    workspace_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateWorkspaceArgs {
    /// Volume or filesystem to copy, as returned by `list_volumes`. A snapshot of it is taken now.
    /// Give this or `source_snapshot_id`, not both.
    #[serde(default)]
    source_volume_id: Option<String>,
    /// An existing snapshot to clone instead (it is left alone when the workspace goes away).
    #[serde(default)]
    source_snapshot_id: Option<String>,
    /// Label for the workspace, for example the agent run id.
    #[serde(default)]
    name: Option<String>,
    /// Lifetime in seconds, 60 up to the server maximum (default 3600). Expired workspaces are
    /// discarded automatically.
    #[serde(default)]
    ttl_secs: Option<i64>,
    /// Refuse a source larger than this many bytes.
    #[serde(default)]
    max_bytes: Option<i64>,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceActionArgs {
    /// Workspace id, as returned by `create_workspace` or `list_workspaces`.
    workspace_id: String,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

/// A non-admin may only act on a workspace they created.
fn require_owner(auth_required: bool, actor: &Actor, owner: &str, id: &str) -> Result<(), McpError> {
    if auth_required && actor.level() < ROLE_ADMIN && actor.id != owner {
        return Err(McpError::invalid_request(
            format!("workspace {id} was created by another caller; only its creator or an admin may change it"),
            None,
        ));
    }
    Ok(())
}

#[tool_router(router = workspace_read_router, vis = "pub(super)")]
impl AtlasMcp {
    #[tool(
        description = "List agent workspaces (disposable clones of native volumes/filesystems) with \
                        state, size, cap and seconds left before expiry. Scoped to the caller's own \
                        tenant unless the caller has the admin role."
    )]
    async fn list_workspaces(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        rest(workspaces::list_workspaces(State(self.state.clone()), Extension(actor)).await)
    }

    #[tool(description = "Get one agent workspace by id: its volume id, state, size, cap and seconds \
                           left before expiry. The caller's tenant must own it.")]
    async fn get_workspace(
        &self,
        Parameters(args): Parameters<WorkspaceIdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        rest(
            workspaces::get_workspace(State(self.state.clone()), Extension(actor), Path(args.workspace_id))
                .await,
        )
    }
}

#[tool_router(router = workspace_write_router, vis = "pub(super)")]
impl AtlasMcp {
    #[tool(
        description = "Create an agent workspace: an instant, isolated copy-on-write clone of an \
                        atlas-native volume or filesystem (or of one of its snapshots) that the agent \
                        can write freely without touching the source. It expires after ttl_secs and is \
                        then discarded automatically. Two-phase: call without confirmation_token to \
                        receive the plan and a 5-minute single-use token, then call again with the same \
                        arguments plus that token to execute. Requires an operator-or-higher token and \
                        the caller's tenant must own the source; tenant quotas apply."
    )]
    async fn create_workspace(
        &self,
        Parameters(args): Parameters<CreateWorkspaceArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let s = &self.state;
        write_guard(s, &actor)?;
        let source = match (&args.source_volume_id, &args.source_snapshot_id) {
            (Some(v), None) => v.clone(),
            (None, Some(sn)) => atlas_inventory::snapshots::get_snapshot(&s.pool, sn)
                .await
                .map_err(internal_error)?
                .map(|snap| snap.volume_id)
                .ok_or_else(|| McpError::invalid_params(format!("unknown snapshot {sn}"), None))?,
            _ => {
                return Err(McpError::invalid_params(
                    "give exactly one of source_volume_id and source_snapshot_id",
                    None,
                ))
            }
        };
        let vol = atlas_inventory::get_volume(&s.pool, &source)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| McpError::invalid_params(format!("unknown volume {source}"), None))?;
        let tenant = atlas_inventory::volume_tenant(&s.pool, &source)
            .await
            .map_err(internal_error)?;
        require_tenant(s.config.auth_required, &actor, &tenant, format!("volume {source}"))
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let name = args.name.clone().filter(|n| !n.trim().is_empty());
        let params = json!({
            "source_volume_id": args.source_volume_id, "source_snapshot_id": args.source_snapshot_id,
            "name": name, "ttl_secs": args.ttl_secs, "max_bytes": args.max_bytes,
        })
        .to_string();

        match gate(s, &actor.id, CREATE_WORKSPACE, &params, args.confirmation_token.as_deref()).await {
            Ok(Gate::Propose(token)) => {
                // The plan changes nothing, so it is the dry run: it also reports whether the execute
                // would be refused (tenant count, size cap, byte budget) rather than leave that to be
                // found out after the token is spent.
                let preflight = match workspaces::resolve_max_bytes(args.max_bytes) {
                    Ok(cap) => workspaces::preflight_create(s, &actor, &tenant, vol.size_bytes, cap).await,
                    Err(e) => Err(e),
                };
                audit(s, &actor, CREATE_WORKSPACE, "volume", &source, "proposed", json!({ "via": "mcp", "confirmed": false })).await;
                return Ok(ToolJson(json!({
                    "executed": false,
                    "plan": {
                        "action": "create_workspace",
                        "source_volume_id": vol.id,
                        "source_volume_name": vol.name,
                        "source_kind": vol.kind,
                        "source_size_bytes": vol.size_bytes,
                        "from_snapshot": args.source_snapshot_id,
                        "name": name,
                        "ttl_secs": args.ttl_secs,
                        "max_bytes": args.max_bytes,
                        "writes_to_source": false,
                        "would_be_refused": preflight.as_ref().err().map(|e| e.to_string()),
                    },
                    "confirmation_token": token,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": "call create_workspace again with the same arguments plus confirmation_token",
                })));
            }
            Ok(Gate::Execute) => {}
            Err(why) => {
                audit(s, &actor, CREATE_WORKSPACE, "volume", &source, "rejected", json!({ "via": "mcp", "reason": why })).await;
                return Err(McpError::invalid_params(why, None));
            }
        }

        let body = CreateWorkspaceBody {
            source_volume_id: args.source_volume_id.clone(),
            source_snapshot_id: args.source_snapshot_id.clone(),
            name,
            ttl_secs: args.ttl_secs,
            max_bytes: args.max_bytes,
        };
        let result = workspaces::create_workspace(State(s.clone()), Extension(actor.clone()), Json(body))
            .await
            .map(|(_, j)| j);
        match &result {
            Ok(Json(v)) => audit(s, &actor, CREATE_WORKSPACE, "volume", &source, "success", json!({ "via": "mcp", "confirmed": true, "result": v })).await,
            Err(e) => audit(s, &actor, CREATE_WORKSPACE, "volume", &source, "failure", json!({ "via": "mcp", "confirmed": true, "error": e.to_string() })).await,
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }

    #[tool(
        description = "Roll an agent workspace back to the state it was created from: everything \
                        written to it is deleted and a fresh clone of the same base snapshot takes its \
                        place (the volume id changes, the expiry does not). The source volume is not \
                        touched. Two-phase: call without confirmation_token to receive the plan and a \
                        5-minute single-use token, then call again with the same arguments plus that \
                        token to execute. Requires an operator-or-higher token; only the workspace's \
                        creator (or an admin) may reset it."
    )]
    async fn reset_workspace(
        &self,
        Parameters(args): Parameters<WorkspaceActionArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.workspace_action(RESET_WORKSPACE, args, context).await
    }

    #[tool(
        description = "Discard an agent workspace: its clone and everything written to it is deleted, \
                        along with the snapshot Atlas took for it. The source volume is not touched. \
                        Two-phase: call without confirmation_token to receive the plan and a 5-minute \
                        single-use token, then call again with the same arguments plus that token to \
                        execute. Requires an operator-or-higher token; only the workspace's creator (or \
                        an admin) may discard it."
    )]
    async fn discard_workspace(
        &self,
        Parameters(args): Parameters<WorkspaceActionArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.workspace_action(DISCARD_WORKSPACE, args, context).await
    }
}

impl AtlasMcp {
    /// Shared body of `reset_workspace` and `discard_workspace`.
    async fn workspace_action(
        &self,
        tool: &'static str,
        args: WorkspaceActionArgs,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let s = &self.state;
        write_guard(s, &actor)?;
        let id = args.workspace_id.as_str();
        let ws = atlas_inventory::workspaces::get(&s.pool, id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| McpError::invalid_params(format!("unknown workspace {id}"), None))?;
        require_tenant(s.config.auth_required, &actor, &ws.tenant_id, format!("workspace {id}"))
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
        require_owner(s.config.auth_required, &actor, &ws.owner, id)?;

        let params = json!({ "workspace_id": id }).to_string();
        match gate(s, &actor.id, tool, &params, args.confirmation_token.as_deref()).await {
            Ok(Gate::Propose(token)) => {
                audit(s, &actor, tool, "workspace", id, "proposed", json!({ "via": "mcp", "confirmed": false })).await;
                return Ok(ToolJson(json!({
                    "executed": false,
                    "plan": {
                        "action": tool,
                        "workspace_id": ws.id,
                        "name": ws.name,
                        "state": ws.state,
                        "volume_id": ws.volume_id,
                        "source_volume_id": ws.source_volume_id,
                        "base_snapshot_id": ws.base_snapshot_id,
                        "deletes_everything_written_to_the_workspace": true,
                        "writes_to_source": false,
                    },
                    "confirmation_token": token,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": format!("call {tool} again with the same arguments plus confirmation_token"),
                })));
            }
            Ok(Gate::Execute) => {}
            Err(why) => {
                audit(s, &actor, tool, "workspace", id, "rejected", json!({ "via": "mcp", "reason": why })).await;
                return Err(McpError::invalid_params(why, None));
            }
        }

        let result = if tool == RESET_WORKSPACE {
            workspaces::reset_workspace(State(s.clone()), Extension(actor.clone()), Path(id.to_string())).await
        } else {
            workspaces::discard_workspace(State(s.clone()), Extension(actor.clone()), Path(id.to_string())).await
        };
        match &result {
            Ok(Json(v)) => audit(s, &actor, tool, "workspace", id, "success", json!({ "via": "mcp", "confirmed": true, "result": v })).await,
            Err(e) => audit(s, &actor, tool, "workspace", id, "failure", json!({ "via": "mcp", "confirmed": true, "error": e.to_string() })).await,
        }
        let out = rest(result)?;
        Ok(ToolJson(json!({ "executed": true, "result": out.0 })))
    }
}
