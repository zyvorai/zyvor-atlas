// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Integration tests for the native PagerDuty/Opsgenie/Slack alerting sinks
//! (crates/atlas-monitor/src/notify/{pagerduty,opsgenie,slack}.rs): each is pointed at a tiny
//! local axum server standing in for the real API, so these exercise the real HTTP round trip and
//! the `alert_notifications` delivery-tracking table, not just the request-building logic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use atlas_monitor::notify::{opsgenie, pagerduty, slack};
use atlas_monitor::{OpsgenieConfig, PagerDutyConfig};
use axum::{extract::State, routing::post, Router};
use serde_json::{json, Value};
use sqlx::AnyPool;

static NEXT: AtomicU64 = AtomicU64::new(0);

async fn fresh_pool() -> AnyPool {
    let db = format!(
        "{}/atlas-notify-sinks-test-{}-{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
    );
    let _ = std::fs::remove_file(&db);
    let url = format!("sqlite://{db}?mode=rwc");
    let pool = atlas_inventory::connect(&url).await.unwrap();
    atlas_inventory::migrate(&pool, &url).await.unwrap();
    pool
}

async fn seed_open_alert(pool: &AnyPool, id: &str) {
    atlas_inventory::alerts::upsert_open(
        pool,
        id,
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

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(String, Value)>>>);

impl Recorder {
    fn record(&self, path: &str, body: Value) {
        self.0.lock().unwrap().push((path.to_string(), body));
    }
    fn calls(&self) -> Vec<(String, Value)> {
        self.0.lock().unwrap().clone()
    }
}

/// A mock server generic enough to stand in for PagerDuty's single `/enqueue` endpoint, Opsgenie's
/// `/v2/alerts` + `/v2/alerts/{alias}/close`, and Slack's bare webhook URL — records every POST's
/// path and JSON body, always replies 202.
async fn spawn_mock(rec: Recorder) -> String {
    async fn handler(
        State(rec): State<Recorder>,
        req: axum::extract::Request,
    ) -> axum::http::StatusCode {
        let path = req.uri().path().to_string();
        let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        rec.record(&path, body);
        axum::http::StatusCode::ACCEPTED
    }
    let app = Router::new()
        .route("/", post(handler))
        .route("/v2/alerts", post(handler))
        .route("/v2/alerts/{alias}/close", post(handler))
        .with_state(rec);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn pagerduty_triggers_once_and_resolves_on_clear() {
    let pool = fresh_pool().await;
    seed_open_alert(&pool, "alert_cluster_unhealthy_cls_1").await;
    let rec = Recorder::default();
    let base = spawn_mock(rec.clone()).await;
    let cfg = PagerDutyConfig {
        routing_key: "rk".into(),
    };

    // First dispatch: triggers once.
    let (t, r) = pagerduty::dispatch_to(&pool, &cfg, &format!("{base}/")).await.unwrap();
    assert_eq!((t, r), (1, 0));
    assert_eq!(rec.calls().len(), 1);
    assert_eq!(rec.calls()[0].1["event_action"], "trigger");
    assert_eq!(rec.calls()[0].1["dedup_key"], "alert_cluster_unhealthy_cls_1");

    // Second dispatch with the alert still open: no re-trigger (already recorded as sent).
    let (t, r) = pagerduty::dispatch_to(&pool, &cfg, &format!("{base}/")).await.unwrap();
    assert_eq!((t, r), (0, 0));
    assert_eq!(rec.calls().len(), 1);

    // Alert clears -> resolve event sent exactly once.
    atlas_inventory::alerts::resolve(&pool, "alert_cluster_unhealthy_cls_1")
        .await
        .unwrap();
    let (t, r) = pagerduty::dispatch_to(&pool, &cfg, &format!("{base}/")).await.unwrap();
    assert_eq!((t, r), (0, 1));
    assert_eq!(rec.calls().len(), 2);
    assert_eq!(rec.calls()[1].1["event_action"], "resolve");
    assert!(
        rec.calls()[1].1.get("payload").is_none(),
        "resolve shouldn't resend the trigger payload"
    );

    // Idempotent: a third dispatch after resolution sends nothing further.
    let (t, r) = pagerduty::dispatch_to(&pool, &cfg, &format!("{base}/")).await.unwrap();
    assert_eq!((t, r), (0, 0));
    assert_eq!(rec.calls().len(), 2);
}

#[tokio::test]
async fn opsgenie_creates_and_closes_by_alias() {
    let pool = fresh_pool().await;
    seed_open_alert(&pool, "alert_cluster_unhealthy_cls_1").await;
    let rec = Recorder::default();
    let base = spawn_mock(rec.clone()).await;
    let cfg = OpsgenieConfig {
        api_key: "key".into(),
        region: "us".into(),
    };

    let (t, _) = opsgenie::dispatch_to(&pool, &cfg, &base).await.unwrap();
    assert_eq!(t, 1);
    assert_eq!(rec.calls()[0].0, "/v2/alerts");
    assert_eq!(rec.calls()[0].1["alias"], "alert_cluster_unhealthy_cls_1");
    assert_eq!(rec.calls()[0].1["priority"], "P1");

    atlas_inventory::alerts::resolve(&pool, "alert_cluster_unhealthy_cls_1")
        .await
        .unwrap();
    let (_, r) = opsgenie::dispatch_to(&pool, &cfg, &base).await.unwrap();
    assert_eq!(r, 1);
    assert_eq!(rec.calls()[1].0, "/v2/alerts/alert_cluster_unhealthy_cls_1/close");
}

#[tokio::test]
async fn slack_posts_trigger_and_resolve_messages() {
    let pool = fresh_pool().await;
    seed_open_alert(&pool, "alert_cluster_unhealthy_cls_1").await;
    let rec = Recorder::default();
    let base = spawn_mock(rec.clone()).await;

    let (t, _) = slack::dispatch(&pool, &base).await.unwrap();
    assert_eq!(t, 1);
    assert!(rec.calls()[0].1["text"]
        .as_str()
        .unwrap()
        .contains("Cluster unhealthy"));

    atlas_inventory::alerts::resolve(&pool, "alert_cluster_unhealthy_cls_1")
        .await
        .unwrap();
    let (_, r) = slack::dispatch(&pool, &base).await.unwrap();
    assert_eq!(r, 1);
    assert!(rec.calls()[1].1["text"].as_str().unwrap().contains("resolved"));
}
