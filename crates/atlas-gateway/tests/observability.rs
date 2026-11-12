// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Fake-driver coverage for the observability / read surface and cross-cutting guards that the
//! existing suites don't touch: Ceph-native passthrough, Prometheus + JSON metrics, the unified
//! events/audit feeds, `/readyz`, per-tenant quota admission (409), and NFS/ZFS drivers flowing
//! through discovery → inventory → REST. No Ceph, no Kubernetes.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

/// Knobs for the handful of variations the tests need.
#[derive(Default, Clone, Copy)]
struct Opts {
    auth_required: bool,
    initial_discovery: bool,
    extra_backends: bool, // enable the NFS + ZFS drivers
}

fn base_config(database_url: String, o: Opts) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "obs-test-secret-key-at-least-32-bytes!".into(),
        jwt_secret_previous: None,
        auth_required: o.auth_required,
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
        nfs_enable: o.extra_backends,
        nfs_server: None,
        nfs_exports: Vec::new(),
        nfs_driver_mode: atlas_common::config::DriverMode::Fake,
        zfs_enable: o.extra_backends,
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

async fn spawn_with(o: Opts) -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("observability").await;
    let state = build_state(
        base_config(database_url, o),
        BuildOptions {
            enable_k8s: false,
            initial_discovery: o.initial_discovery,
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

async fn spawn() -> SocketAddr {
    spawn_with(Opts {
        initial_discovery: true,
        ..Default::default()
    })
    .await
    .0
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// The four Ceph-native passthrough endpoints return the FakeCephDriver's canned JSON (200 + body).
#[tokio::test]
async fn ceph_native_passthrough_returns_json() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = client();
    for path in ["ceph/status", "ceph/osd-tree", "ceph/df", "ceph/osd-df"] {
        let r = c.get(format!("{base}/{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path} should be 200");
        let v: Value = r.json().await.unwrap();
        assert!(v.is_object() || v.is_array(), "{path} should return JSON");
    }
    // ceph status is the HEALTH_WARN fixture (1 OSD down) — check the actual value, not just presence.
    let status: Value = c
        .get(format!("{base}/ceph/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["health"]["status"], "HEALTH_WARN",
        "fixture cluster should report HEALTH_WARN: {status}"
    );
    assert_eq!(
        status["osdmap"]["num_up_osds"], 5,
        "fixture has exactly 1 OSD down: {status}"
    );
}

/// `GET /metrics` (Prometheus text) exposes the atlas_* gauges when auth is disabled (dev default).
#[tokio::test]
async fn prometheus_text_metrics_exposed() {
    let addr = spawn().await;
    let body = client()
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "atlas_build_info",
        "atlas_pools",
        "atlas_volumes",
        "atlas_alerts_open",
    ] {
        assert!(body.contains(needle), "/metrics should expose {needle}");
    }
}

/// `GET /metrics` leaks backend names, volume/pool counts, and capacity — it must require the same
/// bearer token as the rest of the API once `ATLAS_AUTH_REQUIRED=1`, not be scrapeable anonymously.
#[tokio::test]
async fn prometheus_text_metrics_requires_auth_when_required() {
    let addr = spawn_with(Opts {
        auth_required: true,
        initial_discovery: true,
        ..Default::default()
    })
    .await
    .0;
    let status = client()
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(
        status, 401,
        "/metrics should reject anonymous scrapes once auth is required"
    );
}

/// The JSON metrics endpoints all answer 200 (ceph/history are empty-but-OK without a scrape/sampler).
#[tokio::test]
async fn json_metrics_endpoints_ok() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = client();
    for path in [
        "metrics/summary",
        "metrics/ceph",
        "metrics/history?minutes=60",
        "metrics/forecast",
    ] {
        let r = c.get(format!("{base}/{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path} should be 200");
    }
    // summary reflects the fake cluster (1 cluster, 3 pools from discovery).
    let summary: Value = c
        .get(format!("{base}/metrics/summary"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary["clusters"], 1);
    assert_eq!(summary["pools"], 3);
}

/// The Ops Advisor works without a model and exposes only explainable, read-only recommendations.
#[tokio::test]
async fn ai_advisor_local_mode_is_safe_and_explainable() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let r = client()
        .post(format!("{base}/ai/advisor"))
        .json(&serde_json::json!({
            "question": "What should the storage team handle first?",
            "mode": "local"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["mode"], "local");
    assert_eq!(body["can_execute"], false);
    assert!(body["risk_score"].as_u64().is_some());
    assert!(body["evidence"].is_object());
    let actions = body["actions"].as_array().expect("advisor actions");
    assert!(!actions.is_empty());
    assert!(actions.iter().all(|a| a["inspect"]
        .as_str()
        .is_some_and(|path| path.starts_with("/api/atlas/v1/"))));
}

/// Cluster-wide telemetry must not be exported or summarized for a read-only viewer.
#[tokio::test]
async fn ai_advisor_requires_operator_when_auth_is_enabled() {
    let secret = "obs-test-secret-key-at-least-32-bytes!";
    let (addr, _) = spawn_with(Opts {
        auth_required: true,
        initial_discovery: true,
        ..Default::default()
    })
    .await;
    let url = format!("http://{addr}/api/atlas/v1/ai/advisor");
    let c = client();
    let (viewer, _, _) =
        atlas_gateway::auth::mint_token(secret, "viewer", "viewer", "global", 600).unwrap();
    let (operator, _, _) =
        atlas_gateway::auth::mint_token(secret, "operator", "operator", "global", 600).unwrap();

    let denied = c
        .post(&url)
        .bearer_auth(viewer)
        .json(&serde_json::json!({ "mode": "local" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    let allowed = c
        .post(&url)
        .bearer_auth(operator)
        .json(&serde_json::json!({ "mode": "local" }))
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
}

/// Related alerts are grouped into one explainable incident instead of producing alert fatigue.
#[tokio::test]
async fn ai_incidents_correlates_related_alerts() {
    let (addr, pool) = spawn_with(Opts {
        initial_discovery: true,
        ..Default::default()
    })
    .await;
    for (id, source, title, severity) in [
        ("cap-1", "capacity", "Pool near full", "warning"),
        ("cap-2", "forecast", "Capacity forecast at risk", "critical"),
    ] {
        atlas_inventory::alerts::upsert_open(
            &pool,
            id,
            severity,
            source,
            "pool",
            "pool-a",
            title,
            title,
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    }

    let body: Value = client()
        .get(format!("http://{addr}/api/atlas/v1/ai/incidents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["can_execute"], false);
    let capacity = body["incidents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|incident| incident["category"] == "capacity")
        .expect("capacity incident");
    assert_eq!(capacity["severity"], "critical");
    assert_eq!(capacity["signals"].as_array().unwrap().len(), 2);
}

/// What-if planning projects risk without mutating the fake-driver inventory.
#[tokio::test]
async fn ai_what_if_projects_capacity_and_validates_inputs() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = client();
    let body: Value = c
        .post(format!("{base}/ai/what-if"))
        .json(&serde_json::json!({
            "add_capacity_bytes": 1_099_511_627_776_i64,
            "horizon_days": 90,
            "projected_growth_bytes_per_day": 1_073_741_824_i64,
            "assume_alerts_resolved": true,
            "assume_recovery_complete": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["horizon_days"], 90);
    assert_eq!(body["can_execute"], false);
    assert!(body["baseline"]["risk_score"].as_u64().is_some());
    assert!(body["projected"]["capacity_used_percent"]
        .as_f64()
        .is_some());

    let invalid = c
        .post(format!("{base}/ai/what-if"))
        .json(&serde_json::json!({ "add_capacity_bytes": -1, "horizon_days": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
}

/// The anomaly endpoint uses counter deltas and a robust baseline to surface a latest-interval
/// spike without needing an external model or training data.
#[tokio::test]
async fn ai_anomalies_detects_a_write_spike() {
    let (addr, pool) = spawn_with(Opts::default()).await;
    let mib = 1_048_576_i64;
    for (index, writes) in [0, mib, mib * 2, mib * 3, mib * 20].into_iter().enumerate() {
        let minutes_ago = 5 - index as i64;
        let ts = atlas_inventory::now_rfc3339(
            chrono::Utc::now() - chrono::Duration::minutes(minutes_ago),
        );
        sqlx::query(
            "INSERT INTO metrics_history
             (ts, raw_capacity_bytes, used_capacity_bytes, volumes, snapshots, read_bytes,
              write_bytes, read_ops, write_ops, jobs_running, alerts_open)
             VALUES ($1, 1000000000, 100000000,
                     1, 0, 0, $2, 0, $3, 1, 0)",
        )
        .bind(ts)
        .bind(writes as f64)
        .bind(writes as f64)
        .execute(&pool)
        .await
        .unwrap();
    }

    let base = format!("http://{addr}/api/atlas/v1/ai/anomalies");
    let body: Value = client()
        .get(format!("{base}?minutes=60&sensitivity=3.5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["model"], "robust_median_mad_v1");
    assert_eq!(body["sample_count"], 5);
    assert_eq!(body["can_execute"], false);
    assert!(body["anomalies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|anomaly| anomaly["metric"] == "write_bytes"));

    let invalid = client()
        .get(format!("{base}?minutes=60&sensitivity=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
}

/// `/readyz` is a deep check: it probes the actual driver (not a hardcoded ok) and reports worker
/// heartbeats; `/livez` is the shallow always-alive signal; `/version` names the service.
#[tokio::test]
async fn readyz_probes_driver_and_livez_is_alive() {
    let addr = spawn().await;
    let c = client();

    // livez: process liveness, always 200 and distinct from readiness.
    let live = c.get(format!("http://{addr}/livez")).send().await.unwrap();
    assert_eq!(live.status(), 200);
    assert_eq!(live.json::<Value>().await.unwrap()["status"], "alive");

    // readyz: 200, driver genuinely probed (fake reports HEALTH_WARN — one simulated OSD down,
    // consistent with cluster()/osds()/ceph_status()), workers component present. Warn still
    // counts as ready (readyz treats Ok|Warn as reachable-and-serving), so `ok` stays true.
    let r = c.get(format!("http://{addr}/readyz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let driver = &v["components"]["ceph_driver"];
    assert_eq!(driver["mode"], "fake");
    assert_eq!(driver["ok"], true);
    assert_eq!(driver["status"], "warn"); // real probe result, not a hardcoded true
    assert!(v["components"]["workers"].is_array());

    let ver: Value = c
        .get(format!("http://{addr}/version"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ver["name"], "atlas-gateway");
}

/// The unified `/events` and `/audit` feeds are operator-gated: viewer → 403, operator → 200,
/// no token → 401.
#[tokio::test]
async fn events_and_audit_are_operator_gated() {
    let secret = "obs-test-secret-key-at-least-32-bytes!";
    let (addr, _) = spawn_with(Opts {
        auth_required: true,
        ..Default::default()
    })
    .await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = client();
    let (op, _, _) =
        atlas_gateway::auth::mint_token(secret, "svc", "operator", "global", 600).unwrap();
    let (vw, _, _) =
        atlas_gateway::auth::mint_token(secret, "svc", "viewer", "global", 600).unwrap();

    for feed in ["events", "audit"] {
        let anon = c.get(format!("{base}/{feed}")).send().await.unwrap();
        assert_eq!(anon.status(), 401, "{feed} without a token → 401");

        let viewer = c
            .get(format!("{base}/{feed}"))
            .bearer_auth(&vw)
            .send()
            .await
            .unwrap();
        assert_eq!(viewer.status(), 403, "{feed} as viewer → 403");

        let operator = c
            .get(format!("{base}/{feed}"))
            .bearer_auth(&op)
            .send()
            .await
            .unwrap();
        assert_eq!(operator.status(), 200, "{feed} as operator → 200");
    }
}

/// Per-tenant quota admission rejects an over-limit volume with 409 before any job is enqueued —
/// pure gateway + SQLite logic, no cluster needed.
#[tokio::test]
async fn quota_admission_rejects_oversized_volume() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = client();
    let gib = 1_073_741_824i64;

    // Cap tenant "t" at 1 GiB.
    let q = c
        .put(format!("{base}/tenants/t/quota"))
        .json(&serde_json::json!({ "max_bytes": gib, "max_volumes": 100 }))
        .send()
        .await
        .unwrap();
    assert_eq!(q.status(), 200, "setting quota should succeed");

    // A 2 GiB volume for tenant "t" exceeds the byte quota → 409, no job created.
    let over = c
        .post(format!("{base}/volumes"))
        .json(&serde_json::json!({
            "tenant_id": "t", "name": "too-big", "size_bytes": 2 * gib,
            "policy": "database", "kubernetes": { "namespace": "default" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), 409, "over-quota volume must be rejected");

    // The rejection must happen before enqueue — no job should exist for tenant "t" yet.
    let jobs_after_reject: Value = c
        .get(format!("{base}/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        jobs_after_reject
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["tenant_id"] != "t"),
        "an over-quota create must not enqueue a job: {jobs_after_reject}"
    );

    // A within-quota volume is admitted (202 — the job then fails without k8s, which is fine here).
    let ok = c
        .post(format!("{base}/volumes"))
        .json(&serde_json::json!({
            "tenant_id": "t", "name": "small", "size_bytes": gib / 2,
            "policy": "database", "kubernetes": { "namespace": "default" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 202, "within-quota volume should be accepted");
}

/// The NFS + ZFS pluggable drivers flow through discovery → inventory → REST: their pools
/// (`nfs_export` / `zpool`) and filesystem volumes appear on `/pools` and `/volumes`.
#[tokio::test]
async fn nfs_and_zfs_backends_surface_over_http() {
    let (addr, _) = spawn_with(Opts {
        initial_discovery: true,
        extra_backends: true,
        ..Default::default()
    })
    .await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = client();

    let pools: Value = c
        .get(format!("{base}/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<&str> = pools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"nfs_export"),
        "NFS export pool should be discovered: {kinds:?}"
    );
    assert!(
        kinds.contains(&"zpool"),
        "ZFS zpool should be discovered: {kinds:?}"
    );

    // Each driver contributes at least one filesystem volume — filter the inventory by backend.
    for backend in ["bkd_nfs_lab", "bkd_zfs_lab"] {
        let vols: Value = c
            .get(format!("{base}/volumes?backend={backend}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let list = vols.as_array().unwrap();
        assert!(!list.is_empty(), "{backend} should contribute volumes");
        assert!(
            list.iter().any(|v| v["kind"] == "filesystem"),
            "{backend} volumes should be filesystem shares: {list:?}"
        );
    }
}
