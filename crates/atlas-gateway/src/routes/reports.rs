// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Read-only operations reports (`docs/REPORTS.md`): snapshot hygiene, tenant quota headroom,
//! snapshot-schedule coverage and a Ceph health-check explainer. Each response carries
//! `can_execute: false` and an `assumptions` object naming every threshold. Nothing here deletes,
//! changes or calls a backend; the assessment functions are pure so they are tested with
//! fabricated inputs, and the handlers only read the catalog.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    extract::{Query, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_api_types::{SnapshotSchedule, StorageSnapshot, StorageVolume, TenantQuota};
use atlas_common::{AppError, AppResult};

use crate::state::AppState;

/// Findings returned per report; the summary always counts all of them.
const MAX_FINDINGS: usize = 1_000;
/// Codes accepted by the health explainer in one request.
const MAX_CODES: usize = 200;

fn bad<T>(msg: impl Into<String>) -> AppResult<T> {
    Err(AppError::Validation(msg.into()))
}

fn finding(code: &str, severity: &str, subject: &str, detail: String, fix: &str) -> Value {
    json!({ "code": code, "severity": severity, "subject": subject, "detail": detail, "remediation": fix })
}

fn sev_rank(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "warning" => 1,
        _ => 2,
    }
}

fn sort_findings(f: &mut [Value]) {
    f.sort_by(|a, b| {
        let ka = (
            sev_rank(a["severity"].as_str().unwrap_or("")),
            a["code"].as_str().unwrap_or("").to_string(),
            a["subject"].as_str().unwrap_or("").to_string(),
        );
        let kb = (
            sev_rank(b["severity"].as_str().unwrap_or("")),
            b["code"].as_str().unwrap_or("").to_string(),
            b["subject"].as_str().unwrap_or("").to_string(),
        );
        ka.cmp(&kb)
    });
}

fn count_by(f: &[Value], key: &str) -> Value {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for v in f {
        *m.entry(v[key].as_str().unwrap_or("").to_string())
            .or_default() += 1;
    }
    json!(m)
}

