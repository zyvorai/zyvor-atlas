// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Read-only planning and reporting (`docs/PLANNING.md`): capacity time-to-full forecasts, a DR
//! readiness score with explicit gaps, and a dry-run scrub schedule. Nothing here touches a
//! backend or the data path; every response carries `can_execute: false`, and the scoring and
//! planning functions are pure so they are tested with fabricated inputs only.
//!
//! The thresholds and default intervals are heuristics, stated as such in each response's
//! `assumptions`; they are not backend requirements.

use axum::{extract::State, Extension, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use crate::state::AppState;

const DAY: f64 = 86_400.0;
/// Hard caps on caller-supplied input.
pub(crate) const MAX_POOLS: usize = 256;
pub(crate) const MAX_SAMPLES: usize = 10_000;
pub(crate) const MAX_SCRUB_ITEMS: usize = 1_000;
/// A forecast beyond ten years is reported as "beyond horizon" rather than as a date.
const HORIZON_DAYS: f64 = 3_650.0;
/// Below this growth (bytes/day) a series counts as flat.
const NOISE_BYTES_PER_DAY: f64 = 1_048_576.0;

fn bad<T>(msg: impl Into<String>) -> AppResult<T> {
    Err(AppError::Validation(msg.into()))
}

// ---------------------------------------------------------------- capacity forecast

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Sample {
    /// Unix seconds.
    pub ts: i64,
    pub used_bytes: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PoolSeries {
    pub id: String,
    pub capacity_bytes: f64,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForecastBody {
    pub pools: Vec<PoolSeries>,
    /// Alert when time-to-full is at most this many days (default 30).
    pub warn_days: Option<f64>,
    /// Critical at most this many days (default 7).
    pub critical_days: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Thresholds {
    pub warn_days: f64,
    pub critical_days: f64,
}

pub(crate) fn thresholds(warn: Option<f64>, crit: Option<f64>) -> AppResult<Thresholds> {
    let w = warn.unwrap_or(30.0);
    let c = crit.unwrap_or(7.0);
    if !w.is_finite() || !c.is_finite() || c <= 0.0 || w < c || w > HORIZON_DAYS {
        return bad("warn_days and critical_days must be finite with 0 < critical_days <= warn_days <= 3650");
    }
    Ok(Thresholds {
        warn_days: w,
        critical_days: c,
    })
}

/// Ordinary least squares of `y` on `x`; `(slope, intercept, r_squared)`. `None` when `x` has no
/// spread (all samples at one instant).
fn ols(x: &[f64], y: &[f64]) -> Option<(f64, f64, f64)> {
    let n = x.len() as f64;
    if x.len() < 2 {
        return None;
    }
    let mx = x.iter().sum::<f64>() / n;
    let my = y.iter().sum::<f64>() / n;
    let sxx: f64 = x.iter().map(|v| (v - mx) * (v - mx)).sum();
    if sxx <= 0.0 || !sxx.is_finite() {
        return None;
    }
    let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let syy: f64 = y.iter().map(|v| (v - my) * (v - my)).sum();
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let r2 = if syy <= 0.0 {
        1.0
    } else {
        (sxy * sxy / (sxx * syy)).clamp(0.0, 1.0)
    };
    Some((slope, intercept, r2))
}

/// Forecast one pool. Never panics on hostile input: bad values are a validation error,
/// too little data is a normal `insufficient_data` result.
pub(crate) fn forecast_pool(p: &PoolSeries, t: &Thresholds) -> AppResult<Value> {
    if p.id.is_empty() || p.id.len() > 200 {
        return bad("pool id must be 1..=200 characters");
    }
    if !p.capacity_bytes.is_finite() || p.capacity_bytes <= 0.0 {
        return bad(format!(
            "pool {}: capacity_bytes must be finite and positive",
            p.id
        ));
    }
    if p.samples.len() > MAX_SAMPLES {
        return bad(format!("pool {}: at most {MAX_SAMPLES} samples", p.id));
    }
    for s in &p.samples {
        if !s.used_bytes.is_finite() || s.used_bytes < 0.0 {
            return bad(format!(
                "pool {}: used_bytes must be finite and non-negative",
                p.id
            ));
        }
    }
    let mut samples = p.samples.clone();
    samples.sort_by_key(|s| s.ts);
    samples.dedup_by_key(|s| s.ts);
    let base = json!({ "id": p.id, "capacity_bytes": p.capacity_bytes, "samples": samples.len() });
    let with = |extra: Value| -> Value {
        let mut o = base.clone();
        if let (Some(a), Some(b)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        o
    };
    let Some(last) = samples.last() else {
        return Ok(with(
            json!({ "status": "insufficient_data", "severity": "unknown",
            "reason": "no samples" }),
        ));
    };
    let used_now = last.used_bytes;
    let used_ratio = used_now / p.capacity_bytes;
    if used_now >= p.capacity_bytes {
        return Ok(with(json!({ "status": "full", "severity": "critical",
            "used_bytes": used_now, "used_ratio": used_ratio, "days_to_full": 0.0 })));
    }
    let span = last.ts.saturating_sub(samples[0].ts) as f64;
    if samples.len() < 3 || span < 3_600.0 {
        return Ok(with(
            json!({ "status": "insufficient_data", "severity": "unknown",
            "used_bytes": used_now, "used_ratio": used_ratio,
            "reason": "need at least 3 samples spanning at least one hour" }),
        ));
    }
    let t0 = samples[0].ts;
    let x: Vec<f64> = samples.iter().map(|s| s.ts as f64 - t0 as f64).collect();
    let y: Vec<f64> = samples.iter().map(|s| s.used_bytes).collect();
    let Some((slope, intercept, r2)) = ols(&x, &y) else {
        return Ok(with(
            json!({ "status": "insufficient_data", "severity": "unknown",
            "reason": "samples have no time spread" }),
        ));
    };
    let per_day = slope * DAY;
    // Trend check: slope of the first half against the second half.
    let mid = samples.len() / 2;
    let half = |r: std::ops::Range<usize>| ols(&x[r.clone()], &y[r]).map(|v| v.0 * DAY);
    let (early, late) = (half(0..mid), half(mid..samples.len()));
    let trend = match (early, late) {
        (Some(e), Some(l)) if l > e * 1.25 + NOISE_BYTES_PER_DAY => "accelerating",
        (Some(e), Some(l)) if l < e * 0.75 - NOISE_BYTES_PER_DAY => "decelerating",
        (Some(_), Some(_)) => "steady",
        _ => "unknown",
    };
    // Fit quality: low r2 means the line is a poor description, so say so.
    let confidence = if samples.len() >= 10 && r2 >= 0.8 {
        "high"
    } else if r2 >= 0.5 {
        "medium"
    } else {
        "low"
    };
    let free = p.capacity_bytes - used_now;
    let (status, severity, days) = if per_day <= NOISE_BYTES_PER_DAY {
        ("not_filling", "ok", None)
    } else {
        let d = free / per_day;
        if d > HORIZON_DAYS {
            ("beyond_horizon", "ok", None)
        } else {
            let sev = if d <= t.critical_days {
                "critical"
            } else if d <= t.warn_days {
                "warning"
            } else {
                "ok"
            };
            ("filling", sev, Some((d * 10.0).round() / 10.0))
        }
    };
    let to_ratio = |ratio: f64| -> Option<f64> {
        let target = p.capacity_bytes * ratio;
        if used_now >= target {
            Some(0.0)
        } else if per_day > NOISE_BYTES_PER_DAY {
            let d = (target - used_now) / per_day;
            (d <= HORIZON_DAYS).then_some((d * 10.0).round() / 10.0)
        } else {
            None
        }
    };
    Ok(with(json!({
        "status": status, "severity": severity, "trend": trend, "confidence": confidence,
        "used_bytes": used_now, "used_ratio": used_ratio,
        "growth_bytes_per_day": per_day.round(), "r_squared": (r2 * 1000.0).round() / 1000.0,
        "fit_intercept_bytes": intercept.round(),
        "days_to_full": days, "days_to_80_percent": to_ratio(0.8),
        "window_seconds": span as i64,
    })))
}

pub(crate) fn forecast_all(body: &ForecastBody) -> AppResult<Value> {
    if body.pools.is_empty() || body.pools.len() > MAX_POOLS {
        return bad(format!("pools must hold 1..={MAX_POOLS} entries"));
    }
    let t = thresholds(body.warn_days, body.critical_days)?;
    let mut out = Vec::with_capacity(body.pools.len());
    for p in &body.pools {
        out.push(forecast_pool(p, &t)?);
    }
    let rank = |v: &Value| match v["severity"].as_str() {
        Some("critical") => 0,
        Some("warning") => 1,
        Some("unknown") => 2,
        _ => 3,
    };
    out.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            let da = a["days_to_full"].as_f64().unwrap_or(f64::MAX);
            let db = b["days_to_full"].as_f64().unwrap_or(f64::MAX);
            da.total_cmp(&db)
        })
    });
    Ok(json!({
        "can_execute": false,
        "pools": out,
        "assumptions": {
            "model": "ordinary least squares on used bytes; linear growth continues",
            "warn_days": t.warn_days, "critical_days": t.critical_days,
            "min_samples": 3, "min_span_seconds": 3600,
            "flat_below_bytes_per_day": NOISE_BYTES_PER_DAY,
            "horizon_days": HORIZON_DAYS,
            "note": "heuristic projection, not a guarantee; bursty or step growth fits poorly (see confidence and trend)",
        },
    }))
}

/// `POST /capacity/forecast` — time-to-full per pool from caller-supplied usage samples.
pub(crate) async fn post_capacity_forecast(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Json(body): Json<ForecastBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(forecast_all(&body)?))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClusterQuery {
    pub minutes: Option<i64>,
    pub warn_days: Option<f64>,
    pub critical_days: Option<f64>,
}

/// `GET /capacity/forecast?minutes=` — the same model over the persisted cluster-wide history.
pub(crate) async fn get_capacity_forecast(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    axum::extract::Query(q): axum::extract::Query<ClusterQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let minutes = q.minutes.unwrap_or(10_080).clamp(60, 43_200);
    let rows = atlas_inventory::metrics::history(&s.pool, minutes).await?;
    let mut samples = Vec::new();
    let mut cap = 0.0_f64;
    for r in &rows {
        let ts = r["ts"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|d| d.timestamp());
        if let (Some(ts), Some(u)) = (ts, r["used_capacity_bytes"].as_f64()) {
            samples.push(Sample { ts, used_bytes: u });
        }
        cap = r["raw_capacity_bytes"].as_f64().unwrap_or(cap);
    }
    if cap <= 0.0 {
        return Ok(Json(json!({ "can_execute": false, "pools": [],
            "note": "no capacity history recorded yet" })));
    }
    let body = ForecastBody {
        pools: vec![PoolSeries {
            id: "cluster".into(),
            capacity_bytes: cap,
            samples,
        }],
        warn_days: q.warn_days,
        critical_days: q.critical_days,
    };
    Ok(Json(forecast_all(&body)?))
}

// ---------------------------------------------------------------- DR readiness

#[derive(Debug, Clone)]
pub(crate) struct DrParams {
    /// Target recovery point objective in seconds.
    pub target_rpo_secs: i64,
    /// A failover drill older than this many days counts as stale.
    pub drill_max_age_days: i64,
    /// An observation older than this many seconds counts as stale.
    pub stale_after_secs: i64,
}

impl Default for DrParams {
    fn default() -> Self {
        Self {
            target_rpo_secs: 3_600,
            drill_max_age_days: 90,
            stale_after_secs: 3_600,
        }
    }
}

fn gap(code: &str, severity: &str, subject: &str, detail: String, fix: &str) -> Value {
    json!({ "code": code, "severity": severity, "subject": subject,
            "detail": detail, "remediation": fix })
}

fn age_secs(ts: Option<&str>, now: DateTime<Utc>) -> Option<i64> {
    let d = DateTime::parse_from_rfc3339(ts?).ok()?;
    Some((now - d.with_timezone(&Utc)).num_seconds())
}

/// Score DR readiness from the catalog rows (`dr::list_peers` / `dr::list_mirrors` JSON).
/// Penalties: critical 25, warning 10, info 2, floor 0. Missing data is a gap, never a pass.
pub(crate) fn assess_dr(
    peers: &[Value],
    mirrors: &[Value],
    now: DateTime<Utc>,
    p: &DrParams,
) -> Value {
    let mut gaps: Vec<Value> = Vec::new();
    if peers.is_empty() {
        gaps.push(gap(
            "NO_PEER",
            "critical",
            "cluster",
            "no mirroring peer is registered, so there is no recovery site".into(),
            "register a peer (POST /dr/peers)",
        ));
    }
    if mirrors.is_empty() {
        gaps.push(gap(
            "NO_MIRRORS",
            "critical",
            "cluster",
            "no volume is mirrored".into(),
            "enable mirroring on the volumes that need DR",
        ));
    }
    let mut worst_rpo: Option<i64> = None;
    let mut last_drill_age: Option<i64> = None;
    for m in mirrors {
        let id = m["id"].as_str().unwrap_or("?");
        if m["peer_id"].is_null() {
            gaps.push(gap(
                "MIRROR_NO_PEER",
                "warning",
                id,
                "mirror is not tied to a peer".into(),
                "re-enable the mirror with a peer",
            ));
        }
        if m["state"] == "error" {
            let why = m["last_error"]
                .as_str()
                .unwrap_or("no error text")
                .to_string();
            gaps.push(gap(
                "MIRROR_ERROR",
                "critical",
                id,
                format!(
                    "mirror is in error state: {}",
                    why.chars().take(200).collect::<String>()
                ),
                "inspect GET /dr/mirrors/{id}/status and resync if needed",
            ));
        }
        match m["rpo_seconds"].as_i64() {
            None => gaps.push(gap(
                "RPO_UNKNOWN",
                "warning",
                id,
                "no observed RPO recorded, lag is unknown".into(),
                "refresh the RPO (POST /dr/mirrors/{id}/rpo) from replication status",
            )),
            Some(r) if r < 0 => gaps.push(gap(
                "RPO_INVALID",
                "warning",
                id,
                format!("recorded RPO {r}s is negative"),
                "re-measure the replication lag",
            )),
            Some(r) => {
                worst_rpo = Some(worst_rpo.map_or(r, |w| w.max(r)));
                if r > p.target_rpo_secs.saturating_mul(2) {
                    gaps.push(gap(
                        "RPO_BREACH",
                        "critical",
                        id,
                        format!(
                            "RPO {r}s is more than twice the {}s target",
                            p.target_rpo_secs
                        ),
                        "check peer link and replay; a failover now would lose this much data",
                    ));
                } else if r > p.target_rpo_secs {
                    gaps.push(gap(
                        "RPO_OVER_TARGET",
                        "warning",
                        id,
                        format!("RPO {r}s exceeds the {}s target", p.target_rpo_secs),
                        "check replication throughput or loosen the target",
                    ));
                }
            }
        }
        match age_secs(m["updated_at"].as_str(), now) {
            Some(a) if a > p.stale_after_secs => gaps.push(gap(
                "STATE_STALE",
                "warning",
                id,
                format!("mirror state last updated {a}s ago"),
                "refresh mirror status",
            )),
            None => gaps.push(gap(
                "STATE_TIME_UNKNOWN",
                "info",
                id,
                "mirror has no readable update time".into(),
                "refresh mirror status",
            )),
            _ => {}
        }
        if m["force_promoted"] == true {
            gaps.push(gap(
                "FORCE_PROMOTED",
                "warning",
                id,
                "last promote was forced, the old primary may need resync".into(),
                "resync the demoted copy before relying on this pair",
            ));
        }
        if let Some(a) = age_secs(m["last_failover_at"].as_str(), now) {
            last_drill_age = Some(last_drill_age.map_or(a, |x| x.min(a)));
        }
    }
    if !mirrors.is_empty() {
        let max = p.drill_max_age_days.saturating_mul(86_400);
        match last_drill_age {
            None => gaps.push(gap(
                "NO_DRILL",
                "warning",
                "cluster",
                "no failover drill is recorded, so RTO has never been measured".into(),
                "run a planned failover drill and failback",
            )),
            Some(a) if a > max => gaps.push(gap(
                "DRILL_STALE",
                "warning",
                "cluster",
                format!(
                    "last failover drill was {} days ago (limit {})",
                    a / 86_400,
                    p.drill_max_age_days
                ),
                "repeat the drill",
            )),
            _ => {}
        }
    }
    let penalty: i64 = gaps
        .iter()
        .map(|g| match g["severity"].as_str() {
            Some("critical") => 25,
            Some("warning") => 10,
            _ => 2,
        })
        .sum();
    let score = (100 - penalty).clamp(0, 100);
    let grade = match score {
        90..=100 => "ready",
        60..=89 => "degraded",
        _ => "not_ready",
    };
    json!({
        "can_execute": false, "score": score, "grade": grade,
        "peers": peers.len(), "mirrors": mirrors.len(),
        "worst_rpo_seconds": worst_rpo, "target_rpo_seconds": p.target_rpo_secs,
        "rto_measured_seconds": Value::Null,
        "last_drill_age_seconds": last_drill_age,
        "gaps": gaps,
        "assumptions": {
            "penalties": { "critical": 25, "warning": 10, "info": 2 },
            "rpo_source": "dr_mirrors.rpo_seconds as last recorded; Atlas does not poll it",
            "rto": "Atlas does not time drills, so RTO is reported as not measured",
            "drill_max_age_days": p.drill_max_age_days, "stale_after_seconds": p.stale_after_secs,
        },
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct DrQuery {
    pub target_rpo_secs: Option<i64>,
    pub drill_max_age_days: Option<i64>,
}

/// `GET /dr/readiness` — DR readiness score and explicit gaps from the DR catalog.
pub(crate) async fn get_dr_readiness(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    axum::extract::Query(q): axum::extract::Query<DrQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let mut p = DrParams::default();
    if let Some(v) = q.target_rpo_secs {
        if !(1..=31_536_000).contains(&v) {
            return bad("target_rpo_secs must be 1..=31536000");
        }
        p.target_rpo_secs = v;
    }
    if let Some(v) = q.drill_max_age_days {
        if !(1..=3_650).contains(&v) {
            return bad("drill_max_age_days must be 1..=3650");
        }
        p.drill_max_age_days = v;
    }
    let peers = atlas_inventory::dr::list_peers(&s.pool).await?;
    let mirrors = atlas_inventory::dr::list_mirrors(&s.pool).await?;
    Ok(Json(assess_dr(&peers, &mirrors, Utc::now(), &p)))
}

// ---------------------------------------------------------------- scrub plan

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScrubItem {
    pub id: String,
    /// `zfs`, `ceph` or `native` (anything else uses the generic default interval).
    pub kind: String,
    pub size_bytes: Option<f64>,
    /// Unix seconds of the last completed scrub; absent means never or unknown.
    pub last_scrub_ts: Option<i64>,
    pub interval_days: Option<f64>,
    /// True while a scrub, resilver or rebalance is already running on it.
    pub busy: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScrubBody {
    pub items: Vec<ScrubItem>,
    /// Scrubs allowed at once (default 1, max 16).
    pub max_concurrent: Option<usize>,
    /// Assumed scrub throughput per item in MiB/s (default 100).
    pub throughput_mib_s: Option<f64>,
}

fn default_interval_days(kind: &str) -> f64 {
    match kind {
        "zfs" => 30.0,
        "ceph" => 7.0,
        _ => 14.0,
    }
}

pub(crate) fn plan_scrubs(body: &ScrubBody, now_ts: i64) -> AppResult<Value> {
    if body.items.is_empty() || body.items.len() > MAX_SCRUB_ITEMS {
        return bad(format!("items must hold 1..={MAX_SCRUB_ITEMS} entries"));
    }
    let lanes_n = body.max_concurrent.unwrap_or(1);
    if !(1..=16).contains(&lanes_n) {
        return bad("max_concurrent must be 1..=16");
    }
    let tp = body.throughput_mib_s.unwrap_or(100.0);
    if !tp.is_finite() || !(1.0..=1_000_000.0).contains(&tp) {
        return bad("throughput_mib_s must be 1..=1000000");
    }
    struct Row {
        id: String,
        kind: String,
        overdue_secs: Option<i64>,
        ratio: f64,
        dur: f64,
        busy: bool,
        never: bool,
    }
    let mut rows = Vec::new();
    for it in &body.items {
        if it.id.is_empty() || it.id.len() > 200 || it.kind.len() > 40 {
            return bad("item id must be 1..=200 characters and kind at most 40");
        }
        let iv = it
            .interval_days
            .unwrap_or_else(|| default_interval_days(&it.kind));
        if !iv.is_finite() || !(0.04..=3_650.0).contains(&iv) {
            return bad(format!("item {}: interval_days must be 0.04..=3650", it.id));
        }
        let size = it.size_bytes.unwrap_or(0.0);
        if !size.is_finite() || size < 0.0 {
            return bad(format!(
                "item {}: size_bytes must be finite and non-negative",
                it.id
            ));
        }
        let dur = (size / (tp * 1_048_576.0)).clamp(60.0, 30.0 * DAY);
        let (overdue, ratio, never) = match it.last_scrub_ts {
            None => (None, f64::MAX, true),
            Some(ts) => {
                let age = now_ts.saturating_sub(ts).max(0) as f64;
                (Some((age - iv * DAY) as i64), age / (iv * DAY), false)
            }
        };
        rows.push(Row {
            id: it.id.clone(),
            kind: it.kind.clone(),
            overdue_secs: overdue,
            ratio,
            dur,
            busy: it.busy.unwrap_or(false),
            never,
        });
    }
    // Most overdue first; never scrubbed counts as most overdue; ties by id for stable output.
    rows.sort_by(|a, b| b.ratio.total_cmp(&a.ratio).then_with(|| a.id.cmp(&b.id)));
    let mut lanes = vec![0.0_f64; lanes_n];
    let mut plan = Vec::new();
    let mut skipped = Vec::new();
    for r in &rows {
        if r.ratio < 1.0 && !r.never {
            skipped.push(json!({ "id": r.id, "reason": "not due yet" }));
            continue;
        }
        // Items already running are reported, not scheduled again.
        if r.busy {
            skipped.push(
                json!({ "id": r.id, "reason": "already busy (scrub, resilver or rebalance)" }),
            );
            continue;
        }
        let (idx, start) = lanes
            .iter()
            .copied()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0, 0.0));
        lanes[idx] = start + r.dur;
        plan.push(json!({
            "id": r.id, "kind": r.kind, "lane": idx,
            "start_offset_seconds": start as i64, "estimated_duration_seconds": r.dur as i64,
            "overdue_seconds": r.overdue_secs,
            "reason": if r.never { "no scrub recorded" } else { "past its scrub interval" },
        }));
    }
    Ok(json!({
        "can_execute": false, "dry_run": true, "plan": plan, "skipped": skipped,
        "total_seconds": lanes.iter().cloned().fold(0.0, f64::max) as i64,
        "assumptions": {
            "max_concurrent": lanes_n, "throughput_mib_s": tp,
            "default_interval_days": { "zfs": 30, "ceph": 7, "other": 14 },
            "note": "heuristic ordering only; this plan starts nothing. Scrub I/O competes with clients, so run it in a quiet window",
        },
    }))
}

/// `POST /maintenance/scrub-plan` — dry-run scrub ordering from caller-supplied scrub history.
pub(crate) async fn post_scrub_plan(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Json(body): Json<ScrubBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    Ok(Json(plan_scrubs(&body, Utc::now().timestamp())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(cap: f64, pts: &[(i64, f64)]) -> PoolSeries {
        PoolSeries {
            id: "p".into(),
            capacity_bytes: cap,
            samples: pts
                .iter()
                .map(|&(ts, u)| Sample { ts, used_bytes: u })
                .collect(),
        }
    }
    fn th() -> Thresholds {
        thresholds(None, None).unwrap()
    }
    const GIB: f64 = 1_073_741_824.0;

    #[test]
    fn linear_growth_gives_days_to_full() {
        // +10 GiB/day on 100 GiB, 40 used: 6 days left.
        let pts: Vec<_> = (0..10)
            .map(|i| (i * 86_400, i as f64 * 10.0 * GIB))
            .collect();
        // used_now = 90 GiB, growth 10/day, 10 GiB free: 1 day.
        let v = forecast_pool(&series(100.0 * GIB, &pts), &th()).unwrap();
        assert_eq!(v["status"], "filling");
        assert_eq!(v["severity"], "critical");
        let d = v["days_to_full"].as_f64().unwrap();
        assert!((d - 1.0).abs() < 0.11, "{d}");
        assert_eq!(v["confidence"], "high");
        assert_eq!(v["trend"], "steady");
    }

    #[test]
    fn flat_and_shrinking_are_not_filling() {
        let flat: Vec<_> = (0..6).map(|i| (i * 7_200, 10.0 * GIB)).collect();
        assert_eq!(
            forecast_pool(&series(100.0 * GIB, &flat), &th()).unwrap()["status"],
            "not_filling"
        );
        let shrink: Vec<_> = (0..6)
            .map(|i| (i * 7_200, 50.0 * GIB - i as f64 * GIB))
            .collect();
        let v = forecast_pool(&series(100.0 * GIB, &shrink), &th()).unwrap();
        assert_eq!(v["status"], "not_filling");
        assert!(v["days_to_full"].is_null());
    }

    #[test]
    fn full_pool_and_insufficient_data() {
        let v = forecast_pool(&series(10.0, &[(0, 10.0)]), &th()).unwrap();
        assert_eq!(v["status"], "full");
        assert_eq!(
            forecast_pool(&series(10.0, &[]), &th()).unwrap()["status"],
            "insufficient_data"
        );
        // three samples inside one hour
        let v = forecast_pool(&series(1e12, &[(0, 1.0), (10, 2.0), (20, 3.0)]), &th()).unwrap();
        assert_eq!(v["status"], "insufficient_data");
        // duplicate timestamps collapse
        let v = forecast_pool(&series(1e12, &[(5, 1.0), (5, 2.0), (5, 3.0)]), &th()).unwrap();
        assert_eq!(v["status"], "insufficient_data");
    }

    #[test]
    fn hostile_numbers_are_rejected() {
        for cap in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert!(
                forecast_pool(&series(cap, &[(0, 1.0)]), &th()).is_err(),
                "{cap}"
            );
        }
        for u in [f64::NAN, f64::NEG_INFINITY, -5.0] {
            assert!(
                forecast_pool(&series(100.0, &[(0, u)]), &th()).is_err(),
                "{u}"
            );
        }
        let big = PoolSeries {
            id: "x".into(),
            capacity_bytes: 1.0,
            samples: (0..=MAX_SAMPLES as i64)
                .map(|t| Sample {
                    ts: t,
                    used_bytes: 0.0,
                })
                .collect(),
        };
        assert!(forecast_pool(&big, &th()).is_err());
        let mut p = series(1.0, &[]);
        p.id = String::new();
        assert!(forecast_pool(&p, &th()).is_err());
        // extreme timestamps must not overflow or panic
        let v = forecast_pool(
            &series(1e12, &[(i64::MIN / 4, 1.0), (0, 2.0), (i64::MAX / 4, 3.0)]),
            &th(),
        );
        assert!(v.is_ok());
    }

    #[test]
    fn trend_detects_acceleration() {
        let mut pts = Vec::new();
        for i in 0..10i64 {
            pts.push((i * 86_400, i as f64 * GIB));
        }
        for i in 10..20i64 {
            pts.push((i * 86_400, 10.0 * GIB + (i - 10) as f64 * 6.0 * GIB));
        }
        let v = forecast_pool(&series(1000.0 * GIB, &pts), &th()).unwrap();
        assert_eq!(v["trend"], "accelerating");
    }

    #[test]
    fn noisy_series_has_low_confidence() {
        let pts: Vec<_> = (0..8)
            .map(|i| (i * 7_200, if i % 2 == 0 { 10.0 * GIB } else { 40.0 * GIB }))
            .collect();
        let v = forecast_pool(&series(1000.0 * GIB, &pts), &th()).unwrap();
        assert_eq!(v["confidence"], "low");
    }

    #[test]
    fn thresholds_validate_and_all_sorts_by_risk() {
        assert!(thresholds(Some(1.0), Some(5.0)).is_err());
        assert!(thresholds(Some(f64::NAN), None).is_err());
        assert!(thresholds(None, Some(-1.0)).is_err());
        let fast: Vec<_> = (0..6)
            .map(|i| (i * 86_400, 50.0 * GIB + i as f64 * 5.0 * GIB))
            .collect();
        let slow: Vec<_> = (0..6)
            .map(|i| (i * 86_400, 1.0 * GIB + i as f64 * 0.1 * GIB))
            .collect();
        let mut a = series(100.0 * GIB, &slow);
        a.id = "slow".into();
        let mut b = series(100.0 * GIB, &fast);
        b.id = "fast".into();
        let out = forecast_all(&ForecastBody {
            pools: vec![a, b],
            warn_days: None,
            critical_days: None,
        })
        .unwrap();
        assert_eq!(out["pools"][0]["id"], "fast");
        assert_eq!(out["can_execute"], false);
        assert!(forecast_all(&ForecastBody {
            pools: vec![],
            warn_days: None,
            critical_days: None
        })
        .is_err());
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    fn peer() -> Value {
        json!({ "id": "peer1", "name": "dr" })
    }
    fn mirror(rpo: Value, state: &str) -> Value {
        json!({ "id": "m1", "peer_id": "peer1", "state": state, "rpo_seconds": rpo,
                "updated_at": "2026-11-23T11:50:00Z", "last_failover_at": "2026-11-01T00:00:00Z",
                "force_promoted": false, "last_error": null })
    }
    fn codes(v: &Value) -> Vec<String> {
        v["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["code"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn dr_healthy_scores_100() {
        let v = assess_dr(
            &[peer()],
            &[mirror(json!(120), "up")],
            now(),
            &DrParams::default(),
        );
        assert_eq!(v["score"], 100);
        assert_eq!(v["grade"], "ready");
        assert!(v["rto_measured_seconds"].is_null());
    }

    #[test]
    fn dr_empty_is_not_ready() {
        let v = assess_dr(&[], &[], now(), &DrParams::default());
        assert_eq!(v["score"], 50);
        assert!(codes(&v).contains(&"NO_PEER".to_string()));
        assert!(codes(&v).contains(&"NO_MIRRORS".to_string()));
    }

    fn no_drill(mut m: Value) -> Value {
        m["last_failover_at"] = Value::Null;
        m
    }

    #[test]
    fn dr_gaps_are_explicit() {
        let mut m = mirror(json!(10_000), "error");
        m["last_error"] = json!("x".repeat(5000));
        m["force_promoted"] = json!(true);
        m["updated_at"] = json!("garbage");
        m["last_failover_at"] = Value::Null;
        let v = assess_dr(
            &[peer()],
            &[
                m,
                no_drill(mirror(Value::Null, "up")),
                no_drill(mirror(json!(-5), "up")),
            ],
            now(),
            &DrParams::default(),
        );
        let c = codes(&v);
        for want in [
            "MIRROR_ERROR",
            "RPO_BREACH",
            "FORCE_PROMOTED",
            "STATE_TIME_UNKNOWN",
            "RPO_UNKNOWN",
            "RPO_INVALID",
            "NO_DRILL",
        ] {
            assert!(c.contains(&want.to_string()), "{want} in {c:?}");
        }
        assert_eq!(v["grade"], "not_ready");
        assert!(v["score"].as_i64().unwrap() >= 0);
        let detail = v["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["code"] == "MIRROR_ERROR")
            .unwrap()["detail"]
            .as_str()
            .unwrap()
            .len();
        assert!(detail < 300);
    }

    #[test]
    fn dr_stale_drill_and_over_target() {
        let mut m = mirror(json!(4_000), "up");
        m["last_failover_at"] = json!("2025-01-01T00:00:00Z");
        m["updated_at"] = json!("2026-11-23T01:00:00Z");
        let c = codes(&assess_dr(&[peer()], &[m], now(), &DrParams::default()));
        assert!(c.contains(&"RPO_OVER_TARGET".to_string()));
        assert!(c.contains(&"DRILL_STALE".to_string()));
        assert!(c.contains(&"STATE_STALE".to_string()));
    }

    fn item(id: &str, kind: &str, last: Option<i64>) -> ScrubItem {
        ScrubItem {
            id: id.into(),
            kind: kind.into(),
            size_bytes: Some(100.0 * GIB),
            last_scrub_ts: last,
            interval_days: None,
            busy: None,
        }
    }

    #[test]
    fn scrub_plan_orders_by_overdue_and_respects_lanes() {
        let now = 100 * 86_400;
        let body = ScrubBody {
            items: vec![
                item("recent", "zfs", Some(now - 86_400)),
                item("old", "zfs", Some(now - 60 * 86_400)),
                item("never", "ceph", None),
                item("old2", "ceph", Some(now - 70 * 86_400)),
            ],
            max_concurrent: Some(1),
            throughput_mib_s: None,
        };
        let v = plan_scrubs(&body, now).unwrap();
        let plan = v["plan"].as_array().unwrap();
        assert_eq!(plan[0]["id"], "never");
        assert_eq!(plan.len(), 3);
        assert_eq!(v["skipped"][0]["id"], "recent");
        // serial: second starts when first ends
        assert_eq!(
            plan[1]["start_offset_seconds"],
            plan[0]["estimated_duration_seconds"]
        );
        let two = ScrubBody {
            max_concurrent: Some(2),
            ..body
        };
        let v2 = plan_scrubs(&two, now).unwrap();
        assert_eq!(v2["plan"][1]["start_offset_seconds"], 0);
        assert_eq!(v2["dry_run"], true);
    }

    #[test]
    fn scrub_plan_skips_busy_and_rejects_hostile() {
        let mut b = item("a", "zfs", None);
        b.busy = Some(true);
        let body = ScrubBody {
            items: vec![b],
            max_concurrent: None,
            throughput_mib_s: None,
        };
        let v = plan_scrubs(&body, 0).unwrap();
        assert!(v["plan"].as_array().unwrap().is_empty());
        let mk = |f: &dyn Fn(&mut ScrubItem)| {
            let mut i = item("a", "zfs", None);
            f(&mut i);
            ScrubBody {
                items: vec![i],
                max_concurrent: None,
                throughput_mib_s: None,
            }
        };
        assert!(plan_scrubs(&mk(&|i| i.size_bytes = Some(f64::NAN)), 0).is_err());
        assert!(plan_scrubs(&mk(&|i| i.size_bytes = Some(-1.0)), 0).is_err());
        assert!(plan_scrubs(&mk(&|i| i.interval_days = Some(0.0)), 0).is_err());
        assert!(plan_scrubs(&mk(&|i| i.interval_days = Some(f64::INFINITY)), 0).is_err());
        assert!(plan_scrubs(&mk(&|i| i.id = String::new()), 0).is_err());
        let mut z = mk(&|_| {});
        z.max_concurrent = Some(0);
        assert!(plan_scrubs(&z, 0).is_err());
        let mut z = mk(&|_| {});
        z.throughput_mib_s = Some(f64::NAN);
        assert!(plan_scrubs(&z, 0).is_err());
        let many = ScrubBody {
            items: (0..=MAX_SCRUB_ITEMS)
                .map(|i| item(&i.to_string(), "zfs", None))
                .collect(),
            max_concurrent: None,
            throughput_mib_s: None,
        };
        assert!(plan_scrubs(&many, 0).is_err());
        // future last_scrub_ts and huge sizes do not panic
        let mut f = item("f", "zfs", Some(i64::MAX));
        f.size_bytes = Some(f64::MAX);
        assert!(plan_scrubs(
            &ScrubBody {
                items: vec![f],
                max_concurrent: None,
                throughput_mib_s: None
            },
            0
        )
        .is_ok());
    }
}
