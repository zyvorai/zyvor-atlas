// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 governance: per-actor rate limiting. In its own test binary so setting the
//! `ATLAS_RATE_LIMIT_RPM` env var doesn't race other tests. Auth off → all requests share the
//! `anonymous` bucket, so flooding one client past the limit trips 429.

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};

mod common;

fn config(database_url: String) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "rl-test-secret-key-at-least-32-bytes!!!".into(),
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
    }
}

#[tokio::test]
async fn rate_limit_trips_429() {
    let database_url = common::fresh_database_url("ratelimit").await;
    // build_state reads ATLAS_RATE_LIMIT_RPM at startup; set it before, clear it after (value captured).
    std::env::set_var("ATLAS_RATE_LIMIT_RPM", "5");
    let state = build_state(
        config(database_url),
        BuildOptions {
            enable_k8s: false,
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .unwrap();
    std::env::remove_var("ATLAS_RATE_LIMIT_RPM");

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Counters reset on each wall-clock minute; 12 requests straddling one would be allowed twice.
    let into_minute = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        % 60.0;
    if into_minute > 55.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(60.5 - into_minute)).await;
    }

    let c = reqwest::Client::new();
    let mut ok = 0;
    let mut limited = 0;
    for _ in 0..12 {
        let r = c
            .get(format!("http://{addr}/api/atlas/v1/alerts"))
            .send()
            .await
            .unwrap();
        match r.status().as_u16() {
            200 => ok += 1,
            429 => limited += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(ok, 5, "the first 5 requests/min are allowed");
    assert_eq!(limited, 7, "the rest are rate-limited (429)");
}
