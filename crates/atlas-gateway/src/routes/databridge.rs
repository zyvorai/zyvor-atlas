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

// ---- DataBridge: sources (cloud-to-edge DB migration) ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreateSourceBody {
    name: String,
    /// postgres | mysql | mariadb | oracle | sqlserver | mongodb
    kind: String,
    /// rds | aurora | cloudsql | generic
    #[serde(default)]
    cloud: Option<String>,
    endpoint: Option<String>,
    port: Option<i64>,
    database: Option<String>,
    /// k8s Secret with the source credentials (used only in real driver mode).
    secret_ref: Option<String>,
    secret_namespace: Option<String>,
    tls_mode: Option<String>,
    /// fake (default) | real
    driver_mode: Option<String>,
}

pub(crate) async fn db_list_sources(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::sources::list_sources(&s.pool).await?
    )))
}

pub(crate) async fn db_get_source(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let src = atlas_inventory::databridge::sources::get_source(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("source {id}")))?;
    Ok(Json(json!(src)))
}

/// `POST /databridge/sources` — register a cloud/source database (synchronous; no job).
pub(crate) async fn db_create_source(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateSourceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    if atlas_databridge::SourceKind::parse(&body.kind).is_none() {
        return Err(AppError::Validation(
            "kind must be one of: postgres, mysql, mariadb, oracle, sqlserver, mongodb".into(),
        ));
    }
    let source_id = ids::source_id();
    let cloud = body.cloud.as_deref().unwrap_or("generic");
    let tls_mode = body.tls_mode.as_deref().unwrap_or("require");
    let driver_mode = body.driver_mode.as_deref().unwrap_or("fake");
    atlas_inventory::databridge::sources::insert_source(
        &s.pool,
        &source_id,
        "global",
        &body.name,
        &body.kind,
        cloud,
        body.endpoint.as_deref(),
        body.port,
        body.database.as_deref(),
        body.secret_ref.as_deref(),
        body.secret_namespace.as_deref(),
        tls_mode,
        driver_mode,
    )
    .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "databridge.source.create",
        "migration.source",
        &source_id,
        "success",
        Some(json!({ "name": body.name, "kind": body.kind, "cloud": cloud })),
        None,
    )
    .await;
    let src = atlas_inventory::databridge::sources::get_source(&s.pool, &source_id)
        .await?
        .ok_or_else(|| AppError::Internal("source vanished after insert".into()))?;
    Ok((StatusCode::CREATED, Json(json!(src))))
}

pub(crate) async fn db_delete_source(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    // `migration_plans.source_id` is `ON DELETE CASCADE` — an unguarded delete here would silently
    // destroy a plan's entire migration history (including one already cut over to production) with
    // no recovery path. Block it instead; the operator must delete the referencing plan(s) first.
    let plans = atlas_inventory::databridge::plans::list_for_source(&s.pool, &id).await?;
    if !plans.is_empty() {
        let names: Vec<&str> = plans.iter().map(|p| p.name.as_str()).collect();
        return Err(AppError::Conflict(format!(
            "cannot delete source: {} migration plan(s) still reference it ({}) — delete them first",
            plans.len(),
            names.join(", ")
        )));
    }
    atlas_inventory::databridge::sources::delete_source_row(&s.pool, &id).await?;
    Ok(Json(json!({ "source_id": id, "deleted": true })))
}

/// `POST /databridge/sources/{id}/discover` — discover the source's schema as an async job.
pub(crate) async fn db_discover_source(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::sources::get_source(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("source {id}")))?;
    let job_id = ids::job_id();
    let spec = JobSpec::SourceDiscover {
        source_id: id.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "source_id": id })))
}

// ---- DataBridge: migration plans ----

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePlanBody {
    name: String,
    source_id: String,
    /// Rollback window after cutover, in seconds (default 72h).
    rollback_window_secs: Option<i64>,
}

pub(crate) async fn db_list_plans(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::plans::list_plans(&s.pool).await?
    )))
}

pub(crate) async fn db_get_plan(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let p = atlas_inventory::databridge::plans::get_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {id}")))?;
    Ok(Json(json!(p)))
}

/// `POST /databridge/plans` — create a migration plan for a source (synchronous).
pub(crate) async fn db_create_plan(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreatePlanBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    let source = atlas_inventory::databridge::sources::get_source(&s.pool, &body.source_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("source {}", body.source_id)))?;
    let plan_id = ids::migration_plan_id();
    let window = body.rollback_window_secs.unwrap_or(259_200);
    atlas_inventory::databridge::plans::insert_plan(
        &s.pool, &plan_id, "global", &body.name, &source.id, window,
    )
    .await?;
    // If the source is already discovered, reflect that in the plan state.
    if source.state == "discovered" {
        atlas_inventory::databridge::plans::set_state(&s.pool, &plan_id, "discovered").await?;
    }
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "databridge.plan.create",
        "migration.plan",
        &plan_id,
        "success",
        Some(json!({ "name": body.name, "source_id": source.id })),
        None,
    )
    .await;
    let p = atlas_inventory::databridge::plans::get_plan(&s.pool, &plan_id)
        .await?
        .ok_or_else(|| AppError::Internal("plan vanished after insert".into()))?;
    Ok((StatusCode::CREATED, Json(json!(p))))
}

