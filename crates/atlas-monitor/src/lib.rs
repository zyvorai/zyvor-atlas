// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Monitor worker (PDF §15): periodically refresh inventory and evaluate alert rules
//! (cluster unhealthy, pool near-full, OSD down) into `storage_alerts`.
//!
//! Rules are deterministic and idempotent — each (rule, resource) maps to a stable alert id, so a
//! recurring condition updates one row instead of piling up duplicates, and a cleared condition
//! resolves it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use atlas_api_types::Health;
use atlas_driver_core::StorageDriver;
use serde_json::json;
use sqlx::{AnyPool, Row};

pub mod audit_export;
pub mod notify;
pub mod prometheus;

pub use notify::{OpsgenieConfig, PagerDutyConfig};

/// Pool utilization thresholds (PDF §15.2).
const POOL_WARN: f64 = 0.75;
const POOL_CRITICAL: f64 = 0.85;

/// Spawn the monitor loop. `interval_secs == 0` disables it. `prometheus_url`, when set, is scraped
/// each tick for Ceph capacity/latency metrics. `k8s`, when attached, is used to re-attribute raw
/// RBD images to their owning PVC on every tick — without it, a volume created through Atlas after
/// startup loses its namespace/PVC/storage-class correlation (and even its intended name) on the
/// very next periodic discovery pass and can never recover it without a full gateway restart, since
/// only the one-time startup discovery previously did this lookup.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    pool: AnyPool,
    driver: Arc<dyn StorageDriver>,
    interval_secs: u64,
    prometheus_url: Option<String>,
    webhook_url: Option<String>,
    is_leader: Arc<std::sync::atomic::AtomicBool>,
    k8s: Option<Arc<atlas_driver_k8s::K8sDriver>>,
    rook_namespace: String,
    pagerduty: Option<PagerDutyConfig>,
    opsgenie: Option<OpsgenieConfig>,
    slack_webhook_url: Option<String>,
) {
    if interval_secs == 0 {
        tracing::info!("monitor disabled (interval = 0)");
        return;
    }
    tokio::spawn(async move {
        tracing::info!(interval_secs, "monitor worker started");
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            // HA: only the leader replica evaluates alerts / runs discovery, so a multi-replica
            // deployment doesn't duplicate work.
            if !is_leader.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            let rbd_owners = match &k8s {
                Some(k8s) => match k8s.rbd_image_owners().await {
                    Ok(m) => Some(
                        m.into_iter()
                            .map(|(img, o)| (img, (o.namespace, o.pvc_name, o.storage_class)))
                            .collect(),
                    ),
                    Err(e) => {
                        tracing::warn!("monitor rbd_image_owners failed: {e:#}");
                        None
                    }
                },
                None => None,
            };
            let rook_pool_kinds = match &k8s {
                Some(k8s) => match k8s.known_rook_pool_kinds(&rook_namespace).await {
                    Ok(m) => Some(
                        m.into_iter()
                            .map(|(pool, kind)| (pool, kind.as_str().to_string()))
                            .collect(),
                    ),
                    Err(e) => {
                        tracing::warn!("monitor known_rook_pool_kinds failed: {e:#}");
                        None
                    }
                },
                None => None,
            };
            if let Err(e) = atlas_discovery::run_discovery(
                &pool,
                driver.clone(),
                rbd_owners.as_ref(),
                rook_pool_kinds.as_ref(),
            )
            .await
            {
                tracing::warn!("monitor discovery failed: {e:#}");
            }
            if let Err(e) = reconcile_snapshots(&pool, &k8s).await {
                tracing::warn!("monitor snapshot reconcile failed: {e:#}");
            }
            if let Err(e) = evaluate(&pool).await {
                tracing::warn!("monitor evaluate failed: {e:#}");
            }
            if let Some(url) = &prometheus_url {
                match prometheus::scrape(&pool, url).await {
                    Ok(n) => tracing::debug!("scraped {n} ceph metrics"),
                    Err(e) => tracing::warn!("prometheus scrape failed: {e:#}"),
                }
            }
            // Push newly-fired alerts to every configured sink (after evaluate + scrape so they
            // see this tick's alerts). Evaluate runs again next tick, so a failed post retries
            // then. Sinks are independent — a deployment can run several at once.
            if let Some(url) = &webhook_url {
                match notify::dispatch(&pool, url).await {
                    Ok(n) if n > 0 => tracing::info!("pushed {n} alert notification(s)"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("alert notify failed: {e:#}"),
                }
            }
            if let Some(cfg) = &pagerduty {
                match notify::pagerduty::dispatch(&pool, cfg).await {
                    Ok((0, 0)) => {}
                    Ok((t, r)) => tracing::info!("pagerduty: {t} triggered, {r} resolved"),
                    Err(e) => tracing::warn!("pagerduty dispatch failed: {e:#}"),
                }
            }
            if let Some(cfg) = &opsgenie {
                match notify::opsgenie::dispatch(&pool, cfg).await {
                    Ok((0, 0)) => {}
                    Ok((t, r)) => tracing::info!("opsgenie: {t} triggered, {r} resolved"),
                    Err(e) => tracing::warn!("opsgenie dispatch failed: {e:#}"),
                }
            }
            if let Some(url) = &slack_webhook_url {
                match notify::slack::dispatch(&pool, url).await {
                    Ok((0, 0)) => {}
                    Ok((t, r)) => tracing::info!("slack: {t} triggered, {r} resolved"),
                    Err(e) => tracing::warn!("slack dispatch failed: {e:#}"),
                }
            }
        }
    });
}

