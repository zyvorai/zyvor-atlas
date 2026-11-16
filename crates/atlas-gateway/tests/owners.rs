// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Owner registry + workflow intent routes (local stubs; no product RPC).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("owners").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "owners-test-secret-key-at-least-32-bytes!".into(),
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

#[tokio::test]
async fn owners_list_includes_suite_wire_ids_and_workflows_round_trip() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let list: Value = c
        .get(format!("{base}/owners"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let wires: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["wire_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        wires,
        [
            "hyper2kvm",
            "machina",
            "veyron",
            "guestkit",
            "packetwolf",
            "fabric",
            "netra",
            "kavira",
        ]
    );

    let missing = c
        .get(format!("{base}/owners/transiva/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let post = c
        .post(format!("{base}/owners/netra/workflow"))
        .json(&serde_json::json!({ "intent": "observe", "note": "suite smoke" }))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);
    let created: Value = post.json().await.unwrap();
    assert_eq!(created["owner_id"], "netra");
    assert_eq!(created["intent"], "observe");
    assert_eq!(created["called_remote"], false);

    let listed: Value = c
        .get(format!("{base}/owners/netra/workflows"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["owner_id"], "netra");
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["intent"], "observe");
    assert_eq!(items[0]["note"], "suite smoke");

    let empty: Value = c
        .get(format!("{base}/owners/fabric/workflows"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["items"].as_array().unwrap().is_empty());
}