fn parse_ts(ts: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn ranged_i64(name: &str, v: Option<i64>, default: i64, lo: i64, hi: i64) -> AppResult<i64> {
    match v {
        None => Ok(default),
        Some(x) if (lo..=hi).contains(&x) => Ok(x),
        Some(_) => bad(format!("{name} must be {lo}..={hi}")),
    }
}

// ---------------------------------------------------------------- snapshot hygiene

#[derive(Debug, Clone)]
pub(crate) struct HygieneParams {
    pub stale_days: i64,
    pub max_per_volume: i64,
    pub lock_warn_days: i64,
    pub stuck_minutes: i64,
}

impl Default for HygieneParams {
    fn default() -> Self {
        Self {
            stale_days: 90,
            max_per_volume: 30,
            lock_warn_days: 7,
            stuck_minutes: 60,
        }
    }
}

/// Assess the snapshot catalog. `volume_ids` is every volume id the catalog knows; a snapshot
/// whose volume is not in it is an orphan. A snapshot is "actively locked" while its recorded
/// `locked_until` is in the future; locked snapshots are never reported as old or reclaimable.
pub(crate) fn assess_snapshots(
    snaps: &[StorageSnapshot],
    volume_ids: &HashSet<String>,
    now: DateTime<Utc>,
    p: &HygieneParams,
) -> Value {
    let now_ts = now.timestamp();
    let mut out: Vec<Value> = Vec::new();
    // volume -> (count, oldest_ts, newest_ts)
    let mut per_vol: HashMap<&str, (u64, Option<i64>, Option<i64>)> = HashMap::new();
    let mut locked_active = 0u64;

    for s in snaps {
        let created = parse_ts(s.created_at.as_deref()).map(|d| d.timestamp());
        let e = per_vol
            .entry(s.volume_id.as_str())
            .or_insert((0, None, None));
        e.0 += 1;
        if let Some(c) = created {
            e.1 = Some(e.1.map_or(c, |o| o.min(c)));
            e.2 = Some(e.2.map_or(c, |n| n.max(c)));
        }
        let locked = s.locked_until.map(|u| u > now_ts).unwrap_or(false);
        if locked {
            locked_active += 1;
        }
        if !volume_ids.contains(&s.volume_id) {
            out.push(finding(
                "ORPHAN_SNAPSHOT",
                "warning",
                &s.id,
                format!(
                    "snapshot {} refers to volume {} that is not in the catalog{}",
                    s.name,
                    s.volume_id,
                    if locked { " (retention-locked: not deletable yet)" } else { "" }
                ),
                "Confirm nothing needs a restore from it, then DELETE /snapshots/{id}?force=true once any lock has expired.",
            ));
        }
        match s.state.as_str() {
            "ready" => {}
            "error" | "failed" => out.push(finding(
                "FAILED_SNAPSHOT",
                "warning",
                &s.id,
                format!("snapshot {} is in state {}", s.name, s.state),
                "Inspect the job that created it (GET /jobs), then delete the row with DELETE /snapshots/{id}.",
            )),
            other => {
                if let Some(c) = created {
                    let age_min = (now_ts - c) / 60;
                    if age_min >= p.stuck_minutes {
                        out.push(finding(
                            "STUCK_SNAPSHOT",
                            "warning",
                            &s.id,
                            format!("snapshot {} has been {other} for {age_min} minutes", s.name),
                            "Check the creating job (GET /jobs); a snapshot that never reaches ready may have lost its job.",
                        ));
                    }
                }
            }
        }
        match (created, s.state.as_str()) {
            (None, _) => out.push(finding(
                "NO_TIMESTAMP",
                "info",
                &s.id,
                format!(
                    "snapshot {} has no parseable creation time; age checks skipped",
                    s.name
                ),
                "None needed; the catalog row predates timestamps or was imported.",
            )),
            (Some(c), "ready") => {
                let age_days = (now_ts - c) / 86_400;
                if age_days >= p.stale_days && !s.protected && !locked {
                    out.push(finding(
                        "OLD_SNAPSHOT",
                        "info",
                        &s.id,
                        format!("snapshot {} is {age_days} days old, unprotected and unlocked", s.name),
                        "Review whether it is still needed; delete with DELETE /snapshots/{id} if not.",
                    ));
                }
            }
            _ => {}
        }
        if let Some(until) = s.locked_until {
            if until <= now_ts {
                out.push(finding(
                    "LOCK_EXPIRED",
                    "info",
                    &s.id,
                    format!(
                        "retention lock on {} ended {} days ago; it can now be deleted",
                        s.name,
                        (now_ts - until) / 86_400
                    ),
                    "Delete it if retention is satisfied, or POST /snapshots/{id}/lock to extend.",
                ));
            } else if until - now_ts <= p.lock_warn_days * 86_400 {
                out.push(finding(
                    "LOCK_EXPIRING",
                    "info",
                    &s.id,
                    format!("retention lock on {} ends in {:.1} days", s.name, (until - now_ts) as f64 / 86_400.0),
                    "Extend with POST /snapshots/{id}/lock if the data must stay protected (locks only extend).",
                ));
            }
        }
    }

    let mut volumes: Vec<Value> = Vec::new();
    for (vol, (n, oldest, newest)) in &per_vol {
        if *n as i64 > p.max_per_volume {
            out.push(finding(
                "OVER_RETAINED",
                "warning",
                vol,
                format!(
                    "volume {vol} has {n} snapshots, above the limit of {}",
                    p.max_per_volume
                ),
                "Lower the schedule's keep, or delete the oldest unprotected snapshots.",
            ));
        }
        volumes.push(json!({
            "volume_id": vol,
            "snapshots": n,
            "oldest_age_days": oldest.map(|o| (now_ts - o) / 86_400),
            "newest_age_days": newest.map(|o| (now_ts - o) / 86_400),
            "in_catalog": volume_ids.contains(*vol),
        }));
    }
    volumes.sort_by(|a, b| {
        b["snapshots"]
            .as_u64()
            .cmp(&a["snapshots"].as_u64())
            .then_with(|| a["volume_id"].as_str().cmp(&b["volume_id"].as_str()))
    });
    sort_findings(&mut out);
    let total = out.len();
    let by_code = count_by(&out, "code");
    let by_severity = count_by(&out, "severity");
    out.truncate(MAX_FINDINGS);
    volumes.truncate(50);
    json!({
        "can_execute": false,
        "summary": {
            "snapshots": snaps.len(),
            "volumes_with_snapshots": per_vol.len(),
            "actively_locked": locked_active,
            "findings": total,
            "by_code": by_code,
            "by_severity": by_severity,
        },
        "findings": out,
        "findings_truncated": total > MAX_FINDINGS,
        "busiest_volumes": volumes,
        "assumptions": {
            "stale_days": p.stale_days,
            "max_per_volume": p.max_per_volume,
            "lock_warn_days": p.lock_warn_days,
            "stuck_minutes": p.stuck_minutes,
            "note": "Thresholds are operator hygiene heuristics, not backend limits. Only snapshot rows in the Atlas catalog are examined; locks set directly on a native node are not visible.",
        },
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct HygieneQuery {
    pub stale_days: Option<i64>,
    pub max_per_volume: Option<i64>,
    pub lock_warn_days: Option<i64>,
    pub stuck_minutes: Option<i64>,
}

/// `GET /snapshots/hygiene` — orphan, failed, stuck, over-retained, old and lock-expiry findings.
pub(crate) async fn get_snapshot_hygiene(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Query(q): Query<HygieneQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let d = HygieneParams::default();
    let p = HygieneParams {
        stale_days: ranged_i64("stale_days", q.stale_days, d.stale_days, 1, 3_650)?,
        max_per_volume: ranged_i64(
            "max_per_volume",
            q.max_per_volume,
            d.max_per_volume,
            1,
            100_000,
        )?,
        lock_warn_days: ranged_i64("lock_warn_days", q.lock_warn_days, d.lock_warn_days, 1, 365)?,
        stuck_minutes: ranged_i64(
            "stuck_minutes",
            q.stuck_minutes,
            d.stuck_minutes,
            1,
            100_000,
        )?,
    };
    let snaps = atlas_inventory::snapshots::list_snapshots(&s.pool, None).await?;
    let ids: HashSet<String> = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .map(|v| v.id)
        .collect();
    Ok(Json(assess_snapshots(&snaps, &ids, Utc::now(), &p)))
}

// ---------------------------------------------------------------- quota headroom

/// Classify one dimension: `None` when the limit is 0 (unlimited).
fn pct(used: i64, limit: i64) -> Option<f64> {
    if limit <= 0 {
        None
    } else {
        Some(used.max(0) as f64 * 100.0 / limit as f64)
    }
}

/// Per-tenant quota headroom. `exceeded` means usage is above a limit (possible after the limit
/// was lowered); `critical` and `warning` are at or above the percent thresholds.
pub(crate) fn assess_quotas(rows: &[TenantQuota], warn: f64, critical: f64) -> Value {
    let mut tenants: Vec<(f64, Value)> = Vec::new();
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for r in rows {
        let bp = pct(r.used_bytes, r.max_bytes);
        let vp = pct(r.volume_count, r.max_volumes);
        let (worst, dim) = match (bp, vp) {
            (None, None) => (None, "none"),
            (Some(b), None) => (Some(b), "bytes"),
            (None, Some(v)) => (Some(v), "volumes"),
            (Some(b), Some(v)) => {
                if v > b {
                    (Some(v), "volumes")
                } else {
                    (Some(b), "bytes")
                }
            }
        };
        let over = (r.max_bytes > 0 && r.used_bytes > r.max_bytes)
            || (r.max_volumes > 0 && r.volume_count > r.max_volumes);
        let status = match worst {
            None => "unlimited",
            Some(_) if over => "exceeded",
            Some(w) if w >= critical => "critical",
            Some(w) if w >= warn => "warning",
            Some(_) => "ok",
        };
        *counts.entry(status).or_default() += 1;
        let round = |x: f64| (x * 10.0).round() / 10.0;
        tenants.push((
            worst.unwrap_or(-1.0),
            json!({
                "tenant_id": r.tenant_id,
                "status": status,
                "limiting_dimension": dim,
                "percent_used": worst.map(round),
                "bytes": { "used": r.used_bytes, "limit": r.max_bytes,
                           "percent": bp.map(round),
                           "headroom": (r.max_bytes > 0).then(|| r.max_bytes - r.used_bytes) },
                "volumes": { "used": r.volume_count, "limit": r.max_volumes,
                             "percent": vp.map(round),
                             "headroom": (r.max_volumes > 0).then(|| r.max_volumes - r.volume_count) },
            }),
        ));
    }
    tenants.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1["tenant_id"].as_str().cmp(&b.1["tenant_id"].as_str()))
    });
    json!({
        "can_execute": false,
        "summary": { "tenants": rows.len(), "by_status": counts },
        "tenants": tenants.into_iter().map(|t| t.1).collect::<Vec<_>>(),
        "assumptions": {
            "warn_percent": warn,
            "critical_percent": critical,
            "note": "Usage is provisioned volume bytes and volume count, the same numbers volume-create admission checks. A limit of 0 means unlimited.",
        },
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct HeadroomQuery {
    pub warn_percent: Option<f64>,
    pub critical_percent: Option<f64>,
}

/// `GET /tenants/quota-headroom` — every tenant's usage against its limits, most urgent first.
pub(crate) async fn get_quota_headroom(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Query(q): Query<HeadroomQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let warn = q.warn_percent.unwrap_or(80.0);
    let crit = q.critical_percent.unwrap_or(95.0);
    if !(warn.is_finite() && crit.is_finite()) || warn <= 0.0 || crit > 100.0 || warn >= crit {
        return bad("need 0 < warn_percent < critical_percent <= 100");
    }
    let rows = atlas_inventory::tenants::list_overview(&s.pool).await?;
    Ok(Json(assess_quotas(&rows, warn, crit)))
}

// ---------------------------------------------------------------- schedule coverage

#[derive(Debug, Clone)]
pub(crate) struct CoverageParams {
    pub min_keep: i64,
    pub overdue_intervals: i64,
}

impl Default for CoverageParams {
    fn default() -> Self {
        Self {
            min_keep: 2,
            overdue_intervals: 2,
        }
    }
}

/// Which volumes have a working snapshot or backup schedule, and which schedules look broken.
/// Volumes already being deleted or failed are not expected to be scheduled.
pub(crate) fn assess_coverage(
    volumes: &[StorageVolume],
    schedules: &[SnapshotSchedule],
    now: DateTime<Utc>,
    p: &CoverageParams,
) -> Value {
    let ids: HashSet<&str> = volumes.iter().map(|v| v.id.as_str()).collect();
    let now_ts = now.timestamp();
    let mut out: Vec<Value> = Vec::new();
    let mut covered: HashSet<&str> = HashSet::new();

    for sc in schedules {
        if !ids.contains(sc.volume_id.as_str()) {
            out.push(finding(
                "ORPHAN_SCHEDULE",
                "warning",
                &sc.id,
                format!(
                    "schedule {} targets volume {} that is not in the catalog",
                    sc.id, sc.volume_id
                ),
                "DELETE /schedules/{id}.",
            ));
            continue;
        }
        if !sc.enabled {
            out.push(finding(
                "SCHEDULE_DISABLED",
                "info",
                &sc.id,
                format!(
                    "{} schedule {} on {} is disabled",
                    sc.kind, sc.id, sc.volume_id
                ),
                "Delete it if abandoned; a disabled schedule gives no protection.",
            ));
            continue;
        }
        covered.insert(sc.volume_id.as_str());
        if sc.keep == 0 {
            out.push(finding(
                "NO_RETENTION_LIMIT",
                "info",
                &sc.id,
                format!(
                    "schedule {} keeps everything (keep=0); snapshots grow without bound",
                    sc.id
                ),
                "Set a keep count so old snapshots are pruned.",
            ));
        } else if sc.keep < p.min_keep {
            out.push(finding(
                "LOW_RETENTION",
                "info",
                &sc.id,
                format!(
                    "schedule {} keeps {} (below {}): one failed run leaves no restore point",
                    sc.id, sc.keep, p.min_keep
                ),
                "Raise keep to at least 2.",
            ));
        }
        if let Some(next) = parse_ts(Some(&sc.next_run_at)) {
            let overdue = now_ts - next.timestamp();
            let grace = (sc.interval_secs.max(1) * p.overdue_intervals).max(300);
            if overdue > grace {
                out.push(finding(
                    "STALLED_SCHEDULE",
                    "warning",
                    &sc.id,
                    format!("schedule {} should have run {} minutes ago (interval {}s)", sc.id, overdue / 60, sc.interval_secs),
                    "Check that the snapshot tick is enabled (ATLAS_SNAPSHOT_TICK_SECS) and this gateway holds the leader lease.",
                ));
            }
        }
    }

    let mut unprotected = 0u64;
    let mut eligible = 0u64;
    for v in volumes {
        if matches!(
            v.state.as_str(),
            "deleting" | "deleted" | "failed" | "error"
        ) {
            continue;
        }
        eligible += 1;
        if !covered.contains(v.id.as_str()) {
            unprotected += 1;
            out.push(finding(
                "UNSCHEDULED_VOLUME",
                "warning",
                &v.id,
                format!(
                    "volume {} has no enabled snapshot or backup schedule",
                    v.name
                ),
                "POST /volumes/{id}/schedule to add one.",
            ));
        }
    }
    sort_findings(&mut out);
    let total = out.len();
    let by_code = count_by(&out, "code");
    out.truncate(MAX_FINDINGS);
    json!({
        "can_execute": false,
        "summary": {
            "volumes_considered": eligible,
            "scheduled": eligible - unprotected,
            "unscheduled": unprotected,
            "coverage_percent": if eligible == 0 { Value::Null }
                else { json!(((eligible - unprotected) as f64 * 1000.0 / eligible as f64).round() / 10.0) },
            "schedules": schedules.len(),
            "findings": total,
            "by_code": by_code,
        },
        "findings": out,
        "findings_truncated": total > MAX_FINDINGS,
        "assumptions": {
            "min_keep": p.min_keep,
            "overdue_intervals": p.overdue_intervals,
            "note": "A volume counts as scheduled when it has at least one enabled schedule (snapshot or backup). A schedule is stalled when next_run_at is more than max(overdue_intervals x interval, 5 minutes) in the past. This is catalog coverage only: whether a snapshot is restorable is not tested.",
        },
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoverageQuery {
    pub min_keep: Option<i64>,
    pub overdue_intervals: Option<i64>,
}

/// `GET /schedules/coverage` — unscheduled volumes and unhealthy schedules.
pub(crate) async fn get_schedule_coverage(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Query(q): Query<CoverageQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let d = CoverageParams::default();
    let p = CoverageParams {
        min_keep: ranged_i64("min_keep", q.min_keep, d.min_keep, 1, 10_000)?,
        overdue_intervals: ranged_i64(
            "overdue_intervals",
            q.overdue_intervals,
            d.overdue_intervals,
            1,
            1_000,
        )?,
    };
    let volumes = atlas_inventory::list_volumes(&s.pool).await?;
    let schedules = atlas_inventory::schedules::list(&s.pool, None).await?;
    Ok(Json(assess_coverage(&volumes, &schedules, Utc::now(), &p)))
}

// ---------------------------------------------------------------- ceph health explainer

/// (code, severity, meaning, first step). Taken from the Ceph health-checks reference
/// (https://docs.ceph.com/en/latest/rados/operations/health-checks/, read 2026-11-24). Severity
/// is Atlas's own triage hint (`data_risk`, `degraded`, `capacity`, `hygiene`), not Ceph's.
const CEPH_CHECKS: &[(&str, &str, &str, &str)] = &[
    ("OSD_DOWN", "degraded", "One or more OSDs are marked down, often because a daemon, host or network failed.", "Verify the host is healthy, the daemon is running and the network works."),
    ("OSD_NEARFULL", "capacity", "One or more OSDs passed the nearfull threshold, an early warning that the cluster is filling.", "ceph df"),
    ("OSD_FULL", "data_risk", "One or more OSDs passed the full threshold, which blocks writes.", "ceph df; short-term: ceph osd set-full-ratio <ratio>"),
    ("POOL_FULL", "data_risk", "A pool reached or is close to its quota and no longer accepts writes.", "ceph df detail"),
    ("PG_DEGRADED", "degraded", "Some data has fewer replicas or erasure-code fragments than desired.", "ceph health detail"),
    ("PG_AVAILABILITY", "data_risk", "Some PGs cannot serve reads or writes (peering, stale or incomplete).", "ceph health detail; then ceph tell <pgid> query"),
    ("SLOW_OPS", "degraded", "OSD or monitor requests are taking too long to process.", "ceph daemon osd.<id> ops (on the daemon's host)"),
    ("OSDMAP_FLAGS", "hygiene", "Cluster-wide flags such as noout or pauserd are set.", "ceph osd unset <flag> if the flag was left on after maintenance"),
    ("OSD_FLAGS", "hygiene", "Flags are set on specific OSDs, CRUSH nodes or device classes.", "ceph osd unset-group <flags> <who>"),
    ("MON_CLOCK_SKEW", "degraded", "Clocks on monitor hosts differ by more than the allowed drift.", "Synchronise clocks with ntpd or chrony (no Ceph command fixes it)."),
    ("MON_DOWN", "degraded", "One or more monitor daemons are down, reducing quorum margin.", "Restore or restart the down monitors."),
    ("RECENT_CRASH", "hygiene", "A daemon crashed recently and the crash has not been archived.", "ceph crash ls-new"),
    ("PG_NOT_DEEP_SCRUBBED", "hygiene", "PGs have gone too long without a deep scrub.", "ceph health detail; then ceph pg deep-scrub <pgid>"),
    ("PG_NOT_SCRUBBED", "hygiene", "PGs have gone too long without a scrub.", "ceph pg scrub <pgid>"),
    ("OSD_SCRUB_ERRORS", "data_risk", "Recent scrubs found inconsistencies, usually alongside PG_DAMAGED.", "See the Ceph 'Monitoring OSDs and PGs' documentation."),
    ("PG_DAMAGED", "data_risk", "Scrubbing found data consistency problems in one or more PGs.", "See the Ceph 'Monitoring OSDs and PGs' documentation."),
    ("POOL_APP_NOT_ENABLED", "hygiene", "A pool exists but has not been tagged for an application.", "rbd pool init <pool> for RBD, or ceph osd pool application enable <pool> <app>"),
    ("TOO_MANY_PGS", "capacity", "PG count per OSD exceeds mon_max_pg_per_osd, which blocks pool creation and PG increases.", "Add OSDs, or mark out OSDs in with ceph osd in <id>"),
    ("OBJECT_MISPLACED", "hygiene", "Some objects are not on the nodes CRUSH prefers, usually while data migration is in progress.", "Normally none; Ceph says it is not dangerous by itself."),
    ("OSD_ORPHAN", "hygiene", "An OSD is in the CRUSH hierarchy but does not exist.", "ceph osd crush rm osd.<id>"),
    ("BLUESTORE_SPURIOUS_READ_ERRORS", "degraded", "BlueStore hit read errors on the main device and recovered by retrying; may point to hardware or I/O problems.", "Check host OS, kernel and hardware monitoring."),
    ("DEVICE_HEALTH", "degraded", "One or more OSD devices are expected to fail soon.", "ceph device info <device-id>"),
];

const CEPH_DOC: &str = "https://docs.ceph.com/en/latest/rados/operations/health-checks/";

/// Explain the given Ceph health-check codes. Unknown codes are listed, never guessed at.
pub(crate) fn explain_ceph_checks(codes: &[String]) -> AppResult<Value> {
    if codes.len() > MAX_CODES {
        return bad(format!("at most {MAX_CODES} codes"));
    }
    let mut known: Vec<Value> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let entry = |c: &(&str, &str, &str, &str)| {
        json!({ "code": c.0, "triage": c.1, "meaning": c.2, "first_step": c.3,
                "reference": format!("{CEPH_DOC}#{}", c.0.to_lowercase().replace('_', "-")) })
    };
    if codes.is_empty() {
        known = CEPH_CHECKS.iter().map(entry).collect();
    }
    for raw in codes {
        let code = raw.trim().to_uppercase();
        if code.is_empty()
            || code.len() > 64
            || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return bad(format!("not a Ceph check code: {raw:?}"));
        }
        if !seen.insert(code.clone()) {
            continue;
        }
        match CEPH_CHECKS.iter().find(|c| c.0 == code) {
            Some(c) => known.push(entry(c)),
            None => unknown.push(code),
        }
    }
    let rank = |t: &str| match t {
        "data_risk" => 0,
        "degraded" => 1,
        "capacity" => 2,
        _ => 3,
    };
    known.sort_by_key(|k| {
        (
            rank(k["triage"].as_str().unwrap_or("")),
            k["code"].as_str().unwrap_or("").to_string(),
        )
    });
    Ok(json!({
        "can_execute": false,
        "checks": known,
        "unknown": unknown,
        "source": CEPH_DOC,
        "note": "Static reference, not a live diagnosis: first_step is the command or action the Ceph reference names. Triage is Atlas's ordering (data_risk, degraded, capacity, hygiene). Codes not in this table are returned under unknown.",
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExplainQuery {
    /// Comma-separated codes; omit for the whole table.
    pub codes: Option<String>,
}

/// `GET /ceph/health-explain[?codes=OSD_DOWN,PG_DEGRADED]`.
pub(crate) async fn get_ceph_health_explain(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Query(q): Query<ExplainQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let codes: Vec<String> = q
        .codes
        .as_deref()
        .map(|c| {
            c.split(',')
                .filter(|x| !x.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(explain_ceph_checks(&codes)?))
}

/// Codes from a request body: `{"codes": [..]}` or the `checks` object of
/// `ceph health detail -f json` (its keys are the codes).
pub(crate) fn codes_from_body(body: &Value) -> AppResult<Vec<String>> {
    if let Some(a) = body.get("codes").and_then(|c| c.as_array()) {
        return a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| AppError::Validation("codes must be strings".into()))
            })
            .collect();
    }
    if let Some(m) = body.get("checks").and_then(|c| c.as_object()) {
        return Ok(m.keys().cloned().collect());
    }
    bad("body needs `codes` (array of strings) or `checks` (object keyed by code)")
}

/// `POST /ceph/health-explain` — same, from a body (paste `ceph health detail -f json`).
pub(crate) async fn post_ceph_health_explain(
    State(s): State<AppState>,
    Extension(actor): Extension<crate::auth::Actor>,
    Json(body): Json<Value>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let codes = codes_from_body(&body)?;
    if codes.is_empty() {
        return bad("no codes given");
    }
    Ok(Json(explain_ceph_checks(&codes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }
    fn iso(ago_secs: i64) -> String {
        atlas_inventory::now_rfc3339(Utc.timestamp_opt(now().timestamp() - ago_secs, 0).unwrap())
    }
    fn snap(id: &str, vol: &str, state: &str, age_days: i64) -> StorageSnapshot {
        StorageSnapshot {
            id: id.into(),
            tenant_id: "t".into(),
            volume_id: vol.into(),
            name: format!("n-{id}"),
            backend_native_id: None,
            consistency: "crash".into(),
            state: state.into(),
            protected: false,
            parent_snapshot_id: None,
            created_at: Some(iso(age_days * 86_400)),
            locked_until: None,
        }
    }
    fn codes(v: &Value) -> Vec<(String, String)> {
        v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["code"].as_str().unwrap().into(),
                    f["subject"].as_str().unwrap().into(),
                )
            })
            .collect()
    }
    fn has(v: &Value, code: &str, subject: &str) -> bool {
        codes(v).contains(&(code.to_string(), subject.to_string()))
    }

    #[test]
    fn snapshot_findings() {
        let vols: HashSet<String> = ["v1".to_string()].into();
        let mut locked = snap("locked-old", "v1", "ready", 400);
        locked.locked_until = Some(now().timestamp() + 3 * 86_400);
        let mut expired = snap("expired", "v1", "ready", 10);
        expired.locked_until = Some(now().timestamp() - 2 * 86_400);
        let mut prot = snap("prot-old", "v1", "ready", 400);
        prot.protected = true;
        let mut nots = snap("nots", "v1", "ready", 1);
        nots.created_at = None;
        let snaps = vec![
            snap("fine", "v1", "ready", 1),
            snap("old", "v1", "ready", 200),
            locked,
            expired,
            prot,
            nots,
            snap("orph", "gone", "ready", 1),
            snap("bad", "v1", "failed", 1),
            snap("stuck", "v1", "creating", 2),
            snap("fresh-creating", "v1", "creating", 0),
        ];
        let r = assess_snapshots(&snaps, &vols, now(), &HygieneParams::default());
        assert_eq!(r["can_execute"], false);
        assert!(has(&r, "OLD_SNAPSHOT", "old"));
        assert!(
            !has(&r, "OLD_SNAPSHOT", "locked-old"),
            "locked is not reclaimable"
        );
        assert!(
            !has(&r, "OLD_SNAPSHOT", "prot-old"),
            "protected is not reclaimable"
        );
        assert!(has(&r, "LOCK_EXPIRING", "locked-old"));
        assert!(has(&r, "LOCK_EXPIRED", "expired"));
        assert!(has(&r, "NO_TIMESTAMP", "nots"));
        assert!(has(&r, "ORPHAN_SNAPSHOT", "orph"));
        assert!(has(&r, "FAILED_SNAPSHOT", "bad"));
        assert!(has(&r, "STUCK_SNAPSHOT", "stuck"));
        assert!(!has(&r, "STUCK_SNAPSHOT", "fresh-creating"));
        assert!(!codes(&r).iter().any(|c| c.1 == "fine"));
        assert_eq!(r["summary"]["actively_locked"], 1);
        // warnings sort before info
        assert_eq!(r["findings"][0]["severity"], "warning");
    }

    #[test]
    fn over_retained_and_empty_catalog() {
        let vols: HashSet<String> = ["v1".to_string()].into();
        let snaps: Vec<_> = (0..5)
            .map(|i| snap(&format!("s{i}"), "v1", "ready", 1))
            .collect();
        let p = HygieneParams {
            max_per_volume: 4,
            ..Default::default()
        };
        let r = assess_snapshots(&snaps, &vols, now(), &p);
        assert!(has(&r, "OVER_RETAINED", "v1"));
        assert_eq!(r["busiest_volumes"][0]["snapshots"], 5);
        let e = assess_snapshots(&[], &vols, now(), &p);
        assert_eq!(e["summary"]["findings"], 0);
    }

    fn q(t: &str, mb: i64, ub: i64, mv: i64, uv: i64) -> TenantQuota {
        TenantQuota {
            tenant_id: t.into(),
            max_bytes: mb,
            max_volumes: mv,
            used_bytes: ub,
            volume_count: uv,
        }
    }

    #[test]
    fn quota_status_and_order() {
        let rows = vec![
            q("free", 0, 5_000, 0, 9),
            q("ok", 1000, 100, 10, 1),
            q("warn", 1000, 850, 0, 3),
            q("crit", 1000, 960, 0, 1),
            q("over", 1000, 1200, 0, 1),
            q("vols", 0, 0, 10, 10),
        ];
        let r = assess_quotas(&rows, 80.0, 95.0);
        let status = |t: &str| {
            r["tenants"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["tenant_id"] == t)
                .unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(status("free"), "unlimited");
        assert_eq!(status("ok"), "ok");
        assert_eq!(status("warn"), "warning");
        assert_eq!(status("crit"), "critical");
        assert_eq!(status("over"), "exceeded");
        assert_eq!(
            status("vols"),
            "critical",
            "100% of the volume limit, not over it"
        );
        // most urgent first, unlimited last
        assert_eq!(r["tenants"][0]["tenant_id"], "over");
        assert_eq!(r["tenants"][5]["tenant_id"], "free");
        let over = &r["tenants"][0];
        assert_eq!(over["bytes"]["headroom"], -200);
        assert_eq!(over["limiting_dimension"], "bytes");
        assert!(r["tenants"][5]["bytes"]["headroom"].is_null());
        assert_eq!(r["summary"]["by_status"]["critical"], 2);
    }

    fn vol(id: &str, state: &str) -> StorageVolume {
        StorageVolume {
            id: id.into(),
            cluster_id: None,
            pool_id: None,
            name: format!("name-{id}"),
            kind: atlas_api_types::VolumeKind::Block,
            backend_native_id: None,
            size_bytes: 1,
            used_bytes: None,
            state: state.into(),
            health: Default::default(),
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }
    }
    fn sched(id: &str, vol: &str, enabled: bool, keep: i64, next_ago: i64) -> SnapshotSchedule {
        SnapshotSchedule {
            id: id.into(),
            tenant_id: "t".into(),
            volume_id: vol.into(),
            kind: "snapshot".into(),
            bucket_id: None,
            mode: "manifest".into(),
            interval_secs: 3600,
            keep,
            enabled,
            last_run_at: None,
            next_run_at: iso(next_ago),
            created_at: None,
        }
    }

    #[test]
    fn coverage_findings() {
        let vols = vec![
            vol("a", "ready"),
            vol("b", "ready"),
            vol("c", "ready"),
            vol("d", "ready"),
            vol("gone", "deleting"),
        ];
        let scheds = vec![
            sched("s-a", "a", true, 7, -3600),
            sched("s-b", "b", false, 7, -3600),
            sched("s-c", "c", true, 1, 3 * 3600),
            sched("s-x", "nope", true, 7, -3600),
            sched("s-d", "d", true, 0, 0),
        ];
        let r = assess_coverage(&vols, &scheds, now(), &CoverageParams::default());
        assert!(
            has(&r, "UNSCHEDULED_VOLUME", "b"),
            "disabled schedule gives no protection"
        );
        assert!(has(&r, "SCHEDULE_DISABLED", "s-b"));
        assert!(has(&r, "ORPHAN_SCHEDULE", "s-x"));
        assert!(has(&r, "LOW_RETENTION", "s-c"));
        assert!(has(&r, "STALLED_SCHEDULE", "s-c"));
        assert!(has(&r, "NO_RETENTION_LIMIT", "s-d"));
        assert!(!codes(&r).iter().any(|c| c.1 == "s-a" || c.1 == "a"));
        assert!(
            !codes(&r).iter().any(|c| c.1 == "gone"),
            "deleting volumes are not expected to be scheduled"
        );
        assert_eq!(r["summary"]["volumes_considered"], 4);
        assert_eq!(r["summary"]["scheduled"], 3);
        assert_eq!(r["summary"]["coverage_percent"], 75.0);
        let none = assess_coverage(&[], &[], now(), &CoverageParams::default());
        assert!(none["summary"]["coverage_percent"].is_null());
    }

    #[test]
    fn ceph_explainer() {
        let all = explain_ceph_checks(&[]).unwrap();
        assert_eq!(all["checks"].as_array().unwrap().len(), CEPH_CHECKS.len());
        assert_eq!(all["checks"][0]["triage"], "data_risk");
        let r = explain_ceph_checks(&[
            "osd_down".into(),
            "OSD_DOWN".into(),
            "NOT_A_CHECK".into(),
            " pg_damaged ".into(),
        ])
        .unwrap();
        let c = r["checks"].as_array().unwrap();
        assert_eq!(c.len(), 2, "duplicates collapse");
        assert_eq!(c[0]["code"], "PG_DAMAGED", "data risk sorts first");
        assert_eq!(r["unknown"], json!(["NOT_A_CHECK"]));
        assert!(explain_ceph_checks(&["bad code!".into()]).is_err());
        assert!(explain_ceph_checks(&vec!["OSD_DOWN".to_string(); MAX_CODES + 1]).is_err());
    }

    #[test]
    fn body_forms() {
        let a = codes_from_body(&json!({"codes": ["A", "B"]})).unwrap();
        assert_eq!(a, vec!["A", "B"]);
        let mut b = codes_from_body(
            &json!({"checks": {"OSD_DOWN": {"severity": "HEALTH_WARN"}, "SLOW_OPS": {}}}),
        )
        .unwrap();
        b.sort();
        assert_eq!(b, vec!["OSD_DOWN", "SLOW_OPS"]);
        assert!(codes_from_body(&json!({"codes": [1]})).is_err());
        assert!(codes_from_body(&json!({})).is_err());
    }
}
