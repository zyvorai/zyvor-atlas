// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atlas_api_types::JobRecord;
use atlas_driver_core::DriverRegistry;
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;
use tokio::sync::{mpsc, Notify, Semaphore};

use crate::spec::JobSpec;

/// The job the worker is currently executing, plus a signal to cancel it. Single-worker, so at
/// most one entry ever exists.
struct RunningJob {
    id: String,
    cancel: Arc<Notify>,
}

/// Cancel signals of the `image.import` jobs in flight or waiting for a slot, by job id. Imports run
/// beside the main worker, so there can be several.
type ImportRegistry = Arc<Mutex<HashMap<String, Arc<Notify>>>>;

/// The storage drivers jobs that write volume data go through. Set once, after the engine starts,
/// because the gateway builds its driver registry after the engine; jobs read it when they run.
type DriverCell = Arc<OnceLock<Arc<DriverRegistry>>>;

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
    /// Cancel handles of running or waiting `image.import` jobs (see [`ImportRegistry`]).
    imports: ImportRegistry,
    drivers: DriverCell,
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
        let imports: ImportRegistry = Arc::new(Mutex::new(HashMap::new()));
        let drivers: DriverCell = Arc::new(OnceLock::new());
        let worker_id = format!(
            "{}-{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
            std::process::id()
        );
        let worker_pool = pool.clone();
        let worker_tx = tx.clone();
        let worker_paused = paused.clone();
        let worker_current = current.clone();
        let worker_imports = imports.clone();
        let worker_drivers = drivers.clone();
        let worker_id_clone = worker_id.clone();
        tokio::spawn(async move {
            run_worker(
                worker_pool,
                k8s,
                rx,
                worker_tx,
                worker_paused,
                worker_current,
                worker_imports,
                worker_drivers,
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
            imports,
            drivers,
        }
    }

    /// Give jobs that write volume data (`image.import`) the storage drivers. Call once, after the
    /// driver registry exists; a later call is ignored.
    pub fn set_driver_registry(&self, registry: Arc<DriverRegistry>) {
        let _ = self.drivers.set(registry);
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
        let cancel = cancel.or_else(|| self.imports.lock().unwrap().get(job_id).cloned());
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
#[allow(clippy::too_many_arguments)]
async fn run_worker(
    pool: AnyPool,
    k8s: Option<Arc<K8sDriver>>,
    mut rx: mpsc::UnboundedReceiver<String>,
    tx: mpsc::UnboundedSender<String>,
    paused: Arc<AtomicBool>,
    current: Arc<Mutex<Option<RunningJob>>>,
    imports: ImportRegistry,
    drivers: DriverCell,
    worker_id: String,
) {
    tracing::info!(%worker_id, "atlas-jobs worker started");
    let import_slots = Arc::new(Semaphore::new(import_concurrency()));
    while let Some(job_id) = rx.recv().await {
        // Maintenance pause: hold the job (leave it `queued`, don't mark_running) until resumed.
        while paused.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // An image import can run for hours. It gets its own task, limited to a few at a time, so
        // it never holds up the snapshots, deletes and resizes queued behind it.
        if is_import(&pool, &job_id).await {
            spawn_import(
                pool.clone(),
                k8s.clone(),
                tx.clone(),
                imports.clone(),
                import_slots.clone(),
                drivers.clone(),
                worker_id.clone(),
                job_id,
            );
            continue;
        }
        let cancel = Arc::new(Notify::new());
        *current.lock().unwrap() = Some(RunningJob {
            id: job_id.clone(),
            cancel: cancel.clone(),
        });
        let outcome = execute_job(&pool, &k8s, &drivers, &job_id, &worker_id, &cancel).await;
        *current.lock().unwrap() = None;
        settle(&pool, &tx, &job_id, outcome).await;
    }
    tracing::warn!("atlas-jobs worker channel closed");
}

/// What to do with a finished job: nothing on success; on failure, re-queue it with exponential
/// backoff while it has retry budget left, otherwise mark it `failed`.
async fn settle(
    pool: &AnyPool,
    tx: &mpsc::UnboundedSender<String>,
    job_id: &str,
    outcome: Result<()>,
) {
    if let Err(e) = outcome {
        let err = format!("{e:#}");
        match atlas_inventory::jobs::retry_budget(pool, job_id).await {
            Ok((count, max)) if count < max => {
                // Exponential backoff capped at 64s: 2^(attempt), attempt in [0, 6].
                let secs = 1u64 << (count.clamp(0, 6) as u32);
                let _ = atlas_inventory::jobs::bump_retry(pool, job_id, secs as i64).await;
                tracing::warn!(job = %job_id, "job failed, retry {}/{max} in {secs}s: {err}", count + 1);
                // Fast wake after backoff; the durable poller also picks up when due.
                let tx = tx.clone();
                let id = job_id.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    let _ = tx.send(id);
                });
            }
            _ => {
                tracing::warn!(job = %job_id, "job failed: {err}");
                let _ = atlas_inventory::jobs::mark_failed(pool, job_id, &err).await;
            }
        }
    }
}

