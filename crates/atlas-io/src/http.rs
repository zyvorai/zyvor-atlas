// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent HTTP surface (REST + Prometheus text). Independent of atlas-gateway.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::{score, CapacitySignal, Collector};

#[derive(Clone)]
pub struct AppState {
    pub collector: Arc<Collector>,
}

/// Router without authentication (tests, `make run`, loopback use).
pub fn router(collector: Arc<Collector>) -> Router {
    router_with_token(collector, None)
}

/// Router that, when `token` is `Some`, requires `Authorization: Bearer <token>` on everything
/// except the liveness endpoints (`/health`, `/io/health`) that kubelet probes hit.
pub fn router_with_token(collector: Arc<Collector>, token: Option<String>) -> Router {
    let app = Router::new()
        .route("/health", get(health))
        .route("/io/health", get(health))
        .route("/io/summary", get(summary))
        .route("/io/histograms", get(histograms))
        .route("/io/heatmap", get(heatmap))
        .route("/io/workloads", get(workloads))
        .route("/io/rca", get(rca))
        .route("/io/risk", get(risk))
        .route("/io/leases", get(leases).post(grant_lease))
        .route("/io/coverage", get(coverage))
        .route("/io/native", get(native))
        .route("/metrics", get(prom))
        .route("/io/poll", post(poll))
        .with_state(AppState {
            collector: collector.clone(),
        });
    #[cfg(feature = "mcp")]
    let app = app.nest_service("/mcp", crate::mcp::service(collector, token.is_some()));
    match token {
        Some(t) => app.layer(axum::middleware::from_fn_with_state(
            Arc::<str>::from(t),
            require_bearer,
        )),
        None => app,
    }
}

/// Constant-time equality (length is not secret: tokens are fixed-size in practice).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer(
    State(token): State<Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if matches!(req.uri().path(), "/health" | "/io/health") {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(p) if ct_eq(p.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response(),
    }
}

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.collector.health())
}

async fn summary(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    let h = st.collector.health();
    let hists = st.collector.histograms();
    Json(serde_json::json!({
        "health": h,
        "histograms": hists.len(),
        "workloads": st.collector.workloads().len(),
        "leases": st.collector.leases().len(),
    }))
}

async fn histograms(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    Json(st.collector.histograms())
}

#[derive(Deserialize)]
struct HeatQuery {
    /// How far back to return, in minutes (default 15, at most 60).
    minutes: Option<u64>,
}

async fn heatmap(State(st): State<AppState>, Query(q): Query<HeatQuery>) -> impl IntoResponse {
    st.collector.poll();
    let minutes = q.minutes.unwrap_or(15).clamp(1, crate::heat::MAX_COLUMNS as u64);
    Json(st.collector.heatmap(minutes))
}

async fn workloads(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    Json(st.collector.workloads())
}

#[derive(Deserialize)]
struct RcaQuery {
    volume: Option<String>,
}

async fn rca(State(st): State<AppState>, Query(q): Query<RcaQuery>) -> impl IntoResponse {
    st.collector.poll();
    Json(st.collector.rca(q.volume.as_deref()))
}

#[derive(Deserialize)]
struct RiskQuery {
    volume: Option<String>,
    used_percent: Option<f64>,
    days_to_full: Option<f64>,
}

/// Read-only risk level over the current RCA, optionally combined with a capacity signal the
/// caller supplies (the agent has no capacity telemetry of its own). Invalid capacity values are
/// ignored by the scorer; `capacity_used` reports whether one was supplied.
async fn risk(State(st): State<AppState>, Query(q): Query<RiskQuery>) -> impl IntoResponse {
    st.collector.poll();
    let rca = st.collector.rca(q.volume.as_deref());
    let capacity = q.used_percent.map(|used_percent| CapacitySignal {
        used_percent,
        days_to_full: q.days_to_full,
    });
    let level = score(&rca, capacity);
    Json(serde_json::json!({
        "level": level.as_str(),
        "capacity_used": capacity.is_some(),
        "can_execute": false,
        "evidence": rca.iter().map(|r| serde_json::json!({
            "device": r.device,
            "volume_id": r.volume_id,
            "verdict": r.verdict,
            "notes": r.notes,
        })).collect::<Vec<_>>(),
    }))
}

