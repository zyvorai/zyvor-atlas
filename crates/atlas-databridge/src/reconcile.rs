// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! The DataBridge reconciler: a periodic worker (mirrors `atlas_jobs::spawn_scheduler`) that
//! advances long-running pipeline work the single-shot job engine can't hold open — polling edge
//! operator CR status to `ready`, and (later slices) watching full-load/validation Jobs and CDC lag.
//!
//! In `fake` mode most stages complete synchronously in the job handlers, so the reconciler is
//! mostly idle; the real (k8s) paths give it its work. Errors are logged and swallowed so a
//! transient failure just retries next tick.

use std::sync::Arc;
use std::time::Duration;

use sqlx::AnyPool;

use crate::cr::{cnpg, mysql_operator, psmdb};

/// Spawn the reconciler loop. `interval_secs == 0` disables it (the test/Default config).
pub fn spawn_reconciler(
    pool: AnyPool,
    k8s: Option<Arc<atlas_driver_k8s::K8sDriver>>,
    interval_secs: u64,
    is_leader: Arc<std::sync::atomic::AtomicBool>,
) {
    if interval_secs == 0 {
        tracing::info!("databridge reconciler disabled (interval = 0)");
        return;
    }
    tokio::spawn(async move {
        tracing::info!(interval_secs, "databridge reconciler started");
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            // HA: only the leader replica advances migration pipelines.
            if !is_leader.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if let Err(e) = reconcile_once(&pool, k8s.as_deref()).await {
                tracing::warn!("databridge reconcile tick failed: {e:#}");
            }
        }
    });
}

