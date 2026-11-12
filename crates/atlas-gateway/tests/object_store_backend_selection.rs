// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `POST /buckets` backend selection: Ceph RGW (`bkd_ceph_lab`) is the default when `backend_id`
//! is omitted. The retired RustFS backend id is rejected. No real k8s cluster is attached, so
//! Ceph jobs reach a clean `failed` state (never a fabricated success).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("object-store-backend-selection").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "obj-backend-test-secret-at-least-32-bytes!".into(),
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

async fn poll_job_to_terminal(base: &str, job_id: &str) -> Value {
    let c = reqwest::Client::new();
    for _ in 0..50 {
        let job: Value = c
            .get(format!("{base}/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let state = job["state"].as_str().unwrap_or_default();
        if state == "succeeded" || state == "failed" {
            return job;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("job {job_id} never reached a terminal state");
}

#[tokio::test]
async fn create_bucket_defaults_to_ceph_rgw_when_backend_id_omitted() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/buckets"))
        .json(&json!({ "name": "default-bucket" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = resp.json().await.unwrap();
    assert_eq!(accepted["resource"]["backend_id"], "bkd_ceph_lab");
    let job_id = accepted["job_id"].as_str().unwrap().to_string();

    // No k8s attached — the OBC path needs a cluster, so this must fail cleanly.
    let job = poll_job_to_terminal(&base, &job_id).await;
    assert_eq!(job["state"], "failed", "job: {job}");
}

#[tokio::test]
async fn create_bucket_explicit_ceph_backend_id_keeps_working() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/buckets"))
        .json(&json!({ "name": "ceph-bucket", "backend_id": "bkd_ceph_lab" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = resp.json().await.unwrap();
    assert_eq!(accepted["resource"]["backend_id"], "bkd_ceph_lab");
    let job_id = accepted["job_id"].as_str().unwrap().to_string();

    let job = poll_job_to_terminal(&base, &job_id).await;
    assert_eq!(job["state"], "failed", "job: {job}");
}

#[tokio::test]
async fn create_bucket_unknown_backend_id_is_rejected() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/buckets"))
        .json(&json!({ "name": "weird-bucket", "backend_id": "bkd_nonsense" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_bucket_retired_rustfs_backend_id_is_rejected() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/buckets"))
        .json(&json!({ "name": "legacy-rustfs-bucket", "backend_id": "bkd_rustfs_lab" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}
