// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Bearer-token gate on the agent API (`ATLAS_IO_TOKEN`) and, with `--features mcp`, the MCP view.

use atlas_io::devmap::DeviceMap;
use atlas_io::http::{router, router_with_token};
use atlas_io::source::FakeSource;
use atlas_io::Collector;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn collector() -> std::sync::Arc<Collector> {
    Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab())
}

async fn status(app: axum::Router, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut req = Request::builder().uri(uri);
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    app.oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn token_gates_everything_but_health() {
    let app = router_with_token(collector(), Some("s3cret-token".into()));
    for uri in ["/io/rca", "/io/risk", "/io/histograms", "/metrics", "/io/leases"] {
        assert_eq!(status(app.clone(), uri, None).await, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            status(app.clone(), uri, Some("Bearer wrong")).await,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
        assert_eq!(
            status(app.clone(), uri, Some("s3cret-token")).await,
            StatusCode::UNAUTHORIZED,
            "scheme required: {uri}"
        );
        assert_eq!(
            status(app.clone(), uri, Some("Bearer s3cret-token")).await,
            StatusCode::OK,
            "{uri}"
        );
    }
    // Probes stay open.
    assert_eq!(status(app.clone(), "/health", None).await, StatusCode::OK);
    assert_eq!(status(app, "/io/health", None).await, StatusCode::OK);
}

#[tokio::test]
async fn no_token_keeps_the_open_behaviour() {
    let app = router(collector());
    assert_eq!(status(app, "/io/risk", None).await, StatusCode::OK);
}

#[cfg(feature = "mcp")]
#[tokio::test]
async fn mcp_requires_the_token_and_serves_tools() {
    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let app = router_with_token(collector(), Some("s3cret-token".into()));
    let post = |auth: Option<&str>| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "atlas-io-agent.zyvor-system:5111")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        req.body(Body::from(INIT)).unwrap()
    };
    let denied = app.clone().oneshot(post(None)).await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let ok = app
        .oneshot(post(Some("Bearer s3cret-token")))
        .await
        .unwrap();
    assert!(ok.status().is_success(), "initialize: {}", ok.status());
    let body = axum::body::to_bytes(ok.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("atlas-io") || text.contains("I/O sensor"), "{text}");
    assert!(text.contains("\"prompts\"") && text.contains("\"resources\""), "{text}");
}
