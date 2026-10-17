// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A fleet Mac through the gateway: the backend registers, discovery fills the inventory with APFS
//! containers (pools) and volumes, and a failing drive shows up as a critical pool. Fixture driver,
//! no Mac needed (`atlas-driver-macos` tests the parser against recorded `diskutil` plists).

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("macos_fleet").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "macos-test-secret-at-least-32-bytes!!".into(),
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
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let agent = atlas_mac_agent::router(
        Arc::new(atlas_driver_macos::FakeMacosDriver::new("bkd_macos", "mac-lab")),
        Some("fleet-token".into()),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let agent_url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, agent).await.unwrap() });
    std::env::set_var("ATLAS_MACOS_AGENTS", format!("lab-mac={agent_url}"));
    std::env::set_var("ATLAS_MACOS_AGENT_TOKEN", "fleet-token");
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}


#[tokio::test]
async fn a_remote_mac_appears_as_its_own_backend_with_pools_and_volumes() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let backends: Value = reqwest::get(format!("{base}/backends")).await.unwrap().json().await.unwrap();
    let b = backends
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "bkd_macos_lab_mac")
        .unwrap_or_else(|| panic!("remote Mac backend missing: {backends}"));
    assert_eq!(b["backend_type"], "macos");
    let pools: Value = reqwest::get(format!("{base}/pools")).await.unwrap().json().await.unwrap();
    assert!(pools.as_array().unwrap().iter().any(|p| p["kind"] == "apfs_container"));
    let vols: Value = reqwest::get(format!("{base}/volumes")).await.unwrap().json().await.unwrap();
    assert!(vols.as_array().unwrap().iter().any(|v| v["name"] == "Backup"));
}
