// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Integration tests for `startup::resolve_vault_secrets` against a local mock Vault KV v2
//! endpoint — exercises the real HTTP round trip and header/path shape, not just the parsing
//! logic. Tests mutate process-wide `ATLAS_*` env vars, so they run serially (`#[serial]`-style
//! via a shared mutex) to avoid racing each other.

use tokio::sync::Mutex;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::startup::resolve_vault_secrets;
use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

/// Serializes the tests in this file — they all mutate the same `ATLAS_VAULT_*`/
/// `ATLAS_SECRETS_BACKEND` process env vars, which would otherwise race under cargo's default
/// parallel-test-per-thread execution.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn base_config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url: "sqlite://:memory:".into(),
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "unresolved-dev-secret".into(),
        jwt_secret_previous: None,
        auth_required: true,
        bootstrap_admin_token: None,
        admin_username: "admin".into(),
        admin_password: "unresolved-dev-password".into(),
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

/// A mock Vault KV v2 endpoint: replies with `body` to any GET carrying `expected_token` in
/// `X-Vault-Token`, 403 otherwise.
async fn spawn_mock_vault(body: Value, expected_token: &'static str) -> String {
    async fn handler(
        State((body, expected_token)): State<(Value, &'static str)>,
        headers: axum::http::HeaderMap,
    ) -> Result<Json<Value>, axum::http::StatusCode> {
        let token = headers
            .get("X-Vault-Token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if token != expected_token {
            return Err(axum::http::StatusCode::FORBIDDEN);
        }
        Ok(Json(body))
    }
    let app = Router::new()
        .route("/v1/secret/data/atlas/gateway-auth", get(handler))
        .with_state((body, expected_token));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn clear_vault_env() {
    for k in [
        "ATLAS_SECRETS_BACKEND",
        "ATLAS_VAULT_ADDR",
        "ATLAS_VAULT_TOKEN",
        "ATLAS_VAULT_SECRET_PATH",
    ] {
        std::env::remove_var(k);
    }
}

#[tokio::test]
async fn defaults_to_env_backend_as_a_no_op() {
    let _g = ENV_LOCK.lock().await;
    clear_vault_env();
    let mut cfg = base_config();
    resolve_vault_secrets(&mut cfg).await.unwrap();
    assert_eq!(cfg.jwt_secret, "unresolved-dev-secret");
    assert_eq!(cfg.admin_password, "unresolved-dev-password");
}

#[tokio::test]
async fn vault_backend_overwrites_jwt_and_admin_password() {
    let _g = ENV_LOCK.lock().await;
    clear_vault_env();
    let base = spawn_mock_vault(
        json!({
            "data": {
                "data": {
                    "jwt-secret": "vault-jwt-secret-32-bytes-minimum!!",
                    "admin-password": "vault-sourced-admin-password"
                }
            }
        }),
        "test-vault-token",
    )
    .await;

    std::env::set_var("ATLAS_SECRETS_BACKEND", "vault");
    std::env::set_var("ATLAS_VAULT_ADDR", &base);
    std::env::set_var("ATLAS_VAULT_TOKEN", "test-vault-token");
    std::env::set_var("ATLAS_VAULT_SECRET_PATH", "secret/data/atlas/gateway-auth");

    let mut cfg = base_config();
    resolve_vault_secrets(&mut cfg).await.unwrap();
    assert_eq!(cfg.jwt_secret, "vault-jwt-secret-32-bytes-minimum!!");
    assert_eq!(cfg.admin_password, "vault-sourced-admin-password");

    clear_vault_env();
}

#[tokio::test]
async fn wrong_vault_token_fails_closed() {
    let _g = ENV_LOCK.lock().await;
    clear_vault_env();
    let base = spawn_mock_vault(json!({"data": {"data": {}}}), "correct-token").await;

    std::env::set_var("ATLAS_SECRETS_BACKEND", "vault");
    std::env::set_var("ATLAS_VAULT_ADDR", &base);
    std::env::set_var("ATLAS_VAULT_TOKEN", "wrong-token");
    std::env::set_var("ATLAS_VAULT_SECRET_PATH", "secret/data/atlas/gateway-auth");

    let mut cfg = base_config();
    let err = resolve_vault_secrets(&mut cfg).await.unwrap_err();
    assert!(
        err.to_string().contains("403"),
        "expected a 403 error, got: {err}"
    );
    // Original values are untouched on failure.
    assert_eq!(cfg.jwt_secret, "unresolved-dev-secret");

    clear_vault_env();
}

#[tokio::test]
async fn vault_backend_without_required_vars_errors() {
    let _g = ENV_LOCK.lock().await;
    clear_vault_env();
    std::env::set_var("ATLAS_SECRETS_BACKEND", "vault");
    let mut cfg = base_config();
    let err = resolve_vault_secrets(&mut cfg).await.unwrap_err();
    assert!(err.to_string().contains("ATLAS_VAULT_ADDR"));
    clear_vault_env();
}
