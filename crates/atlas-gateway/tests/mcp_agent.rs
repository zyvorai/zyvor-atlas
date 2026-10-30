// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent-safety scenarios for the MCP edge: a scoped agent token (`Claims::tools`) sees and can
//! call only its listed tools, never escalates past its role, and is refused on every non-MCP
//! route. Scripted tool-call scenarios over the real wire protocol, no model involved.
#![cfg(feature = "mcp")]

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};

mod common;

async fn spawn(auth_required: bool, secret: &str) -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("mcp_agent").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: secret.into(),
        jwt_secret_previous: None,
        auth_required,
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
    .unwrap();
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, pool)
}


const SECRET: &str = "mcp-agent-test-secret-at-least-32-bytes!!";

async fn connect_token(
    addr: SocketAddr,
    token: &str,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, Box<dyn std::error::Error>> {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;
    let http_client = reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            headers
        })
        .build()
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            format!("http://{addr}/api/atlas/v1/mcp"),
        ),
    );
    Ok(().serve(transport).await?)
}

fn scoped(role: &str, tools: &[&str]) -> String {
    let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
    atlas_gateway::auth::mint_scoped_token(SECRET, "agent-x", role, "global", 3600, &tools)
        .unwrap()
        .0
}

async fn denied(
    c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
) -> bool {
    use rmcp::model::CallToolRequestParams;
    match c.call_tool(CallToolRequestParams::new(tool.to_string())).await {
        Err(_) => true,
        Ok(r) => r.is_error == Some(true),
    }
}

#[tokio::test]
async fn a_scoped_token_lists_and_calls_only_its_tools() {
    use rmcp::model::CallToolRequestParams;
    let (addr, _pool) = spawn(true, SECRET).await;
    let c = connect_token(addr, &scoped("operator", &["list_clusters", "ceph_nvme"]))
        .await
        .unwrap();
    let tools = c.list_tools(Default::default()).await.unwrap();
    let mut names: Vec<_> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["ceph_nvme", "list_clusters"]);

    let ok = c.call_tool(CallToolRequestParams::new("list_clusters")).await.unwrap();
    assert_eq!(ok.is_error, Some(false));
    // Tools outside the scope are refused even though the operator role would allow them.
    for t in ["ops_advisor", "list_volumes", "ceph_status", "create_snapshot", "ack_alert"] {
        assert!(denied(&c, t).await, "{t} must be refused outside the scope");
    }
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn a_scope_never_widens_the_role() {
    // A viewer scoped to an operator-only tool is still refused: scope only narrows.
    let (addr, _pool) = spawn(true, SECRET).await;
    let c = connect_token(addr, &scoped("viewer", &["ceph_nvme"])).await.unwrap();
    assert!(denied(&c, "ceph_nvme").await);
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn an_unscoped_token_is_unchanged() {
    let (addr, _pool) = spawn(true, SECRET).await;
    let (token, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "agent", "operator", "global", 3600).unwrap();
    let c = connect_token(addr, &token).await.unwrap();
    let n = c.list_tools(Default::default()).await.unwrap().tools.len();
    assert!(n > 20, "an unscoped operator sees the full tool set, saw {n}");
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn a_scoped_token_is_refused_outside_mcp() {
    let (addr, _pool) = spawn(true, SECRET).await;
    let token = scoped("admin", &["list_clusters"]);
    let http = reqwest::Client::new();
    for path in ["/api/atlas/v1/clusters", "/api/atlas/v1/volumes", "/metrics"] {
        let r = http
            .get(format!("http://{addr}{path}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN, "{path}");
    }
    // The same role without a scope still works on REST, so the refusal is the scope's doing.
    let (plain, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "agent-x", "admin", "global", 3600).unwrap();
    let r = http
        .get(format!("http://{addr}/api/atlas/v1/clusters"))
        .bearer_auth(plain)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
}

#[tokio::test]
async fn an_admin_can_issue_a_scoped_token_and_bad_scopes_are_rejected() {
    let (addr, _pool) = spawn(true, SECRET).await;
    let (admin, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "root", "admin", "global", 3600).unwrap();
    let http = reqwest::Client::new();
    let issue = |tools: serde_json::Value| {
        http.post(format!("http://{addr}/api/atlas/v1/auth/tokens"))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "subject": "agent-y", "role": "operator", "tools": tools }))
            .send()
    };
    let ok = issue(serde_json::json!(["list_clusters", "list_clusters"])).await.unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::CREATED);
    let body: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(body["tools"], serde_json::json!(["list_clusters"]), "duplicates dropped");
    let c = connect_token(addr, body["token"].as_str().unwrap()).await.unwrap();
    assert_eq!(c.list_tools(Default::default()).await.unwrap().tools.len(), 1);
    c.cancel().await.unwrap();

    for bad in [serde_json::json!(["Not A Tool"]), serde_json::json!([""])] {
        let r = issue(bad).await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}
