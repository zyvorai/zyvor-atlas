// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The agent and the gateway-side remote driver against each other, fixture data, over real HTTP.

use std::sync::Arc;
use std::time::Duration;

use atlas_driver_core::{DriverError, StorageDriver};
use atlas_driver_macos::{FakeMacosDriver, RemoteMacosDriver};

async fn agent(token: Option<&str>) -> String {
    let app = atlas_mac_agent::router(
        Arc::new(FakeMacosDriver::new("bkd_macos", "mac01")),
        token.map(String::from),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn remote(url: &str, token: Option<&str>) -> RemoteMacosDriver {
    RemoteMacosDriver::new("bkd_macos_lab", url, token.map(String::from), None, Duration::from_secs(5))
        .unwrap()
}

#[tokio::test]
async fn remote_driver_reads_the_agents_inventory_under_its_own_backend_id() {
    let url = agent(Some("s3cret")).await;
    let d = remote(&url, Some("s3cret"));
    let r = d.discover().await.unwrap();
    assert_eq!(r.cluster.backend_id, "bkd_macos_lab");
    assert!(r.pools.iter().any(|p| p.kind == "apfs_container"));
    assert!(r.volumes.iter().any(|v| v.name == "Backup"));
    assert!(!d.list_pools().await.unwrap().is_empty());
    let pool = &d.list_pools().await.unwrap()[0];
    assert!(d.list_volumes(&pool.id).await.is_ok());
    assert!(d.metrics().await.unwrap().iter().any(|m| m.name == "macos_volumes_total"));
}

#[tokio::test]
async fn the_agent_requires_its_token_but_health_stays_open() {
    let url = agent(Some("s3cret")).await;
    assert!(matches!(
        remote(&url, Some("wrong")).discover().await,
        Err(DriverError::Backend(m)) if m.contains("401")
    ));
    assert!(remote(&url, None).discover().await.is_err());
    assert_eq!(reqwest::get(format!("{url}/health")).await.unwrap().status(), 200);
    assert_eq!(reqwest::get(format!("{url}/v1/discover")).await.unwrap().status(), 401);
}

#[tokio::test]
async fn an_unreachable_agent_is_an_error_not_an_empty_inventory() {
    // Bound but never listening: connecting is refused, and the port stays ours, so a server started by a
    // parallel test cannot take it between the bind and the request (the old drop-then-connect version could).
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    assert!(matches!(remote(&url, None).discover().await, Err(DriverError::Unreachable(_))));
    drop(socket);
}

#[test]
fn only_http_urls_are_accepted() {
    assert!(RemoteMacosDriver::new("b", "file:///etc/passwd", None, None, Duration::from_secs(1)).is_err());
    assert!(RemoteMacosDriver::new("b", "not a url", None, None, Duration::from_secs(1)).is_err());
}

// ---- lifecycle through the agent (fixture layout, nothing is executed) ----

use atlas_driver_macos::{Lifecycle, MacosOps};

async fn writable_agent(token: &str) -> String {
    let app = atlas_mac_agent::router_with_ops(
        Arc::new(FakeMacosDriver::new("bkd_macos", "mac01")),
        Some(token.into()),
        Arc::new(MacosOps::fake(false)),
    )
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

#[test]
fn write_endpoints_are_never_built_without_a_token() {
    let r = atlas_mac_agent::router_with_ops(
        Arc::new(FakeMacosDriver::new("bkd_macos", "mac01")),
        None,
        Arc::new(MacosOps::fake(false)),
    );
    assert!(r.is_err());
}

#[tokio::test]
async fn a_read_only_agent_has_no_write_routes() {
    let url = agent(Some("s3cret")).await;
    let r = reqwest::Client::new()
        .post(format!("{url}/v1/ops/erase"))
        .bearer_auth("s3cret")
        .json(&serde_json::json!({ "device": "disk4", "name": "X" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(remote(&url, Some("s3cret")).ops().devices().await.is_err());
}

#[tokio::test]
async fn write_routes_need_the_token() {
    let url = writable_agent("s3cret").await;
    let c = reqwest::Client::new();
    for (m, path) in [("GET", "/v1/devices"), ("POST", "/v1/ops/eject")] {
        let req = if m == "GET" { c.get(format!("{url}{path}")) } else { c.post(format!("{url}{path}")) };
        assert_eq!(req.send().await.unwrap().status(), 401, "{path}");
    }
}

#[tokio::test]
async fn refusals_come_back_from_the_agent_and_cannot_be_dry_run_around() {
    let url = writable_agent("s3cret").await;
    let ops = remote(&url, Some("s3cret")).ops();

    let rows = ops.devices().await.unwrap();
    let internal = rows.iter().find(|r| r.internal).expect("fixture has an internal disk");
    assert!(internal.erase_refusal.is_some());
    // Erasing the internal disk is refused on the Mac, in a dry run and for real.
    for dry in [true, false] {
        let r = if dry {
            ops.check_erase(&internal.device, "Nope").await.map(drop)
        } else {
            ops.erase_disk(&internal.device, "Nope").await.map(drop)
        };
        assert!(
            matches!(&r, Err(DriverError::Backend(m)) if m.starts_with("refused: ")),
            "dry_run={dry}: {r:?}"
        );
    }
    assert!(matches!(
        ops.eject(&internal.device).await,
        Err(DriverError::Backend(m)) if m.starts_with("refused: ")
    ));
    // A bad name is a validation error, not a refusal.
    let external = rows.iter().find(|r| r.erase_refusal.is_none());
    if let Some(ext) = external {
        assert!(matches!(
            ops.check_erase(&ext.device, "bad/name").await,
            Err(DriverError::Backend(m)) if m.starts_with("invalid: ")
        ));
    }
}

#[tokio::test]
async fn resize_is_refused_on_the_boot_container_and_runs_on_an_external_one() {
    let url = writable_agent("s3cret").await;
    let ops = remote(&url, Some("s3cret")).ops();
    assert!(matches!(
        ops.resize_container("disk3", 1 << 40).await,
        Err(DriverError::Backend(m)) if m.starts_with("refused: ")
    ));
    ops.check_resize_container("disk7", 1 << 40).await.unwrap_or_else(|e| panic!("{e}"));
    let out = ops.resize_container("disk7", 1 << 40).await.unwrap();
    assert!(out.starts_with("fake: diskutil apfs resizeContainer disk7 1099511627776B"), "{out}");
}

#[tokio::test]
async fn snapshots_list_create_and_validate_the_date_on_the_agent() {
    let url = writable_agent("s3cret").await;
    let ops = remote(&url, Some("s3cret")).ops();
    assert!(ops.list_local_snapshots().await.unwrap().is_empty());
    assert!(ops.create_local_snapshot().await.unwrap().starts_with("fake: tmutil localsnapshot"));
    ops.check_delete_local_snapshot("2026-10-19-151606").await.unwrap();
    assert!(ops.delete_local_snapshot("2026-10-19-151606").await.unwrap().contains("deletelocalsnapshots"));
    assert!(matches!(
        ops.delete_local_snapshot("not-a-date").await,
        Err(DriverError::Backend(m)) if m.starts_with("invalid: ")
    ));
}

#[tokio::test]
async fn an_allowed_operation_runs_on_the_agent() {
    let url = writable_agent("s3cret").await;
    let ops = remote(&url, Some("s3cret")).ops();
    let rows = ops.devices().await.unwrap();
    let ext = rows
        .iter()
        .find(|r| r.erase_refusal.is_none())
        .expect("fixture has an erasable external disk");
    ops.check_erase(&ext.device, "Scratch").await.unwrap();
    let out = ops.erase_disk(&ext.device, "Scratch").await.unwrap();
    assert!(out.starts_with("fake: diskutil eraseDisk"), "{out}");
}
