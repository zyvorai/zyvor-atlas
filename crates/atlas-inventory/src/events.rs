// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Unified activity feed: merges recent jobs, audit records, and alerts into one time-sorted
//! timeline. All source tables stamp `created_at`/`updated_at` as `%Y-%m-%dT%H:%M:%fZ`, so ISO
//! strings sort lexicographically = chronologically and we merge in Rust.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{AnyPool, Row};

/// Newest-first activity across jobs, audit, and alerts. Each source contributes up to `limit`
/// rows; the merged result is truncated to `limit`.
pub async fn feed(pool: &AnyPool, limit: i64) -> Result<Vec<Value>> {
    let limit = limit.clamp(1, 500);
    let mut out: Vec<Value> = Vec::new();

    // Jobs — ts is last transition; state drives severity.
    let jobs = sqlx::query(
        "SELECT id, job_type, state, requested_by, error, updated_at
         FROM storage_jobs ORDER BY updated_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in jobs {
        let state: String = r.get("state");
        let severity = match state.as_str() {
            "failed" => "critical",
            "succeeded" => "ok",
            _ => "info",
        };
        out.push(json!({
            "ts": r.get::<String, _>("updated_at"),
            "kind": "job",
            "id": r.get::<String, _>("id"),
            "severity": severity,
            "title": r.get::<String, _>("job_type"),
            "detail": r.get::<Option<String>, _>("error").unwrap_or(state),
            "resource_type": "job",
            "resource_id": r.get::<String, _>("id"),
            "actor": r.get::<String, _>("requested_by"),
        }));
    }

    // Audit — a non-"ok" status is worth flagging.
    let audit = sqlx::query(
        "SELECT id, actor_id, action, resource_type, resource_id, status, created_at
         FROM storage_audit_logs ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in audit {
        let status: String = r.get("status");
        let severity = if status == "ok" || status == "success" {
            "info"
        } else {
            "warning"
        };
        out.push(json!({
            "ts": r.get::<String, _>("created_at"),
            "kind": "audit",
            "id": r.get::<i64, _>("id").to_string(),
            "severity": severity,
            "title": r.get::<String, _>("action"),
            "detail": status,
            "resource_type": r.get::<String, _>("resource_type"),
            "resource_id": r.get::<String, _>("resource_id"),
            "actor": r.get::<String, _>("actor_id"),
        }));
    }

    // Alerts — carry their own severity + state.
    let alerts = sqlx::query(
        "SELECT id, severity, source, resource_type, resource_id, title, description, state, created_at
         FROM storage_alerts ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for r in alerts {
        let state: String = r.get("state");
        out.push(json!({
            "ts": r.get::<String, _>("created_at"),
            "kind": "alert",
            "id": r.get::<String, _>("id"),
            "severity": r.get::<String, _>("severity"),
            "title": r.get::<String, _>("title"),
            "detail": format!("{} — {}", state, r.get::<String, _>("description")),
            "resource_type": r.get::<String, _>("resource_type"),
            "resource_id": r.get::<String, _>("resource_id"),
            "actor": r.get::<String, _>("source"),
        }));
    }

    // Merge: newest first by ISO timestamp, then truncate.
    out.sort_by(|a, b| {
        let ta = a.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        tb.cmp(ta)
    });
    out.truncate(limit as usize);
    Ok(out)
}
