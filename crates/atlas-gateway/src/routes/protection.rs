// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Per-volume "Protection Status" — see `atlas_inventory::protection` for the synthesis logic.

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use crate::auth::Actor;
use crate::state::AppState;

fn verdict_str(v: atlas_api_types::ClusterHealthState) -> &'static str {
    use atlas_api_types::ClusterHealthState::*;
    match v {
        Healthy => "healthy",
        Degraded => "degraded",
        Rebuilding => "rebuilding",
        AtRisk => "at_risk",
        Critical => "critical",
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProtectionQuery {
    tenant: Option<String>,
    verdict: Option<String>,
}

/// `GET /protection-status[?tenant=&verdict=]` — fleet-wide protection posture (operator).
pub(crate) async fn list_protection_status(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(mut q): Query<ProtectionQuery>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    // Tenant isolation: a non-admin actor can only ever see their own tenant's volumes, even if
    // they pass a different `?tenant=` explicitly — same rule as GET /volumes.
    if let Some(t) = crate::auth::tenant_scope(s.config.auth_required, &actor) {
        q.tenant = Some(t.to_string());
    }
    let mut rows =
        atlas_inventory::protection::list_protection_status(&s.pool, q.tenant.as_deref()).await?;
    if let Some(v) = q.verdict.as_deref() {
        rows.retain(|r| verdict_str(r.verdict) == v);
    }
    Ok(Json(json!(rows)))
}

/// `GET /volumes/{id}/protection` — a single volume's protection status (operator).
pub(crate) async fn get_volume_protection(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let resource_tenant = atlas_inventory::volume_tenant(&s.pool, &id).await?;
    crate::auth::require_tenant(
        s.config.auth_required,
        &actor,
        &resource_tenant,
        format!("volume {id}"),
    )?;
    let status = atlas_inventory::protection::volume_protection_status(&s.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("volume {id}")))?;
    Ok(Json(json!(status)))
}
