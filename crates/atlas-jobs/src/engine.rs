// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atlas_api_types::JobRecord;
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;
use tokio::sync::{mpsc, Notify};

use crate::spec::JobSpec;

/// The job the worker is currently executing, plus a signal to cancel it. Single-worker, so at
/// most one entry ever exists.
struct RunningJob {
    id: String,
    cancel: Arc<Notify>,
}

/// Handle used by the gateway to enqueue jobs.
#[derive(Clone)]
pub struct JobEngine {
    pool: AnyPool,
    tx: mpsc::UnboundedSender<String>,
    /// Maintenance pause: when set, the worker holds jobs (leaves them `queued`) until resumed.
    paused: Arc<AtomicBool>,
    /// Stable id stamped on `locked_by` when this process claims a job.
    worker_id: String,
    /// The job currently occupying the single worker, if any — lets `cancel_job` signal it
    /// directly instead of waiting out `job_timeout()` (see that function's doc comment: with no
    /// operator override, one wedged job blocks every other job on the gateway for up to 2 hours).
    current: Arc<Mutex<Option<RunningJob>>>,
}

/// Tunables for the durable queue poller. Defaults match production; tests pass zeros to disable.
#[derive(Debug, Clone, Copy)]
pub struct JobEngineOptions {
    pub poll_secs: u64,
    pub stale_secs: u64,
}

impl Default for JobEngineOptions {
    fn default() -> Self {
        Self {
            poll_secs: 2,
            stale_secs: 900,
        }
    }
}

impl JobEngine {
    /// Start the engine: spawn the worker + durable DB poller and return a cloneable handle. On
    /// start it recovers work that a previous restart lost — fail-safe any `running` job and
    /// re-enqueue due `queued`/`pending` rows (honoring `next_attempt_at`).
    pub fn start(pool: AnyPool, k8s: Option<Arc<K8sDriver>>) -> Self {
        Self::start_with(pool, k8s, JobEngineOptions::default())
    }

    pub fn start_with(
        pool: AnyPool,
        k8s: Option<Arc<K8sDriver>>,
        opts: JobEngineOptions,
    ) -> Self {
        let started_at = chrono::Utc::now();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let paused = Arc::new(AtomicBool::new(false));
        let current = Arc::new(Mutex::new(None));
        let worker_id = format!(
            "{}-{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
            std::process::id()
        );
        let worker_pool = pool.clone();
        let worker_tx = tx.clone();
        let worker_paused = paused.clone();
        let worker_current = current.clone();
        let worker_id_clone = worker_id.clone();
        tokio::spawn(async move {
            run_worker(
                worker_pool,
                k8s,
                rx,
                worker_tx,
                worker_paused,
                worker_current,
                worker_id_clone,
            )
            .await
        });

        // Recover across restart (channel lost its contents); the worker above is already draining.
        let recover_pool = pool.clone();
        let recover_tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = recover_since(&recover_pool, &recover_tx, started_at).await {
                tracing::warn!("job recovery failed: {e:#}");
            }
        });

        // Durable poller: DB is source of truth for due work + stale lock reclaim.
        if opts.poll_secs > 0 {
            let poll_pool = pool.clone();
            let poll_tx = tx.clone();
            let poll_paused = paused.clone();
            tokio::spawn(async move {
                run_poller(
                    poll_pool,
                    poll_tx,
                    poll_paused,
                    opts.poll_secs,
                    opts.stale_secs,
                )
                .await
            });
        }

        Self {
            pool,
            tx,
            paused,
            worker_id,
            current,
        }
    }

    /// Pause or resume job execution (maintenance mode). Paused jobs stay `queued` until resumed.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        tracing::info!(paused, "job engine maintenance pause toggled");
    }

    /// Whether the job worker is currently paused for maintenance.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Instance id stamped on `locked_by` when this process claims a job.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Enqueue a job. Honors idempotency: a repeated key returns the existing job (PDF §17.4).
    /// The DB row is the durable queue; the channel send is best-effort (the poller picks up
    /// anything the channel missed).
    pub async fn enqueue(
        &self,
        job_id: &str,
        tenant_id: &str,
        requested_by: &str,
        spec: JobSpec,
        idempotency_key: Option<&str>,
    ) -> Result<JobRecord> {
        if let Some(key) = idempotency_key {
            if let Some(existing) =
                atlas_inventory::jobs::find_by_idempotency(&self.pool, key).await?
            {
                return Ok(existing);
            }
        }
        let request = serde_json::to_value(&spec)?;
        if let Err(e) = atlas_inventory::jobs::insert_job(
            &self.pool,
            job_id,
            tenant_id,
            spec.job_type(),
            requested_by,
            &request,
            idempotency_key,
        )
        .await
        {
            // The find-then-insert above is not atomic: two concurrent enqueue() calls with the
            // same idempotency key (a client retry racing the original request, or two gateway
            // requests) can both see "no existing job" before either commits. The partial UNIQUE
            // index on idempotency_key (migrations/0024) turns the loser's INSERT into a
            // constraint violation instead of a silent duplicate job row; recover by returning
            // the winner's job instead of propagating the error.
            let is_dup_key = idempotency_key.is_some()
                && e.downcast_ref::<sqlx::Error>()
                    .and_then(|se| se.as_database_error())
                    .map(|de| de.is_unique_violation())
                    .unwrap_or(false);
            if is_dup_key {
                if let Some(existing) =
                    atlas_inventory::jobs::find_by_idempotency(&self.pool, idempotency_key.unwrap())
                        .await?
                {
                    return Ok(existing);
                }
            }
            return Err(e);
        }
        atlas_inventory::jobs::set_state(&self.pool, job_id, "queued", 0).await?;
        if self.tx.send(job_id.to_string()).is_err() {
            tracing::warn!(
                job = %job_id,
                "job worker channel closed — row stays queued for the durable poller"
            );
        }
        atlas_inventory::jobs::get_job(&self.pool, job_id)
            .await?
            .ok_or_else(|| anyhow!("job disappeared after insert"))
    }

    /// Cancel a job: if it's the one currently occupying the worker, signal it directly (the
    /// dispatch future is dropped, killing any child process via `kill_on_drop`); otherwise, if
    /// it's still `queued`/`pending`, mark it failed before the worker ever picks it up. Returns
    /// `false` if the job doesn't exist or has already reached a terminal state.
    pub async fn cancel_job(&self, job_id: &str) -> Result<bool> {
        let cancel = {
            let guard = self.current.lock().unwrap();
            guard
                .as_ref()
                .filter(|r| r.id == job_id)
                .map(|r| r.cancel.clone())
        };
        if let Some(cancel) = cancel {
            cancel.notify_one();
            return Ok(true);
        }
        atlas_inventory::jobs::cancel_if_queued(&self.pool, job_id).await
    }
}

