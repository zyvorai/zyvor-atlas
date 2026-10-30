// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `POST /zfs/pools/from-device` and `POST /ceph/devices` — provisioning a raw, unformatted disk.
//! Fake driver, no infra: the fixture device path is the only one fake mode ever accepts, proving
//! it never fabricates a formatted disk for anything else.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn(zfs_enable: bool) -> SocketAddr {
    let database_url = common::fresh_database_url("disks").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "disks-test-secret-at-least-32-bytes!".into(),
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
        zfs_enable,
        zfs_host: Some("zfs01.example.test".into()),
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

// ---- ZFS: provision a raw device into a new zpool ----

#[tokio::test]
async fn zfs_from_device_requires_confirm() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/zfs/pools/from-device"))
        .json(&json!({ "pool_name": "tank2", "device_path": "/dev/vdz", "confirm": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn zfs_from_device_refuses_a_syntactically_unsafe_path() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    for bad_path in ["/dev/sda", "/dev/sdb1", "not-a-device-path"] {
        let resp = c
            .post(format!("{base}/zfs/pools/from-device"))
            .json(&json!({ "pool_name": "tank2", "device_path": bad_path, "confirm": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "path {bad_path} should have been refused"
        );
    }
}

#[tokio::test]
async fn zfs_from_device_requires_the_backend_to_be_enabled() {
    let base = format!("http://{}/api/atlas/v1", spawn(false).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/zfs/pools/from-device"))
        .json(&json!({ "pool_name": "tank2", "device_path": "/dev/vdz", "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn zfs_from_device_fixture_succeeds_and_pool_appears() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/zfs/pools/from-device"))
        .json(&json!({ "pool_name": "tank2", "device_path": "/dev/vdz", "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = resp.json().await.unwrap();
    let job_id = accepted["job_id"].as_str().unwrap().to_string();

    let job = poll_job_to_terminal(&base, &job_id).await;
    assert_eq!(job["state"], "succeeded", "job: {job}");

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
            .any(|p| p["name"] == "tank2" && p["kind"] == "zpool"),
        "the new zpool should appear in inventory: {pools}"
    );
}

#[tokio::test]
async fn zfs_pool_destroy_requires_matching_confirmation_and_removes_the_pool() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/zfs/pools/from-device"))
        .json(&json!({ "pool_name": "tank3", "device_path": "/dev/vdz", "confirm": true }))
        .send()
        .await
        .unwrap();
    let accepted: Value = resp.json().await.unwrap();
    let job = poll_job_to_terminal(&base, accepted["job_id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "succeeded", "job: {job}");

    let wrong = c
        .post(format!("{base}/zfs/pools/tank3/destroy"))
        .json(&json!({ "confirm_pool_name": "other" }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::BAD_REQUEST);

    let unknown = c
        .post(format!("{base}/zfs/pools/nosuch/destroy"))
        .json(&json!({ "confirm_pool_name": "nosuch" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

    let resp = c
        .post(format!("{base}/zfs/pools/tank3/destroy"))
        .json(&json!({ "confirm_pool_name": "tank3" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = resp.json().await.unwrap();
    let job = poll_job_to_terminal(&base, accepted["job_id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "succeeded", "job: {job}");

    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !pools.as_array().unwrap().iter().any(|p| p["name"] == "tank3"),
        "destroyed pool should be gone from inventory: {pools}"
    );
}

/// Fake mode only ever "succeeds" against its one fixture device path — proving it never
/// fabricates having formatted an arbitrary operator-supplied device it never actually touched.
#[tokio::test]
async fn zfs_from_device_fake_mode_refuses_any_non_fixture_device() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/zfs/pools/from-device"))
        .json(&json!({ "pool_name": "tank3", "device_path": "/dev/nvme1n1", "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: Value = resp.json().await.unwrap();
    let job_id = accepted["job_id"].as_str().unwrap().to_string();

    let job = poll_job_to_terminal(&base, &job_id).await;
    assert_eq!(job["state"], "failed", "job: {job}");
    assert!(job["error"]
        .as_str()
        .unwrap_or_default()
        .contains("fixture"));
}

// ---- Ceph/Rook: claim a raw device as a new OSD ----

#[tokio::test]
async fn ceph_add_device_requires_confirm() {
    let base = format!("http://{}/api/atlas/v1", spawn(false).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/ceph/devices"))
        .json(&json!({ "node_name": "node-1", "device_path": "/dev/sdb", "confirm": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

/// No Kubernetes cluster is attached in these tests — the route must refuse cleanly (503) rather
/// than panic or silently pretend the device was claimed.
#[tokio::test]
async fn ceph_add_device_requires_kubernetes() {
    let base = format!("http://{}/api/atlas/v1", spawn(false).await);
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{base}/ceph/devices"))
        .json(&json!({ "node_name": "node-1", "device_path": "/dev/sdb", "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn nvmeof_namespace_requires_confirm_and_kubernetes() {
    let base = format!("http://{}/api/atlas/v1", spawn(false).await);
    let c = reqwest::Client::new();
    let body = |confirm: bool| {
        json!({ "tenant_id": "tenant_default", "name": "ns1", "storage_class": "nvmeof",
                "size_bytes": 1073741824, "confirm": confirm })
    };
    let url = format!("{base}/ceph/nvmeof/namespaces");
    let r = c.post(&url).json(&body(false)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    // No Kubernetes cluster is attached: refuse cleanly rather than pretend to provision.
    let r = c.post(&url).json(&body(true)).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn io_ai_reports_unavailable_without_an_agent_never_fine() {
    let base = format!("http://{}/api/atlas/v1", spawn(false).await);
    let r = reqwest::Client::new().get(format!("{base}/io/ai")).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "unavailable");
    assert_eq!(v["devices"], json!([]));
    assert!(v["detail"].as_str().unwrap().contains("ATLAS_IO_AGENT_URL"));
}

// ---- GET /zfs/devices: the Disks UI's device picker, not just a blind text field ----

#[tokio::test]
async fn zfs_devices_fake_mode_reports_only_the_fixture_device() {
    let base = format!("http://{}/api/atlas/v1", spawn(true).await);
    let c = reqwest::Client::new();
    let resp = c.get(format!("{base}/zfs/devices")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let devices: Value = resp.json().await.unwrap();
    let devices = devices.as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["path"], "/dev/vdz");
    assert_eq!(devices[0]["status"], "empty");
}
