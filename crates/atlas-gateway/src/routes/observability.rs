// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use super::util::csv_field;
use crate::auth::Actor;
use crate::state::AppState;

pub(crate) async fn metrics_summary(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(atlas_inventory::metrics_summary(&s.pool).await?))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MetricQuery {
    prefix: Option<String>,
}

/// `GET /metrics/ceph[?prefix=ceph_osd]` — latest Ceph metrics scraped from the mgr Prometheus module.
pub(crate) async fn metrics_ceph(
    State(s): State<AppState>,
    Query(q): Query<MetricQuery>,
) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::metrics::list(&s.pool, q.prefix.as_deref()).await?
    )))
}

#[derive(Debug, Deserialize)]
pub(crate) struct HistoryQuery {
    minutes: Option<i64>,
}

/// `GET /metrics/history[?minutes=60]` — persisted capacity/IO/job time-series for trend charts.
pub(crate) async fn metrics_history(
    State(s): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Json<Value>> {
    let minutes = q.minutes.unwrap_or(60).clamp(1, 2880);
    Ok(Json(json!(
        atlas_inventory::metrics::history(&s.pool, minutes).await?
    )))
}

/// `GET /metrics/forecast[?minutes=1440]` — least-squares projection of days-until-full.
pub(crate) async fn metrics_forecast(
    State(s): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Json<Value>> {
    let minutes = q.minutes.unwrap_or(1440).clamp(1, 20160);
    Ok(Json(
        atlas_inventory::metrics::forecast(&s.pool, minutes).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct AlertQuery {
    state: Option<String>,
}

/// `GET /alerts[?state=open]` — alerts produced by the monitor worker (PDF §15.2).
pub(crate) async fn list_alerts(
    State(s): State<AppState>,
    Query(q): Query<AlertQuery>,
) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::alerts::list(&s.pool, q.state.as_deref()).await?
    )))
}

/// `POST /alerts/evaluate` — run the alert rules on demand (also runs on the monitor interval).
pub(crate) async fn evaluate_alerts(State(s): State<AppState>) -> AppResult<Json<Value>> {
    atlas_monitor::evaluate(&s.pool).await?;
    let open = atlas_inventory::alerts::count_open(&s.pool).await?;
    Ok(Json(json!({ "evaluated": true, "open_alerts": open })))
}

/// `POST /alerts/{id}/ack` — operator acknowledges an alert (records who saw it; not a resolve).
pub(crate) async fn ack_alert(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if !atlas_inventory::alerts::acknowledge(&s.pool, &id, &actor.id).await? {
        return Err(AppError::NotFound(format!("alert {id}")));
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "alert.ack",
        "alert",
        &id,
        "success",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "id": id, "acknowledged_by": actor.id })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SilenceQuery {
    /// Silence window in seconds (default 1h, capped at 30d).
    pub(crate) secs: Option<i64>,
}

/// `POST /alerts/{id}/silence[?secs=]` — suppress webhook notification for a window; the condition
/// keeps being tracked and still shows in `/alerts`.
pub(crate) async fn silence_alert(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<SilenceQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let secs = q.secs.unwrap_or(3600).clamp(1, 30 * 24 * 3600);
    if !atlas_inventory::alerts::silence(&s.pool, &id, secs).await? {
        return Err(AppError::NotFound(format!("alert {id}")));
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "alert.silence",
        "alert",
        &id,
        "success",
        None,
        Some(json!({ "secs": secs })),
    )
    .await;
    Ok(Json(json!({ "id": id, "silenced_secs": secs })))
}

/// `POST /alerts/{id}/resolve` — operator override to resolve an open alert.
pub(crate) async fn resolve_alert(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if !atlas_inventory::alerts::resolve_manual(&s.pool, &id).await? {
        return Err(AppError::NotFound(format!("open alert {id}")));
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "alert.resolve",
        "alert",
        &id,
        "success",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "id": id, "state": "resolved" })))
}
// ---- jobs ----

pub(crate) async fn list_jobs(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::jobs::list_jobs(&s.pool, 100).await?
    )))
}

pub(crate) async fn get_job(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let job = atlas_inventory::jobs::get_job(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id}")))?;
    Ok(Json(json!(job)))
}

/// `POST /jobs/{id}/cancel` — the operator escape hatch for a wedged job. The job engine's worker
/// is single-threaded (PDF-aligned ordering guarantee), so a job stuck inside a shelled-out
/// `ceph`/`rbd` call that never errors or returns (e.g. `rbd migration prepare` against a degraded
/// pool — verified live) blocks every other job on the gateway for up to the 2h default
/// `ATLAS_JOB_TIMEOUT_SECS`, for every tenant, with no prior recovery path short of finding and
/// killing the underlying OS process inside the pod by hand. `409` if the job is already terminal.
pub(crate) async fn cancel_job(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let job = atlas_inventory::jobs::get_job(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id}")))?;
    if job.state == "succeeded" || job.state == "failed" {
        return Err(AppError::Conflict(format!(
            "job {id} is already {}",
            job.state
        )));
    }
    let cancelled = s.jobs.cancel_job(&id).await?;
    if !cancelled {
        return Err(AppError::Conflict(format!(
            "job {id} could not be cancelled (already terminal)"
        )));
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "job.cancel",
        "job",
        &id,
        "success",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "id": id, "cancelled": true })))
}

