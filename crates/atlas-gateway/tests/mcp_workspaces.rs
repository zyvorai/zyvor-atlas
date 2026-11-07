// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent workspace MCP tools (`ATLAS_MCP_WRITE_ENABLE`): two-phase and single-use tokens, scoped-token
//! denial, tenant isolation and creator-only reset/discard. Own test binary: the write gate is a
//! process-wide environment variable. The workspace behaviour itself is covered in `workspaces.rs`.
#![cfg(feature = "mcp")]

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use atlas_gateway::state::AppState;
use rmcp::model::CallToolRequestParams;
use serde_json::{json, Value};

mod common;

const SECRET: &str = "workspace-test-secret-at-least-32-bytes!!";
const MIB: u64 = 1 << 20;

async fn spawn() -> (SocketAddr, AppState) {
    let database_url = common::fresh_database_url("mcp_workspaces").await;
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
    let driver = Arc::new(atlas_driver_native::FakeNativeDriver::new(NATIVE_BACKEND_ID));
    attach_native_driver(&state.pool, &state.drivers, driver)
        .await
        .unwrap();
    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

fn mint(subject: &str, role: &str, tenant: &str, tools: &[&str]) -> String {
    let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
    atlas_gateway::auth::mint_scoped_token(SECRET, subject, role, tenant, 3600, &tools)
        .unwrap()
        .0
}

async fn connect(addr: SocketAddr, token: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
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

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn call(c: &Client, tool: &'static str, v: Value) -> Result<Value, String> {
    match c
        .call_tool(CallToolRequestParams::new(tool).with_arguments(v.as_object().unwrap().clone()))
        .await
    {
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

/// Plan then execute with the token the plan returned.
async fn confirmed(c: &Client, tool: &'static str, mut args: Value) -> Result<Value, String> {
    let plan = call(c, tool, args.clone()).await?;
    assert_eq!(plan["executed"], false, "{plan}");
    args["confirmation_token"] = plan["confirmation_token"].clone();
    call(c, tool, args).await
}

async fn rest(addr: SocketAddr, tok: &str, m: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    let mut r = reqwest::Client::new()
        .request(m, format!("http://{addr}/api/atlas/v1{path}"))
        .bearer_auth(tok);
    if let Some(b) = body {
        r = r.json(&b);
    }
    r.send().await.unwrap().json().await.unwrap_or(Value::Null)
}

async fn workspaces(pool: &sqlx::AnyPool) -> usize {
    atlas_inventory::workspaces::list(pool, None).await.unwrap().len()
}

#[tokio::test]
async fn workspace_tools_are_guarded_two_phase_scoped_and_tenant_isolated() {
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let (addr, state) = spawn().await;
    let pool = state.pool.clone();
    let admin = mint("root", "admin", "global", &[]);
    let seed = |tenant: &'static str, name: &'static str| {
        let admin = admin.clone();
        async move {
            rest(addr, &admin, reqwest::Method::POST, "/volumes", Some(json!({
                "tenant_id": tenant, "name": name, "size_bytes": 8 * MIB,
                "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
            })))
            .await["volume_id"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let src_a = seed("tenant-a", "a-data").await;

    let agent = connect(addr, &mint("agent-a", "operator", "tenant-a", &[])).await;
    let names: Vec<_> = agent.list_tools(Default::default()).await.unwrap().tools.iter().map(|t| t.name.to_string()).collect();
    for want in ["list_workspaces", "get_workspace", "create_workspace", "reset_workspace", "discard_workspace"] {
        assert!(names.contains(&want.to_string()), "missing {want}: {names:?}");
    }

    // Phase 1 changes nothing and the token is bound to the exact arguments.
    let args = json!({ "source_volume_id": src_a, "name": "run-1", "ttl_secs": 600 });
    let plan = call(&agent, "create_workspace", args.clone()).await.unwrap();
    assert_eq!(plan["executed"], false);
    assert_eq!(plan["plan"]["writes_to_source"], false);
    let token = plan["confirmation_token"].as_str().unwrap().to_string();
    assert_eq!(workspaces(&pool).await, 0, "planning must not create anything");
    let mut other = args.clone();
    other["ttl_secs"] = json!(900);
    other["confirmation_token"] = json!(token);
    assert!(call(&agent, "create_workspace", other).await.is_err(), "token is bound to the arguments");
    assert_eq!(workspaces(&pool).await, 0);

    // Phase 2 executes once; replaying the token is refused and creates nothing more.
    let mut exec = args.clone();
    exec["confirmation_token"] = json!(token);
    let done = call(&agent, "create_workspace", exec.clone()).await.unwrap();
    assert_eq!(done["executed"], true, "{done}");
    let wid = done["result"]["id"].as_str().unwrap().to_string();
    let wvol = done["result"]["volume_id"].as_str().unwrap().to_string();
    assert_eq!(done["result"]["owner"], "agent-a");
    let replay = call(&agent, "create_workspace", exec).await.unwrap_err();
    assert!(replay.contains("already used"), "{replay}");
    assert_eq!(workspaces(&pool).await, 1, "replay must not create another");

    // The read tools show it.
    let listed = call(&agent, "list_workspaces", json!({})).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    let got = call(&agent, "get_workspace", json!({ "workspace_id": wid })).await.unwrap();
    assert_eq!(got["volume_id"], wvol.as_str());

    // Reset: two-phase, single-use (the second use is refused as already used), new clone.
    let plan = call(&agent, "reset_workspace", json!({ "workspace_id": wid })).await.unwrap();
    assert_eq!(plan["plan"]["deletes_everything_written_to_the_workspace"], true);
    let rtoken = plan["confirmation_token"].as_str().unwrap().to_string();
    let rargs = json!({ "workspace_id": wid, "confirmation_token": rtoken });
    let wrong = call(&agent, "reset_workspace", json!({ "workspace_id": "ws_other", "confirmation_token": rtoken })).await;
    assert!(wrong.is_err());
    let done = call(&agent, "reset_workspace", rargs.clone()).await.unwrap();
    assert_eq!(done["executed"], true, "{done}");
    assert_ne!(done["result"]["volume_id"], wvol.as_str(), "a reset is a fresh clone");
    assert_eq!(done["result"]["reset_count"], 1);
    let replay = call(&agent, "reset_workspace", rargs).await.unwrap_err();
    assert!(replay.contains("already used"), "{replay}");
    // A token for one tool does not open another.
    let plan = call(&agent, "reset_workspace", json!({ "workspace_id": wid })).await.unwrap();
    let cross = call(&agent, "discard_workspace", json!({ "workspace_id": wid, "confirmation_token": plan["confirmation_token"] })).await;
    assert!(cross.is_err());
    assert_eq!(workspaces(&pool).await, 1, "a cross-tool token discards nothing");

    // Another tenant's operator cannot create from A's volume nor see, reset or discard A's workspace.
    let intruder = connect(addr, &mint("agent-b", "operator", "tenant-b", &[])).await;
    assert!(call(&intruder, "create_workspace", json!({ "source_volume_id": src_a })).await.is_err());
    assert!(call(&intruder, "get_workspace", json!({ "workspace_id": wid })).await.is_err());
    assert!(call(&intruder, "reset_workspace", json!({ "workspace_id": wid })).await.is_err());
    assert!(call(&intruder, "discard_workspace", json!({ "workspace_id": wid })).await.is_err());
    assert_eq!(call(&intruder, "list_workspaces", json!({})).await.unwrap().as_array().unwrap().len(), 0);
    intruder.cancel().await.unwrap();

    // A viewer can read but cannot even get a plan; a teammate may not touch someone else's workspace,
    // an admin may.
    let viewer = connect(addr, &mint("watcher", "viewer", "tenant-a", &[])).await;
    assert_eq!(call(&viewer, "list_workspaces", json!({})).await.unwrap().as_array().unwrap().len(), 1);
    assert!(call(&viewer, "create_workspace", json!({ "source_volume_id": src_a })).await.is_err());
    assert!(call(&viewer, "discard_workspace", json!({ "workspace_id": wid })).await.is_err());
    viewer.cancel().await.unwrap();
    let mate = connect(addr, &mint("agent-a2", "operator", "tenant-a", &[])).await;
    let denied = call(&mate, "discard_workspace", json!({ "workspace_id": wid })).await.unwrap_err();
    assert!(denied.contains("created by another caller"), "{denied}");
    mate.cancel().await.unwrap();
    assert_eq!(workspaces(&pool).await, 1);

    // Scoped token: a list-only allowlist neither lists nor can call the write tools.
    let scoped = connect(addr, &mint("agent-s", "operator", "tenant-a", &["list_workspaces"])).await;
    let visible: Vec<_> = scoped.list_tools(Default::default()).await.unwrap().tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(visible, vec!["list_workspaces".to_string()]);
    for tool in ["create_workspace", "reset_workspace", "discard_workspace"] {
        let r = call(&scoped, tool, json!({ "workspace_id": wid, "source_volume_id": src_a })).await;
        assert!(r.is_err(), "{tool} must be denied to a token scoped to list_workspaces");
    }
    assert_eq!(call(&scoped, "list_workspaces", json!({})).await.unwrap().as_array().unwrap().len(), 1);
    scoped.cancel().await.unwrap();
    assert_eq!(workspaces(&pool).await, 1, "denied calls change nothing");
    let denied_rows = atlas_inventory::audit::list(&pool, None, Some("mcp.tool.denied"), None, None, 20).await.unwrap();
    assert!(denied_rows.len() >= 3, "{denied_rows:?}");
    // The scoped token is not valid on REST either.
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/api/atlas/v1/workspaces"))
        .bearer_auth(mint("agent-s", "operator", "tenant-a", &["create_workspace"]))
        .json(&json!({ "source_volume_id": src_a }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);

    // The creator discards with the two-phase flow; the source survives.
    let done = confirmed(&agent, "discard_workspace", json!({ "workspace_id": wid })).await.unwrap();
    assert_eq!(done["executed"], true, "{done}");
    assert_eq!(workspaces(&pool).await, 0);
    assert!(call(&agent, "get_workspace", json!({ "workspace_id": wid })).await.is_err());
    let src = rest(addr, &admin, reqwest::Method::GET, &format!("/volumes/{src_a}"), None).await;
    assert_eq!(src["id"], src_a.as_str());
    // An admin may discard another caller's workspace.
    let made = confirmed(&agent, "create_workspace", json!({ "source_volume_id": src_a })).await.unwrap();
    let wid2 = made["result"]["id"].as_str().unwrap().to_string();
    let root = connect(addr, &admin).await;
    let gone = confirmed(&root, "discard_workspace", json!({ "workspace_id": wid2 })).await.unwrap();
    assert_eq!(gone["executed"], true, "{gone}");
    root.cancel().await.unwrap();
    agent.cancel().await.unwrap();
    assert_eq!(workspaces(&pool).await, 0);

    // Every step that got past authorization is audited with its outcome.
    for (tool, want) in [
        ("mcp.create_workspace", vec!["proposed", "rejected", "success"]),
        ("mcp.reset_workspace", vec!["proposed", "rejected", "success"]),
        ("mcp.discard_workspace", vec!["proposed", "success"]),
    ] {
        let rows = atlas_inventory::audit::list(&pool, None, Some(tool), None, None, 50).await.unwrap();
        let statuses: Vec<_> = rows.iter().filter_map(|r| r["status"].as_str()).collect();
        for w in want {
            assert!(statuses.contains(&w), "{tool} audit statuses: {statuses:?}");
        }
    }
}
