// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 alerting maturity: the new monitor rules (failed jobs, CDC replication error, tenant quota
//! approaching) raise alerts on conditions that previously failed silently, and the manual lifecycle
//! (ack / silence / resolve) works end-to-end. Fake driver, no infra.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("alerting").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "alert-test-secret-key-at-least-32-byte".into(),
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

fn ids(alerts: &Value) -> Vec<String> {
    alerts
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_str().map(String::from))
        .collect()
}

/// The three new rules raise alerts for conditions that used to fail silently.
#[tokio::test]
async fn new_rules_raise_alerts() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // 1. a recently-failed job
    atlas_inventory::jobs::insert_job(
        &pool,
        "j1",
        "acme",
        "volume.create",
        "me",
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap();
    atlas_inventory::jobs::mark_failed(&pool, "j1", "boom")
        .await
        .unwrap();

    // 2. a CDC stream in error
    atlas_inventory::databridge::cdc::insert_stream(
        &pool, "cdc1", "acme", "plan1", "postgres", "connect1", "src1", "db1",
    )
    .await
    .unwrap();
    atlas_inventory::databridge::cdc::set_state(&pool, "cdc1", "error")
        .await
        .unwrap();

    // 3. a tenant at 90% of its byte quota
    atlas_inventory::tenants::set_quota(&pool, "acme", 1000, 100)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state)
         VALUES ('v1', 'acme', 'bkd_ceph_lab', 'db', 'block', 900, 'ready')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Evaluate the rules on demand.
    let r = c
        .post(format!("{base}/alerts/evaluate"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let open: Value = c
        .get(format!("{base}/alerts?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let open_ids = ids(&open);
    assert!(
        open_ids.contains(&"alert_jobs_failing".to_string()),
        "jobs rule: {open_ids:?}"
    );
    assert!(
        open_ids.contains(&"alert_cdc_error_cdc1".to_string()),
        "cdc rule: {open_ids:?}"
    );
    assert!(
        open_ids.contains(&"alert_tenant_quota_acme".to_string()),
        "quota rule: {open_ids:?}"
    );
}

/// Ack, silence, and manual resolve work; a silenced alert is skipped by the webhook notifier.
#[tokio::test]
async fn ack_silence_resolve_lifecycle() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // Produce one alert to act on.
    atlas_inventory::jobs::insert_job(
        &pool,
        "j1",
        "acme",
        "volume.create",
        "me",
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap();
    atlas_inventory::jobs::mark_failed(&pool, "j1", "boom")
        .await
        .unwrap();
    c.post(format!("{base}/alerts/evaluate"))
        .send()
        .await
        .unwrap();
    let id = "alert_jobs_failing";

    // Acknowledge.
    let ack = c
        .post(format!("{base}/alerts/{id}/ack"))
        .send()
        .await
        .unwrap();
    assert_eq!(ack.status(), 200);
    let ack_body: Value = ack.json().await.unwrap();
    assert_eq!(
        ack_body["acknowledged_by"], "anonymous",
        "ack response should name the acking actor"
    );
    // The ack must actually be persisted, not just accepted — fetch it back and check the row.
    let acked: Value = c
        .get(format!("{base}/alerts?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = acked
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .expect("acked alert should still be open and listed");
    assert_eq!(
        row["acknowledged_by"], "anonymous",
        "alert record should persist who acknowledged it"
    );
    assert!(
        !row["acknowledged_at"].is_null(),
        "alert record should persist when it was acknowledged"
    );

    // Silence for 60s → the notifier should skip it.
    let sil = c
        .post(format!("{base}/alerts/{id}/silence?secs=60"))
        .send()
        .await
        .unwrap();
    assert_eq!(sil.status(), 200);
    let unnotified = atlas_inventory::alerts::list_unnotified_open(&pool)
        .await
        .unwrap();
    assert!(
        !unnotified.iter().any(|a| a.id == id),
        "a silenced alert must be excluded from webhook delivery"
    );

    // Acking an unknown alert → 404.
    let missing = c
        .post(format!("{base}/alerts/does_not_exist/ack"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // Manual resolve.
    let res = c
        .post(format!("{base}/alerts/{id}/resolve"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let open: Value = c
        .get(format!("{base}/alerts?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !ids(&open).contains(&id.to_string()),
        "resolved alert should not be open"
    );
}
