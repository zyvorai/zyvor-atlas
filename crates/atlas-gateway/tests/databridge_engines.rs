// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Every DataBridge source engine drives the full migration pipeline over REST with the fake
//! connector, and each provisions the correct edge target. Complements `databridge_pipeline.rs`
//! (which covers the Postgres state machine + cutover guard in depth) by proving all six engines —
//! including the heterogeneous Oracle/SQL Server → Postgres and the MongoDB → PSMDB routes — reach
//! `validated` and land on the expected operator. No cloud, no Kubernetes.

use std::net::SocketAddr;
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("databridge-engines").await;
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
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

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

/// One row per engine: registered `kind`, the engine label discovery should report, and the edge
/// operator + engine the pipeline should provision onto.
struct Case {
    kind: &'static str,
    discovered_engine: &'static str,
    edge_operator: &'static str,
    edge_engine: &'static str,
}

const CASES: &[Case] = &[
    Case {
        kind: "postgres",
        discovered_engine: "postgres",
        edge_operator: "cnpg",
        edge_engine: "postgres",
    },
    Case {
        kind: "mysql",
        discovered_engine: "mysql",
        edge_operator: "percona",
        edge_engine: "mysql",
    },
    Case {
        kind: "mariadb",
        discovered_engine: "mariadb",
        edge_operator: "percona",
        edge_engine: "mysql",
    },
    // Heterogeneous: Oracle / SQL Server land on a Postgres (CNPG) edge.
    Case {
        kind: "oracle",
        discovered_engine: "oracle",
        edge_operator: "cnpg",
        edge_engine: "postgres",
    },
    Case {
        kind: "sqlserver",
        discovered_engine: "sqlserver",
        edge_operator: "cnpg",
        edge_engine: "postgres",
    },
    // Document: MongoDB lands on Percona Server for MongoDB.
    Case {
        kind: "mongodb",
        discovered_engine: "mongodb",
        edge_operator: "psmdb",
        edge_engine: "mongodb",
    },
];

#[tokio::test]
async fn every_engine_runs_pipeline_and_routes_to_its_edge() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    for case in CASES {
        // 1. register + discover
        let src: Value = c
            .post(format!("{base}/databridge/sources"))
            .json(
                &json!({ "name": format!("{}-src", case.kind), "kind": case.kind, "cloud": "rds" }),
            )
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

        let discovered: Value = c
            .get(format!("{base}/databridge/sources/{sid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            discovered["discovered"]["engine"], case.discovered_engine,
            "{} should discover engine {}",
            case.kind, case.discovered_engine
        );

        // 2. plan + walk to validated
        let plan: Value = c
            .post(format!("{base}/databridge/plans"))
            .json(&json!({ "name": format!("{}-plan", case.kind), "source_id": sid }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pid = plan["id"].as_str().unwrap().to_string();
        let plan_url = format!("{base}/databridge/plans/{pid}");
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

        // 3. the edge cluster provisioned for this plan is on the expected operator/engine
        let edges: Value = c
            .get(format!("{base}/databridge/edge-clusters"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let edge = edges
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["plan_id"] == json!(pid))
            .unwrap_or_else(|| panic!("{}: no edge cluster for plan {pid}", case.kind));
        assert_eq!(
            edge["operator"], case.edge_operator,
            "{} should provision onto operator {}",
            case.kind, case.edge_operator
        );
        assert_eq!(
            edge["engine"], case.edge_engine,
            "{} edge engine should be {}",
            case.kind, case.edge_engine
        );
        assert_eq!(edge["state"], "ready", "{} edge should be ready", case.kind);
    }
}
