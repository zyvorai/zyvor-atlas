// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The atlas-native backend through the gateway: registration, synchronous volume and snapshot
//! create/delete via the driver, and refusals for what the backend does not support. Fixture
//! driver, no infra (`atlas-driver-native` tests the real driver against live nodes).

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use serde_json::{json, Value};

mod common;

async fn spawn() -> SocketAddr {
    let database_url = common::fresh_database_url("native_backend").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "native-test-secret-at-least-32-bytes!!".into(),
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

#[tokio::test]
async fn native_backend_volume_and_snapshot_lifecycle() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let (_, backends) = send(c.get(format!("{base}/backends"))).await;
    let native = backends
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == NATIVE_BACKEND_ID)
        .expect("native backend registered");
    assert_eq!(native["backend_type"], "native");
    assert_eq!(native["capabilities"]["block"], true);
    assert_eq!(native["capabilities"]["snapshots"], true);

    let (st, created) = send(c.post(format!("{base}/volumes")).json(&json!({
        "tenant_id": "acme",
        "name": "db-disk",
        "size_bytes": 8_388_608,
        "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
    })))
    .await;
    assert_eq!(st, 201, "{created}");
    let vid = created["volume_id"].as_str().unwrap().to_string();
    assert!(vid.starts_with("vol_native_"), "{vid}");

    let (st, vol) = send(c.get(format!("{base}/volumes/{vid}"))).await;
    assert_eq!(st, 200, "{vol}");
    assert_eq!(vol["size_bytes"], 8_388_608);
    assert_eq!(vol["kind"], "block");
    let (_, acme) = send(c.get(format!("{base}/volumes?tenant=acme"))).await;
    assert!(
        acme.to_string().contains(&vid),
        "volume is attributed to its tenant: {acme}"
    );

    let (st, _) = send(
        c.put(format!("{base}/volumes/{vid}/data?offset=4090"))
            .body(b"hello native".to_vec()),
    )
    .await;
    assert_eq!(st, 204);
    let r = c
        .get(format!("{base}/volumes/{vid}/data?offset=4088&len=16"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(&r.bytes().await.unwrap()[..], b"\0\0hello native\0\0");
    let (st, body) = send(c.get(format!("{base}/volumes/{vid}/data?offset=8388600&len=16"))).await;
    assert_eq!(st, 400, "read past the end: {body}");
    let (st, body) = send(c.get(format!("{base}/volumes/{vid}/data?offset=0&len=999999999"))).await;
    assert_eq!(st, 400, "len over the cap: {body}");
    let r = c
        .put(format!("{base}/volumes/{vid}/data?offset=0"))
        .body(vec![0u8; (4 << 20) + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let (_, all) = send(c.get(format!("{base}/volumes"))).await;
    let other = all
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .find(|id| !id.starts_with("vol_native_"))
        .expect("a fixture volume on another backend");
    let (st, body) = send(c.get(format!("{base}/volumes/{other}/data?offset=0&len=1"))).await;
    assert_eq!(st, 400, "{body}");

    let (st, snap) = send(
        c.post(format!("{base}/volumes/{vid}/snapshots"))
            .json(&json!({ "name": "nightly" })),
    )
    .await;
    assert_eq!(st, 201, "{snap}");
    let sid = snap["snapshot_id"].as_str().unwrap().to_string();
    let (_, snaps) = send(c.get(format!("{base}/snapshots"))).await;
    assert!(snaps.to_string().contains(&sid), "{snaps}");

    let (st, body) = send(
        c.post(format!("{base}/volumes/{vid}/expand"))
            .json(&json!({ "new_size_bytes": 16_777_216 })),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let (_, vol) = send(c.get(format!("{base}/volumes/{vid}"))).await;
    assert_eq!(vol["size_bytes"], 16_777_216);

    let (st, clone) = send(
        c.post(format!("{base}/snapshots/{sid}/clone"))
            .json(&json!({ "name": "copy" })),
    )
    .await;
    assert_eq!(st, 201, "{clone}");
    let cid = clone["volume_id"].as_str().unwrap().to_string();
    assert!(cid.starts_with("vol_native_"), "{cid}");
    let (st, cvol) = send(c.get(format!("{base}/volumes/{cid}"))).await;
    assert_eq!(st, 200, "{cvol}");
    assert_eq!(cvol["name"], "copy");
    assert_eq!(cvol["size_bytes"], 16_777_216);
    let (_, acme) = send(c.get(format!("{base}/volumes?tenant=acme"))).await;
    assert!(acme.to_string().contains(&cid), "{acme}");

    let (st, restored) = send(
        c.post(format!("{base}/snapshots/{sid}/restore"))
            .json(&json!({})),
    )
    .await;
    assert_eq!(st, 201, "{restored}");
    let rid = restored["volume_id"].as_str().unwrap().to_string();

    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 409, "clones depend on the snapshot: {body}");
    for id in [&cid, &rid] {
        let (st, body) = send(c.delete(format!("{base}/volumes/{id}"))).await;
        assert_eq!(st, 200, "{body}");
    }
    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = send(c.delete(format!("{base}/volumes/{vid}"))).await;
    assert_eq!(st, 200, "{body}");
    let (st, _) = send(c.get(format!("{base}/volumes/{vid}"))).await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn native_backend_locked_snapshot_refuses_delete() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let (st, created) = send(c.post(format!("{base}/volumes")).json(&json!({
        "tenant_id": "acme",
        "name": "vault",
        "size_bytes": 8_388_608,
        "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
    })))
    .await;
    assert_eq!(st, 201, "{created}");
    let vid = created["volume_id"].as_str().unwrap().to_string();
    let (st, snap) = send(
        c.post(format!("{base}/volumes/{vid}/snapshots"))
            .json(&json!({ "name": "immutable" })),
    )
    .await;
    assert_eq!(st, 201, "{snap}");
    let sid = snap["snapshot_id"].as_str().unwrap().to_string();

    for bad in [0u64, u64::MAX] {
        let (st, body) = send(
            c.post(format!("{base}/snapshots/{sid}/lock"))
                .json(&json!({ "retain_secs": bad })),
        )
        .await;
        assert_eq!(st, 400, "{body}");
    }
    let (st, body) = send(
        c.post(format!("{base}/snapshots/{sid}/lock"))
            .json(&json!({ "retain_secs": 3600 })),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    assert!(body["retain_until_unix"].as_u64().unwrap() > 0);

    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 409, "locked: {body}");
    let (st, body) = send(
        c.post(format!("{base}/snapshots/{sid}/lock"))
            .json(&json!({ "retain_secs": 10 })),
    )
    .await;
    assert_eq!(st, 409, "a lock cannot be shortened: {body}");
    let (st, body) = send(
        c.post(format!("{base}/snapshots/nope/lock"))
            .json(&json!({ "retain_secs": 60 })),
    )
    .await;
    assert_eq!(st, 404, "{body}");
}

#[tokio::test]
async fn native_backend_filesystem_snapshot_clone_and_restore() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();

    let (_, backends) = send(c.get(format!("{base}/backends"))).await;
    let native = backends
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == NATIVE_BACKEND_ID)
        .unwrap()
        .clone();
    assert_eq!(native["capabilities"]["file"], true);
    assert_eq!(native["capabilities"]["clone"], true);

    let (st, created) = send(c.post(format!("{base}/volumes")).json(&json!({
        "tenant_id": "acme",
        "name": "shared",
        "size_bytes": 1_073_741_824,
        "kind": "filesystem",
        "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
    })))
    .await;
    assert_eq!(st, 201, "{created}");
    let fid = created["volume_id"].as_str().unwrap().to_string();
    assert!(fid.starts_with("vol_native_fs_"), "{fid}");
    let (st, vol) = send(c.get(format!("{base}/volumes/{fid}"))).await;
    assert_eq!(st, 200, "{vol}");
    assert_eq!(vol["kind"], "filesystem");
    assert_eq!(
        vol["size_bytes"], 0,
        "a filesystem's size is the bytes in it"
    );

    let (st, body) = send(c.get(format!("{base}/volumes/{fid}/data?offset=0&len=1"))).await;
    assert_eq!(st, 400, "no block I/O on a filesystem: {body}");
    let (st, body) = send(
        c.post(format!("{base}/volumes/{fid}/expand"))
            .json(&json!({ "new_size_bytes": 2_147_483_648u64 })),
    )
    .await;
    assert_eq!(st, 400, "a filesystem has no size limit: {body}");

    let (st, snap) = send(
        c.post(format!("{base}/volumes/{fid}/snapshots"))
            .json(&json!({ "name": "before-upgrade" })),
    )
    .await;
    assert_eq!(st, 201, "{snap}");
    let sid = snap["snapshot_id"].as_str().unwrap().to_string();
    assert!(sid.starts_with("snap_native_fs_"), "{sid}");

    // An empty filesystem (size 0) can still be cloned and restored without a size.
    let (st, clone) = send(
        c.post(format!("{base}/snapshots/{sid}/clone"))
            .json(&json!({ "name": "shared-copy" })),
    )
    .await;
    assert_eq!(st, 201, "{clone}");
    let cid = clone["volume_id"].as_str().unwrap().to_string();
    let (_, cvol) = send(c.get(format!("{base}/volumes/{cid}"))).await;
    assert_eq!(cvol["kind"], "filesystem");
    assert_eq!(cvol["name"], "shared-copy");
    let (st, restored) = send(
        c.post(format!("{base}/snapshots/{sid}/restore"))
            .json(&json!({})),
    )
    .await;
    assert_eq!(st, 201, "{restored}");
    let rid = restored["volume_id"].as_str().unwrap().to_string();
    assert!(rid.starts_with("vol_native_fs_"), "{rid}");

    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 409, "clones depend on the snapshot: {body}");
    for id in [&cid, &rid] {
        let (st, body) = send(c.delete(format!("{base}/volumes/{id}"))).await;
        assert_eq!(st, 200, "{body}");
    }
    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = send(c.delete(format!("{base}/volumes/{fid}"))).await;
    assert_eq!(st, 200, "{body}");
    let (st, _) = send(c.get(format!("{base}/volumes/{fid}"))).await;
    assert_eq!(st, 404);
}

