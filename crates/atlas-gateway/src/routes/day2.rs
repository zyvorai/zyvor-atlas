// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_jobs::JobSpec;

use super::util::accepted;
use crate::auth::Actor;
use crate::state::AppState;

// ---- maintenance & cluster ops (day-2) ----

/// `POST /backends/{id}/cordon` — stop new provisioning onto a backend (existing volumes untouched).
pub(crate) async fn cordon_backend(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    set_cordon(&s, &actor, &id, true).await
}

/// `POST /backends/{id}/uncordon` — resume provisioning onto a backend.
pub(crate) async fn uncordon_backend(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    set_cordon(&s, &actor, &id, false).await
}

pub(crate) async fn set_cordon(
    s: &AppState,
    actor: &Actor,
    id: &str,
    cordoned: bool,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_ADMIN)?;
    if !atlas_inventory::set_backend_cordoned(&s.pool, id, cordoned).await? {
        return Err(AppError::NotFound(format!("backend {id}")));
    }
    let action = if cordoned {
        "backend.cordon"
    } else {
        "backend.uncordon"
    };
    let _ = atlas_inventory::audit::record(
        &s.pool, None, &actor.id, action, "backend", id, "ok", None, None,
    )
    .await;
    Ok(Json(json!({ "id": id, "cordoned": cordoned })))
}

