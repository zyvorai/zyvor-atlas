// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Audit-log writes (PDF §14.3). Every state-changing or sensitive action should append here.

use anyhow::Result;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

tokio::task_local! {
    /// The agent run id of the request being served (see [`valid_run_id`]). The MCP edge scopes a call
    /// to it, so every audit row written on that task, including those of the REST handlers a tool
    /// calls in-process, carries it. Work spawned onto another task does not inherit it.
    pub static RUN_ID: String;
}

/// A run id is 1 to 64 characters from `[A-Za-z0-9_.:-]`, starting with a letter or digit, so it is
/// safe to store, filter on and put in a log line unescaped.
pub fn valid_run_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'))
}

/// The run id in scope on this task, if any.
pub fn current_run_id() -> Option<String> {
    RUN_ID.try_with(|r| r.clone()).ok()
}

/// Append an audit record. `request`/`result` are optional JSON blobs. The row carries the run id in
/// scope on this task ([`RUN_ID`]), if any.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &AnyPool,
    tenant_id: Option<&str>,
    actor_id: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    status: &str,
    request: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
) -> Result<()> {
    record_with_run(
        pool,
        current_run_id().as_deref(),
        tenant_id,
        actor_id,
        action,
        resource_type,
        resource_id,
        status,
        request,
        result,
    )
    .await
}

/// [`record`] with the run id given explicitly, for code that runs on a task of its own.
#[allow(clippy::too_many_arguments)]
pub async fn record_with_run(
    pool: &AnyPool,
    run_id: Option<&str>,
    tenant_id: Option<&str>,
    actor_id: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    status: &str,
    request: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_audit_logs
            (tenant_id, actor_id, action, resource_type, resource_id, status, request, result, run_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(tenant_id)
    .bind(actor_id)
    .bind(action)
    .bind(resource_type)
    .bind(resource_id)
    .bind(status)
    .bind(request.map(|v| v.to_string()))
    .bind(result.map(|v| v.to_string()))
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Query the audit trail, newest first, with a bounded limit. `actor`/`action` are matched as
/// case-insensitive substrings (the UI exposes them as free-text search boxes); `resource_type`/
/// `resource_id` stay exact-match (used for programmatic drill-down, not fuzzy search).
pub async fn list(
    pool: &AnyPool,
    actor: Option<&str>,
    action: Option<&str>,
    resource_type: Option<&str>,
    resource_id: Option<&str>,
    limit: i64,
) -> Result<Vec<serde_json::Value>> {
    list_by_run(pool, actor, action, resource_type, resource_id, None, limit).await
}

/// [`list`] that can also match one agent run id exactly.
pub async fn list_by_run(
    pool: &AnyPool,
    actor: Option<&str>,
    action: Option<&str>,
    resource_type: Option<&str>,
    resource_id: Option<&str>,
    run_id: Option<&str>,
    limit: i64,
) -> Result<Vec<serde_json::Value>> {
    let limit = limit.clamp(1, 1000);
    let actor_like = actor.map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));
    let action_like = action.map(|s| format!("%{}%", s.replace('%', "\\%").replace('_', "\\_")));
    let rows = sqlx::query(
        "SELECT id, tenant_id, actor_id, action, resource_type, resource_id, status,
                request, result, created_at, run_id
         FROM storage_audit_logs
         WHERE ($1 IS NULL OR actor_id LIKE $2 ESCAPE '\\')
           AND ($3 IS NULL OR action LIKE $4 ESCAPE '\\')
           AND ($5 IS NULL OR resource_type = $6)
           AND ($7 IS NULL OR resource_id = $8)
           AND ($9 IS NULL OR run_id = $10)
         ORDER BY id DESC
         LIMIT $11",
    )
    .bind(&actor_like)
    .bind(&actor_like)
    .bind(&action_like)
    .bind(&action_like)
    .bind(resource_type)
    .bind(resource_type)
    .bind(resource_id)
    .bind(resource_id)
    .bind(run_id)
    .bind(run_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let parse = |s: Option<String>| -> serde_json::Value {
        s.and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<i64, _>("id"),
                "tenant_id": r.get::<Option<String>, _>("tenant_id"),
                "actor_id": r.get::<String, _>("actor_id"),
                "action": r.get::<String, _>("action"),
                "resource_type": r.get::<String, _>("resource_type"),
                "resource_id": r.get::<String, _>("resource_id"),
                "status": r.get::<String, _>("status"),
                "request": parse(r.get::<Option<String>, _>("request")),
                "result": parse(r.get::<Option<String>, _>("result")),
                "created_at": r.get::<String, _>("created_at"),
                "run_id": r.get::<Option<String>, _>("run_id"),
            })
        })
        .collect())
}

