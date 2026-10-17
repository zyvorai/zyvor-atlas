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
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    assert!(matches!(remote(&url, None).discover().await, Err(DriverError::Unreachable(_))));
}

#[test]
fn only_http_urls_are_accepted() {
    assert!(RemoteMacosDriver::new("b", "file:///etc/passwd", None, None, Duration::from_secs(1)).is_err());
    assert!(RemoteMacosDriver::new("b", "not a url", None, None, Duration::from_secs(1)).is_err());
}
