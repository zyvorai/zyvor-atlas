// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use super::util::CEPH_BACKEND_ID;
use crate::state::AppState;

/// `GET /metrics` — Atlas's own operational state in Prometheus text-exposition format, so a
/// Prometheus/Grafana stack can scrape the control plane itself (unauthenticated, like `/health`).
pub(crate) async fn prometheus_metrics(
    State(s): State<AppState>,
) -> impl axum::response::IntoResponse {
    use std::fmt::Write;
    let mut out = String::with_capacity(1024);
    let summary = atlas_inventory::metrics_summary(&s.pool)
        .await
        .unwrap_or(Value::Null);
    let g = |o: &mut String, name: &str, help: &str, v: i64| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}");
    };
    let n = |k: &str| summary.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let _ = writeln!(
        out,
        "# HELP atlas_build_info Atlas gateway build info.\n# TYPE atlas_build_info gauge\natlas_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    g(&mut out, "atlas_volumes", "Total volumes.", n("volumes"));
    g(
        &mut out,
        "atlas_snapshots",
        "Total snapshots.",
        n("snapshots"),
    );
    g(&mut out, "atlas_buckets", "Total buckets.", n("buckets"));
    g(&mut out, "atlas_backups", "Total backups.", n("backups"));
    g(&mut out, "atlas_pools", "Total pools.", n("pools"));
    g(&mut out, "atlas_clusters", "Total clusters.", n("clusters"));
    g(
        &mut out,
        "atlas_capacity_raw_bytes",
        "Raw cluster capacity (bytes).",
        n("raw_capacity_bytes"),
    );
    g(
        &mut out,
        "atlas_capacity_used_bytes",
        "Used cluster capacity (bytes).",
        n("used_capacity_bytes"),
    );
    // Jobs by state.
    if let Ok(states) = atlas_inventory::jobs::count_by_state(&s.pool).await {
        let _ = writeln!(
            out,
            "# HELP atlas_jobs Total jobs by state.\n# TYPE atlas_jobs gauge"
        );
        for (state, count) in states {
            let _ = writeln!(out, "atlas_jobs{{state=\"{state}\"}} {count}");
        }
    }
    // Open alerts.
    if let Ok(alerts) = atlas_inventory::alerts::list(&s.pool, Some("open")).await {
        g(
            &mut out,
            "atlas_alerts_open",
            "Open alerts.",
            alerts.len() as i64,
        );
    }
    // Per-backend breakdown (labelled by backend id + type).
    if let Ok(backends) = atlas_inventory::backend_breakdown(&s.pool).await {
        g(
            &mut out,
            "atlas_backends",
            "Registered backends.",
            backends.len() as i64,
        );
        let bn = |b: &Value, k: &str| b.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let bs = |b: &Value, k: &str| b.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let _ = writeln!(
            out,
            "# HELP atlas_backend_volumes Volumes per backend.\n# TYPE atlas_backend_volumes gauge"
        );
        for b in &backends {
            let _ = writeln!(
                out,
                "atlas_backend_volumes{{backend=\"{}\",type=\"{}\"}} {}",
                bs(b, "backend_id"),
                bs(b, "backend_type"),
                bn(b, "volumes")
            );
        }
        let _ = writeln!(
            out,
            "# HELP atlas_backend_capacity_raw_bytes Raw capacity per backend.\n# TYPE atlas_backend_capacity_raw_bytes gauge"
        );
        for b in &backends {
            let _ = writeln!(
                out,
                "atlas_backend_capacity_raw_bytes{{backend=\"{}\",type=\"{}\"}} {}",
                bs(b, "backend_id"),
                bs(b, "backend_type"),
                bn(b, "raw_capacity_bytes")
            );
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
}

/// The compiled Storage Center SPA (`crates/atlas-gateway/ui/dist`), embedded into the binary.
#[derive(rust_embed::Embed)]
#[folder = "ui/dist"]
struct Ui;

/// Serve an embedded UI asset by path; fall back to `index.html` for client-side routes.
pub(crate) async fn spa_handler(uri: axum::http::Uri) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (body, file) = match Ui::get(path) {
        Some(f) => (f.data, path.to_string()),
        None => match Ui::get("index.html") {
            Some(f) => (f.data, "index.html".to_string()),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "UI not built — run `make ui` (or build the Docker image)",
                )
                    .into_response()
            }
        },
    };
    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    (
        [(axum::http::header::CONTENT_TYPE, mime.as_ref())],
        body.into_owned(),
    )
        .into_response()
}

