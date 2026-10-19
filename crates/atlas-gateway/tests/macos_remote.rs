// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Lifecycle on another Mac through its agent, driven from the gateway routes (`?backend=`). The agent
//! is in-process with fixture data and fake ops: nothing is executed, but the agent's refusals are
//! the real ones. Its own test file because it sets `ATLAS_MACOS_AGENTS` for the whole process.

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_driver_macos::{FakeMacosDriver, MacosOps};
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn agent() -> String {
    let app = atlas_mac_agent::router_with_ops(
        Arc::new(FakeMacosDriver::new("bkd_macos", "mac01")),
        Some("agent-token".into()),
        Arc::new(MacosOps::fake(false)),
    )
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("macos_remote").await;
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}


async fn call(c: &reqwest::Client, method: &str, url: String, body: Option<Value>) -> (u16, Value) {
    let mut r = match method {
        "GET" => c.get(url),
        _ => c.post(url),
    };
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn lifecycle_runs_on_the_named_mac_and_its_refusals_hold() {
    let url = agent().await;
    std::env::set_var("ATLAS_MACOS_AGENTS", format!("studio={url}"));
    std::env::set_var("ATLAS_MACOS_AGENT_TOKEN", "agent-token");
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let b = "backend=bkd_macos_studio";

    let (st, rows) = call(&c, "GET", format!("{base}/macos/devices?{b}"), None).await;
    assert_eq!(st, 200, "{rows}");
    let rows = rows.as_array().unwrap();
    let internal = rows.iter().find(|r| r["internal"] == true).expect("internal disk");
    let external = rows
        .iter()
        .find(|r| r["erase_refusal"].is_null())
        .expect("erasable external disk");

    // The agent refuses the internal disk: 409, no job.
    let dev = internal["device"].as_str().unwrap();
    let (st, body) = call(
        &c,
        "POST",
        format!("{base}/macos/disks/erase?{b}"),
        Some(json!({ "device": dev, "confirm_device": dev, "name": "Nope" })),
    )
    .await;
    assert_eq!(st, 409, "{body}");
    let (st, _) = call(&c, "POST", format!("{base}/macos/disks/{dev}/eject?{b}"), None).await;
    assert_eq!(st, 409);

    // An external disk is accepted as a job aimed at that Mac.
    let dev = external["device"].as_str().unwrap();
    let (st, body) = call(
        &c,
        "POST",
        format!("{base}/macos/disks/erase?{b}"),
        Some(json!({ "device": dev, "confirm_device": dev, "name": "Scratch" })),
    )
    .await;
    assert_eq!(st, 202, "{body}");
    assert!(body["job_id"].as_str().is_some());
    assert_eq!(body["resource"]["detail"]["backend"], "bkd_macos_studio");
    // The job runs against that agent too (the agent's fake ops execute nothing).
    let job_id = body["job_id"].as_str().unwrap().to_string();
    let mut last = Value::Null;
    for _ in 0..50 {
        let (_, j) = call(&c, "GET", format!("{base}/jobs/{job_id}"), None).await;
        last = j;
        if matches!(last["state"].as_str(), Some("succeeded" | "failed")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(last["state"], "succeeded", "{last}");
    assert_eq!(last["result"]["operation"], "erase");
    assert!(last["result"]["output"].as_str().unwrap().starts_with("fake: diskutil eraseDisk"));

    // Resize: the boot container is refused (409); an external one is accepted and the job runs.
    let (st, _) = call(
        &c,
        "POST",
        format!("{base}/macos/containers/disk3/resize?{b}"),
        Some(json!({ "size_bytes": 1u64 << 40, "confirm_device": "disk3" })),
    )
    .await;
    assert_eq!(st, 409);
    let (st, _) = call(
        &c,
        "POST",
        format!("{base}/macos/containers/disk7/resize?{b}"),
        Some(json!({ "size_bytes": 1u64 << 40, "confirm_device": "disk8" })),
    )
    .await;
    assert_eq!(st, 400, "the container must be typed back");
    let (st, body) = call(
        &c,
        "POST",
        format!("{base}/macos/containers/disk7/resize?{b}"),
        Some(json!({ "size_bytes": 1u64 << 40, "confirm_device": "disk7" })),
    )
    .await;
    assert_eq!(st, 202, "{body}");

    // A bad name is a validation error from the agent, not a 5xx.
    let (st, _) = call(
        &c,
        "POST",
        format!("{base}/macos/disks/erase?{b}"),
        Some(json!({ "device": dev, "confirm_device": dev, "name": "bad/name" })),
    )
    .await;
    assert_eq!(st, 400);

    // Unknown agent, and something that is not a macOS backend.
    let (st, _) = call(&c, "GET", format!("{base}/macos/devices?backend=bkd_macos_nope"), None).await;
    assert_eq!(st, 404);
    let (st, _) = call(&c, "GET", format!("{base}/macos/devices?backend=bkd_ceph_lab"), None).await;
    assert_eq!(st, 400);
    // No ?backend= means this Mac, which is not enabled in this gateway.
    let (st, _) = call(&c, "GET", format!("{base}/macos/devices"), None).await;
    assert_eq!(st, 503);
}
