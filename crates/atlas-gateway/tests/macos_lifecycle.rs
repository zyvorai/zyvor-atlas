// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! macOS volume lifecycle through the gateway on the fixture driver (nothing is executed): the
//! confirmations, the hard refusals (internal disk, boot container, system volume, disk image),
//! and the happy path as jobs.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("macos_lifecycle").await;
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
    std::env::set_var("ATLAS_MACOS_ENABLE", "1");
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


use serde_json::json;

async fn post(base: &str, path: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn job_state(base: &str, id: &str) -> Value {
    for _ in 0..100 {
        let j: Value = reqwest::get(format!("{base}/jobs/{id}")).await.unwrap().json().await.unwrap();
        if matches!(j["state"].as_str(), Some("succeeded" | "failed")) {
            return j;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("job {id} never finished");
}

async fn base() -> String {
    format!("http://{}/api/atlas/v1", spawn().await)
}

#[tokio::test]
async fn devices_list_marks_what_cannot_be_erased() {
    let b = base().await;
    let rows: Value = reqwest::get(format!("{b}/macos/devices")).await.unwrap().json().await.unwrap();
    let by = |d: &str| rows.as_array().unwrap().iter().find(|r| r["device"] == d).cloned().unwrap();
    assert!(by("disk0")["erase_refusal"].as_str().unwrap().contains("internal"));
    assert!(by("disk6")["erase_refusal"].is_null());
    assert!(rows.as_array().unwrap().iter().all(|r| r["device"] != "disk9"));
}

#[tokio::test]
async fn erase_needs_the_device_typed_back_and_refuses_internal_disks() {
    let b = base().await;
    let (c, _) = post(&b, "/macos/disks/erase", json!({"device":"disk6","confirm_device":"disk7","name":"X"})).await;
    assert_eq!(c, 400);
    let (c, v) = post(&b, "/macos/disks/erase", json!({"device":"disk0","confirm_device":"disk0","name":"X"})).await;
    assert_eq!(c, 409, "{v}");
    let (c, _) = post(&b, "/macos/disks/erase", json!({"device":"disk6;ls","confirm_device":"disk6;ls","name":"X"})).await;
    assert!(c == 409 || c == 400);
    let (c, _) = post(&b, "/macos/disks/erase", json!({"device":"disk6","confirm_device":"disk6","name":"-bad"})).await;
    assert_eq!(c, 400);
    let (c, _) = post(&b, "/macos/disks/erase", json!({"device":"disk9","confirm_device":"disk9","name":"X"})).await;
    assert_eq!(c, 409, "disk images are refused without the test flag");
}

#[tokio::test]
async fn external_disk_lifecycle_runs_as_jobs() {
    let b = base().await;
    let (c, v) = post(&b, "/macos/disks/erase", json!({"device":"disk6","confirm_device":"disk6","name":"Fresh"})).await;
    assert_eq!(c, 202, "{v}");
    let j = job_state(&b, v["job_id"].as_str().unwrap()).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    let (c, v) = post(&b, "/macos/containers/disk7/volumes", json!({"name":"New","quota_bytes":1073741824u64})).await;
    assert_eq!(c, 202, "{v}");
    assert_eq!(job_state(&b, v["job_id"].as_str().unwrap()).await["state"], "succeeded");
    let (c, v) = post(&b, "/macos/volumes/disk7s2/delete", json!({"confirm_device":"disk7s2"})).await;
    assert_eq!(c, 202, "{v}");
    assert_eq!(job_state(&b, v["job_id"].as_str().unwrap()).await["state"], "succeeded");
    let (c, v) = post(&b, "/macos/disks/disk6/eject", json!({})).await;
    assert_eq!(c, 202, "{v}");
    assert_eq!(job_state(&b, v["job_id"].as_str().unwrap()).await["state"], "succeeded");
}

#[tokio::test]
async fn system_volumes_and_the_boot_container_are_refused() {
    let b = base().await;
    for vol in ["disk3s1", "disk3s5", "disk3s4"] {
        let (c, _) = post(&b, &format!("/macos/volumes/{vol}/delete"), json!({"confirm_device": vol})).await;
        assert_eq!(c, 409, "{vol}");
    }
    let (c, _) = post(&b, "/macos/volumes/disk7s2/delete", json!({"confirm_device":"disk7s1"})).await;
    assert_eq!(c, 400);
    let (c, _) = post(&b, "/macos/containers/disk3/volumes", json!({"name":"X"})).await;
    assert_eq!(c, 409);
    let (c, _) = post(&b, "/macos/disks/disk0/eject", json!({})).await;
    assert_eq!(c, 409);
}