pub(crate) async fn db_delete_plan(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::plans::delete_plan_row(&s.pool, &id).await?;
    Ok(Json(json!({ "plan_id": id, "deleted": true })))
}

/// `POST /databridge/plans/{id}/assess` — score readiness from the source's discovered schema (async).
pub(crate) async fn db_assess_plan(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let plan = atlas_inventory::databridge::plans::get_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {id}")))?;
    let source = atlas_inventory::databridge::sources::get_source(&s.pool, &plan.source_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("source {}", plan.source_id)))?;
    if source.state != "discovered" {
        return Err(AppError::Validation(format!(
            "source {} must be discovered before assessing (state: {})",
            source.id, source.state
        )));
    }
    let job_id = ids::job_id();
    let spec = JobSpec::MigrationAssess {
        plan_id: id.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "plan_id": id })))
}

/// `POST /databridge/plans/{id}/provision` — provision the edge DB cluster on Ceph (async).
pub(crate) async fn db_provision_edge(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let plan = atlas_inventory::databridge::plans::get_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {id}")))?;
    if plan.readiness_score == 0 && plan.state == "draft" {
        return Err(AppError::Validation(
            "assess the plan before provisioning".into(),
        ));
    }
    let job_id = ids::job_id();
    let spec = JobSpec::EdgeDbProvision {
        plan_id: id.clone(),
    };
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "plan_id": id })))
}

pub(crate) async fn db_list_edge_clusters(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::edge_clusters::list_edge_clusters(&s.pool).await?
    )))
}

pub(crate) async fn db_get_edge_cluster(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let c = atlas_inventory::databridge::edge_clusters::get_edge_cluster(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("edge cluster {id}")))?;
    Ok(Json(json!(c)))
}

/// `DELETE /databridge/edge-clusters/{id}` — remove a stale/orphaned edge-cluster inventory row
/// (the operator CR, if any, is torn down separately).
pub(crate) async fn db_delete_edge_cluster(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::edge_clusters::delete_edge_cluster(&s.pool, &id).await?;
    Ok(Json(json!({ "edge_cluster_id": id, "deleted": true })))
}

/// Cutover is refused unless the plan is validated + last validation passed + CDC lag is under this.
const CUTOVER_MAX_LAG_SECS: i64 = 10;

/// Enqueue a plan-scoped DataBridge stage job (operator role). Shared by the simple stage triggers.
pub(crate) async fn db_stage_job(
    s: &AppState,
    actor: &Actor,
    plan_id: &str,
    spec: JobSpec,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::plans::get_plan(&s.pool, plan_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {plan_id}")))?;
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(&job_id, "global", &actor.id, spec, None)
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "plan_id": plan_id })))
}

pub(crate) async fn db_full_load(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    db_stage_job(
        &s,
        &actor,
        &id,
        JobSpec::FullLoad {
            plan_id: id.clone(),
        },
    )
    .await
}

pub(crate) async fn db_cdc_start(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    db_stage_job(
        &s,
        &actor,
        &id,
        JobSpec::CdcStart {
            plan_id: id.clone(),
        },
    )
    .await
}

pub(crate) async fn db_cdc_stop(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    db_stage_job(
        &s,
        &actor,
        &id,
        JobSpec::CdcStop {
            plan_id: id.clone(),
        },
    )
    .await
}

/// `POST /databridge/plans/{id}/cdc/restart` — re-establish a stalled/errored CDC stream (self-heal).
pub(crate) async fn db_cdc_restart(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    db_stage_job(
        &s,
        &actor,
        &id,
        JobSpec::CdcRestart {
            plan_id: id.clone(),
        },
    )
    .await
}

pub(crate) async fn db_validate(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    db_stage_job(
        &s,
        &actor,
        &id,
        JobSpec::ValidateRun {
            plan_id: id.clone(),
            kind: "rowcount".into(),
        },
    )
    .await
}

