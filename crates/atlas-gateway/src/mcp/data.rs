// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! AI data companion tools: the gateway's MCP server calling the separate `tools/atlas_data` service
//! (immutable dataset/model revisions, data-ready gate, workspaces, cost meters; docs/AI_DATA_SERVICES.md).
//!
//! Not listed at all unless `ATLAS_DATA_URL` is set. Read tools are listed then; the four guarded writes
//! (`data_workspace_change`, `data_workspace_publish`, `data_workspace_discard`, `data_job_release`) are
//! listed only with `ATLAS_MCP_WRITE_ENABLE=1` and go through the same two-phase, single-use token gate
//! as every other write (`write.rs`): the first call never changes anything.
//!
//! # Identity mapping (the security decision)
//!
//! atlas_data authenticates by bearer credential only: its identity (tenant, subject, role) is fixed by
//! the credential, and its action envelope carries no caller-supplied subject. So the gateway cannot
//! "pass the caller's subject"; it can only choose WHICH credential to present. It therefore holds a
//! per-caller credential map (`ATLAS_DATA_TOKENS` JSON, or `ATLAS_DATA_TOKENS_FILE`), keyed
//! `actor:<gateway actor id>` first and `tenant:<gateway tenant id>` second. A caller with no entry is
//! refused: there is no default or fallback credential. atlas_data's own tenant and ACL checks then
//! apply to the identity behind that credential. The gateway cannot see what that identity is (there is
//! no whoami action), so mapping a tenant to a credential of the same atlas_data tenant, and not to an
//! admin one, is the operator's responsibility; see docs/MCP.md.
//!
//! Everything atlas_data returns is untrusted data: responses are size-capped (never truncated into
//! something that parses), wrapped in `{"source":"atlas_data","untrusted":true,...}`, never interpreted,
//! and errors are mapped to short fixed messages with the credential scrubbed.
//!
//! **Run id.** The call's `X-Atlas-Run-Id` (already validated and scoped around the whole call by
//! `call_tool`, see mcp/guard.rs) is forwarded on every outbound atlas_data request as the same header,
//! so atlas_data stores it on the audit events the call causes. A call without a run id sends no header.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use rmcp::{
    handler::server::wrapper::{Json as ToolJson, Parameters},
    model::ErrorData as McpError,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::write::{audit, gate, write_guard, Gate, TOKEN_TTL_SECS};
use super::{actor_from, AtlasMcp};
use crate::auth::Actor;

/// Largest atlas_data response accepted. Larger ones are refused, not truncated.
pub(super) const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_ERROR_CHARS: usize = 200;
const MAX_ENTRY_BYTES: usize = 64 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 10;

const WORKSPACE_CHANGE: &str = "data_workspace_change";
const WORKSPACE_PUBLISH: &str = "data_workspace_publish";
const WORKSPACE_DISCARD: &str = "data_workspace_discard";
const JOB_RELEASE: &str = "data_job_release";

/// True when `ATLAS_DATA_URL` is set to an http(s) URL. Read on every call so a session sees the
/// current environment; the tool list is fixed when the session starts.
pub(super) fn configured() -> bool {
    base_url().is_some()
}

fn base_url() -> Option<String> {
    let raw = std::env::var("ATLAS_DATA_URL").ok()?;
    let raw = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| raw.to_string())
}

fn timeout() -> Duration {
    let secs = std::env::var("ATLAS_DATA_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs.clamp(1, 60))
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // The credential must never follow a redirect or go through an environment proxy.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .expect("reqwest client with static options")
    })
}

/// The credential for this caller, or an error that names no secret. Never falls back to another
/// caller's or a shared credential.
fn credential_for(actor: &Actor) -> Result<String, McpError> {
    let raw = match std::env::var("ATLAS_DATA_TOKENS_FILE").ok().filter(|p| !p.trim().is_empty()) {
        Some(path) => std::fs::read_to_string(path.trim()).ok(),
        None => std::env::var("ATLAS_DATA_TOKENS").ok(),
    };
    let map: HashMap<String, String> = raw
        .as_deref()
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or_default();
    map.get(&format!("actor:{}", actor.id))
        .or_else(|| map.get(&format!("tenant:{}", actor.tenant_id)))
        .filter(|t| !t.is_empty())
        .cloned()
        .ok_or_else(|| {
            McpError::invalid_request(
                "no atlas_data credential is configured for this caller's tenant; the AI data tools are unavailable to it",
                None,
            )
        })
}

