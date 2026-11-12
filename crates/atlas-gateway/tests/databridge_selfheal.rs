// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 DataBridge self-heal: a stalled/errored CDC stream can be re-established. Drives a fake
//! plan to `cdc_streaming`, forces the stream to `error`, then the restart endpoint re-establishes it
//! (`streaming`) and bumps the restart counter. No cloud, no Kubernetes.

use std::net::SocketAddr;
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("databridge-selfheal").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "selfheal-test-secret-at-least-32-bytes!".into(),
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
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, pool)
}

async fn wait_state(c: &reqwest::Client, url: &str, want: &str) {
    for _ in 0..60 {
        if let Ok(r) = c.get(url).send().await {
            if r.json::<Value>().await.unwrap_or(Value::Null)["state"] == want {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {url} to reach '{want}'");
}

#[tokio::test]
async fn errored_cdc_stream_restarts() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // Register + discover a fake source, plan, and drive to cdc_streaming.
    let sid = c
        .post(format!("{base}/databridge/sources"))
        .json(&json!({ "name": "orders", "kind": "postgres", "cloud": "rds" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    c.post(format!("{base}/databridge/sources/{sid}/discover"))
        .send()
        .await
        .unwrap();
    wait_state(
        &c,
        &format!("{base}/databridge/sources/{sid}"),
        "discovered",
    )
    .await;
    let pid = c
        .post(format!("{base}/databridge/plans"))
        .json(&json!({ "name": "orders", "source_id": sid }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let plan_url = format!("{base}/databridge/plans/{pid}");
    for (stage, want) in [
        ("assess", "assessed"),
        ("provision", "provisioned"),
        ("full-load", "loaded"),
        ("cdc/start", "cdc_streaming"),
    ] {
        c.post(format!("{base}/databridge/plans/{pid}/{stage}"))
            .send()
            .await
            .unwrap();
        wait_state(&c, &plan_url, want).await;
    }

    // Force the stream into `error` (as the reconciler would on a broken connector).
    sqlx::query("UPDATE cdc_streams SET state='error' WHERE plan_id=$1")
        .bind(&pid)
        .execute(&pool)
        .await
        .unwrap();

    let stream_state = |c: &reqwest::Client, pid: &str| {
        let base = base.clone();
        let pid = pid.to_string();
        let c = c.clone();
        async move {
            let streams: Value = c
                .get(format!("{base}/databridge/cdc-streams"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            streams
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["plan_id"] == json!(pid))
                .cloned()
                .unwrap()
        }
    };
    assert_eq!(stream_state(&c, &pid).await["state"], "error");

    // Restart → the stream is re-established and the restart counter bumped.
    let r = c
        .post(format!("{base}/databridge/plans/{pid}/cdc/restart"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);

    let mut healed = None;
    for _ in 0..60 {
        let s = stream_state(&c, &pid).await;
        if s["state"] == "streaming" {
            healed = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = healed.expect("stream should return to streaming after restart");
    assert_eq!(s["restart_count"], 1, "restart should bump the counter");
}