#[tokio::test]
async fn native_backend_cannot_be_added_at_runtime() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let (st, body) = send(
        reqwest::Client::new()
            .post(format!("{base}/backends"))
            .json(&json!({ "name": "n", "backend_type": "native" })),
    )
    .await;
    assert_eq!(st, 400, "{body}");
}

async fn make_fs(base: &str, c: &reqwest::Client, name: &str) -> String {
    let (st, created) = send(c.post(format!("{base}/volumes")).json(&json!({
        "tenant_id": "acme",
        "name": name,
        "size_bytes": 1_073_741_824,
        "kind": "filesystem",
        "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
    })))
    .await;
    assert_eq!(st, 201, "{created}");
    created["volume_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn ransomware_check_and_protect_lock_a_snapshot_only_when_warranted() {
    let base = format!("http://{}/api/atlas/v1", spawn().await);
    let c = reqwest::Client::new();
    let calm = make_fs(&base, &c, "calm").await;
    let drill = make_fs(&base, &c, "ransomware-drill").await;

    let (st, a) = send(c.get(format!("{base}/volumes/{calm}/ransomware"))).await;
    assert_eq!(st, 200, "{a}");
    assert_eq!(a["verdict"], "normal", "{a}");
    assert_eq!(a["can_execute"], false);
    let (st, body) = send(c.post(format!("{base}/volumes/{calm}/ransomware/protect"))).await;
    assert_eq!(st, 409, "normal activity must not snapshot: {body}");

    let (st, a) = send(c.get(format!("{base}/volumes/{drill}/ransomware"))).await;
    assert_eq!(st, 200, "{a}");
    assert_eq!(a["verdict"], "likely_ransomware", "{a}");

    let (st, body) = send(
        c.post(format!("{base}/volumes/{drill}/ransomware/protect"))
            .json(&json!({ "retain_secs": 60 })),
    )
    .await;
    assert_eq!(st, 400, "retention below an hour is refused: {body}");
    let (st, body) = send(c.post(format!("{base}/volumes/{drill}/ransomware/protect"))).await;
    assert_eq!(st, 201, "{body}");
    let sid = body["snapshot_id"].as_str().unwrap().to_string();
    assert!(body["retain_until_unix"].as_u64().unwrap() > 0);
    let (st, body) = send(c.delete(format!("{base}/snapshots/{sid}"))).await;
    assert_eq!(st, 409, "the protective snapshot is locked: {body}");

    let (st, _) = send(c.get(format!("{base}/volumes/nope/ransomware"))).await;
    assert_eq!(st, 404);
}