/// Recover jobs the in-memory channel lost across a restart: fail-safe any `running` job (it was
/// interrupted mid-flight and may have applied partial side effects — discovery/reconcilers re-derive
/// real state) and re-enqueue anything still due (`queued`/`pending` with `next_attempt_at` ready).
pub async fn recover(pool: &AnyPool, tx: &mpsc::UnboundedSender<String>) -> Result<()> {
    recover_since(pool, tx, chrono::Utc::now()).await
}

/// [`recover`] for an engine that started at `started_at`: a job this process already claimed
/// since then is running normally, not interrupted.
pub async fn recover_since(
    pool: &AnyPool,
    tx: &mpsc::UnboundedSender<String>,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let interrupted = atlas_inventory::jobs::fail_running_claimed_before(
        pool,
        "interrupted by control-plane restart",
        started_at,
    )
    .await?;
    if interrupted > 0 {
        tracing::warn!("job recovery: reset {interrupted} interrupted running job(s) to failed");
    }
    let pending = atlas_inventory::jobs::due_ids(pool, 10_000).await?;
    let n = pending.len();
    for id in pending {
        let _ = tx.send(id);
    }
    if n > 0 {
        tracing::info!("job recovery: re-enqueued {n} due pending job(s)");
    }
    Ok(())
}

/// Periodic DB poller: reclaim stale locks, wake the worker for due jobs. The channel may already
/// hold the same ids; `try_claim` makes double delivery safe.
async fn run_poller(
    pool: AnyPool,
    tx: mpsc::UnboundedSender<String>,
    paused: Arc<AtomicBool>,
    poll_secs: u64,
    stale_secs: u64,
) {
    tracing::info!(poll_secs, stale_secs, "atlas-jobs durable poller started");
    let mut tick = tokio::time::interval(Duration::from_secs(poll_secs.max(1)));
    loop {
        tick.tick().await;
        if paused.load(Ordering::Relaxed) {
            continue;
        }
        match atlas_inventory::jobs::reclaim_stale_running(&pool, stale_secs as i64).await {
            Ok(n) if n > 0 => tracing::warn!("job poller: reclaimed {n} stale running job(s)"),
            Ok(_) => {}
            Err(e) => tracing::warn!("job poller: reclaim failed: {e:#}"),
        }
        match atlas_inventory::jobs::due_ids(&pool, 64).await {
            Ok(ids) => {
                for id in ids {
                    let _ = tx.send(id);
                }
            }
            Err(e) => tracing::warn!("job poller: due scan failed: {e:#}"),
        }
    }
}

