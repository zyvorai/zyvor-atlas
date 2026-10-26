// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Run a dedup-savings estimate (`docs/DEDUP_ESTIMATE.md`) as a background job. The scan is one
//! driver call that can run for hours (`zdb -S` on a big pool), so a heartbeat keeps the job from
//! looking stale to the durable poller while it runs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use atlas_api_types::DedupEstimateOptions;
use atlas_driver_core::DriverRegistry;
use sqlx::AnyPool;

use crate::spec::JobSpec;

/// How often a running scan touches its row. The poller reclaims a `running` job whose row has not
/// moved for `ATLAS_JOB_STALE_SECS` (default 900), so this must stay well under that.
const HEARTBEAT: Duration = Duration::from_secs(60);

/// Aborts the heartbeat task when the dispatch future ends or is dropped (cancel, timeout).
struct Heartbeat(tokio::task::JoinHandle<()>);

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn start_heartbeat(pool: AnyPool, job_id: String) -> Heartbeat {
    Heartbeat(tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(HEARTBEAT);
        tick.tick().await; // the first tick is immediate
        loop {
            tick.tick().await;
            let note = serde_json::json!({ "phase": "scanning", "elapsed_secs": started.elapsed().as_secs() });
            if let Err(e) =
                atlas_inventory::jobs::set_progress_result(&pool, &job_id, 0, &note).await
            {
                tracing::warn!(job = %job_id, "dedup estimate heartbeat failed: {e}");
            }
        }
    }))
}

pub(crate) async fn dispatch_dedup(
    pool: &AnyPool,
    drivers: &Option<Arc<DriverRegistry>>,
    job_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    let JobSpec::DedupEstimate {
        pool_id,
        pool_name,
        backend_id,
        max_bytes,
        block_size,
        max_secs,
    } = spec
    else {
        bail!("not a dedup.estimate job");
    };
    let registry = drivers
        .as_ref()
        .ok_or_else(|| anyhow!("the driver registry is not ready"))?;
    let driver = registry
        .get(&backend_id)
        .ok_or_else(|| anyhow!("no driver for backend {backend_id}"))?;
    let _beat = start_heartbeat(pool.clone(), job_id.to_string());
    let est = driver
        .estimate_dedup(
            &pool_name,
            DedupEstimateOptions {
                max_bytes,
                block_size,
                max_secs,
            },
        )
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let mut out = serde_json::to_value(est)?;
    out["pool_id"] = serde_json::Value::String(pool_id);
    Ok(out)
}
