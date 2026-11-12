// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/health-rollup` pins the real route → driver → classify() path against the fake
//! driver's known fixture (1 OSD down, 8/289 PGs active+undersized+degraded, no active recovery).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("health-rollup").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "health-rollup-test-secret-at-least-32-bytes!!".into(),
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn health_rollup_classifies_fake_fixture_as_degraded() {
    let addr = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    let resp = c
        .get(format!("{base}/ceph/health-rollup"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // FakeCephDriver::ceph_status always reports HEALTH_WARN with 1 osd down and no active
    // recovery (crates/atlas-driver-ceph/src/fake.rs) — that's Degraded, not Rebuilding/AtRisk.
    assert_eq!(body["state"], "degraded", "body: {body}");
    assert_eq!(body["raw_status"], "HEALTH_WARN");
    assert_eq!(body["osds_up"], 5);
    assert_eq!(body["osds_total"], 6);
    assert!(!body["reasons"].as_array().unwrap().is_empty());
}
