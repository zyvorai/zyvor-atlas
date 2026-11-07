// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Shared harness for the atlas_data MCP tool tests: a gateway on an ephemeral port, MCP client
//! helpers and a fake atlas_data HTTP server that mimics its per-credential tenant isolation.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use atlas_gateway::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use rmcp::model::CallToolRequestParams;
use serde_json::{json, Value};

use crate::common;

pub const SECRET: &str = "data-test-secret-at-least-32-bytes!!!!";
pub const TOK_A: &str = "tok-tenant-a-0000000000000000000000000000";
pub const TOK_B: &str = "tok-tenant-b-0000000000000000000000000000";
pub const ART_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const ART_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub async fn spawn() -> (SocketAddr, AppState) {
    let database_url = common::fresh_database_url("mcp_data").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: SECRET.into(),
        jwt_secret_previous: None,
        auth_required: true,
        bootstrap_admin_token: None,
        admin_username: "admin".into(),
        admin_password: "Admin@321".into(),
        monitor_interval_secs: 0,
        ceph_prometheus_url: None,
        alert_webhook_url: None,
        backup_keep: 0,
        backup_max_age_secs: 0,
        rgw_public_endpoint: None,
        snapshot_tick_secs: 0,
        databridge_reconcile_secs: 0,
        job_poll_secs: 0,
        job_stale_secs: 0,
        https_addr: None,
        tls_cert_path: None,
        tls_key_path: None,
        tls_self_signed: false,
        disable_http: false,
        nfs_enable: false,
        nfs_server: None,
        nfs_exports: Vec::new(),
        nfs_driver_mode: atlas_common::config::DriverMode::Fake,
        zfs_enable: false,
        zfs_host: None,
        zfs_pools: Vec::new(),
        zfs_driver_mode: atlas_common::config::DriverMode::Fake,
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let state = build_state(
        config,
        BuildOptions {
            enable_k8s: false,
            initial_discovery: true,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state");
    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}


pub fn mint(subject: &str, role: &str, tenant: &str, tools: &[&str]) -> String {
    let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
    atlas_gateway::auth::mint_scoped_token(SECRET, subject, role, tenant, 3600, &tools)
        .unwrap()
        .0
}

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

pub async fn connect(addr: SocketAddr, token: &str) -> Client {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;
    let http_client = reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
            headers
        })
        .build()
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(format!(
            "http://{addr}/api/atlas/v1/mcp"
        )),
    );
    ().serve(transport).await.unwrap()
}

pub async fn tool_names(c: &Client) -> Vec<String> {
    c.list_tools(Default::default()).await.unwrap().tools.iter().map(|t| t.name.to_string()).collect()
}

/// Ok(parsed JSON) or Err(error text) for either a protocol error or an `is_error` result.
pub async fn call(c: &Client, tool: &'static str, v: Value) -> Result<Value, String> {
    match c.call_tool(CallToolRequestParams::new(tool).with_arguments(v.as_object().unwrap().clone())).await {
        Err(e) => Err(e.to_string()),
        Ok(r) => {
            let text = r.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
            if r.is_error == Some(true) {
                Err(text)
            } else {
                Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
            }
        }
    }
}

/// One recorded request to the fake atlas_data.
#[derive(Clone, Debug)]
pub struct Seen {
    pub action: String,
    pub bearer: String,
    pub body: Value,
}

#[derive(Default)]
pub struct Fake {
    pub seen: Mutex<Vec<Seen>>,
}

impl Fake {
    pub fn count(&self, action: &str) -> usize {
        self.seen.lock().unwrap().iter().filter(|s| s.action == action).count()
    }
}

fn tenant_of(headers: &HeaderMap) -> Option<&'static str> {
    match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) if h == format!("Bearer {TOK_A}") => Some("a"),
        Some(h) if h == format!("Bearer {TOK_B}") => Some("b"),
        _ => None,
    }
}

async fn rpc(
    State(fake): State<Arc<Fake>>,
    Path(action): Path<String>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    fake.seen.lock().unwrap().push(Seen { action: action.clone(), bearer, body: body.clone() });
    let Some(tenant) = tenant_of(&headers) else {
        return (StatusCode::UNAUTHORIZED, json!({"error": "invalid bearer token"}).to_string());
    };
    let ok = |v: Value| (StatusCode::OK, v.to_string());
    let missing = || (StatusCode::NOT_FOUND, json!({"error": "resource not found"}).to_string());
    let own_art = if tenant == "a" { ART_A } else { ART_B };
    let own_ws = if tenant == "a" { "ws1" } else { "ws2" };
    match action.as_str() {
        "artifacts.list" => match body["query"].as_str().unwrap_or("") {
            "huge" => ok(json!({"pad": "x".repeat(400 * 1024)})),
            "boom" => (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("/srv/secret/path {TOK_A}")}).to_string()),
            "echo" => (StatusCode::CONFLICT, json!({"error": format!("conflict {TOK_A} here")}).to_string()),
            "slow" => {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                ok(json!([]))
            }
            "ds" => ok(json!([
                {"id": "r2", "kind": "dataset", "name": "ds", "revision": "v2", "created_at": 20.0, "matches": []},
                {"id": "r1", "kind": "dataset", "name": "ds", "revision": "v1", "created_at": 10.0, "matches": ["f"]},
                {"id": "o", "kind": "model", "name": "ds-other", "revision": "v1", "created_at": 5.0, "matches": []},
            ])),
            _ => ok(json!([{"id": own_art, "kind": "dataset", "name": format!("data-{tenant}"), "revision": "v1"}])),
        },
        "artifacts.get" if body["key"] == own_art => ok(json!({"id": own_art, "name": format!("data-{tenant}"), "files": {}})),
        "jobs.status" if body["job"] == format!("job-{tenant}") => ok(json!({
            "id": body["job"], "node": "n1", "artifacts": [own_art], "data_ready": false, "missing": [own_art], "expires_at": 1.0
        })),
        "costs.report" if body["job"] == format!("job-{tenant}") => ok(json!({"job": body["job"], "micro_usd": 7, "meters": []})),
        "workspaces.review" if body["key"] == own_ws => ok(json!({
            "id": own_ws, "base": own_art, "generation": 3, "state": "open", "expires_at": 9.0, "budget": 100,
            "overlay": {"a.txt": {"size": 1}}, "changed_bytes": 1
        })),
        "workspaces.discard" if body["key"] == own_ws => ok(json!({"state": "discarded"})),
        "workspaces.change" if body["key"] == own_ws => ok(json!({"generation": 4})),
        "workspaces.publish" if body["key"] == own_ws => ok(json!({"id": "new", "kind": "dataset"})),
        "jobs.release" if body["job"] == format!("job-{tenant}") => ok(json!({"released": true})),
        "costs.estimate" => ok(json!({"placements": [], "compute_cost_included": false})),
        _ => missing(),
    }
}

/// Fake atlas_data on an ephemeral port. Token A is tenant a, token B is tenant b; anything else 401.
pub async fn fake_atlas_data() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new().route("/v1/rpc/{action}", post(rpc)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, fake)
}

pub fn tokens_env() -> String {
    json!({ "tenant:tenant-a": TOK_A, "tenant:tenant-b": TOK_B }).to_string()
}