/// `GET /maintenance` — whether the job engine is paused (maintenance mode).
pub(crate) async fn get_maintenance(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!({ "paused": s.jobs.is_paused() })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MaintenanceBody {
    paused: bool,
}

/// `POST /maintenance` `{ "paused": true|false }` — pause/resume job execution. Paused jobs stay
/// `queued` (the worker holds them) and drain when resumed.
pub(crate) async fn set_maintenance(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<MaintenanceBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    s.jobs.set_paused(body.paused);
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        if body.paused {
            "maintenance.pause"
        } else {
            "maintenance.resume"
        },
        "control_plane",
        "job_engine",
        "ok",
        None,
        None,
    )
    .await;
    Ok(Json(json!({ "paused": body.paused })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReweightQuery {
    weight: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OkToStopQuery {
    ids: Option<String>,
}

/// Most OSDs one safety check may name; a host rarely has more.
const OK_TO_STOP_MAX_IDS: usize = 64;

fn parse_osd_ids(raw: &str) -> AppResult<Vec<i64>> {
    let mut ids = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let id: i64 = part
            .parse()
            .map_err(|_| AppError::Validation(format!("'{part}' is not an OSD id")))?;
        if id < 0 {
            return Err(AppError::Validation(format!("OSD id {id} is negative")));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err(AppError::Validation(
            "ids is required: a comma-separated list of OSD ids, e.g. ids=0,1".into(),
        ));
    }
    if ids.len() > OK_TO_STOP_MAX_IDS {
        return Err(AppError::Validation(format!(
            "at most {OK_TO_STOP_MAX_IDS} OSD ids per check"
        )));
    }
    Ok(ids)
}

/// `GET /osds/ok-to-stop?ids=0,1` — would stopping these OSDs together leave every placement group
/// available (`ceph osd ok-to-stop`)? Read-only: it changes nothing. Ask this before taking a node
/// down for maintenance; `ok_to_stop: false` comes with Ceph's PG counts and reason.
pub(crate) async fn osds_ok_to_stop(
    State(s): State<AppState>,
    Query(q): Query<OkToStopQuery>,
) -> AppResult<Json<Value>> {
    let ids = parse_osd_ids(q.ids.as_deref().unwrap_or(""))?;
    let driver = s
        .driver_for(super::util::CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    let detail = driver.ceph_osd_ok_to_stop(&ids).await.map_err(|e| match e {
        atlas_driver_core::DriverError::Backend(m) if m.contains("does not exist") => {
            AppError::NotFound(m)
        }
        atlas_driver_core::DriverError::Unreachable(m) => AppError::Unavailable(m),
        other => AppError::Driver(other.to_string()),
    })?;
    Ok(Json(json!({
        "ok_to_stop": detail.get("ok_to_stop").and_then(Value::as_bool).unwrap_or(false),
        "osd_ids": ids,
        "detail": detail,
    })))
}

/// `POST /osds/{osd_id}/out` — drain an OSD (`ceph osd out`) as an async job (admin).
pub(crate) async fn osd_out(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(osd_id): Path<i64>,
) -> AppResult<(StatusCode, Json<Value>)> {
    enqueue_osd_op(&s, &actor, osd_id, "out", None).await
}

/// `POST /osds/{osd_id}/in` — return a drained OSD to service (`ceph osd in`).
pub(crate) async fn osd_in(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(osd_id): Path<i64>,
) -> AppResult<(StatusCode, Json<Value>)> {
    enqueue_osd_op(&s, &actor, osd_id, "in", None).await
}

/// `POST /osds/{osd_id}/reweight?weight=0.8` — reweight an OSD (`ceph osd reweight`).
pub(crate) async fn osd_reweight(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(osd_id): Path<i64>,
    Query(q): Query<ReweightQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let weight = q
        .weight
        .ok_or_else(|| AppError::Validation("weight query param is required (0.0–1.0)".into()))?;
    if !(0.0..=1.0).contains(&weight) {
        return Err(AppError::Validation("weight must be in [0.0, 1.0]".into()));
    }
    enqueue_osd_op(&s, &actor, osd_id, "reweight", Some(weight)).await
}

pub(crate) async fn enqueue_osd_op(
    s: &AppState,
    actor: &Actor,
    osd_id: i64,
    op: &str,
    weight: Option<f64>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_ADMIN)?;
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(
            &job_id,
            "global",
            &actor.id,
            JobSpec::CephOsdOp {
                osd_id,
                action: op.to_string(),
                weight,
            },
            None,
        )
        .await?;
    Ok(accepted(
        &job,
        json!({ "osd_id": osd_id, "op": op, "weight": weight }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct QosQuery {
    iops: Option<i64>,
    bps: Option<i64>,
}

/// `POST /rbd-images/{pool}/{image}/qos?iops=&bps=` — cap a volume's IOPS / bandwidth (async job).
/// `0` clears a cap. At least one of `iops`/`bps` is required.
pub(crate) async fn qos_rbd_image(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((pool_name, image)): Path<(String, String)>,
    Query(q): Query<QosQuery>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if q.iops.is_none() && q.bps.is_none() {
        return Err(AppError::Validation(
            "at least one of iops/bps is required".into(),
        ));
    }
    if q.iops.is_some_and(|v| v < 0) || q.bps.is_some_and(|v| v < 0) {
        return Err(AppError::Validation(
            "qos limits must be >= 0 (0 clears the cap)".into(),
        ));
    }
    let native = format!("rbd:{pool_name}/{image}");
    let volume_id = atlas_inventory::list_volumes(&s.pool)
        .await?
        .into_iter()
        .find(|v| v.backend_native_id.as_deref() == Some(native.as_str()))
        .map(|v| v.id)
        .unwrap_or_default();
    let job_id = ids::job_id();
    let spec = JobSpec::RbdQos {
        volume_id,
        pool: pool_name.clone(),
        image: image.clone(),
        iops_limit: q.iops,
        bps_limit: q.bps,
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await?;
    Ok(accepted(
        &job,
        json!({ "rbd": format!("{pool_name}/{image}"), "iops_limit": q.iops, "bps_limit": q.bps }),
    ))
}

/// `GET /maintenance/orphans` — day-2 hygiene: backups whose source volume no longer exists (their
/// `volume_id` has no FK, so a volume delete leaves them dangling). Clean each via `DELETE /backups/{id}`.
pub(crate) async fn list_orphans(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let orphan_backups = atlas_inventory::backups::list_orphans(&s.pool).await?;
    Ok(Json(
        json!({ "orphan_backups": orphan_backups, "count": orphan_backups.len() }),
    ))
}

/// `GET /upgrade/preflight` — health-gated "is it safe to upgrade / take the control plane down?"
/// Aggregates cluster health, open critical alerts, in-flight jobs, and CDC lag into a ready verdict
/// so an upgrade isn't rolled during an incident. Report-only (200 always); `ready` is the gate.
pub(crate) async fn upgrade_preflight(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let mut checks: Vec<Value> = Vec::new();
    let mut blockers: Vec<String> = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String, blocker: Option<String>| {
        checks.push(json!({ "check": name, "ok": ok, "detail": detail }));
        if let (false, Some(b)) = (ok, blocker) {
            blockers.push(b);
        }
    };

    // No cluster in HEALTH_ERR.
    let critical_clusters: Vec<String> = atlas_inventory::list_clusters(&s.pool)
        .await?
        .into_iter()
        .filter(|c| matches!(c.health, atlas_api_types::Health::Critical))
        .map(|c| c.id)
        .collect();
    let ok = critical_clusters.is_empty();
    add(
        "cluster_health",
        ok,
        if ok {
            "no cluster in HEALTH_ERR".into()
        } else {
            format!("critical: {critical_clusters:?}")
        },
        Some(format!("cluster(s) unhealthy: {critical_clusters:?}")),
    );

    // No open critical alerts.
    let crit = atlas_inventory::alerts::list(&s.pool, Some("open"))
        .await?
        .into_iter()
        .filter(|a| a.severity == "critical")
        .count();
    add(
        "critical_alerts",
        crit == 0,
        format!("{crit} open critical alert(s)"),
        Some(format!(
            "{crit} open critical alert(s) — resolve or silence first"
        )),
    );

    // No in-flight jobs (anything not yet terminal — pause + drain via /maintenance first).
    let active: i64 = atlas_inventory::jobs::count_by_state(&s.pool)
        .await?
        .into_iter()
        .filter(|(st, _)| !matches!(st.as_str(), "succeeded" | "failed"))
        .map(|(_, n)| n)
        .sum();
    add(
        "active_jobs",
        active == 0,
        format!("{active} active job(s)"),
        Some(format!(
            "{active} job(s) in flight — pause + drain via /maintenance first"
        )),
    );

    // No CDC stream lagging (an upgrade window shouldn't lose replication progress).
    const CDC_LAG_THRESHOLD_SECS: i64 = 60;
    let lagging = atlas_inventory::databridge::cdc::list_by_state(&s.pool, "streaming")
        .await?
        .into_iter()
        .filter(|st| st.lag_seconds > CDC_LAG_THRESHOLD_SECS)
        .count();
    add(
        "cdc_lag",
        lagging == 0,
        format!("{lagging} CDC stream(s) lagging > {CDC_LAG_THRESHOLD_SECS}s"),
        Some(format!(
            "{lagging} CDC stream(s) lagging — let them catch up first"
        )),
    );

    let ready = blockers.is_empty();
    Ok(Json(
        json!({ "ready": ready, "checks": checks, "blockers": blockers }),
    ))
}

#[cfg(test)]
mod ok_to_stop_tests {
    use super::*;

    #[test]
    fn parses_a_comma_separated_list_without_duplicates() {
        assert_eq!(parse_osd_ids("0, 1,1,7").unwrap(), vec![0, 1, 7]);
        assert_eq!(parse_osd_ids("3").unwrap(), vec![3]);
    }

    #[test]
    fn rejects_empty_non_numeric_negative_and_oversized_lists() {
        for bad in ["", " , ", "a", "1,x", "-1", "1.5"] {
            assert!(parse_osd_ids(bad).is_err(), "{bad:?}");
        }
        let many = (0..=OK_TO_STOP_MAX_IDS).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert!(parse_osd_ids(&many).is_err());
    }
}
