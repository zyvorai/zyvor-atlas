// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! HTTP side of the Mac agent: a Mac's disk inventory for a central gateway. The gateway registers
//! each agent as a backend (`ATLAS_MACOS_AGENTS`) and polls `GET /v1/discover`.
//!
//! Read-only unless the agent is built with [`router_with_ops`] (`ATLAS_MAC_ALLOW_WRITES=1`), which
//! adds `GET /v1/devices` and `POST /v1/ops/{erase,add-volume,delete-volume,resize-container,eject}`. The refusals
//! (internal disk, running system, system volumes, disk images) run here, against this Mac's live
//! layout, whatever the gateway believed; a body with `"dry_run": true` runs them and nothing else.
//!
//! With a token configured everything but `/health` needs `Authorization: Bearer <token>`.

use std::sync::Arc;

use atlas_driver_macos::{DeviceRow, Lifecycle};
use serde::Deserialize;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use atlas_driver_core::{DriverError, StorageDriver};

pub fn router(driver: Arc<dyn StorageDriver>, token: Option<String>) -> Router {
    build(driver, token, None)
}

/// The read-only router plus the lifecycle endpoints. Refuses to build without a token: a write
/// endpoint must never be open.
pub fn router_with_ops(
    driver: Arc<dyn StorageDriver>,
    token: Option<String>,
    ops: Arc<dyn Lifecycle>,
) -> Result<Router, &'static str> {
    if token.is_none() {
        return Err("lifecycle endpoints need a token");
    }
    Ok(build(driver, token, Some(ops)))
}