/// Re-check snapshots stuck in `creating`: the create job's bind-poll gives up after a bounded
/// timeout (`atlas_jobs::dispatch::helpers::BIND_TIMEOUT`), but the underlying CSI VolumeSnapshot
/// can still bind afterward with nothing left to notice — the job already reported "succeeded" and
/// nothing else ever re-polls the row. Without this a fully-usable snapshot can show "creating"
/// forever.
///
/// Public so `POST /backends/{id}/discover` (`atlas-gateway::routes::backends::discover_backend`)
/// can run it too — that endpoint only ever touched clusters/pools/osds/volumes, so a stuck
/// snapshot was invisible to an operator's manual "resync" and could only self-heal on the next
/// leader-only periodic monitor tick.
pub async fn reconcile_snapshots(
    pool: &AnyPool,
    k8s: &Option<Arc<atlas_driver_k8s::K8sDriver>>,
) -> Result<()> {
    let Some(k8s) = k8s else { return Ok(()) };
    for snap in atlas_inventory::snapshots::list_by_state(pool, "creating").await? {
        let Some(vol) = atlas_inventory::get_volume(pool, &snap.volume_id).await? else {
            continue;
        };
        let Some(ns) = vol.kubernetes_namespace else {
            continue;
        };
        if matches!(
            k8s.volume_snapshot_ready(&ns, &snap.name).await,
            Ok(Some(true))
        ) {
            atlas_inventory::snapshots::set_state(pool, &snap.id, "ready").await?;
            tracing::info!("snapshot {} reconciled creating -> ready", snap.id);
        }
    }
    Ok(())
}

/// Days-until-full thresholds for the capacity-forecast rule.
const FORECAST_WARN_DAYS: f64 = 14.0;
const FORECAST_CRITICAL_DAYS: f64 = 3.0;

/// Evaluate all alert rules once against current inventory. Public for tests.
/// Tenant byte-quota usage thresholds (warn/critical), so tenants get a heads-up before the hard wall.
const QUOTA_WARN: f64 = 0.80;
const QUOTA_CRITICAL: f64 = 0.95;

pub async fn evaluate(pool: &AnyPool) -> Result<()> {
    evaluate_clusters(pool).await?;
    evaluate_pools(pool).await?;
    evaluate_osds(pool).await?;
    evaluate_capacity_forecast(pool).await?;
    // Day-2: conditions that previously failed silently.
    evaluate_failed_jobs(pool).await?;
    evaluate_cdc_streams(pool).await?;
    evaluate_tenant_quotas(pool).await?;
    Ok(())
}

/// Raise a rollup alert when jobs have failed in the recent window (they otherwise fail silently —
/// the job engine has no alerting). Clears once the window passes with no new failures.
async fn evaluate_failed_jobs(pool: &AnyPool) -> Result<()> {
    let id = "alert_jobs_failing";
    let cutoff = atlas_inventory::now_rfc3339(chrono::Utc::now() - chrono::Duration::minutes(15));
    let n: i64 = sqlx::query(
        "SELECT COUNT(*) AS n FROM storage_jobs WHERE state='failed' AND completed_at >= $1",
    )
    .bind(cutoff)
    .fetch_one(pool)
    .await?
    .get("n");
    if n > 0 {
        atlas_inventory::alerts::upsert_open(
            pool,
            id,
            "warning",
            "monitor",
            "jobs",
            "recent",
            "Jobs failing",
            &format!("{n} job(s) failed in the last 15 minutes"),
            &json!({ "failed_15m": n }),
        )
        .await?;
    } else {
        atlas_inventory::alerts::resolve(pool, id).await?;
    }
    Ok(())
}

/// Alert on CDC streams whose connector is in `error` (replication stalled/broken). The reconciler
/// flags the state; without this rule nobody is notified that a live migration has stopped.
async fn evaluate_cdc_streams(pool: &AnyPool) -> Result<()> {
    let rows = sqlx::query("SELECT id, state FROM cdc_streams")
        .fetch_all(pool)
        .await?;
    for r in rows {
        let sid: String = r.get("id");
        let state: String = r.get("state");
        let id = format!("alert_cdc_error_{sid}");
        if state == "error" {
            atlas_inventory::alerts::upsert_open(
                pool,
                &id,
                "critical",
                "monitor",
                "cdc_stream",
                &sid,
                "CDC replication error",
                &format!("CDC stream {sid} connector is not healthy — replication has stalled"),
                &json!({ "state": state }),
            )
            .await?;
        } else {
            atlas_inventory::alerts::resolve(pool, &id).await?;
        }
    }
    Ok(())
}