// ---- meta ----

pub(crate) async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// `GET /readyz` — readiness deep-check: probes the DB, confirms migrations/inventory are readable,
/// and reports driver + Kubernetes attachment. Returns 200 when ready, 503 when not (for k8s probes).
/// `/health` stays a cheap liveness signal (process is up); this checks dependencies.
pub(crate) async fn readyz(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    use atlas_common::config::CephDriverMode;

    // DB reachable + migrated (a readable backend row proves both).
    let (db_ok, db_detail) =
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM storage_backends")
            .fetch_one(&s.pool)
            .await
        {
            Ok(n) => (true, format!("{n} backend(s)")),
            Err(e) => (false, format!("query failed: {e}")),
        };

    let driver_mode = match s.config.ceph_driver_mode {
        CephDriverMode::Real => "real",
        CephDriverMode::Fake => "fake",
    };

    // Probe the actual backend driver (bounded) instead of assuming healthy. Ok/Warn = reachable and
    // serving (possibly degraded); Critical/Unknown/error/timeout = not reachable.
    // Keep this under the k8s readinessProbe timeoutSeconds (5s in deploy/k8s) so kubelet sees a
    // real 503 rather than "context deadline exceeded" and flapping NotReady / empty Endpoints.
    let (driver_ok, driver_status) = match s.driver_for(CEPH_BACKEND_ID) {
        Some(d) => {
            match tokio::time::timeout(std::time::Duration::from_secs(3), d.health()).await {
                Ok(Ok(h)) => (
                    matches!(
                        h.status,
                        atlas_api_types::Health::Ok | atlas_api_types::Health::Warn
                    ),
                    format!("{:?}", h.status).to_lowercase(),
                ),
                Ok(Err(e)) => (false, format!("error: {e}")),
                Err(_) => (false, "probe timed out".into()),
            }
        }
        None => (false, "no driver registered".into()),
    };
    let k8s_ok = s.k8s.is_some();

    // Worker heartbeats — reported for visibility, not gated (a single worker hiccup shouldn't depool
    // the read/inventory API). Stale = no beat within 5× the monitor interval + 30s.
    let interval = s.config.monitor_interval_secs.max(1);
    let stale_after = interval.saturating_mul(5).saturating_add(30);
    let workers: Vec<Value> = s
        .workers
        .ages()
        .into_iter()
        .map(|(name, age, own_interval)| {
            // A worker with its own declared interval is stale after two missed ticks (+30s).
            let limit =
                own_interval.map_or(stale_after, |i| i.saturating_mul(2).saturating_add(30));
            json!({ "worker": name, "age_secs": age, "stale": age > limit })
        })
        .collect();

    // Gate readiness on the DB and a reachable backend driver.
    let ready = db_ok && driver_ok;
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({
            "status": if ready { "ready" } else { "not_ready" },
            "components": {
                "database": { "ok": db_ok, "detail": db_detail },
                "ceph_driver": { "ok": driver_ok, "mode": driver_mode, "status": driver_status },
                "kubernetes": { "ok": k8s_ok, "detail": if k8s_ok { "attached" } else { "not attached" } },
                "workers": workers,
            },
        })),
    )
}

/// `GET /livez` — liveness: the process + async runtime are responsive. Always 200 (a wedged runtime
/// simply won't answer). Distinct from `/readyz`, which gates traffic on dependencies. Wire k8s
/// livenessProbe here (restart on failure) and readinessProbe at `/readyz`.
pub(crate) async fn livez() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "status": "alive" })))
}

pub(crate) async fn version() -> Json<Value> {
    Json(json!({
        "name": "atlas-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "api": "v1",
    }))
}