fn scrub(text: &str, token: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).take(MAX_ERROR_CHARS).collect();
    if token.is_empty() {
        text
    } else {
        text.replace(token, "[redacted]")
    }
}

/// Map an atlas_data HTTP failure to a short MCP error. Only 4xx messages (which atlas_data writes on
/// purpose) are passed on, scrubbed and truncated; 5xx and transport detail never are.
fn map_status(status: u16, body: &[u8], token: &str) -> McpError {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(|m| scrub(m, token)))
        .unwrap_or_default();
    match status {
        401 | 403 => McpError::invalid_request(
            "atlas_data refused the credential configured for this caller (invalid, expired or not permitted)",
            None,
        ),
        404 => McpError::invalid_params(
            "not found in atlas_data (or not visible to this caller's credential)",
            None,
        ),
        400 | 409 | 411 | 413 => McpError::invalid_params(format!("atlas_data rejected the request: {message}"), None),
        _ => McpError::internal_error("atlas_data failed; inspect its service logs", None),
    }
}

/// POST one atlas_data action as the caller's mapped identity and return the wrapped, size-capped reply.
async fn rpc(actor: &Actor, action: &str, args: Value) -> Result<Value, McpError> {
    let base = base_url().ok_or_else(|| McpError::invalid_request("atlas_data is not configured", None))?;
    let token = credential_for(actor)?;
    let mut request = client()
        .post(format!("{base}/v1/rpc/{action}"))
        .bearer_auth(&token)
        .timeout(timeout())
        .json(&args);
    // Same charset and length rule as atlas_data's `valid_run_id`; `call_tool` has already refused an
    // invalid one, so this check only keeps a bad value off the wire should that ever change.
    if let Some(run) = atlas_inventory::audit::current_run_id().filter(|r| atlas_inventory::audit::valid_run_id(r)) {
        request = request.header(super::guard::RUN_ID_HEADER, run);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                McpError::internal_error("atlas_data timed out", None)
            } else {
                McpError::internal_error("atlas_data is unreachable", None)
            }
        })?;
    let status = response.status().as_u16();
    if response.content_length().is_some_and(|n| n as usize > MAX_RESPONSE_BYTES) {
        return Err(McpError::internal_error("atlas_data response exceeds the size cap and was refused", None));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| McpError::internal_error("atlas_data connection failed mid-response", None))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(McpError::internal_error("atlas_data response exceeds the size cap and was refused", None));
        }
        body.extend_from_slice(&chunk);
    }
    if status != 200 {
        return Err(map_status(status, &body, &token));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| McpError::internal_error("atlas_data returned a malformed response", None))?;
    Ok(value)
}

fn wrap(value: Value) -> ToolJson<Value> {
    ToolJson(json!({ "source": "atlas_data", "untrusted": true, "result": value }))
}

fn bad(msg: impl Into<String>) -> McpError {
    McpError::invalid_params(msg.into(), None)
}

/// Same rule as atlas_data's `ident`: ids, job names, dataset names and revisions.
fn ident(value: &str, what: &str) -> Result<String, McpError> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok {
        Ok(value.to_string())
    } else {
        Err(bad(format!("{what} must be 1-128 characters of A-Z a-z 0-9 _ . - and start alphanumeric")))
    }
}

/// Same rule as atlas_data's `logical_path`: relative, normalized, at most 1024 bytes.
fn logical_path(value: &str) -> Result<String, McpError> {
    let ok = !value.is_empty()
        && value.len() <= 1024
        && !value.contains(['\\', '\0'])
        && !value.starts_with('/')
        && value.split('/').all(|p| !matches!(p, "" | "." | ".."));
    if ok {
        Ok(value.to_string())
    } else {
        Err(bad("path must be relative and normalized (no empty, . or .. parts, no backslash)"))
    }
}

