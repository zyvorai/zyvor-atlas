// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Local owner registry and workflow intent.
//!
//! Wire ids match `atlas_client::owners::KnownProduct` and `docs/PRODUCTS.md`. Health is a stub:
//! Atlas does not call Transiva, Veyron, GuestKit, PacketWolf, or Machina. `POST /owners/{id}/workflow`
//! writes a row to `owner_workflows` and returns it.

use axum::{extract::Path, extract::State, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};

use crate::auth::Actor;
use crate::state::AppState;

/// `(wire id, display name)`. Keep this aligned with `crates/atlas-client/src/owners.rs`.
const OWNERS: &[(&str, &str)] = &[
    ("hyper2kvm", "Transiva"),
    ("machina", "Machina"),
    ("veyron", "Veyron"),
    ("guestkit", "GuestKit"),
    ("packetwolf", "PacketWolf"),
];

fn known(id: &str) -> Option<&'static str> {
    OWNERS
        .iter()
        .find(|(wire, _)| *wire == id)
        .map(|(_, name)| *name)
}

fn health_body(wire: &str, display: &str) -> Value {
    json!({
        "wire_id": wire,
        "display_name": display,
        "status": "not_called",
        "called_remote": false,
        "detail": "Atlas records this owner locally and does not call the product",
    })
}

/// `GET /owners` — the local registry.
pub(crate) async fn list_owners(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_VIEWER)?;
    let items: Vec<Value> = OWNERS
        .iter()
        .map(|(wire, name)| health_body(wire, name))
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `GET /owners/{id}/health` — local stub. Unknown wire ids are 404. Display names are not ids.
pub(crate) async fn owner_health(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_VIEWER)?;
    let display = known(&id).ok_or_else(|| AppError::NotFound(format!("owner {id}")))?;
    Ok(Json(health_body(&id, display)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkflowBody {
    intent: String,
    #[serde(default)]
    note: String,
}

fn valid_intent(intent: &str) -> bool {
    (1..=64).contains(&intent.len())
        && intent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// `POST /owners/{id}/workflow` — store intent. Does not call the product.
pub(crate) async fn owner_workflow(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<String>,
    Json(body): Json<WorkflowBody>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if known(&id).is_none() {
        return Err(AppError::NotFound(format!("owner {id}")));
    }
    let intent = body.intent.trim();
    if !valid_intent(intent) {
        return Err(AppError::Validation(
            "intent must be 1 to 64 characters of letters, digits, '_' or '-'".into(),
        ));
    }
    let note = body.note.trim();
    if note.chars().count() > 512 {
        return Err(AppError::Validation(
            "note must be at most 512 characters".into(),
        ));
    }
    let workflow_id = ids::workflow_id();
    atlas_inventory::owners::insert(
        &s.pool,
        &workflow_id,
        &actor.tenant_id,
        &id,
        intent,
        note,
        &actor.id,
    )
    .await?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&actor.tenant_id),
        &actor.id,
        "owner.workflow",
        "owner",
        &id,
        "recorded",
        Some(json!({ "workflow_id": workflow_id, "intent": intent })),
        None,
    )
    .await;
    Ok(Json(json!({
        "id": workflow_id,
        "owner_id": id,
        "intent": intent,
        "note": note,
        "called_remote": false,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_matches_the_client_wire_ids() {
        let ids: Vec<&str> = OWNERS.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            ["hyper2kvm", "machina", "veyron", "guestkit", "packetwolf"]
        );
        assert!(known("transiva").is_none());
        assert_eq!(known("hyper2kvm"), Some("Transiva"));
        assert!(valid_intent("health"));
        assert!(valid_intent("snapshot-now"));
        assert!(!valid_intent(""));
        assert!(!valid_intent("has space"));
    }
}
