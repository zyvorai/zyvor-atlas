// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Guarded MCP write tools (`ATLAS_MCP_WRITE_ENABLE`). Own test binary: the gate is process-wide.
#![cfg(feature = "mcp")]

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};

mod common;

async fn spawn(auth_required: bool, secret: &str) -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("mcp").await;
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


async fn connect(
    addr: SocketAddr,
    secret: &str,
    role: &str,
    tenant: &str,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;
    let (token, _, _) = atlas_gateway::auth::mint_token(secret, "agent", role, tenant, 3600).unwrap();
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
    ().serve(transport).await.unwrap()
}

fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

fn text_of(r: &rmcp::model::CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// One test per process on purpose: the write gate is a process-wide environment variable.
#[tokio::test]
async fn guarded_snapshot_is_two_phase_single_use_and_tenant_scoped() {
    use rmcp::model::CallToolRequestParams;
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");

    let secret = "mcp-test-secret-at-least-32-bytes!!";
    let (addr, pool) = spawn(true, secret).await;
    let vol = atlas_api_types::StorageVolume {
        id: "vol_tenant_a".into(),
        cluster_id: None,
        pool_id: None,
        name: "a-vol".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some("rbd:pool/vol_tenant_a".into()),
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("a-vol".into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "tenant-a", &vol, None)
        .await
        .unwrap();
    // A Ceph-backed snapshot is an async job, so "something changed" = a job was enqueued.
    let snapshots = |pool: sqlx::AnyPool| async move {
        atlas_inventory::jobs::list_jobs(&pool, 100)
            .await
            .unwrap()
            .len()
    };
    let before = snapshots(pool.clone()).await;

    let a = connect(addr, secret, "operator", "tenant-a").await;
    let names: Vec<_> = a
        .list_tools(Default::default())
        .await
        .unwrap()
        .tools
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"create_snapshot".to_string()), "{names:?}");
    for forbidden in ["delete", "failover", "promote", "restore", "rollback", "wipe"] {
        assert!(
            !names.iter().any(|n| n.contains(forbidden)),
            "unexpected destructive tool ({forbidden}): {names:?}"
        );
    }

    // Phase 1: plan only, nothing changes.
    let plan = a
        .call_tool(
            CallToolRequestParams::new("create_snapshot")
                .with_arguments(args(serde_json::json!({"volume_id": "vol_tenant_a", "name": "agent-snap"}))),
        )
        .await
        .unwrap();
    assert_eq!(plan.is_error, Some(false), "{}", text_of(&plan));
    let plan_json: serde_json::Value = serde_json::from_str(&text_of(&plan)).unwrap();
    assert_eq!(plan_json["executed"], false);
    let token = plan_json["confirmation_token"].as_str().unwrap().to_string();
    assert_eq!(snapshots(pool.clone()).await, before, "planning must not enqueue anything");

    // Different arguments with the same token are refused.
    let wrong = a
        .call_tool(
            CallToolRequestParams::new("create_snapshot").with_arguments(args(serde_json::json!({
                "volume_id": "vol_tenant_a", "name": "other-name", "confirmation_token": token
            }))),
        )
        .await;
    assert!(wrong.is_err() || wrong.unwrap().is_error == Some(true));
    assert_eq!(snapshots(pool.clone()).await, before);

    // Phase 2: the same arguments plus the token execute exactly once.
    let call = || {
        a.call_tool(
            CallToolRequestParams::new("create_snapshot").with_arguments(args(serde_json::json!({
                "volume_id": "vol_tenant_a", "name": "agent-snap", "confirmation_token": token
            }))),
        )
    };
    let done = call().await.unwrap();
    assert_eq!(done.is_error, Some(false), "{}", text_of(&done));
    let done_json: serde_json::Value = serde_json::from_str(&text_of(&done)).unwrap();
    assert_eq!(done_json["executed"], true);
    assert!(done_json["result"]["job_id"].is_string(), "{done_json}");
    assert_eq!(done_json["result"]["state"], "queued");
    assert_eq!(snapshots(pool.clone()).await, before + 1);
    let replay = call().await;
    assert!(replay.is_err() || replay.unwrap().is_error == Some(true), "token replay");
    assert_eq!(snapshots(pool.clone()).await, before + 1, "replay must not enqueue another");
    a.cancel().await.unwrap();

    // Another tenant's operator cannot even get a plan; a viewer cannot either.
    let b = connect(addr, secret, "operator", "tenant-b").await;
    let r = b
        .call_tool(
            CallToolRequestParams::new("create_snapshot")
                .with_arguments(args(serde_json::json!({"volume_id": "vol_tenant_a"}))),
        )
        .await;
    assert!(r.is_err() || r.unwrap().is_error == Some(true), "cross-tenant must be refused");
    b.cancel().await.unwrap();
    let v = connect(addr, secret, "viewer", "tenant-a").await;
    let r = v
        .call_tool(
            CallToolRequestParams::new("create_snapshot")
                .with_arguments(args(serde_json::json!({"volume_id": "vol_tenant_a"}))),
        )
        .await;
    assert!(r.is_err() || r.unwrap().is_error == Some(true), "viewer must be refused");
    v.cancel().await.unwrap();

    // Every attempt that got past authorization is audited, with its outcome.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT status FROM storage_audit_logs WHERE action = 'mcp.create_snapshot' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let statuses: Vec<_> = rows.into_iter().map(|r| r.0).collect();
    assert_eq!(statuses, vec!["proposed", "rejected", "success", "rejected"], "{statuses:?}");
}
