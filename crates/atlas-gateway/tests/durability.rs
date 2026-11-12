// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Control-plane durability: a job left mid-flight when the process dies must not stay stuck. On the
//! next boot the job engine recovers — an interrupted `running` job is failed-safe and any `queued`
//! job is re-enqueued. Driven through `build_state` (the real startup path) against the fake driver.

use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::startup::{build_state, BuildOptions};

mod common;

fn config_for(database_url: &str) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url: database_url.to_string(),
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "dur-test-secret-key-at-least-32-bytes!".into(),
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

async fn boot(database_url: &str) -> atlas_gateway::state::AppState {
    build_state(
        config_for(database_url),
        BuildOptions {
            enable_k8s: false,
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state")
}

/// A `running` job that was interrupted by a restart is reset to `failed` (never stuck) on next boot.
#[tokio::test]
async fn interrupted_running_job_is_recovered_on_restart() {
    let database_url = common::fresh_database_url("durability").await;

    // First boot: a job is mid-flight (running) when the process "crashes". Seed it directly in the
    // `running` state (not via insert_job's `pending`→`running`, whose brief `pending` window the
    // boot-recovery could otherwise grab and re-run — we want to observe the fail-safe path only).
    let s1 = boot(&database_url).await;
    sqlx::query(
        "INSERT INTO storage_jobs (id, tenant_id, job_type, state, requested_by, request)
         VALUES ('j_stuck', 't', 'volume.create', 'running', 'tester', '{}')",
    )
    .execute(&s1.pool)
    .await
    .unwrap();
    drop(s1); // simulate the crash / rollout

    // Second boot on the same DB: JobEngine::start runs recovery.
    let s2 = boot(&database_url).await;
    let mut last = String::new();
    for _ in 0..40 {
        if let Some(j) = atlas_inventory::jobs::get_job(&s2.pool, "j_stuck")
            .await
            .unwrap()
        {
            last = j.state.clone();
            if last == "failed" {
                assert!(
                    j.error.unwrap_or_default().contains("interrupted"),
                    "recovered job should note it was interrupted"
                );
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("interrupted job was not recovered (last state: '{last}')");
}