async fn leases(State(st): State<AppState>) -> impl IntoResponse {
    Json(st.collector.leases())
}

#[derive(Deserialize)]
struct GrantBody {
    device: String,
    volume_id: Option<String>,
    #[serde(default = "default_ttl")]
    ttl_secs: u64,
    #[serde(default = "default_reason")]
    reason: String,
}

fn default_ttl() -> u64 {
    60
}
fn default_reason() -> String {
    "operator".into()
}

async fn grant_lease(State(st): State<AppState>, Json(body): Json<GrantBody>) -> impl IntoResponse {
    match st
        .collector
        .grant_lease(body.device, body.volume_id, body.ttl_secs, body.reason)
    {
        Ok(l) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(l).expect("IoLease is serializable")),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn coverage(State(st): State<AppState>) -> impl IntoResponse {
    let h = st.collector.health();
    Json(serde_json::json!({
        "pin_dir": h.pin_dir,
        "loaded": h.programs_loaded,
        "missing": h.programs_missing,
        "mode": h.mode,
    }))
}

async fn native(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    match st.collector.native() {
        Some(n) => Json(serde_json::to_value(n).expect("IoNative is serializable")).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "atlas_native maps not loaded" })),
        )
            .into_response(),
    }
}

async fn poll(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    Json(serde_json::json!({ "seen": st.collector.seen() }))
}

async fn prom(State(st): State<AppState>) -> impl IntoResponse {
    st.collector.poll();
    let mut out = String::from("# HELP atlas_io_events_seen Total bio events ingested\n# TYPE atlas_io_events_seen counter\n");
    out.push_str(&format!("atlas_io_events_seen {}\n", st.collector.seen()));
    out.push_str("# HELP atlas_io_hist_count Events in a (device,op) histogram\n# TYPE atlas_io_hist_count gauge\n");
    for h in st.collector.histograms() {
        out.push_str(&format!(
            "atlas_io_hist_count{{device=\"{}\",op=\"{}\"}} {}\n",
            h.device,
            h.op.as_str(),
            h.count
        ));
        out.push_str(&format!(
            "atlas_io_hist_p99_us{{device=\"{}\",op=\"{}\"}} {}\n",
            h.device,
            h.op.as_str(),
            h.p99_us()
        ));
        if let Some(avg) = h.queue_sum_us.checked_div(h.queued) {
            out.push_str(&format!(
                "atlas_io_hist_queue_avg_us{{device=\"{}\",op=\"{}\"}} {}\n",
                h.device,
                h.op.as_str(),
                avg
            ));
        }
    }
    out.push_str("# HELP atlas_io_risk_level Risk from RCA (0 healthy, 1 elevated, 2 critical)\n# TYPE atlas_io_risk_level gauge\n");
    let level = score(&st.collector.rca(None), None);
    out.push_str(&format!("atlas_io_risk_level {}\n", level as u8));
    if let Some(n) = st.collector.native() {
        out.push_str("# HELP atlas_io_native_tracked_pids Atlas Native processes in the kernel PID set\n# TYPE atlas_io_native_tracked_pids gauge\n");
        out.push_str(&format!(
            "atlas_io_native_tracked_pids {}\n",
            n.tracked_pids.len()
        ));
        out.push_str("# HELP atlas_io_native_ios Atlas Native block I/Os per (device,op,cgroup)\n# TYPE atlas_io_native_ios counter\n");
        for s in &n.stats {
            let labels = format!(
                "device=\"{}\",op=\"{}\",cgroup_id=\"{}\"",
                s.device,
                s.op.as_str(),
                s.cgroup_id
            );
            out.push_str(&format!("atlas_io_native_ios{{{labels}}} {}\n", s.ios));
            out.push_str(&format!("atlas_io_native_bytes{{{labels}}} {}\n", s.bytes));
            out.push_str(&format!(
                "atlas_io_native_errors{{{labels}}} {}\n",
                s.errors
            ));
            out.push_str(&format!(
                "atlas_io_native_max_us{{{labels}}} {}\n",
                s.max_us
            ));
        }
        out.push_str("# HELP atlas_io_native_dropped Native completions not aggregated or slow events not delivered\n# TYPE atlas_io_native_dropped counter\n");
        out.push_str(&format!("atlas_io_native_dropped {}\n", n.dropped));
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
}