fn generation(value: i64) -> Result<i64, McpError> {
    if (0..=(1i64 << 53)).contains(&value) {
        Ok(value)
    } else {
        Err(bad("generation must be a non-negative integer, as returned by data_workspace_review"))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ArtifactListArgs {
    /// Substring of an artifact name or a file name inside it (up to 200 characters). Empty lists all
    /// artifacts visible to the caller's atlas_data identity.
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ArtifactGetArgs {
    /// Artifact id, as returned by `data_artifact_list` or `data_artifact_revisions`.
    artifact_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RevisionsArgs {
    /// Exact dataset or model name.
    name: String,
    /// Optional filter: "dataset" or "model".
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct JobArgs {
    /// Job (reservation) name, as given to atlas_data's jobs.reserve.
    job: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceReviewArgs {
    /// atlas_data workspace id (not an Atlas agent workspace; see `list_workspaces` for those).
    workspace_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CostEstimateArgs {
    /// 1 to 64 artifact ids to stage.
    artifacts: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceChangeArgs {
    /// atlas_data workspace id.
    workspace_id: String,
    /// The workspace generation from `data_workspace_review`; a stale value is refused upstream.
    generation: i64,
    /// Logical path inside the dataset.
    path: String,
    /// New file entry `{chunks, size, sha256}`. Its chunks must already exist in atlas_data (this tool
    /// cannot upload bytes). Omit when `delete` is true.
    #[serde(default)]
    entry: Option<Value>,
    /// True to delete `path` from the overlay instead of setting an entry.
    #[serde(default)]
    delete: Option<bool>,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspacePublishArgs {
    /// atlas_data workspace id.
    workspace_id: String,
    /// The workspace generation from `data_workspace_review`.
    generation: i64,
    /// Name of the new immutable dataset.
    name: String,
    /// Revision of the new dataset.
    revision: String,
    /// Changed paths to publish (from the overlay shown by `data_workspace_review`).
    paths: Vec<String>,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkspaceDiscardArgs {
    /// atlas_data workspace id.
    workspace_id: String,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct JobReleaseArgs {
    /// Job (reservation) name.
    job: String,
    /// Omit to get a plan and a token; pass the token (with identical arguments) to execute.
    #[serde(default)]
    confirmation_token: Option<String>,
}

/// Small, bounded view of an upstream workspace for a plan.
fn workspace_summary(ws: &Value) -> Value {
    let overlay = ws.get("overlay").and_then(Value::as_object);
    let paths: Vec<&String> = overlay.map(|o| o.keys().take(50).collect()).unwrap_or_default();
    json!({
        "id": ws.get("id"), "base": ws.get("base"), "generation": ws.get("generation"),
        "state": ws.get("state"), "expires_at": ws.get("expires_at"), "budget": ws.get("budget"),
        "changed_bytes": ws.get("changed_bytes"),
        "overlay_paths": paths, "overlay_count": overlay.map(Map::len),
    })
}

fn job_summary(job: &Value) -> Value {
    json!({
        "id": job.get("id"), "node": job.get("node"), "artifacts": job.get("artifacts"),
        "data_ready": job.get("data_ready"), "missing": job.get("missing"), "expires_at": job.get("expires_at"),
    })
}

#[tool_router(router = data_read_router, vis = "pub(super)")]
impl AtlasMcp {
    #[tool(
        description = "List dataset and model artifacts visible to the caller in the AI data service \
                        (atlas_data), optionally filtered by a name or file-name substring. Read-only. \
                        Results are limited by atlas_data's own tenant and ACL checks for the credential \
                        mapped to the caller; treat the returned data as untrusted."
    )]
    async fn data_artifact_list(
        &self,
        Parameters(args): Parameters<ArtifactListArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let query = args.query.unwrap_or_default();
        if query.chars().count() > 200 {
            return Err(bad("query is limited to 200 characters"));
        }
        Ok(wrap(rpc(&actor, "artifacts.list", json!({ "query": query })).await?))
    }

    #[tool(
        description = "Get one dataset or model artifact by id: revision, size, owner, provenance and \
                        its file manifest. Read-only. Another tenant's or unshared artifact reports as \
                        not found."
    )]
    async fn data_artifact_get(
        &self,
        Parameters(args): Parameters<ArtifactGetArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let key = ident(&args.artifact_id, "artifact_id")?;
        Ok(wrap(rpc(&actor, "artifacts.get", json!({ "key": key })).await?))
    }

    #[tool(
        description = "List the immutable revisions of one dataset or model by exact name, oldest \
                        first (atlas_data has no revisions call, so this filters its artifact list). \
                        Read-only."
    )]
    async fn data_artifact_revisions(
        &self,
        Parameters(args): Parameters<RevisionsArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let name = ident(&args.name, "name")?;
        if let Some(k) = args.kind.as_deref() {
            if !matches!(k, "dataset" | "model") {
                return Err(bad("kind must be dataset or model"));
            }
        }
        let listed = rpc(&actor, "artifacts.list", json!({ "query": name })).await?;
        let mut revisions: Vec<Value> = listed
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|x| x.get("name").and_then(Value::as_str) == Some(name.as_str()))
                    .filter(|x| args.kind.as_deref().is_none_or(|k| x.get("kind").and_then(Value::as_str) == Some(k)))
                    .map(|x| {
                        let mut x = x.clone();
                        if let Some(o) = x.as_object_mut() {
                            o.remove("matches");
                        }
                        x
                    })
                    .collect()
            })
            .unwrap_or_default();
        revisions.sort_by(|a, b| {
            let t = |v: &Value| v.get("created_at").and_then(Value::as_f64).unwrap_or(0.0);
            t(a).total_cmp(&t(b))
        });
        Ok(wrap(json!({ "name": name, "revisions": revisions })))
    }

    #[tool(
        description = "Data-ready status of a job's reservation in atlas_data: whether every artifact \
                        is staged on the chosen node (data_ready), which are still missing, the node \
                        and the reservation expiry. Read-only. Only the job's owner (per the mapped \
                        credential) can see it. atlas_data exposes no per-node status call."
    )]
    async fn data_job_ready(
        &self,
        Parameters(args): Parameters<JobArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let job = ident(&args.job, "job")?;
        Ok(wrap(rpc(&actor, "jobs.status", json!({ "job": job })).await?))
    }

    #[tool(
        description = "Review one atlas_data workspace (a manifest overlay on a dataset revision): base \
                        artifact, generation, state, expiry, budget, the changed paths and changed \
                        bytes. Read-only; use its generation for the guarded workspace tools. atlas_data \
                        has no workspace listing call, so the id must be known."
    )]
    async fn data_workspace_review(
        &self,
        Parameters(args): Parameters<WorkspaceReviewArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let key = ident(&args.workspace_id, "workspace_id")?;
        Ok(wrap(rpc(&actor, "workspaces.review", json!({ "key": key })).await?))
    }

    #[tool(
        description = "Cost and usage summary for one job from atlas_data's ledger: units and micro-USD \
                        per meter. Read-only; the job owner (per the mapped credential) can read it."
    )]
    async fn data_cost_report(
        &self,
        Parameters(args): Parameters<JobArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        let job = ident(&args.job, "job")?;
        Ok(wrap(rpc(&actor, "costs.report", json!({ "job": job })).await?))
    }

    #[tool(
        description = "Estimate staging cost for 1-64 artifacts: per healthy node, bytes to transfer, \
                        staging seconds and estimated transfer micro-USD. Read-only, reserves nothing. \
                        atlas_data requires an operator-role credential for this call."
    )]
    async fn data_cost_estimate(
        &self,
        Parameters(args): Parameters<CostEstimateArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        if args.artifacts.is_empty() || args.artifacts.len() > 64 {
            return Err(bad("give 1 to 64 artifact ids"));
        }
        let ids = args.artifacts.iter().map(|a| ident(a, "artifact id")).collect::<Result<Vec<_>, _>>()?;
        Ok(wrap(rpc(&actor, "costs.estimate", json!({ "artifacts": ids })).await?))
    }
}

