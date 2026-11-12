// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Persisted Ops Advisor runs (`migrations/0040_advisor_findings.sql`).

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::{AnyPool, Row};

#[derive(Debug, Clone, Serialize)]
pub struct AdvisorFinding {
    pub id: String,
    pub tenant_id: String,
    pub actor_id: String,
    pub question: String,
    pub mode: String,
    pub risk_score: i64,
    pub risk_level: String,
    pub summary: String,
    pub payload: Value,
    pub created_at: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    actor_id: &str,
    question: &str,
    mode: &str,
    risk_score: i64,
    risk_level: &str,
    summary: &str,
    payload: &Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO advisor_findings
            (id, tenant_id, actor_id, question, mode, risk_score, risk_level, summary, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(actor_id)
    .bind(question)
    .bind(mode)
    .bind(risk_score)
    .bind(risk_level)
    .bind(summary)
    .bind(payload.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Newest first. `tenant` limits the rows to one tenant when the caller is tenant-scoped.
pub async fn list(pool: &AnyPool, tenant: Option<&str>, limit: i64) -> Result<Vec<AdvisorFinding>> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, actor_id, question, mode, risk_score, risk_level, summary, payload, created_at
         FROM advisor_findings
         WHERE ($1 IS NULL OR tenant_id = $2)
         ORDER BY created_at DESC
         LIMIT $3",
    )
    .bind(tenant)
    .bind(tenant)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let payload =
                serde_json::from_str(r.get::<String, _>("payload").as_str()).unwrap_or(Value::Null);
            AdvisorFinding {
                id: r.get("id"),
                tenant_id: r.get("tenant_id"),
                actor_id: r.get("actor_id"),
                question: r.get("question"),
                mode: r.get("mode"),
                risk_score: r.get("risk_score"),
                risk_level: r.get("risk_level"),
                summary: r.get("summary"),
                payload,
                created_at: r.get("created_at"),
            }
        })
        .collect())
}