/// Whether `job_id` is an `image.import`.
async fn is_import(pool: &AnyPool, job_id: &str) -> bool {
    matches!(
        atlas_inventory::jobs::get_job(pool, job_id).await,
        Ok(Some(j)) if j.job_type == "image.import"
    )
}

/// How many imports may run at once (`ATLAS_IMPORT_CONCURRENCY`, default 2).
fn import_concurrency() -> usize {
    std::env::var("ATLAS_IMPORT_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(2)
}

/// Record `job_id` as in flight and return its cancel handle, or `None` if it already is.
fn register_import(imports: &ImportRegistry, job_id: &str) -> Option<Arc<Notify>> {
    let mut map = imports.lock().unwrap();
    if map.contains_key(job_id) {
        return None;
    }
    let cancel = Arc::new(Notify::new());
    map.insert(job_id.to_string(), cancel.clone());
    Some(cancel)
}

/// Run an import on its own task: wait for a slot, execute, settle. The job stays `queued` while it
/// waits, and the registry entry lets `cancel_job` reach it in either state.
#[allow(clippy::too_many_arguments)]
fn spawn_import(
    pool: AnyPool,
    k8s: Option<Arc<K8sDriver>>,
    tx: mpsc::UnboundedSender<String>,
    imports: ImportRegistry,
    slots: Arc<Semaphore>,
    drivers: DriverCell,
    worker_id: String,
    job_id: String,
) {
    // The same id can be delivered twice (the startup recovery and the enqueue both send it). A
    // second task would replace this one's cancel handle, so a cancel would reach the task that is
    // only waiting, not the one that is running. An id already in flight is ignored.
    let Some(cancel) = register_import(&imports, &job_id) else {
        tracing::debug!(job = %job_id, "image import already in flight; ignoring duplicate delivery");
        return;
    };
    tokio::spawn(async move {
        let outcome = match slots.acquire_owned().await {
            Ok(_slot) => execute_job(&pool, &k8s, &drivers, &job_id, &worker_id, &cancel).await,
            Err(e) => Err(anyhow!("import slots closed: {e}")),
        };
        imports.lock().unwrap().remove(&job_id);
        settle(&pool, &tx, &job_id, outcome).await;
    });
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
    drivers: &DriverCell,
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
    let registry = drivers.get().cloned();
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
            crate::dispatch::dispatch(pool, k8s, &registry, job_id, &job.tenant_id, spec),
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

#[cfg(test)]
mod import_registry_tests {
    use super::*;

    #[test]
    fn a_job_in_flight_is_registered_once() {
        let imports: ImportRegistry = Arc::new(Mutex::new(HashMap::new()));
        let first = register_import(&imports, "job_a").expect("first delivery registers");
        assert!(
            register_import(&imports, "job_a").is_none(),
            "a duplicate delivery must not replace the running job's cancel handle"
        );
        // The entry still holds the first handle, so a cancel reaches the running task.
        let held = imports.lock().unwrap().get("job_a").cloned().unwrap();
        assert!(Arc::ptr_eq(&first, &held));
        // A different job is unaffected, and an id can be registered again once it has finished.
        assert!(register_import(&imports, "job_b").is_some());
        imports.lock().unwrap().remove("job_a");
        assert!(register_import(&imports, "job_a").is_some());
    }
}
