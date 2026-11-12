// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Recorded owner workflow intent (`migrations/0042_owner_workflows.sql`).
//!
//! A row is what an operator asked Atlas to remember. Nothing here calls Transiva, Veyron,
//! GuestKit, PacketWolf, or Machina.

use anyhow::Result;
use serde::Serialize;
use sqlx::{AnyPool, Row};

#[derive(Debug, Clone, Serialize)]
pub struct OwnerWorkflow {
    pub id: String,
    pub tenant_id: String,
    pub owner_id: String,
    pub intent: String,
    pub note: String,
    pub actor_id: String,
    pub created_at: String,
}

pub async fn insert(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    owner_id: &str,
    intent: &str,
    note: &str,
    actor_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO owner_workflows (id, tenant_id, owner_id, intent, note, actor_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(owner_id)
    .bind(intent)
    .bind(note)
    .bind(actor_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_for_owner(
    pool: &AnyPool,
    owner_id: &str,
    limit: i64,
) -> Result<Vec<OwnerWorkflow>> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, owner_id, intent, note, actor_id, created_at
         FROM owner_workflows
         WHERE owner_id = $1
         ORDER BY created_at DESC
         LIMIT $2",
    )
    .bind(owner_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| OwnerWorkflow {
            id: r.get("id"),
            tenant_id: r.get("tenant_id"),
            owner_id: r.get("owner_id"),
            intent: r.get("intent"),
            note: r.get("note"),
            actor_id: r.get("actor_id"),
            created_at: r.get("created_at"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn pool() -> AnyPool {
        let db = format!(
            "{}/atlas-owners-{}-{}.db",
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

    #[tokio::test]
    async fn a_workflow_row_is_the_intent_and_nothing_else() {
        let p = pool().await;
        insert(
            &p,
            "owf_1",
            "global",
            "machina",
            "health",
            "ping only",
            "op_a",
        )
        .await
        .unwrap();
        let rows = list_for_owner(&p, "machina", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].intent, "health");
        assert_eq!(rows[0].note, "ping only");
        assert!(list_for_owner(&p, "veyron", 10).await.unwrap().is_empty());
    }
}
