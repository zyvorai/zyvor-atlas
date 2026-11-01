// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! In-process async job engine backed by the `storage_jobs` table (PDF Rule 1: every storage
//! operation is a job; §10.5 state machine). A tokio worker consumes job ids from an unbounded
//! channel **and** a durable DB poller, executes them against the Kubernetes driver + policy, and
//! drives the job through `pending → queued → running → verifying → succeeded | failed`.
//!
//! The channel is a fast wake-up; SQLite is the source of truth. A periodic poller re-discovers due
//! `queued`/`pending` rows (honoring `next_attempt_at`) and reclaim stale `running` locks, so work
//! survives process crashes without Redis/NATS. It can be swapped for a shared Postgres queue later
//! without changing the gateway (see `docs/HA.md`).

mod dispatch;
mod engine;
mod scheduler;
mod spec;

pub use engine::{recover, recover_since, JobEngine, JobEngineOptions};
pub use scheduler::spawn_scheduler;
pub use spec::{JobSpec, OwnerRef};

#[cfg(test)]
mod durability_tests {
    use super::recover;
    use sqlx::AnyPool;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    async fn migrated_pool() -> AnyPool {
        // Temp-file (not in-memory) so the connection pool shares one schema.
        let db = format!(
            "{}/atlas-jobs-test-{}-{}.db",
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

    async fn seed(pool: &AnyPool, id: &str, state: &str) {
        atlas_inventory::jobs::insert_job(
            pool,
            id,
            "t",
            "volume.create",
            "me",
            &serde_json::json!({}),
            None,
        )
        .await
        .unwrap();
        atlas_inventory::jobs::set_state(pool, id, state, 0)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recover_fails_interrupted_running_and_requeues_pending() {
        let pool = migrated_pool().await;
        seed(&pool, "j_run", "running").await;
        seed(&pool, "j_queued", "queued").await;

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        recover(&pool, &tx).await.unwrap();

        // The interrupted running job is failed-safe.
        let run = atlas_inventory::jobs::get_job(&pool, "j_run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "failed");
        assert!(run.error.unwrap_or_default().contains("interrupted"));

        // The queued job is re-enqueued onto the channel.
        assert_eq!(rx.try_recv().unwrap(), "j_queued");
    }

    #[tokio::test]
    async fn recover_spares_a_job_claimed_after_the_engine_started() {
        let pool = migrated_pool().await;
        let started = chrono::Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        seed(&pool, "j_live", "queued").await;
        assert!(atlas_inventory::jobs::try_claim(&pool, "j_live", "me")
            .await
            .unwrap());

        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        crate::recover_since(&pool, &tx, started).await.unwrap();

        let live = atlas_inventory::jobs::get_job(&pool, "j_live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.state, "running");
    }

    #[tokio::test]
    async fn recover_skips_jobs_waiting_on_next_attempt() {
        let pool = migrated_pool().await;
        seed(&pool, "j_wait", "queued").await;
        atlas_inventory::jobs::bump_retry(&pool, "j_wait", 3600)
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        recover(&pool, &tx).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "not-yet-due retry must not wake the worker"
        );
    }

    #[tokio::test]
    async fn try_claim_is_exclusive() {
        let pool = migrated_pool().await;
        seed(&pool, "j", "queued").await;
        assert!(atlas_inventory::jobs::try_claim(&pool, "j", "w1")
            .await
            .unwrap());
        assert!(
            !atlas_inventory::jobs::try_claim(&pool, "j", "w2")
                .await
                .unwrap(),
            "second claim must lose"
        );
        let j = atlas_inventory::jobs::get_job(&pool, "j")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(j.state, "running");
    }

    #[tokio::test]
    async fn retry_budget_bumps_count_and_requeues() {
        let pool = migrated_pool().await;
        seed(&pool, "j", "running").await;
        atlas_inventory::jobs::set_max_retries(&pool, "j", 2)
            .await
            .unwrap();

        assert_eq!(
            atlas_inventory::jobs::retry_budget(&pool, "j")
                .await
                .unwrap(),
            (0, 2)
        );
        atlas_inventory::jobs::bump_retry(&pool, "j", 4)
            .await
            .unwrap();

        let (count, max) = atlas_inventory::jobs::retry_budget(&pool, "j")
            .await
            .unwrap();
        assert_eq!((count, max), (1, 2));
        let j = atlas_inventory::jobs::get_job(&pool, "j")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(j.state, "queued", "a retried job returns to the queue");
    }

    /// `enqueue` inserts a job as `pending` and then queues it. If the recovery task or the poller
    /// claims it in between, the late "queue" must not overwrite `running`: that made a job that was
    /// already executing claimable a second time (found by a flaky dedup test).
    #[tokio::test]
    async fn queueing_a_job_never_overwrites_a_claim() {
        let pool = migrated_pool().await;
        for id in ["j_fresh", "j_claimed"] {
            atlas_inventory::jobs::insert_job(&pool, id, "t", "volume.create", "me", &serde_json::json!({}), None)
                .await
                .unwrap();
        }
        assert!(atlas_inventory::jobs::queue_if_pending(&pool, "j_fresh").await.unwrap());
        let fresh = atlas_inventory::jobs::get_job(&pool, "j_fresh").await.unwrap().unwrap();
        assert_eq!(fresh.state, "queued");

        // A worker claims the still-`pending` job before enqueue gets to queue it.
        assert!(atlas_inventory::jobs::try_claim(&pool, "j_claimed", "worker-a").await.unwrap());
        assert!(!atlas_inventory::jobs::queue_if_pending(&pool, "j_claimed").await.unwrap());
        let claimed = atlas_inventory::jobs::get_job(&pool, "j_claimed").await.unwrap().unwrap();
        assert_eq!(claimed.state, "running", "queueing overwrote a running job");
        assert!(
            !atlas_inventory::jobs::try_claim(&pool, "j_claimed", "worker-b").await.unwrap(),
            "a running job must not be claimable twice"
        );
    }
}
