// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Two-person proposals (`migrations/0041_control_proposals.sql`).
//!
//! A proposal is pending until a *different* actor confirms it. Confirm moves the row to
//! `confirming` so two confirms cannot both win, then the caller finishes or aborts.

use anyhow::Result;
use sqlx::{AnyPool, Row};

/// Early unlock of a retention-locked snapshot. Two distinct admins.
pub const KIND_UNLOCK: &str = "snapshot_unlock";
/// Acknowledge an alert. Two distinct operators. The single-step `POST /alerts/{id}/ack` stays.
pub const KIND_ACK: &str = "alert_ack";

/// How long a proposal can be confirmed, in seconds.
pub const TTL_SECS: i64 = 15 * 60;

#[derive(Debug)]
pub enum ConfirmError {
    NotFound,
    Expired,
    /// The confirmer is the same actor who proposed it.
    SameActor,
    NotPending,
    Db(anyhow::Error),
}

#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &AnyPool,
    id: &str,
    kind: &str,
    subject_id: &str,
    tenant_id: &str,
    proposed_by: &str,
    reason: &str,
    expires_unix: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO control_proposals
            (id, kind, subject_id, tenant_id, proposed_by, reason, expires_unix, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')",
    )
    .bind(id)
    .bind(kind)
    .bind(subject_id)
    .bind(tenant_id)
    .bind(proposed_by)
    .bind(reason)
    .bind(expires_unix)
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim a pending proposal for `actor_id`. Fails when the actor proposed it, it expired, or it
/// is no longer pending.
pub async fn begin_confirm(
    pool: &AnyPool,
    id: &str,
    kind: &str,
    subject_id: &str,
    actor_id: &str,
    now_unix: i64,
) -> Result<(), ConfirmError> {
    let res = sqlx::query(
        "UPDATE control_proposals SET state='confirming', confirmed_by=$1
         WHERE id=$2 AND kind=$3 AND subject_id=$4 AND state='pending'
           AND proposed_by <> $6 AND expires_unix > $5",
    )
    .bind(actor_id)
    .bind(id)
    .bind(kind)
    .bind(subject_id)
    .bind(now_unix)
    .bind(actor_id)
    .execute(pool)
    .await
    .map_err(|e| ConfirmError::Db(e.into()))?;
    if res.rows_affected() == 1 {
        return Ok(());
    }
    let row = sqlx::query(
        "SELECT proposed_by, expires_unix, subject_id, kind FROM control_proposals WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| ConfirmError::Db(e.into()))?;
    let Some(row) = row else {
        return Err(ConfirmError::NotFound);
    };
    let proposed_by: String = row.get("proposed_by");
    let exp: i64 = row.get("expires_unix");
    let subject: String = row.get("subject_id");
    let row_kind: String = row.get("kind");
    if row_kind != kind || subject != subject_id {
        return Err(ConfirmError::NotFound);
    }
    if proposed_by == actor_id {
        return Err(ConfirmError::SameActor);
    }
    if exp <= now_unix {
        return Err(ConfirmError::Expired);
    }
    Err(ConfirmError::NotPending)
}

pub async fn finish(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE control_proposals SET state='confirmed' WHERE id=$1 AND state='confirming'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Put a claimed proposal back to pending after the underlying action failed.
pub async fn abort(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE control_proposals SET state='pending', confirmed_by=NULL
         WHERE id=$1 AND state='confirming'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn pool() -> AnyPool {
        let db = format!(
            "{}/atlas-proposals-{}-{}.db",
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
    async fn a_second_actor_confirms_and_the_proposer_cannot() {
        let p = pool().await;
        insert(
            &p,
            "prp_1",
            KIND_UNLOCK,
            "snap_a",
            "global",
            "admin_a",
            "restore",
            2_000,
        )
        .await
        .unwrap();
        let same = begin_confirm(&p, "prp_1", KIND_UNLOCK, "snap_a", "admin_a", 1_000).await;
        assert!(matches!(same, Err(ConfirmError::SameActor)));
        begin_confirm(&p, "prp_1", KIND_UNLOCK, "snap_a", "admin_b", 1_000)
            .await
            .unwrap();
        let again = begin_confirm(&p, "prp_1", KIND_UNLOCK, "snap_a", "admin_c", 1_000).await;
        assert!(matches!(again, Err(ConfirmError::NotPending)));
        finish(&p, "prp_1").await.unwrap();
        let replay = begin_confirm(&p, "prp_1", KIND_UNLOCK, "snap_a", "admin_c", 1_000).await;
        assert!(matches!(replay, Err(ConfirmError::NotPending)));
    }

    #[tokio::test]
    async fn an_expired_proposal_is_refused_and_a_failed_confirm_can_be_retried() {
        let p = pool().await;
        insert(&p, "prp_2", KIND_ACK, "alert_a", "t", "op_a", "", 500)
            .await
            .unwrap();
        let late = begin_confirm(&p, "prp_2", KIND_ACK, "alert_a", "op_b", 500).await;
        assert!(matches!(late, Err(ConfirmError::Expired)));
        begin_confirm(&p, "prp_2", KIND_ACK, "alert_a", "op_b", 100)
            .await
            .unwrap();
        abort(&p, "prp_2").await.unwrap();
        begin_confirm(&p, "prp_2", KIND_ACK, "alert_a", "op_c", 100)
            .await
            .unwrap();
    }
}
