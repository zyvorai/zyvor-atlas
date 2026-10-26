// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /pools/{id}/dedup-estimate` through the gateway against the fake Ceph and ZFS drivers.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    spawn_with(
        atlas_common::config::DriverMode::Fake,
        atlas_common::config::DriverMode::Fake,
    )
    .await
}

async fn spawn_with(
    nfs_driver_mode: atlas_common::config::DriverMode,
    zfs_driver_mode: atlas_common::config::DriverMode,
) -> SocketAddr {
    let database_url = common::fresh_database_url("dedup_estimate").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "dedup-test-secret-at-least-32-bytes!".into(),
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
        nfs_driver_mode,
        zfs_enable: true,
        zfs_host: Some("zfs01.example.test".into()),
        zfs_pools: Vec::new(),
        zfs_driver_mode,
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn pool_id(c: &reqwest::Client, base: &str, kind: &str) -> String {
    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} pool in {pools}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn estimates_dedup_on_an_rbd_pool() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let id = pool_id(&c, &base, "rbd").await;
    let r = c
        .get(format!(
            "{base}/pools/{id}/dedup-estimate?max_bytes=1048576"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let e: Value = r.json().await.unwrap();
    assert_eq!(e["method"], "block_hash_scan");
    assert!(e["dedup_ratio"].as_f64().unwrap() > 2.0, "{e}");
    assert_eq!(e["lower_bound"], false);
}

#[tokio::test]
async fn estimates_dedup_on_a_zfs_pool_with_the_simulated_ddt() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let zfs = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str().unwrap().starts_with("pool_zfs"))
        .unwrap_or_else(|| panic!("no zfs pool in {pools}"));
    let id = zfs["id"].as_str().unwrap();
    let e: Value = c
        .get(format!("{base}/pools/{id}/dedup-estimate"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(e["method"], "zdb_simulated_ddt");
    assert_eq!(e["dedup_ratio"], 1.33);
}

#[tokio::test]
async fn unsupported_pool_kinds_and_unknown_pools_are_refused() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let rgw = pool_id(&c, &base, "rgw").await;
    // The Ceph driver only estimates RBD pools.
    assert_eq!(
        c.get(format!("{base}/pools/{rgw}/dedup-estimate"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.get(format!("{base}/pools/pool_nope/dedup-estimate"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}
