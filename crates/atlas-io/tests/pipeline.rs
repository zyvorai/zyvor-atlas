// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use atlas_io::devmap::DeviceMap;
use atlas_io::http::router;
use atlas_io::source::FakeSource;
use atlas_io::Collector;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn http_histograms_and_rca() {
    let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
    let app = router(c);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/io/histograms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.as_array().unwrap().len() >= 2);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/io/rca?volume=vol_vm_web01")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let found = v
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["verdict"] == "critical_latency");
    assert!(
        found,
        "expected critical write tail on vol_vm_web01, got {v}"
    );

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("atlas_io_events_seen"));
    assert!(text.contains("atlas_io_hist_p99_us"));
}

#[tokio::test]
async fn lease_grant_and_list() {
    let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
    let app = router(c);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/io/leases")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"device":"rbd0","ttl_secs":30,"reason":"incident-42"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let res = app
        .oneshot(
            Request::builder()
                .uri("/io/leases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["device"], "rbd0");
    assert_eq!(v[0]["action"], "write_freeze");
}

#[tokio::test]
async fn native_view_resolves_devices_and_exports_metrics() {
    let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
    let app = router(c);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/io/native")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["tracked_pids"][0], 3100);
    assert_eq!(v["slow_threshold_us"], 5000);
    assert_eq!(v["stats"][0]["device"], "8:48");
    assert_eq!(v["stats"][0]["op"], "write");
    assert_eq!(v["slow"][0]["latency_us"], 7400);

    let res = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("atlas_io_native_tracked_pids 1"));
    assert!(text.contains(r#"atlas_io_native_ios{device="8:48",op="write",cgroup_id="4004"} 1200"#));
}

#[tokio::test]
async fn live_mode_without_bpf_has_no_native_view() {
    use atlas_io::source::LiveSource;
    let app = router(Collector::new(Box::new(LiveSource), DeviceMap::new()));
    let res = app
        .oneshot(
            Request::builder()
                .uri("/io/native")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn live_mode_reports_missing_programs() {
    use atlas_io::source::LiveSource;
    let c = Collector::new(Box::new(LiveSource), DeviceMap::new());
    let h = c.health();
    assert_eq!(h.mode, "live");
    assert!(h.programs_loaded.is_empty());
    assert!(h.programs_missing.contains(&"atlas_bio".into()));
    assert!(h.programs_missing.contains(&"atlas_native".into()));
}

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn risk_route_scores_rca_and_capacity() {
    let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
    let app = router(c);

    // The demo has a critical write tail on vol_vm_web01.
    let v = get_json(app.clone(), "/io/risk?volume=vol_vm_web01").await;
    assert_eq!(v["level"], "critical");
    assert_eq!(v["can_execute"], false);
    assert_eq!(v["capacity_used"], false);
    assert!(!v["evidence"].as_array().unwrap().is_empty());

    // An unknown volume has no RCA evidence: healthy, until capacity says otherwise.
    let v = get_json(app.clone(), "/io/risk?volume=nope").await;
    assert_eq!(v["level"], "healthy");
    let v = get_json(app.clone(), "/io/risk?volume=nope&used_percent=80").await;
    assert_eq!(v["level"], "elevated");
    assert_eq!(v["capacity_used"], true);
    let v = get_json(app.clone(), "/io/risk?volume=nope&used_percent=95").await;
    assert_eq!(v["level"], "critical");
    // Out-of-range capacity is ignored by the scorer, not trusted.
    let v = get_json(app.clone(), "/io/risk?volume=nope&used_percent=500").await;
    assert_eq!(v["level"], "healthy");

    let res = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("atlas_io_risk_level 2"), "{text}");
}
