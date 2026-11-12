// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent guardrails on the MCP edge: a per-token resource scope (deny by default, enforced at the one
//! `call_tool` chokepoint), the per-agent workspace byte budget (refused before anything is created),
//! and the `X-Atlas-Run-Id` correlation id on every audit row a call causes. Own test binary: the
//! write gate is a process-wide environment variable.
#![cfg(feature = "mcp")]

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::auth::ResourceScope;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use atlas_gateway::state::AppState;
use rmcp::model::CallToolRequestParams;
use serde_json::{json, Value};
use sqlx::Row;

mod common;

const SECRET: &str = "guardrails-test-secret-at-least-32-bytes!!";
const MIB: u64 = 1 << 20;

async fn spawn() -> (SocketAddr, AppState) {
    let database_url = common::fresh_database_url("mcp_guardrails").await;
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
        san_enable: false,
        san_server: None,
        san_targets: Vec::new(),
        san_driver_mode: atlas_common::config::DriverMode::Fake,
        cloudblock_enable: false,
        cloudblock_server: None,
        cloudblock_targets: Vec::new(),
        cloudblock_driver_mode: atlas_common::config::DriverMode::Fake,
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
    let driver = Arc::new(atlas_driver_native::FakeNativeDriver::new(
        NATIVE_BACKEND_ID,
    ));
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

fn mint(subject: &str, role: &str, tenant: &str) -> String {
    atlas_gateway::auth::mint_scoped_token(SECRET, subject, role, tenant, 3600, &[])
        .unwrap()
        .0
}

fn mint_agent(
    subject: &str,
    tenant: &str,
    tools: &[&str],
    scope: Option<ResourceScope>,
    workspace_bytes: Option<i64>,
) -> String {
    let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
    atlas_gateway::auth::mint_agent_token(
        SECRET,
        subject,
        "operator",
        tenant,
        3600,
        &tools,
        scope.as_ref(),
        workspace_bytes,
    )
    .unwrap()
    .0
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn connect_with(addr: SocketAddr, token: &str, run_id: Option<&str>) -> Client {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    if let Some(r) = run_id {
        headers.insert("x-atlas-run-id", r.parse().unwrap());
    }
    let http_client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            format!("http://{addr}/api/atlas/v1/mcp"),
        ),
    );
    ().serve(transport).await.unwrap()
}

async fn connect(addr: SocketAddr, token: &str) -> Client {
    connect_with(addr, token, None).await
}

async fn call(c: &Client, tool: &'static str, v: Value) -> Result<Value, String> {
    match c
        .call_tool(CallToolRequestParams::new(tool).with_arguments(v.as_object().unwrap().clone()))
        .await
    {
        Err(e) => Err(e.to_string()),
        Ok(r) => {
            let text = r
                .content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.text.clone())
                .unwrap_or_default();
            if r.is_error == Some(true) {
                Err(text)
            } else {
                Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
            }
        }
    }
}

async fn confirmed(c: &Client, tool: &'static str, mut args: Value) -> Result<Value, String> {
    let plan = call(c, tool, args.clone()).await?;
    assert_eq!(plan["executed"], false, "{plan}");
    args["confirmation_token"] = plan["confirmation_token"].clone();
    call(c, tool, args).await
}

async fn rest(
    addr: SocketAddr,
    tok: &str,
    m: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut r = reqwest::Client::new()
        .request(m, format!("http://{addr}/api/atlas/v1{path}"))
        .bearer_auth(tok);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    (
        resp.status().as_u16(),
        resp.json().await.unwrap_or(Value::Null),
    )
}