/// Warn a tenant approaching its byte quota (before `check_admission` hard-rejects at the wall).
async fn evaluate_tenant_quotas(pool: &AnyPool) -> Result<()> {
    for q in atlas_inventory::tenants::list_overview(pool).await? {
        let id = format!("alert_tenant_quota_{}", q.tenant_id);
        let ratio = if q.max_bytes > 0 {
            q.used_bytes as f64 / q.max_bytes as f64
        } else {
            0.0
        };
        if q.max_bytes > 0 && ratio >= QUOTA_WARN {
            let severity = if ratio >= QUOTA_CRITICAL {
                "critical"
            } else {
                "warning"
            };
            atlas_inventory::alerts::upsert_open(
                pool,
                &id,
                severity,
                "monitor",
                "tenant",
                &q.tenant_id,
                "Tenant quota nearly exhausted",
                &format!(
                    "Tenant {} is at {:.0}% of its {}-byte volume quota",
                    q.tenant_id,
                    ratio * 100.0,
                    q.max_bytes
                ),
                &json!({ "used_bytes": q.used_bytes, "max_bytes": q.max_bytes, "ratio": ratio }),
            )
            .await?;
        } else {
            atlas_inventory::alerts::resolve(pool, &id).await?;
        }
    }
    Ok(())
}

/// Raise an alert when the least-squares fill projection (over the last 6h) crosses a threshold.
async fn evaluate_capacity_forecast(pool: &AnyPool) -> Result<()> {
    let id = "alert_capacity_forecast";
    let f = atlas_inventory::metrics::forecast(pool, 360).await?;
    let days = f.get("days_to_full").and_then(|v| v.as_f64());
    match days {
        Some(d) if d <= FORECAST_WARN_DAYS => {
            let severity = if d <= FORECAST_CRITICAL_DAYS {
                "critical"
            } else {
                "warning"
            };
            atlas_inventory::alerts::upsert_open(
                pool,
                id,
                severity,
                "monitor",
                "cluster",
                "capacity",
                "Capacity filling up",
                &format!("Projected full in ~{d:.1} days at the current growth rate"),
                &f,
            )
            .await?;
        }
        // No projection, or plenty of runway → clear any existing alert.
        _ => atlas_inventory::alerts::resolve(pool, id).await?,
    }
    Ok(())
}

async fn evaluate_clusters(pool: &AnyPool) -> Result<()> {
    for c in atlas_inventory::list_clusters(pool).await? {
        let id = format!("alert_cluster_unhealthy_{}", c.id);
        match c.health {
            Health::Warn => {
                atlas_inventory::alerts::upsert_open(
                    pool,
                    &id,
                    "warning",
                    "monitor",
                    "cluster",
                    &c.id,
                    "Cluster health degraded",
                    &format!("Cluster {} is HEALTH_WARN", c.name),
                    &json!({ "health": "warn" }),
                )
                .await?;
            }
            Health::Critical => {
                atlas_inventory::alerts::upsert_open(
                    pool,
                    &id,
                    "critical",
                    "monitor",
                    "cluster",
                    &c.id,
                    "Cluster unhealthy",
                    &format!("Cluster {} is HEALTH_ERR", c.name),
                    &json!({ "health": "critical" }),
                )
                .await?;
            }
            // Ok / Unknown → clear any existing alert.
            _ => atlas_inventory::alerts::resolve(pool, &id).await?,
        }
    }
    Ok(())
}

async fn evaluate_pools(pool: &AnyPool) -> Result<()> {
    for p in atlas_inventory::list_pools(pool).await? {
        let id = format!("alert_pool_near_full_{}", p.id);
        match (p.used_bytes, p.max_bytes) {
            (Some(used), Some(max)) if max > 0 => {
                let ratio = used as f64 / max as f64;
                if ratio >= POOL_WARN {
                    let severity = if ratio >= POOL_CRITICAL {
                        "critical"
                    } else {
                        "warning"
                    };
                    atlas_inventory::alerts::upsert_open(
                        pool,
                        &id,
                        severity,
                        "monitor",
                        "pool",
                        &p.id,
                        "Pool near full",
                        &format!("Pool {} is {:.0}% full", p.name, ratio * 100.0),
                        &json!({ "used_bytes": used, "max_bytes": max, "ratio": ratio }),
                    )
                    .await?;
                } else {
                    atlas_inventory::alerts::resolve(pool, &id).await?;
                }
            }
            _ => atlas_inventory::alerts::resolve(pool, &id).await?,
        }
    }
    Ok(())
}

async fn evaluate_osds(pool: &AnyPool) -> Result<()> {
    for o in atlas_inventory::list_osds(pool).await? {
        let id = format!("alert_osd_down_{}_{}", o.cluster_id, o.id);
        if !o.up || !o.in_cluster {
            atlas_inventory::alerts::upsert_open(
                pool,
                &id,
                "critical",
                "monitor",
                "osd",
                &format!("osd.{}", o.id),
                "OSD down",
                &format!("osd.{} is {}", o.id, if !o.up { "down" } else { "out" }),
                &json!({ "up": o.up, "in": o.in_cluster }),
            )
            .await?;
        } else {
            atlas_inventory::alerts::resolve(pool, &id).await?;
        }
    }
    Ok(())
}
