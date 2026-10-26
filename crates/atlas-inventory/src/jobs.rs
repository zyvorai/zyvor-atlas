// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Async job records + state-machine transitions (PDF §10.5, §11 storage_jobs).

use anyhow::Result;
use atlas_api_types::JobRecord;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

/// Insert a new job in the `pending` state. Returns the job id.
pub async fn insert_job(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    job_type: &str,
    requested_by: &str,
    request: &serde_json::Value,
    idempotency_key: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_jobs (id, tenant_id, job_type, state, requested_by, request, idempotency_key)
         VALUES ($1, $2, $3, 'pending', $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(job_type)
    .bind(requested_by)
    .bind(request.to_string())
    .bind(idempotency_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return an existing job for an idempotency key, if any (PDF §17.4).
pub async fn find_by_idempotency(pool: &AnyPool, key: &str) -> Result<Option<JobRecord>> {
    let row = sqlx::query(job_select(
        "WHERE idempotency_key = $1 ORDER BY created_at DESC LIMIT 1",
    ))
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_job))
}

pub async fn get_job(pool: &AnyPool, id: &str) -> Result<Option<JobRecord>> {
    let row = sqlx::query(job_select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_job))
}

pub async fn list_jobs(pool: &AnyPool, limit: i64) -> Result<Vec<JobRecord>> {
    list_jobs_filtered(pool, None, limit).await
}

/// List jobs newest-first, optionally filtered by `state`, capped at `limit`.
pub async fn list_jobs_filtered(
    pool: &AnyPool,
    state: Option<&str>,
    limit: i64,
) -> Result<Vec<JobRecord>> {
    let limit = limit.max(1);
    let rows = match state {
        Some(s) if !s.is_empty() => {
            sqlx::query(job_select(
                "WHERE state = $1 ORDER BY created_at DESC LIMIT $2",
            ))
            .bind(s)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        _ => {
            sqlx::query(job_select("ORDER BY created_at DESC LIMIT $1"))
                .bind(limit)
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(row_to_job).collect())
}

/// Count jobs grouped by state (for the Prometheus self-metrics endpoint).
pub async fn count_by_state(pool: &AnyPool) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query("SELECT state, COUNT(*) AS n FROM storage_jobs GROUP BY state")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("state"), r.get::<i64, _>("n")))
        .collect())
}

