// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! HTTP side of the Mac agent: a Mac's disk inventory for a central gateway. Read-only. The gateway
//! registers each agent as a backend (`ATLAS_MACOS_AGENTS`) and polls `GET /v1/discover`.
//!
//! With a token configured everything but `/health` needs `Authorization: Bearer <token>`.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use atlas_driver_core::{DriverError, StorageDriver};

pub fn router(driver: Arc<dyn StorageDriver>, token: Option<String>) -> Router {
    let app = Router::new()
        .route("/health", get(|| async { Json(serde_json::json!({ "status": "ok" })) }))
        .route("/v1/discover", get(discover))
        .route("/v1/metrics", get(metrics))
        .with_state(driver);
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
