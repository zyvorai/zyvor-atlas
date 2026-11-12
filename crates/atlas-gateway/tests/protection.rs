// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /protection-status` and `GET /volumes/{id}/protection` — pin the real route → inventory
//! synthesis path. See `atlas_inventory::protection` for the verdict logic itself (unit-tested
//! there); this file just proves the HTTP surface returns the expected shape.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("protection").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "protection-test-secret-at-least-32-bytes!!".into(),
        jwt_secret_previous: None,
        auth_required: false,
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
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state");
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, pool)
}

#[tokio::test]
async fn protection_status_endpoints_return_expected_shape() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    sqlx::query(
        "INSERT INTO storage_backends (id, backend_type, mode, name, status) \
         VALUES ('bkd1', 'ceph', 'managed_rook', 'ceph', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state) \
         VALUES ('v1', 'global', 'bkd1', 'unprotected-vol', 'block', 1000, 'ready')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Fleet view includes the seeded volume, unprotected -> critical, and rto_note is honest
    // about not being a measured value — a regression guard against ever fabricating a number.
    let resp = c
        .get(format!("{base}/protection-status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let rows = body.as_array().unwrap();
    let v1 = rows
        .iter()
        .find(|r| r["volume_id"] == "v1")
        .expect("v1 present");
    assert_eq!(v1["verdict"], "critical", "body: {v1}");
    assert_eq!(v1["rto_note"], "not measured — no restore drill on record");
    assert!(v1["rto_target_seconds"].is_null());

    // Single-volume view agrees with the fleet view.
    let resp = c
        .get(format!("{base}/volumes/v1/protection"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let single: Value = resp.json().await.unwrap();
    assert_eq!(single["verdict"], "critical");
    assert_eq!(single["volume_id"], "v1");

    // Unknown volume -> 404, not a panic or a null-verdict row.
    let resp = c
        .get(format!("{base}/volumes/does-not-exist/protection"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
