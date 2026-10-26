// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /pools/{id}/dedup-estimate` — what dedup (and compression) would save on a pool, without
//! enabling either (`docs/DEDUP_ESTIMATE.md`). Read-only, but it reads data, so it is admin-only
//! and every limit is capped here regardless of what the caller asks for.

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::Actor;
use crate::state::AppState;
use atlas_api_types::DedupEstimateOptions;
use atlas_common::{AppError, AppResult};

const MAX_BYTES_CAP: u64 = 256 << 30;
const MAX_SECS_CAP: u64 = 3600;

#[derive(Debug, Deserialize)]
pub(crate) struct EstimateQuery {
    max_bytes: Option<u64>,
    block_size: Option<u64>,
    max_secs: Option<u64>,
}

/// Defaults for what the caller left out, and the gateway's ceilings for what they asked for.
fn options(q: &EstimateQuery) -> DedupEstimateOptions {
    let d = DedupEstimateOptions::default();
    DedupEstimateOptions {
        max_bytes: q.max_bytes.unwrap_or(d.max_bytes).clamp(1, MAX_BYTES_CAP),
        block_size: q.block_size.unwrap_or(d.block_size),
        max_secs: q.max_secs.unwrap_or(d.max_secs).clamp(1, MAX_SECS_CAP),
    }
}

pub(crate) async fn pool_dedup_estimate(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Query(q): Query<EstimateQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    let pool = atlas_inventory::list_pools(&s.pool)
        .await?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::NotFound(format!("pool {id}")))?;
    let cluster = atlas_inventory::get_cluster(&s.pool, &pool.cluster_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("cluster {}", pool.cluster_id)))?;
    let driver = s.drivers.get(&cluster.backend_id).ok_or_else(|| {
        AppError::Unavailable(format!("no driver for backend {}", cluster.backend_id))
    })?;
    let est = driver
        .estimate_dedup(&pool.name, options(&q))
        .await
        .map_err(|e| match e {
            atlas_driver_core::DriverError::NotImplemented(_) => AppError::Validation(format!(
                "dedup estimates are not available for backend {}",
                cluster.backend_id
            )),
            e => super::volumes::driver_error(e),
        })?;
    Ok(Json(json!(est)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(max_bytes: Option<u64>, block_size: Option<u64>, max_secs: Option<u64>) -> EstimateQuery {
        EstimateQuery {
            max_bytes,
            block_size,
            max_secs,
        }
    }

    #[test]
    fn defaults_apply_when_the_caller_sends_nothing() {
        assert_eq!(
            options(&q(None, None, None)),
            DedupEstimateOptions::default()
        );
    }

    #[test]
    fn limits_are_capped_whatever_the_caller_asks_for() {
        let o = options(&q(Some(u64::MAX), Some(7), Some(u64::MAX)));
        assert_eq!(o.max_bytes, MAX_BYTES_CAP);
        assert_eq!(o.max_secs, MAX_SECS_CAP);
        assert_eq!(options(&q(Some(0), None, Some(0))).max_bytes, 1);
        assert_eq!(options(&q(Some(0), None, Some(0))).max_secs, 1);
    }
}
