// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `POST /backends` instantiates a live NFS/ZFS driver (not just a catalog row) and discovers it, so
//! its pools appear immediately. Fake driver, no infra (NFS/ZFS are deterministic fixture drivers).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

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
    let database_url = common::fresh_database_url("backends").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "backends-test-secret-at-least-32-bytes!".into(),
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
        zfs_enable: false,
        zfs_host: None,
        zfs_pools: Vec::new(),
        zfs_driver_mode,
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn post_backend_registers_live_nfs_driver() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let created: Value = c
        .post(format!("{base}/backends"))
        .json(&json!({ "name": "extra-nfs", "backend_type": "nfs", "server": "nas.lab", "targets": ["/exports/data"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        created["status"], "active",
        "nfs backend should be live, not pending: {created}"
    );
    let bid = created["id"].as_str().unwrap();

    // The newly-registered backend discovered its export as a pool.
    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        pools
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "nfs_export"),
        "the live NFS backend should have discovered an export pool: {pools}"
    );
    assert!(!bid.is_empty());
}

#[tokio::test]
async fn post_backend_registers_live_san_driver() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let created: Value = c
        .post(format!("{base}/backends"))
        .json(&json!({
            "name": "extra-san",
            "backend_type": "san",
            "server": "10.1.0.9:3260",
            "targets": ["iqn.2026-01.com.zyvor:lun0"]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        created["status"], "active",
        "san backend should be live, not pending: {created}"
    );
    assert_eq!(created["capabilities"]["block"], true, "{created}");
    assert_eq!(created["capabilities"]["expansion"], true, "{created}");

    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        pools
            .as_array()
            .unwrap()
            .iter()
            .any(|p| { p["kind"] == "san_portal" && p["name"] == "10.1.0.9:3260" }),
        "the live SAN backend should have discovered a portal pool: {pools}"
    );
}

#[tokio::test]
async fn post_backend_registers_live_cloudblock_driver() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let created: Value = c
        .post(format!("{base}/backends"))
        .json(&json!({
            "name": "extra-ebs",
            "backend_type": "cloud_block",
            "server": "aws",
            "targets": ["vol-aaa"]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        created["status"], "active",
        "cloud_block backend should be live, not pending: {created}"
    );
    assert_eq!(created["capabilities"]["block"], true, "{created}");
    assert_eq!(created["capabilities"]["expansion"], true, "{created}");

    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        pools
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "cloud_block" && p["name"] == "aws"),
        "the live cloud-block backend should have discovered a provider pool: {pools}"
    );
}

/// With `nfs_driver_mode: Real` and an unreachable server, the live NFS driver's discovery fails
/// (no `showmount` response) — the backend row still ends up `active` (discovery failure is
/// logged, not surfaced to the caller — pre-existing `create_backend` behavior, unchanged here),
/// but critically **no pool is ever reported**, proving the real driver never falls back to
/// fabricated fixture data the way `spawn()`'s default fake mode intentionally always does.
#[tokio::test]
async fn post_backend_with_real_nfs_driver_never_fabricates_a_pool_when_unreachable() {
    let base = format!(
        "http://{}/api/atlas/v1",
        spawn_with(
            atlas_common::config::DriverMode::Real,
            atlas_common::config::DriverMode::Fake,
        )
        .await
    );
    let c = reqwest::Client::new();

    let created: Value = c
        .post(format!("{base}/backends"))
        .json(&json!({
            "name": "unreachable-nfs",
            "backend_type": "nfs",
            "server": "nfs01.invalid.example.invalid",
            "targets": ["/exports/data"]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bid = created["id"].as_str().unwrap().to_string();
    assert!(!bid.is_empty());

    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !pools
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "nfs_export"),
        "an unreachable real NFS server must never produce a fabricated pool: {pools}"
    );
}

/// The RustFS product driver was removed (see docs/RUSTFS.md) — `POST /backends` now rejects
/// `backend_type: "rustfs"` with a clear validation error instead of instantiating a live driver.
#[tokio::test]
async fn post_backend_rejects_retired_rustfs_backend_type() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let resp = c
        .post(format!("{base}/backends"))
        .json(&json!({ "name": "extra-rustfs", "backend_type": "rustfs", "server": "http://rustfs.lab:9000", "targets": ["catalog"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "retired rustfs backend_type must be rejected"
    );
}