/// One reconcile pass. Advances `provisioning` edge clusters to `ready` by polling their operator
/// CR status. (Later slices add: full-load / validation Job watching, CDC lag, cutover draining.)
async fn reconcile_once(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
) -> anyhow::Result<()> {
    // Fake CDC: drain each streaming stream's lag toward zero so the Replication view animates and
    // cutover eventually becomes allowed. (Real lag comes from Kafka Connect / source LSN.)
    drain_fake_cdc(pool).await?;

    let Some(k8s) = k8s else { return Ok(()) }; // fake mode completes provisioning inline

    for edge in
        atlas_inventory::databridge::edge_clusters::list_by_state(pool, "provisioning").await?
    {
        let Some(cr_name) = edge.cr_name.as_deref() else {
            continue;
        };
        let (group, version, kind, ready, endpoint, secret_ref) = match edge.engine.as_str() {
            "postgres" => (
                cnpg::GROUP,
                cnpg::VERSION,
                cnpg::KIND,
                cnpg::is_ready as fn(&serde_json::Value) -> bool,
                cnpg::endpoint(cr_name, &edge.namespace),
                cnpg::secret_ref(cr_name),
            ),
            "mysql" => (
                mysql_operator::GROUP,
                mysql_operator::VERSION,
                mysql_operator::KIND,
                mysql_operator::is_ready as fn(&serde_json::Value) -> bool,
                mysql_operator::endpoint(cr_name, &edge.namespace),
                mysql_operator::secret_ref(cr_name),
            ),
            "mongodb" => (
                psmdb::GROUP,
                psmdb::VERSION,
                psmdb::KIND,
                psmdb::is_ready as fn(&serde_json::Value) -> bool,
                psmdb::endpoint(cr_name, &edge.namespace),
                psmdb::secret_ref(cr_name),
            ),
            other => {
                tracing::warn!("edge cluster {} has unknown engine {other}", edge.id);
                continue;
            }
        };

        match k8s
            .get_cr_status(group, version, kind, &edge.namespace, cr_name)
            .await
        {
            Ok(Some(status)) if ready(&status) => {
                atlas_inventory::databridge::edge_clusters::set_ready(
                    pool,
                    &edge.id,
                    &endpoint,
                    &secret_ref,
                )
                .await?;
                if let Some(plan_id) = edge.plan_id.as_deref() {
                    atlas_inventory::databridge::plans::set_state(pool, plan_id, "provisioned")
                        .await?;
                }
                tracing::info!("edge cluster {} is ready at {endpoint}", edge.id);
            }
            Ok(_) => tracing::debug!("edge cluster {} still provisioning", edge.id),
            Err(e) => tracing::warn!("poll {kind} status for {}: {e}", edge.id),
        }
    }

    // Watch full-load batch Jobs -> advance full_loading -> loaded | failed.
    for plan in atlas_inventory::databridge::plans::list_by_state(pool, "full_loading").await? {
        let job = crate::loader::job_name(&plan.id);
        match k8s
            .get_cr_status(
                crate::loader::JOB_GROUP,
                crate::loader::JOB_VERSION,
                crate::loader::JOB_KIND,
                crate::pipeline::EDGE_NAMESPACE,
                &job,
            )
            .await
        {
            Ok(Some(status)) => match crate::loader::job_outcome(&status) {
                crate::loader::JobOutcome::Succeeded => {
                    atlas_inventory::databridge::plans::set_state(pool, &plan.id, "loaded").await?;
                    if let Some(edge_id) = plan.edge_cluster_id.as_deref() {
                        let src =
                            atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
                                .await?;
                        let bytes = src
                            .and_then(|s| {
                                s.discovered
                                    .get("total_size_bytes")
                                    .and_then(|v| v.as_i64())
                            })
                            .unwrap_or(0);
                        atlas_inventory::databridge::edge_clusters::set_size_bytes(
                            pool, edge_id, bytes,
                        )
                        .await?;
                    }
                    tracing::info!("plan {} full-load complete", plan.id);
                }
                crate::loader::JobOutcome::Failed => {
                    atlas_inventory::databridge::plans::set_state(pool, &plan.id, "failed").await?;
                    tracing::warn!("plan {} full-load Job {job} failed", plan.id);
                }
                crate::loader::JobOutcome::Running => {}
            },
            Ok(None) => tracing::debug!("plan {} full-load Job {job} has no status yet", plan.id),
            Err(e) => tracing::warn!("poll full-load Job {job}: {e}"),
        }
    }

    // Real CDC tracking: a streaming stream backed by a real Debezium `KafkaConnector` (status
    // present) is driven by the connector's health — RUNNING refreshes liveness, anything else flags
    // the stream `error`. Fake streams have no CR (status None) and are left to the synthesized drain
    // above.
    //
    // With the `kafka-lag` feature, a healthy real stream also reports its precise offset-lag
    // (topic end offset − sink consumer offset) measured via an embedded Kafka client; without it,
    // a healthy stream reports caught-up (0).
    for stream in atlas_inventory::databridge::cdc::list_by_state(pool, "streaming").await? {
        let Some(connector) = stream.connector_name.as_deref() else {
            continue;
        };
        match k8s
            .get_cr_status(
                crate::cr::streaming::GROUP,
                crate::cr::streaming::VERSION,
                crate::cr::streaming::CONNECTOR_KIND,
                crate::pipeline::EDGE_NAMESPACE,
                connector,
            )
            .await
        {
            Ok(Some(status)) => {
                if crate::cr::streaming::connector_running(&status) {
                    // Healthy real stream: measure sink lag (0 unless the kafka-lag feature is on).
                    let short = stream
                        .plan_id
                        .as_deref()
                        .map(|p| p[p.len().saturating_sub(8)..].to_string())
                        .unwrap_or_default();
                    let group = crate::kafka_lag::sink_consumer_group(
                        &crate::cr::streaming::sink_connector_name(&short),
                    );
                    let lag_msgs = crate::kafka_lag::measure(
                        "zyvor-kafka-kafka-bootstrap:9092".to_string(),
                        group,
                        crate::cr::streaming::topic_prefix(&short),
                    )
                    .await;
                    atlas_inventory::databridge::cdc::update_lag(
                        pool,
                        &stream.id,
                        lag_msgs,
                        if lag_msgs > 0 { 1 } else { 0 },
                        stream.last_source_lsn.as_deref(),
                        stream.last_applied_lsn.as_deref(),
                        stream.events_total,
                    )
                    .await?;
                } else {
                    // Self-heal: auto-restart a stalled stream a bounded number of times before
                    // giving up (→ `error`, which the monitor's CDC rule then alerts on).
                    const MAX_CDC_RESTARTS: i64 = 3;
                    if stream.restart_count < MAX_CDC_RESTARTS {
                        match stream.plan_id.as_deref() {
                            Some(plan_id) => {
                                match crate::pipeline::restart_cdc(pool, Some(k8s), plan_id).await {
                                    Ok(_) => tracing::info!(
                                    "auto-restarted CDC stream {} (attempt {}/{MAX_CDC_RESTARTS})",
                                    stream.id,
                                    stream.restart_count + 1
                                ),
                                    Err(e) => tracing::warn!(
                                        "CDC auto-restart for {} failed: {e:#}",
                                        stream.id
                                    ),
                                }
                            }
                            None => {
                                atlas_inventory::databridge::cdc::set_state(
                                    pool, &stream.id, "error",
                                )
                                .await?;
                            }
                        }
                    } else {
                        atlas_inventory::databridge::cdc::set_state(pool, &stream.id, "error")
                            .await?;
                        tracing::warn!(
                            "CDC stream {} connector {connector} not RUNNING; giving up after {} restarts",
                            stream.id,
                            stream.restart_count
                        );
                    }
                }
            }
            Ok(None) => {} // fake stream — handled by the synthesized drain
            Err(e) => tracing::warn!("poll CDC connector {connector}: {e}"),
        }
    }

    // Advance draining cutovers: once the plan's CDC stream reports zero lag, tear the streaming
    // stack down (source + sink connectors + the per-plan KafkaConnect cluster), switch, and
    // complete. Past the drain deadline with lag still outstanding, fail the cutover and return the
    // plan to `validated` so the operator can retry once the source quiesces.
    for cut in atlas_inventory::databridge::cutovers::list_by_state(pool, "draining").await? {
        let Some(plan) = atlas_inventory::databridge::plans::get_plan(pool, &cut.plan_id).await?
        else {
            continue;
        };
        let stream = match plan.cdc_stream_id.as_deref() {
            Some(cdc_id) => atlas_inventory::databridge::cdc::get_stream(pool, cdc_id).await?,
            None => None,
        };
        let drained = stream
            .as_ref()
            .map(|st| st.lag_seconds <= 0 && st.lag_bytes <= 0)
            .unwrap_or(true); // no stream to drain
        if drained {
            let s = crate::pipeline::plan_short(&plan.id);
            if let Err(e) = crate::pipeline::teardown_streaming(k8s, s, true).await {
                tracing::warn!(
                    "cutover {} streaming teardown failed (retrying next tick): {e:#}",
                    cut.id
                );
                continue;
            }
            atlas_inventory::databridge::cutovers::set_state(pool, &cut.id, "switching").await?;
            atlas_inventory::databridge::cutovers::set_complete(pool, &cut.id, "complete").await?;
            atlas_inventory::databridge::plans::set_state(pool, &plan.id, "cutover_complete")
                .await?;
            atlas_inventory::databridge::plans::set_cutover_at(
                pool,
                &plan.id,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await?;
            if let Some(cdc_id) = plan.cdc_stream_id.as_deref() {
                atlas_inventory::databridge::cdc::set_state(pool, cdc_id, "stopped").await?;
            }
            tracing::info!("cutover {} complete for plan {}", cut.id, plan.id);
        } else if cut
            .drain_deadline
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .is_some_and(|dl| chrono::Utc::now() > dl)
        {
            atlas_inventory::databridge::cutovers::set_complete(pool, &cut.id, "failed").await?;
            atlas_inventory::databridge::plans::set_state(pool, &plan.id, "validated").await?;
            tracing::warn!(
                "cutover {} for plan {} failed: CDC lag did not drain before the deadline",
                cut.id,
                plan.id
            );
        }
    }

    // Watch validation Jobs -> record passed/failed and advance the plan.
    for v in atlas_inventory::databridge::validations::list_by_state(pool, "running").await? {
        let job = crate::validate::job_name(&v.id);
        match k8s
            .get_cr_status(
                crate::loader::JOB_GROUP,
                crate::loader::JOB_VERSION,
                crate::loader::JOB_KIND,
                crate::pipeline::EDGE_NAMESPACE,
                &job,
            )
            .await
        {
            Ok(Some(status)) => match crate::loader::job_outcome(&status) {
                crate::loader::JobOutcome::Succeeded => {
                    let summary =
                        serde_json::json!({ "note": "row counts matched (see Job logs)" });
                    atlas_inventory::databridge::validations::set_result(
                        pool, &v.id, true, 0, 0, &summary,
                    )
                    .await?;
                    atlas_inventory::databridge::plans::set_state(pool, &v.plan_id, "validated")
                        .await?;
                    tracing::info!("validation {} passed", v.id);
                }
                crate::loader::JobOutcome::Failed => {
                    let summary =
                        serde_json::json!({ "note": "row-count mismatch (see Job logs)" });
                    atlas_inventory::databridge::validations::set_result(
                        pool, &v.id, false, 0, 1, &summary,
                    )
                    .await?;
                    atlas_inventory::databridge::plans::set_state(pool, &v.plan_id, "failed")
                        .await?;
                    tracing::warn!("validation {} failed", v.id);
                }
                crate::loader::JobOutcome::Running => {}
            },
            Ok(None) => tracing::debug!("validation {} Job {job} has no status yet", v.id),
            Err(e) => tracing::warn!("poll validation Job {job}: {e}"),
        }
    }
    Ok(())
}

/// Drain fake CDC streams toward zero lag each tick (demoable replication without a real Kafka).
async fn drain_fake_cdc(pool: &AnyPool) -> anyhow::Result<()> {
    for stream in atlas_inventory::databridge::cdc::list_by_state(pool, "streaming").await? {
        if stream.lag_seconds <= 0 && stream.lag_bytes <= 0 {
            continue;
        }
        // ~40% of remaining backlog per tick, plus a few thousand more events applied.
        let lag_bytes = (stream.lag_bytes * 6 / 10).max(0);
        let lag_seconds = (stream.lag_seconds * 6 / 10).max(0);
        let events = stream.events_total + 5000;
        atlas_inventory::databridge::cdc::update_lag(
            pool,
            &stream.id,
            lag_bytes,
            lag_seconds,
            stream.last_source_lsn.as_deref(),
            stream.last_source_lsn.as_deref(), // caught up to source
            events,
        )
        .await?;
    }
    Ok(())
}