#[tool_router(router = data_write_router, vis = "pub(super)")]
impl AtlasMcp {
    #[tool(
        description = "Set or delete one path in an atlas_data workspace overlay (the base dataset \
                        revision is immutable and is never modified). The new entry's chunks must \
                        already exist in atlas_data. Two-phase: call without confirmation_token to \
                        receive the plan and a 5-minute single-use token, then call again with the same \
                        arguments plus that token to execute. Requires an operator-or-higher token and \
                        an operator-role atlas_data credential."
    )]
    async fn data_workspace_change(
        &self,
        Parameters(args): Parameters<WorkspaceChangeArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        write_guard(&self.state, &actor)?;
        let key = ident(&args.workspace_id, "workspace_id")?;
        let generation = generation(args.generation)?;
        let path = logical_path(&args.path)?;
        let entry = match (&args.entry, args.delete.unwrap_or(false)) {
            (Some(e), false) if e.is_object() && e.to_string().len() <= MAX_ENTRY_BYTES => e.clone(),
            (None, true) => Value::Null,
            _ => return Err(bad("give either entry (an object under 64 KiB) or delete=true, not both")),
        };
        let params = json!({ "workspace_id": key, "generation": generation, "path": path, "entry": entry });
        let call = json!({ "key": key, "generation": generation, "name": path, "entry": entry });
        self.data_write(&actor, WORKSPACE_CHANGE, "data_workspace", &key, &params, "workspaces.change", call,
            args.confirmation_token.as_deref(), Some(("workspaces.review", json!({ "key": key }), false)))
            .await
    }

    #[tool(
        description = "Publish selected changed paths of an atlas_data workspace as a NEW immutable \
                        dataset revision (the base revision is never modified); the new dataset is owned \
                        by the mapped atlas_data identity and readable by it only. Two-phase: call \
                        without confirmation_token to receive the plan and a 5-minute single-use token, \
                        then call again with the same arguments plus that token to execute. Requires an \
                        operator-or-higher token."
    )]
    async fn data_workspace_publish(
        &self,
        Parameters(args): Parameters<WorkspacePublishArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        write_guard(&self.state, &actor)?;
        let key = ident(&args.workspace_id, "workspace_id")?;
        let generation = generation(args.generation)?;
        let name = ident(&args.name, "name")?;
        let revision = ident(&args.revision, "revision")?;
        if args.paths.is_empty() || args.paths.len() > 1000 {
            return Err(bad("give 1 to 1000 paths to publish"));
        }
        let paths = args.paths.iter().map(|p| logical_path(p)).collect::<Result<Vec<_>, _>>()?;
        let params = json!({ "workspace_id": key, "generation": generation, "name": name, "revision": revision, "paths": paths });
        let call = json!({ "key": key, "generation": generation, "name": name, "revision": revision, "selected": paths });
        self.data_write(&actor, WORKSPACE_PUBLISH, "data_workspace", &key, &params, "workspaces.publish", call,
            args.confirmation_token.as_deref(), Some(("workspaces.review", json!({ "key": key }), false)))
            .await
    }

    #[tool(
        description = "Discard an atlas_data workspace: its overlay is dropped and it is closed. The base \
                        revision is not touched. Two-phase: call without confirmation_token to receive \
                        the plan and a 5-minute single-use token, then call again with the same arguments \
                        plus that token to execute. Requires an operator-or-higher token."
    )]
    async fn data_workspace_discard(
        &self,
        Parameters(args): Parameters<WorkspaceDiscardArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        write_guard(&self.state, &actor)?;
        let key = ident(&args.workspace_id, "workspace_id")?;
        let params = json!({ "workspace_id": key });
        self.data_write(&actor, WORKSPACE_DISCARD, "data_workspace", &key, &params, "workspaces.discard",
            json!({ "key": key }), args.confirmation_token.as_deref(),
            Some(("workspaces.review", json!({ "key": key }), false)))
            .await
    }

    #[tool(
        description = "Release a job's data reservation in atlas_data (frees the capacity it holds on \
                        its node; the artifacts stay). Two-phase: call without confirmation_token to \
                        receive the plan and a 5-minute single-use token, then call again with the same \
                        arguments plus that token to execute. Requires an operator-or-higher token; only \
                        the reservation's owner (per the mapped credential) can release it."
    )]
    async fn data_job_release(
        &self,
        Parameters(args): Parameters<JobReleaseArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = actor_from(&context)?;
        write_guard(&self.state, &actor)?;
        let job = ident(&args.job, "job")?;
        let params = json!({ "job": job });
        self.data_write(&actor, JOB_RELEASE, "data_job", &job, &params, "jobs.release", json!({ "job": job }),
            args.confirmation_token.as_deref(), Some(("jobs.status", json!({ "job": job }), true)))
            .await
    }
}

