// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ai/incidents?mode=` — the optional LLM-narrated root-cause paragraph over Atlas's own
//! (always-computed, always-correct) incident correlation. All tests here mutate the process-wide
//! `ATLAS_AI_*` env vars `ProviderConfig::from_env` reads, so — like `vault_secrets.rs` — they
//! serialize on a shared mutex to avoid racing each other under cargo's default parallel-test
//! execution (both within this binary and, since env vars are process-wide, against any other
//! `#[tokio::test]` in this same binary; there are none here besides these four).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use tokio::sync::Mutex;

mod common;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("ai-incident-narrative").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "narrative-test-secret-key-at-least-32b".into(),
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

/// A single open critical alert so `correlate_incidents` returns exactly one incident — enough to
/// exercise the narrative path without asserting on its exact grouping logic (covered elsewhere).
async fn seed_one_incident(pool: &sqlx::AnyPool) {
    atlas_inventory::alerts::upsert_open(
        pool,
        "alert_narrative_test",
        "critical",
        "monitor",
        "cluster",
        "cls_1",
        "Cluster unhealthy",
        "Cluster cls_1 is HEALTH_ERR",
        &json!({}),
    )
    .await
    .unwrap();
}

/// A tiny OpenAI-chat-completions-compatible mock: always replies with a fixed narrative,
/// regardless of the request body (the body's *shape* — one untrusted-data system message plus a
/// JSON user message — is exercised implicitly: a malformed request would fail client-side
/// serialization before ever reaching this handler).
async fn spawn_mock_provider(reply: &'static str) -> String {
    async fn handler(State(reply): State<&'static str>) -> Json<Value> {
        Json(json!({ "choices": [{ "message": { "content": reply } }] }))
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(reply);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn set_provider_env(base_url: &str) {
    std::env::set_var("ATLAS_AI_BASE_URL", base_url);
    std::env::set_var("ATLAS_AI_MODEL", "mock-model");
    std::env::remove_var("ATLAS_AI_API_KEY");
}

fn clear_provider_env() {
    std::env::remove_var("ATLAS_AI_BASE_URL");
    std::env::remove_var("ATLAS_AI_MODEL");
    std::env::remove_var("ATLAS_AI_API_KEY");
}

/// Default `GET /ai/incidents` (no `mode`) never calls out, even when a provider happens to be
/// configured — narration is strictly opt-in here (unlike the advisor's own `auto`-by-default),
/// since this is a GET a dashboard might poll and an unrequested external call would be a real
/// surprise cost.
#[tokio::test]
async fn default_mode_never_calls_the_provider() {
    let _g = ENV_LOCK.lock().await;
    let (addr, pool) = spawn().await;
    seed_one_incident(&pool).await;
    let mock = spawn_mock_provider("should never be requested").await;
    set_provider_env(&mock);

    let body: Value = reqwest::Client::new()
        .get(format!("http://{addr}/api/atlas/v1/ai/incidents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    clear_provider_env();
    assert!(body["count"].as_u64().unwrap() >= 1, "{body}");
    assert!(body["narrative"].is_null(), "{body}");
    assert!(body["narrative_mode"].is_null(), "{body}");
}

/// `?mode=llm` with a configured (mocked) provider returns its narrative verbatim.
#[tokio::test]
async fn llm_mode_returns_provider_narrative() {
    let _g = ENV_LOCK.lock().await;
    let (addr, pool) = spawn().await;
    seed_one_incident(&pool).await;
    let mock = spawn_mock_provider("cls_1 is unhealthy; investigate OSD recovery first.").await;
    set_provider_env(&mock);

    let body: Value = reqwest::Client::new()
        .get(format!("http://{addr}/api/atlas/v1/ai/incidents?mode=llm"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    clear_provider_env();
    assert_eq!(body["narrative_mode"], "llm", "{body}");
    assert_eq!(
        body["narrative"], "cls_1 is unhealthy; investigate OSD recovery first.",
        "{body}"
    );
}

/// `?mode=auto` with NO provider configured behaves exactly like the default — no error, no
/// narrative — since `auto` means "use it if available", not "require it".
#[tokio::test]
async fn auto_mode_without_a_provider_omits_narrative_without_erroring() {
    let _g = ENV_LOCK.lock().await;
    let (addr, pool) = spawn().await;
    seed_one_incident(&pool).await;
    clear_provider_env();

    let r = reqwest::Client::new()
        .get(format!("http://{addr}/api/atlas/v1/ai/incidents?mode=auto"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["narrative"].is_null(), "{body}");
    assert!(body["narrative_mode"].is_null(), "{body}");
}

/// `?mode=llm` with NO provider configured is an error (unlike `auto`) — the caller explicitly
/// asked for a narrative and none can be produced.
#[tokio::test]
async fn llm_mode_without_a_provider_errors() {
    let _g = ENV_LOCK.lock().await;
    let (addr, pool) = spawn().await;
    seed_one_incident(&pool).await;
    clear_provider_env();

    let r = reqwest::Client::new()
        .get(format!("http://{addr}/api/atlas/v1/ai/incidents?mode=llm"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "{}", r.text().await.unwrap());
}