/// The worker loop: one job at a time (storage ops are cheap to serialize and this keeps ordering
/// simple). A failing job with retry budget left is re-queued with exponential backoff; otherwise it
/// is marked `failed`. It never takes down the worker.
async fn run_worker(
    pool: AnyPool,
    k8s: Option<Arc<K8sDriver>>,
    mut rx: mpsc::UnboundedReceiver<String>,
    tx: mpsc::UnboundedSender<String>,
    paused: Arc<AtomicBool>,
    current: Arc<Mutex<Option<RunningJob>>>,
    worker_id: String,
) {
    tracing::info!(%worker_id, "atlas-jobs worker started");
    while let Some(job_id) = rx.recv().await {
        // Maintenance pause: hold the job (leave it `queued`, don't mark_running) until resumed.
        while paused.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let cancel = Arc::new(Notify::new());
        *current.lock().unwrap() = Some(RunningJob {
            id: job_id.clone(),
            cancel: cancel.clone(),
        });
        let outcome = execute_job(&pool, &k8s, &job_id, &worker_id, &cancel).await;
        *current.lock().unwrap() = None;
        if let Err(e) = outcome {
            let err = format!("{e:#}");
            match atlas_inventory::jobs::retry_budget(&pool, &job_id).await {
                Ok((count, max)) if count < max => {
                    // Exponential backoff capped at 64s: 2^(attempt), attempt in [0, 6].
                    let secs = 1u64 << (count.clamp(0, 6) as u32);
                    let _ =
                        atlas_inventory::jobs::bump_retry(&pool, &job_id, secs as i64).await;
                    tracing::warn!(job = %job_id, "job failed, retry {}/{max} in {secs}s: {err}", count + 1);
                    // Fast wake after backoff; the durable poller also picks up when due.
                    let tx = tx.clone();
                    let id = job_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(secs)).await;
                        let _ = tx.send(id);
                    });
                }
                _ => {
                    tracing::warn!(job = %job_id, "job failed: {err}");
                    let _ = atlas_inventory::jobs::mark_failed(&pool, &job_id, &err).await;
                }
            }
        }
    }
    tracing::warn!("atlas-jobs worker channel closed");
}

/// Ceiling on a single job's dispatch — the worker loop is single-threaded and serializes every job
/// type in the system, so a job with no other timeout (an infinite loop, a deadlocked lock, a k8s
/// watch that never resolves) would otherwise wedge every subsequent job forever: the stale-running
/// reclaim only fires after 15 minutes and only for jobs already `running`, never for ones queued
/// behind a stuck one. Generous enough for a large `rbd export-diff` backup stream; overridable for
/// unusually large clusters. `POST /jobs/{id}/cancel` (`JobEngine::cancel_job`) is the operator
/// escape hatch that doesn't require waiting this out — verified live: a real `rbd migration
/// prepare` wedged indefinitely against a degraded destination pool (librbd itself never errored
/// or returned), blocking every other job on the gateway; before this API existed the only fix was
/// shelling into the pod to `kill -9` the underlying `rbd` process by hand.
fn job_timeout() -> Duration {
    std::env::var("ATLAS_JOB_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(2 * 3600))
}

/// Ceiling for an `image.import` job. A multi-terabyte image takes far longer than any other job,
/// and the import resumes from what it recorded if it is cut short, so it gets a day by default.
fn import_timeout() -> Duration {
    std::env::var("ATLAS_IMPORT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(24 * 3600))
}

async fn execute_job(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    job_id: &str,
    worker_id: &str,
    cancel: &Notify,
) -> Result<()> {
    // Atomic claim: channel + poller may both wake us for the same id.
    if !atlas_inventory::jobs::try_claim(pool, job_id, worker_id).await? {
        tracing::debug!(job = %job_id, "skip job — not claimable (already running/done/not due)");
        return Ok(());
    }
    let job = atlas_inventory::jobs::get_job(pool, job_id)
        .await?
        .ok_or_else(|| anyhow!("job {job_id} not found"))?;
    // Reload the raw request payload (get_job doesn't return it).
    let request = load_request(pool, job_id).await?;
    let spec: JobSpec = serde_json::from_value(request).context("decode job request")?;
    let timeout = if matches!(spec, JobSpec::ImageImport { .. }) {
        import_timeout()
    } else {
        job_timeout()
    };

    // Race the dispatch against an operator cancellation. On cancel, the `dispatch` future is
    // dropped — any `tokio::process::Command` it's awaiting on is killed via `kill_on_drop`
    // (crates/atlas-driver-ceph/src/cmd.rs), rather than left running as an orphan. Marked
    // `failed` here (not returned as an error) so run_worker's retry-with-backoff never re-runs
    // an operator's explicit cancel.
    tokio::select! {
        _ = cancel.notified() => {
            tracing::warn!(job = %job_id, "job cancelled by operator");
            atlas_inventory::jobs::mark_failed(pool, job_id, "cancelled by operator").await?;
            Ok(())
        }
        r = tokio::time::timeout(
            timeout,
            crate::dispatch::dispatch(pool, k8s, job_id, &job.tenant_id, spec),
        ) => {
            let result = r.map_err(|_| anyhow!("job exceeded the {timeout:?} timeout"))??;
            atlas_inventory::jobs::mark_succeeded(pool, job_id, &result).await?;
            Ok(())
        }
    }
}

async fn load_request(pool: &AnyPool, job_id: &str) -> Result<serde_json::Value> {
    let s: String = sqlx::query_scalar("SELECT request FROM storage_jobs WHERE id=$1")
        .bind(job_id)
        .fetch_one(pool)
        .await?;
    Ok(serde_json::from_str(&s)?)
}