impl AtlasMcp {
    /// Shared two-phase body of the four data write tools. `lookup` is `(action, args, is_job)`, called
    /// only while planning, so the plan shows current upstream state and an unmapped, unauthorized or
    /// unknown target is rejected before any token is handed out.
    #[allow(clippy::too_many_arguments)]
    async fn data_write(
        &self,
        actor: &Actor,
        tool: &'static str,
        resource_type: &str,
        resource_id: &str,
        params: &Value,
        action: &str,
        call: Value,
        token: Option<&str>,
        lookup: Option<(&str, Value, bool)>,
    ) -> Result<ToolJson<Value>, McpError> {
        let s = &self.state;
        match gate(s, &actor.id, tool, &params.to_string(), token).await {
            Ok(Gate::Propose(issued)) => {
                let current = match lookup {
                    Some((a, args, is_job)) => match rpc(actor, a, args).await {
                        Ok(v) => Some(if is_job { job_summary(&v) } else { workspace_summary(&v) }),
                        Err(e) => {
                            audit(s, actor, tool, resource_type, resource_id, "rejected",
                                json!({ "via": "mcp", "reason": e.message })).await;
                            return Err(e);
                        }
                    },
                    None => None,
                };
                audit(s, actor, tool, resource_type, resource_id, "proposed", json!({ "via": "mcp", "confirmed": false })).await;
                Ok(ToolJson(json!({
                    "executed": false,
                    "plan": { "action": tool, "atlas_data_action": action, "arguments": params, "current": current },
                    "confirmation_token": issued,
                    "expires_in_secs": TOKEN_TTL_SECS,
                    "next": format!("call {tool} again with the same arguments plus confirmation_token"),
                })))
            }
            Ok(Gate::Execute) => {
                let result = rpc(actor, action, call).await;
                match &result {
                    Ok(_) => audit(s, actor, tool, resource_type, resource_id, "success", json!({ "via": "mcp", "confirmed": true })).await,
                    Err(e) => audit(s, actor, tool, resource_type, resource_id, "failure",
                        json!({ "via": "mcp", "confirmed": true, "error": e.message })).await,
                }
                Ok(ToolJson(json!({ "executed": true, "result": wrap(result?).0 })))
            }
            Err(why) => {
                audit(s, actor, tool, resource_type, resource_id, "rejected", json!({ "via": "mcp", "reason": why })).await;
                Err(bad(why))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_paths_follow_atlas_data_rules() {
        assert!(ident("job-1.a_b", "x").is_ok());
        for bad_id in ["", "-x", "a/b", "a b", &"a".repeat(129)] {
            assert!(ident(bad_id, "x").is_err(), "{bad_id}");
        }
        assert!(logical_path("a/b.txt").is_ok());
        for bad_path in ["", "/a", "a//b", "a/../b", "./a", "a\\b", "a\0b"] {
            assert!(logical_path(bad_path).is_err(), "{bad_path:?}");
        }
        assert!(generation(0).is_ok() && generation(-1).is_err());
    }

    #[test]
    fn scrub_removes_the_credential_and_control_characters() {
        let out = scrub("bad s3cr3t-token\nline", "s3cr3t-token");
        assert_eq!(out, "bad [redacted]line");
        assert!(scrub(&"x".repeat(500), "t").len() <= MAX_ERROR_CHARS);
    }

    #[test]
    fn upstream_failures_map_to_fixed_messages() {
        let e = map_status(500, br#"{"error":"/srv/secret/path tok"}"#, "tok");
        assert!(!e.message.contains("secret") && !e.message.contains("tok"), "{e:?}");
        assert!(map_status(403, b"{}", "tok").message.contains("refused"));
        assert!(map_status(404, b"{}", "tok").message.contains("not found"));
        let e = map_status(409, br#"{"error":"conflict with tok"}"#, "tok");
        assert!(e.message.contains("[redacted]") && !e.message.contains("with tok"), "{e:?}");
    }
}
