// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Scrape the Ceph mgr Prometheus module (PDF §15.1, §10.3 "Atlas → Prometheus").
//!
//! We keep a curated whitelist of metrics (capacity, OSD latency/up/in, pool usage, health) — not
//! the full firehose — and store the latest value per (name, labels) for Atlas's summary views and
//! the high-latency alert rule.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sqlx::AnyPool;

/// Metric names we keep from the Ceph exporter.
const WHITELIST: &[&str] = &[
    "ceph_cluster_total_bytes",
    "ceph_cluster_total_used_bytes",
    "ceph_cluster_total_used_raw_bytes",
    "ceph_health_status",
    "ceph_osd_up",
    "ceph_osd_in",
    "ceph_osd_apply_latency_ms",
    "ceph_osd_commit_latency_ms",
    "ceph_pool_bytes_used",
    "ceph_pool_max_avail",
    "ceph_pg_total",
    "ceph_pg_active",
    // Client I/O (per-pool cumulative counters — rate them for IOPS/throughput).
    "ceph_pool_rd",
    "ceph_pool_rd_bytes",
    "ceph_pool_wr",
    "ceph_pool_wr_bytes",
    // Recovery / backfill health (PG states + degraded/misplaced/unfound object counts).
    "ceph_pg_recovering",
    "ceph_pg_backfilling",
    "ceph_pg_backfill_wait",
    "ceph_pg_recovery_wait",
    "ceph_pg_degraded",
    "ceph_pg_undersized",
    "ceph_num_objects_degraded",
    "ceph_num_objects_misplaced",
    "ceph_num_objects_unfound",
];

/// OSD apply-latency thresholds for the high-latency alert (ms).
const LATENCY_WARN_MS: f64 = 100.0;
const LATENCY_CRITICAL_MS: f64 = 1000.0;

/// Scrape the given `/metrics` URL, upsert whitelisted samples, and evaluate the latency alert.
/// Returns the number of samples stored.
pub async fn scrape(pool: &AnyPool, url: &str) -> Result<usize> {
    let body = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()
        .await?;

    let lines = body.lines().map(|l| Ok(l.to_string()));
    let scrape = prometheus_parse::Scrape::parse(lines).context("parse prometheus text")?;

    let mut stored = 0usize;
    for sample in &scrape.samples {
        if !WHITELIST.contains(&sample.metric.as_str()) {
            continue;
        }
        let value = match sample.value {
            prometheus_parse::Value::Counter(v)
            | prometheus_parse::Value::Gauge(v)
            | prometheus_parse::Value::Untyped(v) => v,
            _ => continue,
        };
        let labels: BTreeMap<String, String> = sample
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        atlas_inventory::metrics::upsert(pool, &sample.metric, &labels, value).await?;
        stored += 1;
    }

    evaluate_latency_alert(pool).await?;
    evaluate_recovery_alert(pool).await?;
    Ok(stored)
}

/// Raise/clear a cluster-level high-latency alert from the worst OSD apply latency.
async fn evaluate_latency_alert(pool: &AnyPool) -> Result<()> {
    let id = "alert_osd_latency_high";
    let worst = atlas_inventory::metrics::max_value(pool, "ceph_osd_apply_latency_ms").await?;
    match worst {
        Some(ms) if ms >= LATENCY_WARN_MS => {
            let severity = if ms >= LATENCY_CRITICAL_MS {
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
                "ceph",
                "High OSD latency",
                &format!("Worst OSD apply latency is {ms:.0} ms"),
                &serde_json::json!({ "apply_latency_ms": ms }),
            )
            .await?;
        }
        _ => atlas_inventory::alerts::resolve(pool, id).await?,
    }
    Ok(())
}

/// Raise a recovery/backfill alert while PGs are recovering (warning), escalating to critical when
/// data is at risk (unfound objects). Clears when recovery finishes.
async fn evaluate_recovery_alert(pool: &AnyPool) -> Result<()> {
    let id = "alert_recovery_in_progress";
    let recovering = atlas_inventory::metrics::sum_value(pool, "ceph_pg_recovering").await?;
    let backfilling = atlas_inventory::metrics::sum_value(pool, "ceph_pg_backfilling").await?;
    let backfill_wait = atlas_inventory::metrics::sum_value(pool, "ceph_pg_backfill_wait").await?;
    let recovery_wait = atlas_inventory::metrics::sum_value(pool, "ceph_pg_recovery_wait").await?;
    let unfound = atlas_inventory::metrics::sum_value(pool, "ceph_num_objects_unfound").await?;
    let degraded = atlas_inventory::metrics::sum_value(pool, "ceph_num_objects_degraded").await?;
    let misplaced = atlas_inventory::metrics::sum_value(pool, "ceph_num_objects_misplaced").await?;
    let active_pgs = recovering + backfilling + backfill_wait + recovery_wait;

    if active_pgs > 0.0 || unfound > 0.0 {
        let severity = if unfound > 0.0 { "critical" } else { "warning" };
        atlas_inventory::alerts::upsert_open(
            pool,
            id,
            severity,
            "monitor",
            "cluster",
            "ceph",
            "Recovery/backfill in progress",
            &format!(
                "{recovering:.0} recovering + {backfilling:.0} backfilling PG(s); \
                 {degraded:.0} degraded / {misplaced:.0} misplaced / {unfound:.0} unfound objects"
            ),
            &serde_json::json!({
                "pg_recovering": recovering, "pg_backfilling": backfilling,
                "pg_backfill_wait": backfill_wait, "pg_recovery_wait": recovery_wait,
                "objects_degraded": degraded, "objects_misplaced": misplaced,
                "objects_unfound": unfound
            }),
        )
        .await?;
    } else {
        atlas_inventory::alerts::resolve(pool, id).await?;
    }
    Ok(())
}