async fn seed(addr: SocketAddr, admin: &str, tenant: &str, name: &str) -> String {
    rest(
        addr,
        admin,
        reqwest::Method::POST,
        "/volumes",
        Some(json!({
            "tenant_id": tenant, "name": name, "size_bytes": 8 * MIB,
            "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
        })),
    )
    .await
    .1["volume_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn count(pool: &sqlx::AnyPool, table: &'static str) -> i64 {
    let q = match table {
        "storage_volumes" => "SELECT COUNT(*) AS n FROM storage_volumes",
        "storage_snapshots" => "SELECT COUNT(*) AS n FROM storage_snapshots",
        _ => "SELECT COUNT(*) AS n FROM agent_workspaces",
    };
    sqlx::query(q).fetch_one(pool).await.unwrap().get("n")
}

async fn denied_rows(pool: &sqlx::AnyPool) -> Vec<Value> {
    atlas_inventory::audit::list(pool, None, Some("mcp.tool.denied"), None, None, 50)
        .await
        .unwrap()
}

fn scope(volumes: &[&str], workspaces: &[&str]) -> ResourceScope {
    ResourceScope {
        volumes: volumes.iter().map(|s| s.to_string()).collect(),
        workspaces: workspaces.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_resource_scoped_token_names_only_what_it_lists() {
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let (addr, state) = spawn().await;
    let pool = state.pool.clone();
    let admin = mint("root", "admin", "global");
    let ok_vol = seed(addr, &admin, "tenant-a", "agent-data").await;
    let other_vol = seed(addr, &admin, "tenant-a", "secret-data").await;

    let agent = connect(
        addr,
        &mint_agent(
            "agent-1",
            "tenant-a",
            &[],
            Some(scope(&[&ok_vol], &["ws_*"])),
            None,
        ),
    )
    .await;
    // Allowed: the listed volume, by every tool that names a volume.
    assert_eq!(
        call(&agent, "get_volume", json!({ "id": ok_vol }))
            .await
            .unwrap()["id"],
        ok_vol.as_str()
    );
    // Denied: any other volume, whichever argument carries it, and nothing is created.
    let e = call(&agent, "get_volume", json!({ "id": other_vol }))
        .await
        .unwrap_err();
    assert!(e.contains("scope does not allow volume"), "{e}");
    let e = call(
        &agent,
        "create_workspace",
        json!({ "source_volume_id": other_vol }),
    )
    .await
    .unwrap_err();
    assert!(e.contains("scope does not allow volume"), "{e}");
    let e = call(
        &agent,
        "create_workspace",
        json!({ "source_snapshot_id": "snap_any" }),
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("scope does not allow snapshot"),
        "no snapshot list means no snapshot: {e}"
    );
    assert_eq!(count(&pool, "agent_workspaces").await, 0);
    // Inventory-wide listings cannot be narrowed to the scope, so they are refused outright.
    let e = call(&agent, "list_volumes", json!({})).await.unwrap_err();
    assert!(e.contains("cannot be limited"), "{e}");
    // Tools that name no resource are unaffected by the resource scope.
    assert!(call(&agent, "list_clusters", json!({})).await.is_ok());
    // The workspace the agent makes is named by an id matching `ws_*`.
    let made = confirmed(
        &agent,
        "create_workspace",
        json!({ "source_volume_id": ok_vol }),
    )
    .await
    .unwrap();
    let wid = made["result"]["id"].as_str().unwrap().to_string();
    assert!(
        call(&agent, "get_workspace", json!({ "workspace_id": wid }))
            .await
            .is_ok()
    );
    let e = call(&agent, "get_workspace", json!({ "workspace_id": "other" }))
        .await
        .unwrap_err();
    assert!(e.contains("scope does not allow workspace"), "{e}");
    agent.cancel().await.unwrap();

    // Every refusal is audited with its reason, never with the arguments.
    let rows = denied_rows(&pool).await;
    assert!(rows.len() >= 5, "{rows:?}");
    assert!(rows
        .iter()
        .all(|r| r["status"] == "denied" && r["actor_id"] == "agent-1"));
    assert!(rows
        .iter()
        .any(|r| r["result"]["reason"] == "resource outside token scope"
            && r["result"]["resource"] == other_vol.as_str()));
    assert!(rows
        .iter()
        .any(|r| r["result"]["reason"] == "enumerating tool under a resource scope"));

    // A scope with no list for a kind denies that kind outright (deny by default).
    let only_vol = connect(
        addr,
        &mint_agent(
            "agent-2",
            "tenant-a",
            &[],
            Some(scope(&[&ok_vol], &[])),
            None,
        ),
    )
    .await;
    assert!(
        call(&only_vol, "get_workspace", json!({ "workspace_id": wid }))
            .await
            .is_err()
    );
    only_vol.cancel().await.unwrap();

    // The tool allowlist and the resource scope both apply.
    let both = connect(
        addr,
        &mint_agent(
            "agent-3",
            "tenant-a",
            &["get_volume"],
            Some(scope(&[&ok_vol], &[])),
            None,
        ),
    )
    .await;
    assert!(call(&both, "get_volume", json!({ "id": ok_vol }))
        .await
        .is_ok());
    assert!(call(&both, "get_volume", json!({ "id": other_vol }))
        .await
        .is_err());
    assert!(
        call(&both, "list_clusters", json!({})).await.is_err(),
        "not on the tool allowlist"
    );
    both.cancel().await.unwrap();

    // Unscoped tokens behave exactly as before: any volume, any listing.
    let plain = connect(addr, &mint("plain", "operator", "tenant-a")).await;
    assert!(call(&plain, "get_volume", json!({ "id": other_vol }))
        .await
        .is_ok());
    assert!(call(&plain, "list_volumes", json!({})).await.is_ok());
    assert!(call(&plain, "list_workspaces", json!({})).await.is_ok());
    plain.cancel().await.unwrap();

    // A token that carries a scope is MCP-only: the scope is not bypassed by using it on REST.
    let tok = mint_agent(
        "agent-1",
        "tenant-a",
        &[],
        Some(scope(&[&ok_vol], &[])),
        None,
    );
    let (status, _) = rest(
        addr,
        &tok,
        reqwest::Method::GET,
        &format!("/volumes/{other_vol}"),
        None,
    )
    .await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn the_issue_token_endpoint_mints_a_scope_that_is_enforced() {
    let (addr, _state) = spawn().await;
    let admin = mint("root", "admin", "global");
    let vol = seed(addr, &admin, "tenant-a", "v").await;
    let (status, body) = rest(
        addr,
        &admin,
        reqwest::Method::POST,
        "/auth/tokens",
        Some(json!({
            "subject": "agent-9", "role": "operator", "tenant_id": "tenant-a",
            "scope": { "volumes": [vol.clone()] }, "workspace_bytes": 1000,
        })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["scope"]["volumes"], json!([vol]));
    assert_eq!(body["workspace_bytes"], 1000);
    let c = connect(addr, body["token"].as_str().unwrap()).await;
    assert!(call(&c, "get_volume", json!({ "id": vol })).await.is_ok());
    assert!(call(&c, "get_volume", json!({ "id": "vol_nope" }))
        .await
        .is_err());
    c.cancel().await.unwrap();
    // A bad entry is refused, and an all-empty scope mints an ordinary unscoped token.
    let (status, _) = rest(
        addr,
        &admin,
        reqwest::Method::POST,
        "/auth/tokens",
        Some(json!({
        "subject": "a", "scope": { "volumes": [""] } })),
    )
    .await;
    assert_eq!(status, 400);
    let (_, plain) = rest(
        addr,
        &admin,
        reqwest::Method::POST,
        "/auth/tokens",
        Some(json!({
        "subject": "a", "scope": {} })),
    )
    .await;
    assert_eq!(plain["scope"], Value::Null);
}

#[tokio::test]
async fn the_workspace_byte_budget_refuses_before_anything_is_created() {
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let (addr, state) = spawn().await;
    let pool = state.pool.clone();
    let admin = mint("root", "admin", "global");
    let src = seed(addr, &admin, "tenant-a", "data").await;
    // Budget 10 MiB; each workspace is capped at 8 MiB (the source is 8 MiB).
    let agent = connect(
        addr,
        &mint_agent("agent-b", "tenant-a", &[], None, Some(10 * MIB as i64)),
    )
    .await;
    let args = json!({ "source_volume_id": src, "max_bytes": 8 * MIB });

    // Without a cap the budget cannot be accounted, so the plan says it would be refused, and so does the execute.
    let plan = call(
        &agent,
        "create_workspace",
        json!({ "source_volume_id": src }),
    )
    .await
    .unwrap();
    assert!(
        plan["plan"]["would_be_refused"]
            .as_str()
            .unwrap()
            .contains("max_bytes must be set"),
        "{plan}"
    );
    let e = confirmed(
        &agent,
        "create_workspace",
        json!({ "source_volume_id": src }),
    )
    .await
    .unwrap_err();
    assert!(e.contains("max_bytes must be set"), "{e}");

    let plan = call(&agent, "create_workspace", args.clone())
        .await
        .unwrap();
    assert_eq!(plan["plan"]["would_be_refused"], Value::Null, "{plan}");
    let first = confirmed(&agent, "create_workspace", args.clone())
        .await
        .unwrap();
    let wid = first["result"]["id"].as_str().unwrap().to_string();

    let (vols, snaps, wss) = (
        count(&pool, "storage_volumes").await,
        count(&pool, "storage_snapshots").await,
        count(&pool, "agent_workspaces").await,
    );
    assert_eq!(wss, 1);
    // 8 MiB held + 8 MiB more passes the 10 MiB budget: planned as refused, refused on execute, nothing left behind.
    let plan = call(&agent, "create_workspace", args.clone())
        .await
        .unwrap();
    assert!(
        plan["plan"]["would_be_refused"]
            .as_str()
            .unwrap()
            .contains("budget of"),
        "{plan}"
    );
    let e = confirmed(&agent, "create_workspace", args.clone())
        .await
        .unwrap_err();
    assert!(e.contains("budget"), "{e}");
    assert_eq!(
        (
            count(&pool, "storage_volumes").await,
            count(&pool, "storage_snapshots").await,
            count(&pool, "agent_workspaces").await
        ),
        (vols, snaps, wss),
        "a refused create must leave no snapshot, clone or workspace row"
    );

    // Another agent has its own budget; discarding frees this one's.
    let other = connect(
        addr,
        &mint_agent("agent-c", "tenant-a", &[], None, Some(10 * MIB as i64)),
    )
    .await;
    assert_eq!(
        confirmed(&other, "create_workspace", args.clone())
            .await
            .unwrap()["executed"],
        true
    );
    other.cancel().await.unwrap();
    confirmed(&agent, "discard_workspace", json!({ "workspace_id": wid }))
        .await
        .unwrap();
    assert_eq!(
        confirmed(&agent, "create_workspace", args.clone())
            .await
            .unwrap()["executed"],
        true
    );
    agent.cancel().await.unwrap();
}

async fn run_rows(pool: &sqlx::AnyPool, run: &str) -> Vec<Value> {
    // The per-call audit row is written off the request path; give it a moment.
    for _ in 0..50 {
        let rows =
            atlas_inventory::audit::list_by_run(pool, None, None, None, None, Some(run), 100)
                .await
                .unwrap();
        if rows.iter().any(|r| r["action"] == "mcp.tool.call") {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    atlas_inventory::audit::list_by_run(pool, None, None, None, None, Some(run), 100)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_run_id_is_stored_on_every_audit_row_the_call_causes_and_can_be_filtered() {
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let (addr, state) = spawn().await;
    let pool = state.pool.clone();
    let admin = mint("root", "admin", "global");
    let src = seed(addr, &admin, "tenant-a", "data").await;

    let agent = connect_with(
        addr,
        &mint("agent-r", "operator", "tenant-a"),
        Some("run-abc.1"),
    )
    .await;
    let made = confirmed(
        &agent,
        "create_workspace",
        json!({ "source_volume_id": src }),
    )
    .await
    .unwrap();
    assert_eq!(made["executed"], true);
    let rows = run_rows(&pool, "run-abc.1").await;
    let actions: Vec<_> = rows
        .iter()
        .map(|r| r["action"].as_str().unwrap().to_string())
        .collect();
    for want in [
        "mcp.create_workspace",
        "mcp.tool.call",
        "workspace.create",
        "snapshot.create",
    ] {
        assert!(
            actions.iter().any(|a| a == want),
            "{want} missing from {actions:?}"
        );
    }
    assert!(rows.iter().all(|r| r["run_id"] == "run-abc.1"));

    // The agent can read its own run back through list_audit, filtered by run id.
    let own = call(&agent, "list_audit", json!({ "run_id": "run-abc.1" }))
        .await
        .unwrap();
    assert!(
        own.as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"] == "mcp.create_workspace"),
        "{own}"
    );
    assert!(own
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["run_id"] == "run-abc.1"));
    assert!(call(&agent, "list_audit", json!({ "run_id": "run-zzz" }))
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    assert!(call(&agent, "list_audit", json!({ "run_id": "bad id" }))
        .await
        .is_err());
    agent.cancel().await.unwrap();

    // Without the header nothing is tagged, and another run's id selects none of these rows.
    let plain = connect(addr, &mint("agent-p", "operator", "tenant-a")).await;
    call(&plain, "list_clusters", json!({})).await.unwrap();
    plain.cancel().await.unwrap();
    for _ in 0..50 {
        let all = atlas_inventory::audit::list(
            &pool,
            Some("agent-p"),
            Some("mcp.tool.call"),
            None,
            None,
            10,
        )
        .await
        .unwrap();
        if !all.is_empty() {
            assert_eq!(all[0]["run_id"], Value::Null);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // REST filter, and its validation.
    let (status, body) = rest(
        addr,
        &admin,
        reqwest::Method::GET,
        "/audit?run_id=run-abc.1",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        !body.as_array().unwrap().is_empty()
            && body
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["run_id"] == "run-abc.1")
    );
    assert_eq!(
        rest(
            addr,
            &admin,
            reqwest::Method::GET,
            "/audit?run_id=a%20b",
            None
        )
        .await
        .0,
        400
    );

    // A malformed run id is refused before the tool runs, and the refusal is audited without it.
    let bad = connect_with(
        addr,
        &mint("agent-x", "operator", "tenant-a"),
        Some("not valid!"),
    )
    .await;
    let before = count(&pool, "agent_workspaces").await;
    let e = call(&bad, "create_workspace", json!({ "source_volume_id": src }))
        .await
        .unwrap_err();
    assert!(e.contains("X-Atlas-Run-Id"), "{e}");
    assert_eq!(count(&pool, "agent_workspaces").await, before);
    bad.cancel().await.unwrap();
    let refused = denied_rows(&pool).await;
    assert!(
        refused.iter().any(|r| r["actor_id"] == "agent-x"
            && r["result"]["reason"] == "invalid run id"
            && r["run_id"] == Value::Null),
        "{refused:?}"
    );
}