/// Move a job to `running` and stamp `started_at`.
pub async fn mark_running(pool: &AnyPool, id: &str) -> Result<()> {
    let now = now_rfc3339(chrono::Utc::now());
    sqlx::query(
        // `CASE WHEN ... THEN ... ELSE ... END`, not `MAX(progress_percent, 5)` — SQLite's `max()`
        // is overloaded as a 2-arg scalar function, but Postgres's `MAX()` is aggregate-only
        // (`SELECT MAX(3,5)` errors: "function max(integer, integer) does not exist" — verified
        // live). `CASE WHEN` is portable on both.
        "UPDATE storage_jobs SET state='running',
         progress_percent=(CASE WHEN progress_percent > 5 THEN progress_percent ELSE 5 END),
         started_at=COALESCE(started_at, $1),
         locked_at=$1,
         updated_at=$1 WHERE id=$2",
    )
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update state + progress mid-flight (e.g. `verifying`, 50%).
pub async fn set_state(pool: &AnyPool, id: &str, state: &str, progress: i64) -> Result<()> {
    sqlx::query("UPDATE storage_jobs SET state=$1, progress_percent=$2, updated_at=$3 WHERE id=$4")
        .bind(state)
        .bind(progress)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record mid-flight progress and a partial result without changing the job's state, so a long job
/// (an image import) can be resumed from what it recorded if it is retried or restarted.
pub async fn set_progress_result(
    pool: &AnyPool,
    id: &str,
    progress: i64,
    result: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "UPDATE storage_jobs SET progress_percent=$1, result=$2, updated_at=$3 WHERE id=$4",
    )
    .bind(progress)
    .bind(result.to_string())
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Terminal success: state `succeeded`, 100%, store result.
pub async fn mark_succeeded(pool: &AnyPool, id: &str, result: &serde_json::Value) -> Result<()> {
    let now = now_rfc3339(chrono::Utc::now());
    sqlx::query(
        "UPDATE storage_jobs SET state='succeeded', progress_percent=100, result=$1,
         locked_by=NULL, locked_at=NULL, error=NULL,
         completed_at=$2,
         updated_at=$2 WHERE id=$3",
    )
    .bind(result.to_string())
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Terminal failure: state `failed`, store error message.
pub async fn mark_failed(pool: &AnyPool, id: &str, error: &str) -> Result<()> {
    let now = now_rfc3339(chrono::Utc::now());
    sqlx::query(
        "UPDATE storage_jobs SET state='failed', error=$1, locked_by=NULL, locked_at=NULL,
         completed_at=$2,
         updated_at=$2 WHERE id=$3",
    )
    .bind(error)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Cancel a job that hasn't started running yet (`pending`/`queued`) — the worker never picks it
/// up. A job already `running` can't be cancelled this way (see `JobEngine::cancel_job`, which
/// signals the in-process worker directly instead). Returns `false` if the job doesn't exist or is
/// no longer in one of those states.
pub async fn cancel_if_queued(pool: &AnyPool, id: &str) -> Result<bool> {
    let now = now_rfc3339(chrono::Utc::now());
    let res = sqlx::query(
        "UPDATE storage_jobs SET state='failed', error='cancelled by operator', locked_by=NULL,
         locked_at=NULL, completed_at=$1,
         updated_at=$1
         WHERE id=$2 AND state IN ('pending','queued')",
    )
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Job ids currently in any of `states`, oldest first — used by boot recovery to re-enqueue work the
/// in-memory channel lost across a restart.
pub async fn ids_by_states(pool: &AnyPool, states: &[&str]) -> Result<Vec<String>> {
    if states.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = (1..=states.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id FROM storage_jobs WHERE state IN ({placeholders}) ORDER BY created_at ASC"
    );
    // `sql` only interpolates a generated `$1,$2,…` placeholder list (an integer count, never user
    // input); the actual values are bound below.
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for s in states {
        q = q.bind(*s);
    }
    let rows = q.fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
}

/// Due `queued`/`pending` job ids whose `next_attempt_at` is null or already reached. Honors retry
/// backoff so a boot/poller does not fire a job early. Oldest first, capped at `limit`.
pub async fn due_ids(pool: &AnyPool, limit: i64) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT id FROM storage_jobs
         WHERE state IN ('queued', 'pending')
           AND (next_attempt_at IS NULL
                OR next_attempt_at <= $1)
         ORDER BY created_at ASC
         LIMIT $2",
    )
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
}

/// Atomically claim a due job for `worker_id`: transitions `queued`/`pending` → `running` and
/// stamps `locked_by`/`locked_at`. Returns `true` if this caller won the claim (so two workers /
/// a channel wake + poller race cannot double-execute).
pub async fn try_claim(pool: &AnyPool, id: &str, worker_id: &str) -> Result<bool> {
    let now = now_rfc3339(chrono::Utc::now());
    let res = sqlx::query(
        "UPDATE storage_jobs SET state='running',
         progress_percent=(CASE WHEN progress_percent > 5 THEN progress_percent ELSE 5 END),
         locked_by=$1, locked_at=$2,
         started_at=COALESCE(started_at, $2),
         updated_at=$2
         WHERE id=$3 AND state IN ('queued', 'pending')
           AND (next_attempt_at IS NULL
                OR next_attempt_at <= $2)",
    )
    .bind(worker_id)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Re-queue `running` jobs whose lock/update is older than `stale_secs` (worker died without boot
/// recovery). Clears the lock and stamps an error note. Returns how many were reclaimed.
pub async fn reclaim_stale_running(pool: &AnyPool, stale_secs: i64) -> Result<u64> {
    if stale_secs <= 0 {
        return Ok(0);
    }
    let cutoff = now_rfc3339(chrono::Utc::now() - chrono::Duration::seconds(stale_secs));
    let now = now_rfc3339(chrono::Utc::now());
    let res = sqlx::query(
        // Same scalar-MAX portability issue as mark_running/try_claim: a 2-value `CASE WHEN` in
        // place of `MAX(a, b)`.
        "UPDATE storage_jobs SET state='queued', locked_by=NULL, locked_at=NULL,
         error=COALESCE(error, 'reclaimed: stale running lock'),
         next_attempt_at=NULL,
         updated_at=$1
         WHERE state='running'
           AND (CASE WHEN COALESCE(locked_at, updated_at, started_at, created_at) > updated_at
                THEN COALESCE(locked_at, updated_at, started_at, created_at) ELSE updated_at END)
               < $2",
    )
    .bind(now)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Fail every job stuck in `running` (a crash/restart left it mid-flight; the side effects may be
/// partial, so we fail-safe rather than blindly re-run). Returns how many were reset.
pub async fn fail_running(pool: &AnyPool, error: &str) -> Result<u64> {
    fail_running_claimed_before(pool, error, chrono::Utc::now()).await
}

/// Like [`fail_running`], but spares jobs claimed at or after `cutoff`. Recovery runs on a task
/// spawned at start-up, so a job enqueued right after start can already be running when it fires;
/// only work claimed before this process started can be an interrupted leftover.
pub async fn fail_running_claimed_before(
    pool: &AnyPool,
    error: &str,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    let now = now_rfc3339(chrono::Utc::now());
    let res = sqlx::query(
        "UPDATE storage_jobs SET state='failed', error=$1, locked_by=NULL, locked_at=NULL,
         completed_at=$2,
         updated_at=$2 WHERE state='running' AND (locked_at IS NULL OR locked_at < $3)",
    )
    .bind(error)
    .bind(now)
    .bind(now_rfc3339(cutoff))
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// A job's retry accounting: `(retry_count, max_retries)`.
pub async fn retry_budget(pool: &AnyPool, id: &str) -> Result<(i64, i64)> {
    let row = sqlx::query("SELECT retry_count, max_retries FROM storage_jobs WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok((row.get("retry_count"), row.get("max_retries")))
}

/// Set a job's retry budget (called at enqueue for retryable job types).
pub async fn set_max_retries(pool: &AnyPool, id: &str, max_retries: i64) -> Result<()> {
    sqlx::query("UPDATE storage_jobs SET max_retries=$1 WHERE id=$2")
        .bind(max_retries)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a retry: bump the count, requeue the job, and stamp when the next attempt is due
/// (`delay_secs` seconds from now).
pub async fn bump_retry(pool: &AnyPool, id: &str, delay_secs: i64) -> Result<()> {
    let now = chrono::Utc::now();
    sqlx::query(
        "UPDATE storage_jobs SET state='queued', retry_count=retry_count+1,
         locked_by=NULL, locked_at=NULL,
         next_attempt_at=$1,
         error=NULL, updated_at=$2 WHERE id=$3",
    )
    .bind(now_rfc3339(now + chrono::Duration::seconds(delay_secs)))
    .bind(now_rfc3339(now))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn job_select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, job_type, state, requested_by, progress_percent, error, result, created_at, updated_at
         FROM storage_jobs {tail}"
    ))
}

fn row_to_job(r: sqlx::any::AnyRow) -> JobRecord {
    let result: serde_json::Value = serde_json::from_str(
        r.get::<Option<String>, _>("result")
            .unwrap_or_else(|| "{}".into())
            .as_str(),
    )
    .unwrap_or(serde_json::Value::Null);
    JobRecord {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        job_type: r.get("job_type"),
        state: r.get("state"),
        requested_by: r.get("requested_by"),
        progress_percent: r.get("progress_percent"),
        error: r.get("error"),
        result,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
