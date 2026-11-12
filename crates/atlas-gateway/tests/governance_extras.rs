// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 governance leftovers: audit CSV export + retention prune, per-tenant chargeback, and policy
//! drift detection. Fake driver, no infra.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("governance-extras").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "gov2-test-secret-at-least-32-bytes-ok!".into(),
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

#[tokio::test]
async fn audit_export_and_retention() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    atlas_inventory::audit::record(
        &pool,
        None,
        "me",
        "test.action",
        "res",
        "r1",
        "ok",
        None,
        None,
    )
    .await
    .unwrap();

    // CSV export includes the row + a text/csv content type.
    let r = c.get(format!("{base}/audit.csv")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/csv"));
    let csv = r.text().await.unwrap();
    assert!(csv.contains("test.action"), "csv: {csv}");

    // Retention: an old row is pruned; the recent one survives.
    sqlx::query(
        "INSERT INTO storage_audit_logs (actor_id, action, resource_type, resource_id, status, created_at)
         VALUES ('me', 'old.action', 'res', 'r0', 'ok', '2000-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let pruned = atlas_inventory::audit::prune(&pool, 30).await.unwrap();
    assert!(pruned >= 1, "the year-2000 row should be pruned");
    let after = atlas_inventory::audit::list(&pool, None, None, None, None, 100)
        .await
        .unwrap();
    assert!(after.iter().all(|a| a["action"] != "old.action"));
}

#[tokio::test]
async fn chargeback_reports_tenant_usage() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    atlas_inventory::tenants::set_quota(&pool, "acme", 10_000_000_000, 100)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state)
         VALUES ('v1', 'acme', 'bkd_ceph_lab', 'db', 'block', 2147483648, 'bound')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let cb: Value = c
        .get(format!("{base}/chargeback"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let acme = cb["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tenant_id"] == "acme")
        .expect("acme in chargeback");
    assert_eq!(acme["used_bytes"], 2147483648i64);
    assert_eq!(acme["used_gib"], 2.0);
}

#[tokio::test]
async fn policy_drift_flags_deleted_policy() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // A volume assigned a policy that doesn't exist → drift.
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, policy_id, storage_class_name)
         VALUES ('v1', 't', 'bkd_ceph_lab', 'db', 'block', 1073741824, 'bound', 'pol_gone', 'zyvor-rbd-prod')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let drift: Value = c
        .get(format!("{base}/policy-drift"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(drift["count"], 1, "{drift}");
    assert_eq!(drift["drift"][0]["volume_id"], "v1");
    assert_eq!(drift["drift"][0]["reason"], "policy deleted");
}