/// `POST /databridge/plans/{id}/cutover` — guarded (admin): validated + last validation passed +
/// CDC lag under threshold.
pub(crate) async fn db_cutover(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let plan = atlas_inventory::databridge::plans::get_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {id}")))?;
    if plan.state != "validated" {
        return Err(AppError::Conflict(format!(
            "plan must be validated before cutover (state: {})",
            plan.state
        )));
    }
    let last = atlas_inventory::databridge::validations::latest_for_plan(&s.pool, &id).await?;
    if last.as_ref().map(|v| v.state.as_str()) != Some("passed") {
        return Err(AppError::Conflict(
            "latest validation must have passed before cutover".into(),
        ));
    }
    if let Some(cdc_id) = plan.cdc_stream_id.as_deref() {
        if let Some(stream) = atlas_inventory::databridge::cdc::get_stream(&s.pool, cdc_id).await? {
            if stream.lag_seconds > CUTOVER_MAX_LAG_SECS {
                return Err(AppError::Conflict(format!(
                    "CDC lag ({}s) exceeds the cutover threshold ({CUTOVER_MAX_LAG_SECS}s); wait for it to drain",
                    stream.lag_seconds
                )));
            }
        }
    }
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(
            &job_id,
            "global",
            &actor.id,
            JobSpec::Cutover {
                plan_id: id.clone(),
            },
            None,
        )
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &actor.id,
        "databridge.cutover",
        "migration.plan",
        &id,
        "accepted",
        None,
        None,
    )
    .await;
    Ok(accepted(&job, json!({ "plan_id": id })))
}

/// `GET /databridge/plans/{id}/rollback-drill` — the reverse-cutover steps [`atlas_databridge::pipeline::rollback`]
/// already runs, plus whether the recorded window is open. Does not change the plan.
pub(crate) async fn db_rollback_drill(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let drill = atlas_databridge::pipeline::rollback_drill(&s.pool, &id)
        .await
        .map_err(|e| {
            if e.to_string().contains("not found") {
                AppError::NotFound(format!("plan {id}"))
            } else {
                AppError::from(e)
            }
        })?;
    Ok(Json(drill))
}

/// `POST /databridge/plans/{id}/rollback` — guarded (admin): only within the rollback window.
pub(crate) async fn db_rollback(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    atlas_inventory::databridge::plans::get_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("plan {id}")))?;
    let cut = atlas_inventory::databridge::cutovers::latest_for_plan(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::Conflict("no cutover to roll back".into()))?;
    if let Some(deadline) = cut.rollback_deadline.as_deref() {
        if let Ok(dl) = chrono::DateTime::parse_from_rfc3339(deadline) {
            if chrono::Utc::now() > dl {
                return Err(AppError::Conflict(
                    "rollback window has closed for this cutover".into(),
                ));
            }
        }
    }
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(
            &job_id,
            "global",
            &actor.id,
            JobSpec::Rollback {
                plan_id: id.clone(),
            },
            None,
        )
        .await
        .map_err(AppError::from)?;
    Ok(accepted(&job, json!({ "plan_id": id })))
}

pub(crate) async fn db_list_cdc_streams(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::cdc::list_streams(&s.pool).await?
    )))
}

// ---- DataBridge object leg (S3-protocol object-store migration) ----

#[derive(serde::Deserialize)]
pub(crate) struct CreateObjectMigrationBody {
    name: String,
    /// aws | gcs | s3-compatible | azure-blob | vmware (S3-protocol providers copy today).
    #[serde(default)]
    source_provider: Option<String>,
    #[serde(default)]
    source_endpoint: String,
    #[serde(default)]
    source_region: Option<String>,
    source_bucket: String,
    #[serde(default)]
    source_prefix: Option<String>,
    /// k8s Secret {access_key,secret_key} for the source; never the creds themselves.
    source_secret_ref: Option<String>,
    /// Retired: auto-filling source_endpoint/source_secret_ref from a backend id only ever worked
    /// for a flat, gateway-level endpoint+credential config, which the default Ceph RGW
    /// backend has no equivalent of (each bucket gets its own endpoint/Secret from its own
    /// ObjectBucketClaim). Pass source_endpoint/source_secret_ref explicitly instead. Kept on the
    /// wire (rejected with a clear error if set) so older clients get an actionable message rather
    /// than a silently-wrong migration.
    #[serde(default)]
    source_backend_id: Option<String>,
    #[serde(default)]
    dest_provider: Option<String>,
    #[serde(default)]
    dest_endpoint: String,
    #[serde(default)]
    dest_region: Option<String>,
    dest_bucket: String,
    dest_secret_ref: Option<String>,
    /// Retired: see `source_backend_id`.
    #[serde(default)]
    dest_backend_id: Option<String>,
    #[serde(default)]
    secret_namespace: Option<String>,
    /// full | incremental (default)
    #[serde(default)]
    mode: Option<String>,
    /// objects copied at once (default from Atlas env)
    #[serde(default)]
    concurrency: Option<i64>,
    /// multipart chunk size in MiB (default from Atlas env)
    #[serde(default)]
    part_size_mb: Option<i64>,
}

