// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 cluster-ops & maintenance mode: cordoning a backend rejects new provisioning, the global
//! maintenance pause holds jobs until resumed, and OSD ops enqueue as jobs. Fake driver, no infra
//! (the OSD job fails without a real `ceph`, but the enqueue contract + validation are exercised).

use std::net::SocketAddr;
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

const CEPH: &str = "bkd_ceph_lab";

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("maintenance").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "maint-test-secret-key-at-least-32-byte".into(),
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

fn vol(name: &str) -> Value {
    json!({
        "tenant_id": "t", "name": name, "size_bytes": 1_073_741_824i64,
        "policy": "database", "kubernetes": { "namespace": "default" }
    })
}

/// A cordoned backend rejects new provisioning (503); uncordon restores it.
#[tokio::test]
async fn cordon_blocks_provisioning() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    // Baseline: create accepted (the job fails later without k8s, but enqueue is 202).
    assert_eq!(
        c.post(format!("{base}/volumes"))
            .json(&vol("v0"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );

    // Cordon → new provisioning rejected with 503.
    let cor = c
        .post(format!("{base}/backends/{CEPH}/cordon"))
        .send()
        .await
        .unwrap();
    assert_eq!(cor.status(), 200);
    assert_eq!(cor.json::<Value>().await.unwrap()["cordoned"], true);
    assert_eq!(
        c.post(format!("{base}/volumes"))
            .json(&vol("v1"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );

    // Uncordon → provisioning resumes.
    assert_eq!(
        c.post(format!("{base}/backends/{CEPH}/uncordon"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.post(format!("{base}/volumes"))
            .json(&vol("v2"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );

    // Cordoning an unknown backend → 404.
    assert_eq!(
        c.post(format!("{base}/backends/nope/cordon"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

/// The maintenance pause holds jobs in `queued` until resumed, then they drain.
#[tokio::test]
async fn pause_holds_jobs_until_resumed() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    // Enter maintenance.
    let p = c
        .post(format!("{base}/maintenance"))
        .json(&json!({ "paused": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(p.status(), 200);
    let m: Value = c
        .get(format!("{base}/maintenance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m["paused"], true);

    // Enqueue an OSD op; the worker should hold it (leave it queued) while paused.
    let job: Value = c
        .post(format!("{base}/osds/5/out"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let job_id = job["job_id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(400)).await;
    let held: Value = c
        .get(format!("{base}/jobs/{job_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(held["state"].as_str(), Some("queued") | Some("pending")),
        "job should be held while paused, got {:?}",
        held["state"]
    );

    // Resume → the held job drains to a terminal state (failed here — no real ceph binary).
    assert_eq!(
        c.post(format!("{base}/maintenance"))
            .json(&json!({ "paused": false }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut last = String::new();
    for _ in 0..40 {
        let j: Value = c
            .get(format!("{base}/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        last = j["state"].as_str().unwrap_or("").to_string();
        if last == "failed" || last == "succeeded" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job did not drain after resume (last state: '{last}')");
}

/// `POST /jobs/{id}/cancel` on a job still `queued` (held by maintenance pause) marks it `failed`
/// immediately, without waiting for the worker — the operator escape hatch for a job that would
/// otherwise sit behind a wedged one for up to `ATLAS_JOB_TIMEOUT_SECS`.
#[tokio::test]
async fn cancel_queued_job() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    assert_eq!(
        c.post(format!("{base}/maintenance"))
            .json(&json!({ "paused": true }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let job: Value = c
        .post(format!("{base}/osds/7/out"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let job_id = job["job_id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let held: Value = c
        .get(format!("{base}/jobs/{job_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        held["state"].as_str(),
        Some("queued") | Some("pending")
    ));

    let cancel = c
        .post(format!("{base}/jobs/{job_id}/cancel"))
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), 200);
    assert_eq!(cancel.json::<Value>().await.unwrap()["cancelled"], true);

    let after: Value = c
        .get(format!("{base}/jobs/{job_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["state"], "failed");
    assert_eq!(after["error"], "cancelled by operator");

    // Resuming maintenance must not resurrect the cancelled job — it's terminal, try_claim skips it.
    assert_eq!(
        c.post(format!("{base}/maintenance"))
            .json(&json!({ "paused": false }))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let still: Value = c
        .get(format!("{base}/jobs/{job_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(still["state"], "failed");
}

/// Cancelling an already-terminal or unknown job is rejected rather than silently accepted.
#[tokio::test]
async fn cancel_rejects_terminal_and_unknown() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    // Unknown job id → 404.
    assert_eq!(
        c.post(format!("{base}/jobs/job_does_not_exist/cancel"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // A fake-mode OSD op completes almost immediately (no real `ceph` CLI call) — cancel after
    // it's terminal should be 409, not silently accepted.
    let job: Value = c
        .post(format!("{base}/osds/8/out"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let job_id = job["job_id"].as_str().unwrap().to_string();
    let mut terminal = false;
    for _ in 0..40 {
        let j: Value = c
            .get(format!("{base}/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if matches!(j["state"].as_str(), Some("succeeded") | Some("failed")) {
            terminal = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(terminal, "job never reached a terminal state");
    assert_eq!(
        c.post(format!("{base}/jobs/{job_id}/cancel"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

/// OSD ops enqueue as jobs; reweight validates its weight.
#[tokio::test]
async fn osd_ops_enqueue_and_validate() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let out = c.post(format!("{base}/osds/3/out")).send().await.unwrap();
    assert_eq!(out.status(), 202);
    assert!(out.json::<Value>().await.unwrap()["job_id"].is_string());

    // reweight without a weight → 400; out of range → 400; valid → 202.
    assert_eq!(
        c.post(format!("{base}/osds/3/reweight"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/osds/3/reweight?weight=2.0"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/osds/3/reweight?weight=0.8"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
}
