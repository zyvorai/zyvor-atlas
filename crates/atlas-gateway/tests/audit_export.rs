// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Regression guard for the audit-log export-before-prune hook: rows are only ever deleted after
//! a successful export to the configured sink, never unconditionally, so a SIEM outage can't
//! silently lose audit history.

use std::sync::{Arc, Mutex};

use axum::{extract::State, routing::post, Json, Router};
use sqlx::Row;

mod common;

/// A tiny fake SIEM webhook: captures the last POSTed body and always returns 200.
async fn spawn_fake_sink() -> (std::net::SocketAddr, Arc<Mutex<Option<serde_json::Value>>>) {
    let received: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let state = received.clone();
    let app = Router::new()
        .route(
            "/",
            post(
                |State(s): State<Arc<Mutex<Option<serde_json::Value>>>>,
                 Json(body): Json<serde_json::Value>| async move {
                    *s.lock().unwrap() = Some(body);
                    axum::http::StatusCode::OK
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, received)
}

async fn spawn_db() -> sqlx::AnyPool {
    let url = common::fresh_database_url("audit-export").await;
    let pool = atlas_inventory::connect(&url).await.unwrap();
    atlas_inventory::migrate(&pool, &url).await.unwrap();
    pool
}

async fn insert_old_row(pool: &sqlx::AnyPool, resource_id: &str) {
    sqlx::query(
        "INSERT INTO storage_audit_logs (actor_id, action, resource_type, resource_id, status, created_at)
         VALUES ('me', 'old.action', 'res', $1, 'ok', '2000-01-01T00:00:00.000Z')",
    )
    .bind(resource_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn successful_export_deletes_rows() {
    let pool = spawn_db().await;
    insert_old_row(&pool, "r-success").await;
    let (addr, received) = spawn_fake_sink().await;
    let url = format!("http://{addr}/");

    let n = atlas_monitor::audit_export::export_and_prune(&pool, 30, &url)
        .await
        .unwrap();
    assert!(
        n >= 1,
        "should have exported+pruned at least the seeded row"
    );

    let body = received
        .lock()
        .unwrap()
        .clone()
        .expect("sink should have received a POST");
    let logs = body["audit_logs"].as_array().expect("audit_logs array");
    assert!(logs.iter().any(|r| r["resource_id"] == "r-success"));

    let remaining: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM storage_audit_logs WHERE resource_id = 'r-success'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert_eq!(remaining, 0, "exported row must be deleted");
}

#[tokio::test]
async fn failed_export_keeps_rows() {
    let pool = spawn_db().await;
    insert_old_row(&pool, "r-kept").await;
    // Bind an ephemeral port to get one the OS guarantees is free, then drop the listener so the
    // POST fails to connect — more robust under a parallel `cargo test --workspace` run than a
    // hardcoded low port, whose refused-connection behavior can vary under system load.
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unreachable_addr = reserved.local_addr().unwrap();
    drop(reserved);
    let unreachable_url = format!("http://{unreachable_addr}/");

    let result = atlas_monitor::audit_export::export_and_prune(&pool, 30, &unreachable_url).await;
    assert!(
        result.is_err(),
        "export to an unreachable sink must error, not silently succeed"
    );

    let remaining: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM storage_audit_logs WHERE resource_id = 'r-kept'")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert_eq!(
        remaining, 1,
        "a failed export must not delete the row — retried next tick"
    );
}

/// Live verification against a real network endpoint — the two tests above prove the export
/// logic is correct using an in-process axum mock; this proves the actual HTTP client behaves
/// correctly against an independently-implemented server over a real network path. Opt-in
/// (depends on external infra, `cargo test` doesn't run `#[ignore]`d tests by default):
/// `cargo test --test audit_export -- --ignored`. Points at deploy/siem-lab/'s receiver
/// (`./up.sh` from that directory stands it up); adjust the URL if you deployed it elsewhere.
#[tokio::test]
#[ignore]
async fn live_export_against_siem_lab_receiver() {
    let pool = spawn_db().await;
    insert_old_row(&pool, "r-live-siem-lab").await;
    let url = "http://212.8.248.187:30557/";

    let n = atlas_monitor::audit_export::export_and_prune(&pool, 30, url)
        .await
        .expect("export to the live siem-lab receiver should succeed — is it deployed? see deploy/siem-lab/");
    assert!(n >= 1);

    let remaining: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM storage_audit_logs WHERE resource_id = 'r-live-siem-lab'",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("c");
    assert_eq!(remaining, 0, "exported row must be deleted");
}
