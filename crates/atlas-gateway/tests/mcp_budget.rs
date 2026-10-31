// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Per-token MCP tool-call budget (`ATLAS_MCP_CALLS_PER_MIN`). Its own test binary: the budget is read once per
//! process, so the env var must not leak into other tests. Each token has its own budget; going over it is refused
//! and audited, and does not touch another token.
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

async fn call(
    c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Result<rmcp::model::CallToolResult, rmcp::service::ServiceError> {
    c.call_tool(rmcp::model::CallToolRequestParams::new("list_clusters")).await
}

#[tokio::test]
async fn each_token_has_its_own_call_budget() {
    std::env::set_var("ATLAS_MCP_CALLS_PER_MIN", "3");
    let (addr, pool) = spawn(true, SECRET).await;
    let a = connect_token(addr, &scoped("operator", &["list_clusters"])).await.unwrap();
    let b = connect_token(addr, &scoped("operator", &["list_clusters"])).await.unwrap();

    let (mut ok, mut refused) = (0, 0);
    for _ in 0..6 {
        match call(&a).await {
            Ok(r) if r.is_error != Some(true) => ok += 1,
            _ => refused += 1,
        }
    }
    assert_eq!(ok, 3, "the first three calls fit the budget");
    assert_eq!(refused, 3, "the rest are refused");
    // Another token (own jti) is unaffected by the first one running out.
    assert!(call(&b).await.is_ok(), "a different token has its own budget");

    use sqlx::Row;
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM storage_audit_logs WHERE action = 'mcp.tool.rate_limited'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 3, "each refusal is audited");
}
