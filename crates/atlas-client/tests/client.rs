// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The client against an in-process stand-in for the gateway.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use atlas_api_types::{CreateVolumeRequest, Owner, VolumeKind};
use atlas_client::{AtlasClient, ClientError};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Fake {
    polls: Arc<AtomicUsize>,
}

fn job(id: &str, state: &str, error: Option<&str>) -> Value {
    json!({ "id": id, "tenant_id": "t1", "job_type": "volume.create", "state": state,
            "requested_by": "x", "progress_percent": 0, "error": error,
            "created_at": null, "updated_at": null })
}

async fn spawn() -> (String, Fake) {
    let fake = Fake::default();
    let app = Router::new()
        .route(
            "/api/atlas/v1/volumes",
            post(|h: HeaderMap, Json(b): Json<Value>| async move {
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tok") {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "no" })));
                }
                (
                    StatusCode::ACCEPTED,
                    Json(json!({ "job_id": "j-ok", "state": "queued",
                                 "resource": { "volume_id": "vol_1", "name": b["name"] } })),
                )
            })
            .get(|| async { Json(json!([{ "id": "vol_1" }])) }),
        )
        .route(
            "/api/atlas/v1/jobs/{id}",
            get(|State(f): State<Fake>, Path(id): Path<String>| async move {
                let n = f.polls.fetch_add(1, Ordering::SeqCst);
                Json(match id.as_str() {
                    "j-ok" if n < 2 => job(&id, "running", None),
                    "j-ok" => job(&id, "succeeded", None),
                    "j-bad" => job(&id, "failed", Some("no capacity")),
                    _ => job(&id, "running", None),
                })
            }),
        )
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

fn request() -> CreateVolumeRequest {
    CreateVolumeRequest {
        tenant_id: "t1".into(),
        name: "data".into(),
        size_bytes: 1 << 30,
        kind: VolumeKind::Block,
        policy: Some("production".into()),
        pool: None,
        owner: Some(Owner {
            product: "transiva".into(),
            resource_type: "vm".into(),
            resource_id: "vm-1".into(),
            role: "data_disk".into(),
        }),
        kubernetes: None,
    }
}

const FAST: Duration = Duration::from_millis(5);

#[tokio::test]
async fn create_then_follow_the_job_to_success() {
    let (base, fake) = spawn().await;
    let c = AtlasClient::new(&base, Some("tok".into()));
    let a = c.create_volume(&request()).await.unwrap();
    assert_eq!(a.job_id, "j-ok");
    assert_eq!(a.resource["volume_id"], "vol_1");
    let done = c
        .wait_for_job(&a.job_id, FAST, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(done.state, "succeeded");
    assert!(fake.polls.load(Ordering::SeqCst) >= 3, "it polled until done");
    assert_eq!(c.list_volumes(Some("t1")).await.unwrap()[0]["id"], "vol_1");
}

#[tokio::test]
async fn http_errors_carry_status_and_body() {
    let (base, _) = spawn().await;
    let c = AtlasClient::new(&base, None);
    match c.create_volume(&request()).await {
        Err(ClientError::Status { status: 401, body }) => assert!(body.contains("no")),
        other => panic!("expected 401, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_job_is_an_error_with_its_message() {
    let (base, _) = spawn().await;
    let c = AtlasClient::new(&base, Some("tok".into()));
    match c.wait_for_job("j-bad", FAST, Duration::from_secs(5)).await {
        Err(ClientError::JobFailed { error, .. }) => assert_eq!(error, "no capacity"),
        other => panic!("expected JobFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_job_that_never_finishes_times_out() {
    let (base, _) = spawn().await;
    let c = AtlasClient::new(&base, Some("tok".into()));
    let r = c
        .wait_for_job("j-stuck", FAST, Duration::from_millis(40))
        .await;
    assert!(matches!(r, Err(ClientError::Timeout { .. })), "{r:?}");
}