/// `GET /jobs/{id}/watch` — Server-Sent Events; emits the job on each state change until terminal
/// (REST parity with the gRPC `WatchJob` stream).
pub(crate) async fn watch_job_sse(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::sse::Sse<
    impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let pool = s.pool.clone();
    let stream = async_stream::stream! {
        let mut last = String::new();
        for _ in 0..240 {
            match atlas_inventory::jobs::get_job(&pool, &id).await {
                Ok(Some(j)) => {
                    let terminal = j.state == "succeeded" || j.state == "failed";
                    if j.state != last {
                        last = j.state.clone();
                        let data = serde_json::to_string(&j).unwrap_or_default();
                        yield Ok(Event::default().event("job").data(data));
                    }
                    if terminal {
                        break;
                    }
                }
                Ok(None) => {
                    yield Ok(Event::default().event("error").data(format!("job {id} not found")));
                    break;
                }
                Err(e) => {
                    yield Ok(Event::default().event("error").data(e.to_string()));
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
#[derive(Debug, Deserialize)]
pub(crate) struct AuditQuery {
    actor: Option<String>,
    action: Option<String>,
    resource_type: Option<String>,
    resource_id: Option<String>,
    limit: Option<i64>,
}
pub(crate) async fn list_audit(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<AuditQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let rows = atlas_inventory::audit::list(
        &s.pool,
        q.actor.as_deref(),
        q.action.as_deref(),
        q.resource_type.as_deref(),
        q.resource_id.as_deref(),
        q.limit.unwrap_or(100),
    )
    .await?;
    Ok(Json(json!(rows)))
}

/// `GET /audit.csv` — export the audit trail as CSV (operator; for SIEM / compliance archival).
pub(crate) async fn export_audit_csv(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<axum::response::Response> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let rows = atlas_inventory::audit::list(&s.pool, None, None, None, None, 1000).await?;
    let mut csv = String::from("created_at,actor_id,action,resource_type,resource_id,status\n");
    for r in &rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{}\n",
            csv_field(r, "created_at"),
            csv_field(r, "actor_id"),
            csv_field(r, "action"),
            csv_field(r, "resource_type"),
            csv_field(r, "resource_id"),
            csv_field(r, "status"),
        ));
    }
    use axum::response::IntoResponse;
    Ok(([(axum::http::header::CONTENT_TYPE, "text/csv")], csv).into_response())
}

/// `GET /chargeback` — per-tenant usage + optional cost (showback). Rate from
/// `ATLAS_CHARGEBACK_USD_PER_GIB_MONTH` (0 = usage only). Point-in-time snapshot.
pub(crate) async fn chargeback(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let rate: f64 = std::env::var("ATLAS_CHARGEBACK_USD_PER_GIB_MONTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let gib = 1024.0 * 1024.0 * 1024.0;
    let tenants: Vec<Value> = atlas_inventory::tenants::list_overview(&s.pool)
        .await?
        .into_iter()
        .map(|t| {
            let used_gib = t.used_bytes as f64 / gib;
            json!({
                "tenant_id": t.tenant_id,
                "used_bytes": t.used_bytes,
                "used_gib": (used_gib * 100.0).round() / 100.0,
                "volume_count": t.volume_count,
                "quota_bytes": t.max_bytes,
                "estimated_usd_month": (used_gib * rate * 100.0).round() / 100.0,
            })
        })
        .collect();
    Ok(Json(
        json!({ "usd_per_gib_month": rate, "tenants": tenants }),
    ))
}

/// `GET /policy-drift` — volumes whose applied StorageClass no longer matches their policy (or whose
/// policy was deleted). Day-2 governance: catch configuration drift from the intended policy.
pub(crate) async fn policy_drift(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let drift = atlas_inventory::list_policy_drift(&s.pool).await?;
    Ok(Json(json!({ "drift": drift, "count": drift.len() })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    limit: Option<i64>,
}

/// `GET /events[?limit=100]` — unified activity feed (jobs + audit + alerts), newest first.
/// Operator-gated because it surfaces audit records.
pub(crate) async fn list_events(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let rows = atlas_inventory::events::feed(&s.pool, q.limit.unwrap_or(100)).await?;
    Ok(Json(json!(rows)))
}
