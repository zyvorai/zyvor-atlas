// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::sync::Arc;

use atlas_io::devmap::DeviceMap;
use atlas_io::gpu::GpuMonitor;
use atlas_io::http::router_with_gpu;
use atlas_io::source::FakeSource;
use atlas_io::Collector;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = res.status();
    let b = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, String::from_utf8(b.to_vec()).unwrap())
}

fn app(gpu: GpuMonitor) -> axum::Router {
    let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
    router_with_gpu(c, None, Arc::new(gpu))
}

#[tokio::test]
async fn fake_gpu_correlates_with_demo_workloads() {
    let app = app(GpuMonitor::from_mode("fake"));
    let (st, body) = get(&app, "/io/gpu").await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["mode"], "fake");
    assert_eq!(v["devices"].as_array().unwrap().len(), 3);

    let (_, body) = get(&app, "/io/gpu-correlation").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    // pid 99 (rbd-nbd, 18 ms writes) is on GPU 0 at 8% utilisation.
    assert_eq!(v[0]["verdict"], "storage_starved_suspected");
    assert_eq!(v[1]["verdict"], "compute_bound_or_ok");
    assert_eq!(v[2]["verdict"], "idle");

    let (_, m) = get(&app, "/metrics").await;
    assert!(m.contains("atlas_gpu_utilization_percent{gpu=\"1\""));
    assert!(m.contains("atlas_gpu_storage_starved{gpu=\"0\"} 1"));
}

#[tokio::test]
async fn off_mode_serves_an_empty_honest_report() {
    let app = app(GpuMonitor::off());
    let (st, body) = get(&app, "/io/gpu").await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["mode"], "off");
    let (_, m) = get(&app, "/metrics").await;
    assert!(!m.contains("atlas_gpu_"));
}

#[tokio::test]
async fn health_whatif_and_cgroup_over_http() {
    let app = app(GpuMonitor::from_mode("fake"));
    let (st, body) = get(&app, "/io/gpu-health").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body.trim(), "[]", "the fake fleet is healthy");

    let (st, body) = get(
        &app,
        "/io/gpu-whatif?gpus=8&per_gpu_mbps=2000&storage_mbps=15000",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["verdict"], "starves");
    assert_eq!(v["max_gpus_fed"], 7);

    let (st, _) = get(&app, "/io/gpu-whatif?gpus=0&per_gpu_mbps=1&storage_mbps=1").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = get(&app, "/io/gpu-whatif?gpus=1").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (_, body) = get(&app, "/io/gpu").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["processes"][0]["cgroup"], "/kubepods.slice/pod-trainer-a");
}

#[tokio::test]
async fn xid_events_over_http_and_off_is_unavailable() {
    let app_fake = app(GpuMonitor::from_mode("fake"));
    let (st, body) = get(&app_fake, "/io/gpu-xid").await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v[0]["xid"], 79);
    assert_eq!(v[0]["severity"], "critical");

    let app_off = app(GpuMonitor::off());
    let (st, body) = get(&app_off, "/io/gpu-xid").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("error"));
}
