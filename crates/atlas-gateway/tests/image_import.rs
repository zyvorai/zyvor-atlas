// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `POST /volumes/{id}/import` through the gateway: the request is validated, queued as a job,
//! streamed from a local HTTP server into an atlas-native volume (fixture driver) and verified.
//! The streaming, resume and checksum logic is tested in `atlas-jobs`; this covers the route, the
//! allow-list and the job wiring.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("image_import").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "import-test-secret-at-least-32-bytes!!".into(),
        jwt_secret_previous: None,
        auth_required: false,
        bootstrap_admin_token: None,
        admin_username: "admin".into(),
        admin_password: "Admin@321".into(),
        monitor_interval_secs: 0,
        ceph_prometheus_url: None,
        alert_webhook_url: None,
        backup_keep: 0,
        backup_max_age_secs: 0,
        rgw_public_endpoint: None,
        snapshot_tick_secs: 0,
        databridge_reconcile_secs: 0,
        job_poll_secs: 0,
        job_stale_secs: 0,
        https_addr: None,
        tls_cert_path: None,
        tls_key_path: None,
        tls_self_signed: false,
        disable_http: false,
        nfs_enable: false,
        nfs_server: None,
        nfs_exports: Vec::new(),
        nfs_driver_mode: atlas_common::config::DriverMode::Fake,
        zfs_enable: false,
        zfs_host: None,
        zfs_pools: Vec::new(),
        zfs_driver_mode: atlas_common::config::DriverMode::Fake,
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let state = build_state(
        config,
        BuildOptions {
            enable_k8s: false,
            initial_discovery: true,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state");
    let driver = Arc::new(atlas_driver_native::FakeNativeDriver::new(
        NATIVE_BACKEND_ID,
    ));
    attach_native_driver(&state.pool, &state.drivers, driver)
        .await
        .unwrap();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn send(req: reqwest::RequestBuilder) -> (u16, Value) {
    let r = req.send().await.unwrap();
    let st = r.status().as_u16();
    (st, r.json().await.unwrap_or(Value::Null))
}

fn image(len: usize) -> Vec<u8> {
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8 | 1
        })
        .collect()
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Serves `body` at any path, one response per connection.
async fn serve(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

async fn create_volume(c: &reqwest::Client, base: &str, size: u64) -> String {
    let (st, v) = send(c.post(format!("{base}/volumes")).json(&json!({
        "tenant_id": "acme",
        "name": "imported-disk",
        "size_bytes": size,
        "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
    })))
    .await;
    assert_eq!(st, 201, "{v}");
    v["volume_id"].as_str().unwrap().to_string()
}

/// Poll the job until it leaves the in-flight states.
async fn finish(c: &reqwest::Client, base: &str, job: &str) -> Value {
    for _ in 0..200 {
        let (_, j) = send(c.get(format!("{base}/jobs/{job}"))).await;
        if matches!(j["state"].as_str(), Some("succeeded" | "failed")) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job {job} did not finish");
}

#[tokio::test]
async fn image_import_through_the_gateway() {
    // Env vars are process-wide, so everything that depends on them runs in this one test.
    std::env::remove_var("ATLAS_IMPORT_ALLOWED_HOSTS");
    std::env::set_var("ATLAS_IMPORT_MAX_RETRIES", "0");
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let vid = create_volume(&c, &base, 12 * 1024 * 1024).await;

    let img = image(9 * 1024 * 1024 + 321);
    let host = serve(img.clone()).await;
    let url = format!("http://{host}/disk.raw");
    let body = |u: &str, h: &str| json!({ "url": u, "sha256": h });

    // Off until an allow-list is configured.
    let (st, e) = send(c.post(format!("{base}/volumes/{vid}/import")).json(&body(&url, &sha(&img)))).await;
    assert_eq!(st, 403, "{e}");

    std::env::set_var("ATLAS_IMPORT_ALLOWED_HOSTS", "127.0.0.1");

    // Refusals that never create a job.
    let post = |b: Value| c.post(format!("{base}/volumes/{vid}/import")).json(&b);
    let (st, _) = send(post(body("http://evil.example/x.raw", &sha(&img)))).await;
    assert_eq!(st, 403, "host off the list");
    let (st, _) = send(post(body(&url, "not-a-hash"))).await;
    assert_eq!(st, 400, "bad sha256");
    let (st, _) = send(post(body("file:///etc/passwd", &sha(&img)))).await;
    assert_eq!(st, 400, "scheme");
    let (st, _) = send(post(json!({"url": url, "sha256": sha(&img), "size_bytes": 99_999_999_999u64}))).await;
    assert_eq!(st, 400, "larger than the volume");
    let (st, _) = send(
        c.post(format!("{base}/volumes/vol_missing/import")).json(&body(&url, &sha(&img))),
    )
    .await;
    assert_eq!(st, 404, "unknown volume");

    // A matching import succeeds, is verified and leaves the bytes on the volume.
    let (st, acc) = send(post(json!({"url": url, "sha256": sha(&img).to_uppercase(), "size_bytes": img.len()}))).await;
    assert_eq!(st, 202, "{acc}");
    let job = finish(&c, &base, acc["job_id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["verified"], true, "{job}");
    assert_eq!(job["result"]["bytes"], img.len());
    assert_eq!(job["result"]["sha256"], sha(&img));
    for (off, len) in [(0usize, 4096usize), (4 * 1024 * 1024 - 100, 200), (img.len() - 321, 321)] {
        let r = c
            .get(format!("{base}/volumes/{vid}/data?offset={off}&len={len}"))
            .send()
            .await
            .unwrap();
        assert_eq!(&r.bytes().await.unwrap()[..], &img[off..off + len], "at {off}");
    }

    // A wrong checksum fails the job and says why.
    let (st, acc) = send(post(body(&url, &sha(b"some other image")))).await;
    assert_eq!(st, 202, "{acc}");
    let job = finish(&c, &base, acc["job_id"].as_str().unwrap()).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert!(
        job["error"].as_str().unwrap_or("").contains("checksum mismatch"),
        "{job}"
    );

    // Resuming from a job that is not an import, or from one that is not finished, is refused.
    let (st, _) = send(post(json!({"url": url, "sha256": sha(&img), "resume_from_job": "job_nope"}))).await;
    assert_eq!(st, 404);
    let (st, _) = send(post(json!({"url": url, "sha256": sha(&img), "resume_from_job": acc["job_id"]}))).await;
    assert_eq!(st, 202, "resuming from a failed import of the same volume is allowed");
}
