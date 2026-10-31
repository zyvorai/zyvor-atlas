// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! More read-only MCP tools: Ceph status, DataBridge, policies and the audit trail.
//!
//! Every tool is stricter than (or equal to) its REST route: an explicit operator check where the REST
//! handler takes no actor, tenant filtering wherever the rows carry a `tenant_id`, bounded output, and
//! for the audit trail only the fields an agent needs (the stored request/result bodies are dropped).

use axum::extract::State;
use rmcp::{
    handler::server::wrapper::{Json as ToolJson, Parameters},
    model::ErrorData as McpError,
    service::{RequestContext, RoleServer},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{actor_from, internal_error, rest, AtlasMcp};
use crate::auth::{require_role, tenant_scope, ROLE_OPERATOR};
use crate::routes::{databridge, governance, inventory};

/// Largest response a tool will return. Anything bigger is a sign the REST API is the right tool.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_OSD_LISTED: usize = 50;
const AUDIT_DEFAULT_LIMIT: i64 = 50;
const AUDIT_MAX_LIMIT: i64 = 200;

pub(super) fn bounded(v: Value) -> Result<Value, McpError> {
    if v.to_string().len() > MAX_RESPONSE_BYTES {
        return Err(McpError::internal_error(
            "response too large for MCP; use the REST API for the full document",
            None,
        ));
    }
    Ok(v)
}

/// Keep only the rows of `tenant` when the caller is tenant-scoped (`scope` is `Some`); unscoped
/// callers (admin, or auth disabled) see every row. Rows without a `tenant_id` are dropped for scoped
/// callers rather than assumed to be theirs.
pub(super) fn scope_by_tenant(rows: Value, scope: Option<&str>) -> Value {
    let (Some(tenant), Value::Array(items)) = (scope, &rows) else {
        return rows;
    };
    Value::Array(
        items
            .iter()
            .filter(|r| r.get("tenant_id").and_then(Value::as_str) == Some(tenant))
            .cloned()
            .collect(),
    )
}

/// Compact view of `ceph osd tree`: counts by node type and the OSDs that are not serving
/// (`status != up` or `reweight == 0`), capped. A large cluster's full tree can be megabytes.
pub(super) fn summarise_osd_tree(tree: &Value) -> Value {
    let Some(nodes) = tree.get("nodes").and_then(Value::as_array) else {
        return json!({ "status": "unavailable", "reason": "unrecognised osd tree shape" });
    };
    let mut by_type = serde_json::Map::new();
    let mut not_serving = Vec::new();
    let mut osds = 0u64;
    for n in nodes {
        let ty = n.get("type").and_then(Value::as_str).unwrap_or("unknown");
        let c = by_type.entry(ty.to_string()).or_insert(json!(0));
        *c = json!(c.as_u64().unwrap_or(0) + 1);
        if ty == "osd" {
            osds += 1;
            let down = n.get("status").and_then(Value::as_str).is_some_and(|s| s != "up");
            let out = n.get("reweight").and_then(Value::as_f64) == Some(0.0);
            if down || out {
                not_serving.push(json!({
                    "name": n.get("name"), "status": n.get("status"), "reweight": n.get("reweight")
                }));
            }
        }
    }
    let not_serving_total = not_serving.len();
    not_serving.truncate(MAX_OSD_LISTED);
    json!({
        "status": "ok",
        "nodes_by_type": by_type,
        "osds": osds,
        "osds_not_serving": not_serving_total,
        "not_serving": not_serving,
        "truncated": not_serving_total > MAX_OSD_LISTED,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListAuditArgs {
    /// Only entries by this actor id.
    #[serde(default)]
    actor: Option<String>,
    /// Only entries with this action, e.g. `mcp.create_snapshot`.
    #[serde(default)]
    action: Option<String>,
    /// Only entries about this resource type.
    #[serde(default)]
    resource_type: Option<String>,
    /// Only entries about this resource id.
    #[serde(default)]
    resource_id: Option<String>,
    /// Most recent N entries (default 50, at most 200).
    #[serde(default)]
    limit: Option<i64>,
}

#[tool_router(router = ops_router, vis = "pub(super)")]
impl AtlasMcp {
    fn operator(&self, context: &RequestContext<RoleServer>) -> Result<crate::auth::Actor, McpError> {
        let actor = actor_from(context)?;
        require_role(self.state.config.auth_required, &actor, ROLE_OPERATOR)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
        Ok(actor)
    }

    #[tool(
        description = "Live `ceph status`: health, quorum, OSD map, PG map and client I/O. Read-only. \
                        Requires an operator-or-higher token."
    )]
    async fn ceph_status(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(inventory::get_ceph_status(State(self.state.clone())).await)?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Wear and health of the NVMe drives Ceph tracks (percentage used, media errors, \
                        spare, temperature) from its device-health SMART scrapes. Read-only. \
                        Requires an operator-or-higher token."
    )]
    async fn ceph_nvme(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(
            crate::routes::ceph_nvme::get_ceph_nvme(State(self.state.clone())).await,
        )?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Rook NVMe-oF gateways (CephNVMeOFGateway): instances, pod readiness, high-availability \
                        findings, and the NVMe-oF StorageClasses. Reports `installed: false` when the cluster \
                        has no such CRD, and fails when no Kubernetes cluster is attached. Read-only. \
                        Requires an operator-or-higher token."
    )]
    async fn ceph_nvmeof(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(
            crate::routes::ceph_nvmeof::get_ceph_nvmeof(
                State(self.state.clone()),
                axum::extract::Query(Default::default()),
            )
            .await,
        )?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Cluster-wide compression and sharing savings of the atlas-native backend \
                        (logical, referenced and stored bytes with the derived ratios). Read-only; \
                        fails when the atlas-native driver is not enabled. Requires an \
                        operator-or-higher token."
    )]
    async fn native_reduction(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = self.operator(&context)?;
        let ToolJson(v) = rest(
            crate::routes::reduction::native_reduction(
                State(self.state.clone()),
                axum::Extension(actor),
            )
            .await,
        )?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Ransomware assessment of one atlas-native filesystem volume from the node's \
                        recorded activity windows (writes, renames, deletes, write entropy): a \
                        verdict with the evidence behind it. Read-only: it never snapshots or locks \
                        anything (that is an admin REST call). Limited to the caller's tenant \
                        unless admin. Requires an operator-or-higher token."
    )]
    async fn volume_ransomware(
        &self,
        Parameters(args): Parameters<super::IdArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = self.operator(&context)?;
        let result = crate::routes::ransomware::volume_ransomware(
            State(self.state.clone()),
            axum::Extension(actor),
            axum::extract::Path(args.id),
        )
        .await
        .and_then(|axum::Json(a)| {
            serde_json::to_value(a)
                .map(axum::Json)
                .map_err(|e| atlas_common::AppError::Internal(e.to_string()))
        });
        let ToolJson(v) = rest(result)?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Each OSD's reported object store and Ceph version (`ceph osd metadata`), with counts \
                        and a mixed-versions flag. Read-only. Requires an operator-or-higher token."
    )]
    async fn ceph_osd_objectstore(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(
            crate::routes::ceph_objectstore::get_ceph_osd_objectstore(State(self.state.clone())).await,
        )?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Whether hosts with NVMe drives have the CPU and RAM of the sizing rule of thumb \
                        (default 10 cores per NVMe device, 16 GiB plus 5 GiB per OSD), against Kubernetes \
                        node capacity; hosts it cannot match are `unknown`. Heuristic, read-only. \
                        Requires an operator-or-higher token."
    )]
    async fn ceph_sizing(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(
            crate::routes::ceph_sizing::get_ceph_sizing(State(self.state.clone())).await,
        )?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Cluster and per-pool capacity, usage and object counts (`ceph df`). Read-only. \
                        Requires an operator-or-higher token."
    )]
    async fn ceph_df(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(inventory::get_ceph_df(State(self.state.clone())).await)?;
        Ok(ToolJson(bounded(v)?))
    }

    #[tool(
        description = "Summary of the CRUSH tree: node counts by type and the OSDs that are down or \
                        out (at most 50 listed). Read-only. Requires an operator-or-higher token."
    )]
    async fn ceph_osd_tree(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.operator(&context)?;
        let ToolJson(v) = rest(inventory::get_ceph_osd_tree(State(self.state.clone())).await)?;
        Ok(ToolJson(summarise_osd_tree(&v)))
    }

    #[tool(
        description = "DataBridge migration plans (state, readiness score, cutover time), limited to \
                        the caller's tenant unless admin. Read-only. Requires an operator-or-higher token."
    )]
    async fn databridge_plans(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = self.operator(&context)?;
        let ToolJson(v) = rest(databridge::db_list_plans(State(self.state.clone())).await)?;
        let scope = tenant_scope(self.state.config.auth_required, &actor);
        Ok(ToolJson(bounded(scope_by_tenant(v, scope))?))
    }

    #[tool(
        description = "DataBridge CDC streams (state, lag, events), limited to the caller's tenant \
                        unless admin. Read-only. Requires an operator-or-higher token."
    )]
    async fn databridge_cdc_streams(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = self.operator(&context)?;
        let ToolJson(v) = rest(databridge::db_list_cdc_streams(State(self.state.clone())).await)?;
        let scope = tenant_scope(self.state.config.auth_required, &actor);
        Ok(ToolJson(bounded(scope_by_tenant(v, scope))?))
    }

    #[tool(
        description = "The provisioning policy catalogue: intent, storage class, access mode, volume \
                        mode. Static. Read-only."
    )]
    async fn list_policies(&self) -> Result<ToolJson<Value>, McpError> {
        let axum::Json(v) = governance::list_policies().await;
        Ok(ToolJson(v))
    }

    #[tool(
        description = "Recent audit-trail entries (who did what to which resource), newest first, \
                        limited to the caller's tenant unless admin. Request and result bodies are \
                        not included. Read-only. Requires an operator-or-higher token."
    )]
    async fn list_audit(
        &self,
        Parameters(args): Parameters<ListAuditArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<ToolJson<Value>, McpError> {
        let actor = self.operator(&context)?;
        let scope = tenant_scope(self.state.config.auth_required, &actor).map(str::to_owned);
        let limit = args.limit.unwrap_or(AUDIT_DEFAULT_LIMIT).clamp(1, AUDIT_MAX_LIMIT);
        // A tenant-scoped caller's rows are filtered after the read, so fetch the maximum and trim.
        let fetch = if scope.is_some() { AUDIT_MAX_LIMIT * 5 } else { limit };
        let rows = atlas_inventory::audit::list(
            &self.state.pool,
            args.actor.as_deref(),
            args.action.as_deref(),
            args.resource_type.as_deref(),
            args.resource_id.as_deref(),
            fetch,
        )
        .await
        .map_err(internal_error)?;
        let rows = scope_by_tenant(json!(rows), scope.as_deref());
        let trimmed: Vec<Value> = rows
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit as usize)
            .map(|r| {
                let pick = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
                json!({
                    "created_at": pick("created_at"), "tenant_id": pick("tenant_id"),
                    "actor_id": pick("actor_id"), "action": pick("action"),
                    "resource_type": pick("resource_type"), "resource_id": pick("resource_id"),
                    "status": pick("status"),
                })
            })
            .collect();
        Ok(ToolJson(bounded(json!(trimmed))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_filter_keeps_only_own_rows_and_drops_untagged() {
        let rows = json!([
            {"id": 1, "tenant_id": "acme"}, {"id": 2, "tenant_id": "globex"}, {"id": 3}
        ]);
        let mine = scope_by_tenant(rows.clone(), Some("acme"));
        assert_eq!(mine, json!([{"id": 1, "tenant_id": "acme"}]));
        assert_eq!(scope_by_tenant(rows.clone(), None), rows);
    }

    #[test]
    fn osd_tree_summary_lists_only_what_is_not_serving() {
        let tree = json!({"nodes": [
            {"id": -1, "type": "root", "name": "default"},
            {"id": 0, "type": "osd", "name": "osd.0", "status": "up", "reweight": 1.0},
            {"id": 1, "type": "osd", "name": "osd.1", "status": "down", "reweight": 0.0},
            {"id": 2, "type": "osd", "name": "osd.2", "status": "up", "reweight": 0.0},
        ]});
        let s = summarise_osd_tree(&tree);
        assert_eq!(s["osds"], 3);
        assert_eq!(s["osds_not_serving"], 2);
        assert_eq!(s["nodes_by_type"]["root"], 1);
        assert_eq!(summarise_osd_tree(&json!({"x": 1}))["status"], "unavailable");
    }

    #[test]
    fn osd_list_is_capped_and_flagged() {
        let nodes: Vec<Value> = (0..80)
            .map(|i| json!({"type": "osd", "name": format!("osd.{i}"), "status": "down"}))
            .collect();
        let s = summarise_osd_tree(&json!({ "nodes": nodes }));
        assert_eq!(s["not_serving"].as_array().unwrap().len(), MAX_OSD_LISTED);
        assert_eq!(s["osds_not_serving"], 80);
        assert_eq!(s["truncated"], true);
    }

    #[test]
    fn oversized_responses_are_refused() {
        let big = json!({"x": "a".repeat(MAX_RESPONSE_BYTES + 1)});
        assert!(bounded(big).is_err());
        assert!(bounded(json!({"x": 1})).is_ok());
    }
}
