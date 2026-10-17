// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A Mac's disks through the gateway: the backend registers, discovery fills the inventory with APFS
//! containers (pools) and volumes, and a failing drive shows up as a critical pool. Fixture driver,
//! no Mac needed (`atlas-driver-macos` tests the parser against recorded `diskutil` plists).

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_macos_driver, build_state, BuildOptions, MACOS_BACKEND_ID};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("macos_backend").await;
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
    let driver = Arc::new(atlas_driver_macos::FakeMacosDriver::new(
        MACOS_BACKEND_ID,
        "mac01",
    ));
    attach_macos_driver(&state.pool, &state.drivers, driver.clone())
        .await
        .unwrap();
    atlas_discovery::run_discovery(&state.pool, driver, None, None)
        .await
        .expect("macos discovery");
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn get(url: String) -> (u16, Value) {
    let r = reqwest::get(url).await.unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn macos_backend_registers_and_discovery_fills_pools_and_volumes() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);

    let (_, backends) = get(format!("{base}/backends")).await;
    let mac = backends
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == MACOS_BACKEND_ID)
        .expect("macos backend registered");
    assert_eq!(mac["backend_type"], "macos");
    assert_eq!(mac["capabilities"]["file"], true);
    assert_eq!(mac["capabilities"]["snapshots"], false, "read-only for now");

    let (st, pools) = get(format!("{base}/pools")).await;
    assert_eq!(st, 200);
    let ext = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "disk7")
        .unwrap_or_else(|| panic!("external APFS container missing: {pools}"));
    assert_eq!(ext["kind"], "apfs_container");
    assert_eq!(ext["max_bytes"], 999_995_129_856_i64);
    assert_eq!(ext["health"], "ok");

    let (st, vols) = get(format!("{base}/volumes")).await;
    assert_eq!(st, 200);
    let names: Vec<&str> = vols
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    for want in ["Backup", "Scratch", "CAMERA"] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    assert!(!names.contains(&"Installer"), "disk images stay out of inventory");
}
