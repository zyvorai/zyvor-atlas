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

fn start_heartbeat(pool: AnyPool, job_id: String, period: Duration) -> Heartbeat {
    Heartbeat(tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(period);
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
    let _beat = start_heartbeat(pool.clone(), job_id.to_string(), HEARTBEAT);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobEngine, JobEngineOptions};
    use async_trait::async_trait;
    use atlas_api_types::{
        DedupEstimate, DiscoveryResult, MetricSample, StorageHealth, StoragePool, StorageVolume,
    };
    use atlas_driver_core::{DriverError, StorageDriver};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    async fn migrated_pool() -> AnyPool {
        let db = format!(
            "{}/atlas-dedup-job-test-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = atlas_inventory::connect(&url).await.unwrap();
        atlas_inventory::migrate(&pool, &url).await.unwrap();
        pool
    }

    /// A driver whose estimate never finishes on its own, like `zdb -S` on a huge pool. It records
    /// that the scan started and, via a drop guard, that it was torn down.
    struct SlowDriver {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl StorageDriver for SlowDriver {
        fn backend_id(&self) -> &str {
            "bkd_slow"
        }
        async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
            Err(DriverError::NotImplemented("test"))
        }
        async fn health(&self) -> Result<StorageHealth, DriverError> {
            Err(DriverError::NotImplemented("test"))
        }
        async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
            Err(DriverError::NotImplemented("test"))
        }
        async fn list_volumes(&self, _pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
            Err(DriverError::NotImplemented("test"))
        }
        async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
            Err(DriverError::NotImplemented("test"))
        }
        async fn estimate_dedup(
            &self,
            _pool: &str,
            _opts: DedupEstimateOptions,
        ) -> Result<DedupEstimate, DriverError> {
            let _guard = DropFlag(self.dropped.clone());
            self.started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(600)).await;
            Err(DriverError::Backend("never reached".into()))
        }
    }

    async fn state_of(pool: &AnyPool, id: &str) -> String {
        atlas_inventory::jobs::get_job(pool, id).await.unwrap().unwrap().state
    }

    async fn wait_until<F: Fn() -> bool>(what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn the_heartbeat_touches_a_running_job_and_stops_with_the_dispatch() {
        let pool = migrated_pool().await;
        atlas_inventory::jobs::insert_job(&pool, "j_beat", "t", "dedup.estimate", "me", &serde_json::json!({}), None)
            .await
            .unwrap();
        atlas_inventory::jobs::set_state(&pool, "j_beat", "running", 0).await.unwrap();

        let beat = start_heartbeat(pool.clone(), "j_beat".into(), Duration::from_millis(40));
        let mut seen = None;
        for _ in 0..100 {
            let job = atlas_inventory::jobs::get_job(&pool, "j_beat").await.unwrap().unwrap();
            if job.result["phase"] == "scanning" {
                seen = Some(job.result);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let seen = seen.expect("the heartbeat never wrote the scanning note");
        assert!(seen["elapsed_secs"].is_u64(), "{seen}");

        // Dropping the guard (cancel, timeout, finish) ends it: the row stops moving.
        drop(beat);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let a = atlas_inventory::jobs::get_job(&pool, "j_beat").await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let b = atlas_inventory::jobs::get_job(&pool, "j_beat").await.unwrap().unwrap();
        assert_eq!(a.updated_at, b.updated_at, "the heartbeat kept running after it was dropped");
    }

    #[tokio::test]
    async fn a_slow_scan_does_not_hold_the_shared_worker_and_cancel_tears_it_down() {
        let pool = migrated_pool().await;
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(DriverRegistry::new());
        registry.register(Arc::new(SlowDriver { started: started.clone(), dropped: dropped.clone() }));
        let engine = JobEngine::start_with(
            pool.clone(),
            None,
            JobEngineOptions { poll_secs: 0, ..JobEngineOptions::default() },
        );
        engine.set_driver_registry(registry);

        engine
            .enqueue(
                "j_scan",
                "t",
                "me",
                JobSpec::DedupEstimate {
                    pool_id: "p".into(),
                    pool_name: "tank".into(),
                    backend_id: "bkd_slow".into(),
                    max_bytes: 1,
                    block_size: 4096,
                    max_secs: 600,
                },
                None,
            )
            .await
            .unwrap();
        wait_until("the scan to start", || started.load(Ordering::SeqCst)).await;
        assert_eq!(state_of(&pool, "j_scan").await, "running");

        // A quick job on the shared worker (it fails fast: no k8s driver, no retries) finishes
        // while the scan is still in flight.
        engine
            .enqueue(
                "j_quick",
                "t",
                "me",
                JobSpec::VolumeCreate {
                    volume_id: "v".into(),
                    backend_id: "none".into(),
                    name: "v".into(),
                    namespace: "ns".into(),
                    storage_class: "sc".into(),
                    access_mode: "ReadWriteOnce".into(),
                    volume_mode: "Filesystem".into(),
                    size_bytes: 1,
                    kind: "block".into(),
                    policy: None,
                    owner: None,
                },
                None,
            )
            .await
            .unwrap();
        for _ in 0..200 {
            if state_of(&pool, "j_quick").await == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(state_of(&pool, "j_quick").await, "failed", "the shared worker was blocked by the scan");
        assert_eq!(state_of(&pool, "j_scan").await, "running", "the scan should still be running");

        // Cancel drops the dispatch future, which is what kills zdb/rbd (`kill_on_drop`).
        assert!(engine.cancel_job("j_scan").await.unwrap());
        wait_until("the driver future to be dropped", || dropped.load(Ordering::SeqCst)).await;
        for _ in 0..100 {
            if state_of(&pool, "j_scan").await != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_ne!(state_of(&pool, "j_scan").await, "running");
    }
}