fn build(
    driver: Arc<dyn StorageDriver>,
    token: Option<String>,
    ops: Option<Arc<dyn Lifecycle>>,
) -> Router {
    let mut app = Router::new()
        .route("/health", get(|| async { Json(serde_json::json!({ "status": "ok" })) }))
        .route("/v1/discover", get(discover))
        .route("/v1/metrics", get(metrics))
        .with_state(driver);
    if let Some(ops) = ops {
        let writes = Router::new()
            .route("/v1/devices", get(devices))
            .route("/v1/snapshots", get(snapshots))
            .route("/v1/ops/snapshot-create", post(snapshot_create))
            .route("/v1/ops/snapshot-delete", post(snapshot_delete))
            .route("/v1/ops/erase", post(erase))
            .route("/v1/ops/add-volume", post(add_volume))
            .route("/v1/ops/delete-volume", post(delete_volume))
            .route("/v1/ops/resize-container", post(resize_container))
            .route("/v1/ops/eject", post(eject))
            .with_state(ops);
        app = app.merge(writes);
    }
    match token {
        Some(t) => app.layer(axum::middleware::from_fn_with_state(
            Arc::<str>::from(t),
            require_bearer,
        )),
        None => app,
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer(
    State(token): State<Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(p) if ct_eq(p.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "unauthorized")
            .into_response(),
    }
}

fn fail(e: DriverError) -> Response {
    let code = match e {
        DriverError::Unreachable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    };
    (code, e.to_string()).into_response()
}

async fn discover(State(d): State<Arc<dyn StorageDriver>>) -> Response {
    match d.discover().await {
        Ok(r) => Json(r).into_response(),
        Err(e) => fail(e),
    }
}

async fn metrics(State(d): State<Arc<dyn StorageDriver>>) -> Response {
    match d.metrics().await {
        Ok(m) => Json(m).into_response(),
        Err(e) => fail(e),
    }
}

/// A refusal is a 409, a bad request a 400, an unreachable `diskutil` a 503.
fn op_fail(e: DriverError) -> Response {
    match e {
        DriverError::Backend(m) if m.starts_with("refused: ") => {
            (StatusCode::CONFLICT, m.trim_start_matches("refused: ").to_string()).into_response()
        }
        DriverError::Backend(m) => (StatusCode::BAD_REQUEST, m).into_response(),
        other => fail(other),
    }
}

type Ops = State<Arc<dyn Lifecycle>>;

async fn devices(State(ops): Ops) -> Response {
    match ops.devices().await {
        Ok(rows) => Json::<Vec<DeviceRow>>(rows).into_response(),
        Err(e) => op_fail(e),
    }
}

#[derive(Deserialize)]
struct EraseBody {
    device: String,
    name: String,
    #[serde(default)]
    dry_run: bool,
}

async fn erase(State(ops): Ops, Json(b): Json<EraseBody>) -> Response {
    let r = if b.dry_run {
        ops.check_erase(&b.device, &b.name).await.map(|()| String::new())
    } else {
        tracing::info!(device = %b.device, "erase requested");
        ops.erase_disk(&b.device, &b.name).await
    };
    done(r, b.dry_run)
}

#[derive(Deserialize)]
struct AddVolumeBody {
    container: String,
    name: String,
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    reserve_bytes: Option<u64>,
    #[serde(default)]
    dry_run: bool,
}

async fn add_volume(State(ops): Ops, Json(b): Json<AddVolumeBody>) -> Response {
    let r = if b.dry_run {
        ops.check_add_volume(&b.container, &b.name).await.map(|()| String::new())
    } else {
        tracing::info!(container = %b.container, "add volume requested");
        ops.add_volume(&b.container, &b.name, b.quota_bytes, b.reserve_bytes).await
    };
    done(r, b.dry_run)
}

async fn snapshots(State(ops): Ops) -> Response {
    match ops.list_local_snapshots().await {
        Ok(d) => Json(d).into_response(),
        Err(e) => op_fail(e),
    }
}

#[derive(Deserialize, Default)]
struct SnapshotCreateBody {
    #[serde(default)]
    dry_run: bool,
}

async fn snapshot_create(State(ops): Ops, body: Option<Json<SnapshotCreateBody>>) -> Response {
    let dry_run = body.is_some_and(|Json(b)| b.dry_run);
    if dry_run {
        return done(Ok(String::new()), true);
    }
    tracing::info!("local snapshot requested");
    done(ops.create_local_snapshot().await, false)
}

#[derive(Deserialize)]
struct SnapshotDeleteBody {
    date: String,
    #[serde(default)]
    dry_run: bool,
}

async fn snapshot_delete(State(ops): Ops, Json(b): Json<SnapshotDeleteBody>) -> Response {
    let r = if b.dry_run {
        ops.check_delete_local_snapshot(&b.date).await.map(|()| String::new())
    } else {
        tracing::info!(date = %b.date, "local snapshot delete requested");
        ops.delete_local_snapshot(&b.date).await
    };
    done(r, b.dry_run)
}

#[derive(Deserialize)]
struct ResizeBody {
    container: String,
    size_bytes: u64,
    #[serde(default)]
    dry_run: bool,
}

async fn resize_container(State(ops): Ops, Json(b): Json<ResizeBody>) -> Response {
    let r = if b.dry_run {
        ops.check_resize_container(&b.container, b.size_bytes).await.map(|()| String::new())
    } else {
        tracing::info!(container = %b.container, size_bytes = b.size_bytes, "resize requested");
        ops.resize_container(&b.container, b.size_bytes).await
    };
    done(r, b.dry_run)
}

#[derive(Deserialize)]
struct DeviceBody {
    device: String,
    #[serde(default)]
    dry_run: bool,
}

async fn delete_volume(State(ops): Ops, Json(b): Json<DeviceBody>) -> Response {
    let r = if b.dry_run {
        ops.check_delete_volume(&b.device).await.map(|()| String::new())
    } else {
        tracing::info!(device = %b.device, "delete volume requested");
        ops.delete_volume(&b.device).await
    };
    done(r, b.dry_run)
}

async fn eject(State(ops): Ops, Json(b): Json<DeviceBody>) -> Response {
    let r = if b.dry_run {
        ops.check_eject(&b.device).await.map(|()| String::new())
    } else {
        tracing::info!(device = %b.device, "eject requested");
        ops.eject(&b.device).await
    };
    done(r, b.dry_run)
}

fn done(r: Result<String, DriverError>, dry_run: bool) -> Response {
    match r {
        Ok(output) => Json(serde_json::json!({ "output": output, "dry_run": dry_run })).into_response(),
        Err(e) => op_fail(e),
    }
}
