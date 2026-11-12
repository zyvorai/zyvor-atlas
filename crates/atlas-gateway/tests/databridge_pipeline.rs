// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! End-to-end test for the DataBridge migration pipeline, driven over REST with the fake source
//! connector (no cloud, no Kubernetes). Walks a plan through every stage and asserts the state
//! machine + the cutover guard.

use std::net::SocketAddr;
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("databridge-pipeline").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "test-secret".into(),
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

/// Poll `GET {url}` until its `.state` equals `want`, up to ~3s. Panics with the last state on timeout.
async fn wait_state(c: &reqwest::Client, url: &str, want: &str) {
    let mut last = String::new();
    for _ in 0..60 {
        if let Ok(r) = c.get(url).send().await {
            let v: Value = r.json().await.unwrap_or(Value::Null);
            last = v["state"].as_str().unwrap_or("").to_string();
            if last == want {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {url} to reach '{want}' (last: '{last}')");
}

#[tokio::test]
async fn full_pipeline_fake() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // 1. register + discover source
    let src: Value = c
        .post(format!("{base}/databridge/sources"))
        .json(&json!({ "name": "orders", "kind": "postgres", "cloud": "rds", "endpoint": "prod.rds.aws" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sid = src["id"].as_str().unwrap().to_string();
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

    // 2. create plan
    let plan: Value = c
        .post(format!("{base}/databridge/plans"))
        .json(&json!({ "name": "orders migration", "source_id": sid }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = plan["id"].as_str().unwrap().to_string();
    let plan_url = format!("{base}/databridge/plans/{pid}");

    // 3. walk the pipeline through validate
    for (stage, want) in [
        ("assess", "assessed"),
        ("provision", "provisioned"),
        ("full-load", "loaded"),
        ("cdc/start", "cdc_streaming"),
        ("validate", "validated"),
    ] {
        c.post(format!("{base}/databridge/plans/{pid}/{stage}"))
            .send()
            .await
            .unwrap();
        wait_state(&c, &plan_url, want).await;
    }

    // readiness score was persisted (audit_log has no PK -> 90, not 100)
    let p: Value = c.get(&plan_url).send().await.unwrap().json().await.unwrap();
    assert_eq!(p["readiness_score"], 90);

    // 4. cutover guard: CDC lag starts at 45s (> threshold) -> 409
    let blocked = c
        .post(format!("{base}/databridge/plans/{pid}/cutover"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        blocked.status(),
        409,
        "cutover must be blocked while CDC lag is high"
    );

    // 5. simulate the reconciler draining CDC lag to zero, then cutover succeeds
    sqlx::query("UPDATE cdc_streams SET lag_seconds = 0, lag_bytes = 0")
        .execute(&pool)
        .await
        .unwrap();
    let ok = c
        .post(format!("{base}/databridge/plans/{pid}/cutover"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        202,
        "cutover should be accepted once lag is drained"
    );
    wait_state(&c, &plan_url, "cutover_complete").await;

    // 6. rollback within the window
    let rb = c
        .post(format!("{base}/databridge/plans/{pid}/rollback"))
        .send()
        .await
        .unwrap();
    assert_eq!(rb.status(), 202);
    wait_state(&c, &plan_url, "rolled_back").await;

    // a validation run recorded, all tables matched
    let vals: Value = c
        .get(format!("{base}/databridge/validations?plan_id={pid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(vals[0]["state"], "passed");
    assert_eq!(vals[0]["tables_mismatched"], 0);
}