/// Delete audit rows older than `keep_days` (day-2 retention). Returns how many were pruned.
pub async fn prune(pool: &AnyPool, keep_days: i64) -> Result<u64> {
    let cutoff = now_rfc3339(chrono::Utc::now() - chrono::Duration::days(keep_days.max(1)));
    let res = sqlx::query("DELETE FROM storage_audit_logs WHERE created_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Rows eligible for retention pruning (same cutoff `prune` uses), oldest first — for a caller
/// that wants to export them to an external sink (SIEM/syslog) before they're deleted.
pub async fn rows_older_than(pool: &AnyPool, keep_days: i64) -> Result<Vec<serde_json::Value>> {
    let cutoff = now_rfc3339(chrono::Utc::now() - chrono::Duration::days(keep_days.max(1)));
    let rows = sqlx::query(
        "SELECT id, tenant_id, actor_id, action, resource_type, resource_id, status,
                request, result, created_at, run_id
         FROM storage_audit_logs
         WHERE created_at < $1
         ORDER BY id ASC",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let parse = |s: Option<String>| -> serde_json::Value {
        s.and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<i64, _>("id"),
                "tenant_id": r.get::<Option<String>, _>("tenant_id"),
                "actor_id": r.get::<String, _>("actor_id"),
                "action": r.get::<String, _>("action"),
                "resource_type": r.get::<String, _>("resource_type"),
                "resource_id": r.get::<String, _>("resource_id"),
                "status": r.get::<String, _>("status"),
                "request": parse(r.get::<Option<String>, _>("request")),
                "result": parse(r.get::<Option<String>, _>("result")),
                "created_at": r.get::<String, _>("created_at"),
                "run_id": r.get::<Option<String>, _>("run_id"),
            })
        })
        .collect())
}

/// Delete specific audit rows by id — used after a successful external export, so nothing is
/// lost if the export sink is unreachable (unlike `prune`, which deletes unconditionally).
pub async fn delete_ids(pool: &AnyPool, ids: &[i64]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = (1..=ids.len())
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("DELETE FROM storage_audit_logs WHERE id IN ({placeholders})");
    // `sql` only interpolates a generated `$1,$2,…` placeholder list (an integer count, never user
    // input); the actual values are bound below.
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        q = q.bind(id);
    }
    let res = q.execute(pool).await?;
    Ok(res.rows_affected())
}

pub async fn count_for_action(pool: &AnyPool, action: &str) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_audit_logs WHERE action = $1")
        .bind(action)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn pool() -> AnyPool {
        let db = format!(
            "{}/atlas-audit-run-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let p = crate::connect(&url).await.unwrap();
        crate::migrate(&p, &url).await.unwrap();
        p
    }

    #[test]
    fn run_ids_are_validated() {
        for ok in ["run-1", "a", "R2.D2:3_x", &"x".repeat(64)] {
            assert!(valid_run_id(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".x",
            "a b",
            "a/b",
            "x\u{e9}",
            &"x".repeat(65),
            "a\nb",
            "a'b",
        ] {
            assert!(!valid_run_id(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_row_carries_the_run_id_in_scope_and_can_be_filtered_by_it() {
        let p = pool().await;
        RUN_ID
            .scope("run-7".to_string(), async {
                record(&p, None, "agent", "a.one", "t", "r", "ok", None, None)
                    .await
                    .unwrap();
            })
            .await;
        record(&p, None, "agent", "a.two", "t", "r", "ok", None, None)
            .await
            .unwrap();
        record_with_run(
            &p,
            Some("run-8"),
            None,
            "agent",
            "a.three",
            "t",
            "r",
            "ok",
            None,
            None,
        )
        .await
        .unwrap();

        let all = list(&p, None, None, None, None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[1]["run_id"],
            serde_json::Value::Null,
            "outside a run the column is null"
        );
        let mine = list_by_run(&p, None, None, None, None, Some("run-7"), 10)
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["action"], "a.one");
        assert_eq!(mine[0]["run_id"], "run-7");
        let other = list_by_run(&p, None, None, None, None, Some("run-8"), 10)
            .await
            .unwrap();
        assert_eq!(other[0]["action"], "a.three");
        assert!(list_by_run(&p, None, None, None, None, Some("run-9"), 10)
            .await
            .unwrap()
            .is_empty());
    }
}