/// `POST /databridge/object` — register an object-storage migration (synchronous; no copy yet).
pub(crate) async fn db_object_create(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateObjectMigrationBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if body.name.trim().is_empty() {
        return Err(AppError::Validation("name is required".into()));
    }
    // source_backend_id/dest_backend_id used to auto-fill endpoint+credentials from the gateway's
    // flat gateway-level endpoint config; there is no equivalent for Ceph RGW (each bucket has its own
    // ObjectBucketClaim-issued endpoint/Secret), so this shortcut is retired — reject it with a
    // clear message rather than silently resolving nothing.
    for (which, backend_id) in [
        ("source", &body.source_backend_id),
        ("dest", &body.dest_backend_id),
    ] {
        if backend_id.as_deref().is_some_and(|b| !b.trim().is_empty()) {
            return Err(AppError::Validation(format!(
                "{which}_backend_id is retired; pass {which}_endpoint and {which}_secret_ref explicitly"
            )));
        }
    }
    if body.source_endpoint.trim().is_empty() || body.dest_endpoint.trim().is_empty() {
        return Err(AppError::Validation(
            "source_endpoint and dest_endpoint are required".into(),
        ));
    }
    let mode = body.mode.as_deref().unwrap_or("incremental");
    if mode != "full" && mode != "incremental" {
        return Err(AppError::Validation(
            "mode must be 'full' or 'incremental'".into(),
        ));
    }
    let id = ids::object_migration_id();
    let rec = atlas_inventory::databridge::object_migrations::NewObjectMigration {
        id: id.clone(),
        tenant_id: "global".into(),
        name: body.name.clone(),
        source_provider: body
            .source_provider
            .clone()
            .unwrap_or_else(|| "s3-compatible".into()),
        source_endpoint: body.source_endpoint.clone(),
        source_region: body
            .source_region
            .clone()
            .unwrap_or_else(|| "us-east-1".into()),
        source_bucket: body.source_bucket.clone(),
        source_prefix: body.source_prefix.clone(),
        source_secret_ref: body.source_secret_ref.clone(),
        dest_provider: body
            .dest_provider
            .clone()
            .unwrap_or_else(|| "s3-compatible".into()),
        dest_endpoint: body.dest_endpoint.clone(),
        dest_region: body
            .dest_region
            .clone()
            .unwrap_or_else(|| "us-east-1".into()),
        dest_bucket: body.dest_bucket.clone(),
        dest_secret_ref: body.dest_secret_ref.clone(),
        secret_namespace: body
            .secret_namespace
            .clone()
            .unwrap_or_else(|| "zyvor-databridge".into()),
        mode: mode.into(),
        concurrency: body.concurrency,
        part_size_mb: body.part_size_mb,
    };
    atlas_inventory::databridge::object_migrations::insert(&s.pool, &rec).await?;
    let created = atlas_inventory::databridge::object_migrations::get(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("object migration {id}")))?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// `POST /databridge/object/{id}/start` — enqueue the copy job (returns 202 + job id).
pub(crate) async fn db_object_start(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::object_migrations::get(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("object migration {id}")))?;
    let job_id = ids::job_id();
    let job = s
        .jobs
        .enqueue(
            &job_id,
            "global",
            &actor.id,
            JobSpec::ObjectMigrate {
                migration_id: id.clone(),
            },
            None,
        )
        .await
        .map_err(AppError::from)?;
    atlas_inventory::databridge::object_migrations::set_job(&s.pool, &id, &job.id).await?;
    Ok(accepted(&job, json!({ "object_migration_id": id })))
}

pub(crate) async fn db_object_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let rec = atlas_inventory::databridge::object_migrations::get(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("object migration {id}")))?;
    Ok(Json(json!(rec)))
}

pub(crate) async fn db_object_list(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::object_migrations::list(&s.pool, None).await?
    )))
}

pub(crate) async fn db_object_delete(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    atlas_inventory::databridge::object_migrations::delete(&s.pool, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn db_get_cdc_stream(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let c = atlas_inventory::databridge::cdc::get_stream(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("cdc stream {id}")))?;
    Ok(Json(json!(c)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlanIdQuery {
    plan_id: Option<String>,
}

pub(crate) async fn db_list_validations(
    State(s): State<AppState>,
    Query(q): Query<PlanIdQuery>,
) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::validations::list_validations(&s.pool, q.plan_id.as_deref())
            .await?
    )))
}

pub(crate) async fn db_list_cutovers(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(json!(
        atlas_inventory::databridge::cutovers::list_cutovers(&s.pool).await?
    )))
}
