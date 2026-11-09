// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent workspaces over REST on the atlas-native backend (fixture driver, no infra): create, write,
//! reset, discard, TTL expiry GC, quota and size-cap refusals, tenant isolation and roles.
//! The native engine's own snapshot/clone behaviour is tested against live nodes in
//! `atlas-driver-native`; the fixture driver here keeps written bytes through snapshot and clone.

use std::net::SocketAddr;
use std::sync::Arc;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use atlas_gateway::state::AppState;
use serde_json::{json, Value};

mod common;

const SECRET: &str = "workspace-test-secret-at-least-32-bytes!!";
const MIB: u64 = 1 << 20;

async fn spawn() -> (SocketAddr, AppState) {
    let database_url = common::fresh_database_url("workspaces").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: SECRET.into(),
        jwt_secret_previous: None,
        auth_required: true,
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
    let app = routes::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

fn token(subject: &str, role: &str, tenant: &str) -> String {
    atlas_gateway::auth::mint_token(SECRET, subject, role, tenant, 3600)
        .unwrap()
        .0
}

struct Api {
    base: String,
    c: reqwest::Client,
}

impl Api {
    fn new(addr: SocketAddr) -> Self {
        Self {
            base: format!("http://{addr}/api/atlas/v1"),
            c: reqwest::Client::new(),
        }
    }
    async fn send(
        &self,
        tok: &str,
        m: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = self
            .c
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(tok);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }
    async fn get(&self, tok: &str, path: &str) -> (u16, Value) {
        self.send(tok, reqwest::Method::GET, path, None).await
    }
    async fn post(&self, tok: &str, path: &str, body: Value) -> (u16, Value) {
        self.send(tok, reqwest::Method::POST, path, Some(body))
            .await
    }
    async fn delete(&self, tok: &str, path: &str) -> (u16, Value) {
        self.send(tok, reqwest::Method::DELETE, path, None).await
    }
    /// A native block volume owned by `tenant`.
    async fn volume(&self, admin: &str, tenant: &str, name: &str, size: u64) -> String {
        let (st, v) = self
            .post(
                admin,
                "/volumes",
                json!({
                    "tenant_id": tenant, "name": name, "size_bytes": size,
                    "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
                }),
            )
            .await;
        assert_eq!(st, 201, "{v}");
        v["volume_id"].as_str().unwrap().to_string()
    }
    async fn write(&self, tok: &str, vol: &str, offset: u64, data: &[u8]) {
        let r = self
            .c
            .put(format!("{}/volumes/{vol}/data?offset={offset}", self.base))
            .bearer_auth(tok)
            .body(data.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 204, "write to {vol}");
    }
    async fn read(&self, tok: &str, vol: &str, offset: u64, len: u64) -> Vec<u8> {
        let r = self
            .c
            .get(format!(
                "{}/volumes/{vol}/data?offset={offset}&len={len}",
                self.base
            ))
            .bearer_auth(tok)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "read from {vol}");
        r.bytes().await.unwrap().to_vec()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn snapshot_count(api: &Api, admin: &str) -> usize {
    api.get(admin, "/snapshots")
        .await
        .1
        .as_array()
        .map_or(0, Vec::len)
}

async fn volume_count(api: &Api, admin: &str) -> usize {
    api.get(admin, "/volumes")
        .await
        .1
        .as_array()
        .map_or(0, Vec::len)
}

#[tokio::test]
async fn create_write_and_discard_leaves_the_source_alone() {
    let (addr, _) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    api.write(&admin, &src, 0, b"base-data").await;
    let snaps_before = snapshot_count(&api, &admin).await;

    let (st, ws) = api
        .post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "name": "run-42", "ttl_secs": 600 }),
        )
        .await;
    assert_eq!(st, 201, "{ws}");
    let wid = ws["id"].as_str().unwrap().to_string();
    let wvol = ws["volume_id"].as_str().unwrap().to_string();
    assert_eq!(ws["state"], "active");
    assert_eq!(ws["name"], "run-42");
    assert_eq!(ws["owner"], "agent-1");
    assert_eq!(ws["tenant_id"], "acme");
    assert_ne!(wvol, src, "a workspace is its own volume");
    let remaining = ws["remaining_secs"].as_i64().unwrap();
    assert!((590..=600).contains(&remaining), "{ws}");

    // It starts with the source's content, and writes stay in the workspace.
    assert_eq!(api.read(&op, &wvol, 0, 9).await, b"base-data");
    api.write(&op, &wvol, 100, b"agent-scratch").await;
    assert_eq!(api.read(&op, &wvol, 100, 13).await, b"agent-scratch");
    assert_eq!(
        api.read(&op, &src, 100, 13).await,
        vec![0u8; 13],
        "the source never sees the write"
    );
    assert_eq!(api.read(&op, &src, 0, 9).await, b"base-data");

    let (st, list) = api.get(&op, "/workspaces").await;
    assert_eq!(st, 200);
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let (st, one) = api.get(&op, &format!("/workspaces/{wid}")).await;
    assert_eq!(st, 200, "{one}");
    assert_eq!(one["volume_id"], wvol.as_str());

    // Discard removes the clone, the snapshot Atlas took and the record; the source stays.
    let (st, out) = api.delete(&op, &format!("/workspaces/{wid}")).await;
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["discarded"], true);
    assert_eq!(api.get(&op, &format!("/workspaces/{wid}")).await.0, 404);
    assert_eq!(
        api.get(&admin, &format!("/volumes/{wvol}")).await.0,
        404,
        "clone volume deleted"
    );
    assert_eq!(api.get(&admin, &format!("/volumes/{src}")).await.0, 200);
    assert_eq!(api.read(&op, &src, 0, 9).await, b"base-data");
    assert_eq!(
        snapshot_count(&api, &admin).await,
        snaps_before,
        "the base snapshot is gone too"
    );
    assert_eq!(
        api.delete(&op, &format!("/workspaces/{wid}")).await.0,
        404,
        "second discard"
    );
}

#[tokio::test]
async fn reset_rolls_back_to_the_base_snapshot() {
    let (addr, _) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    api.write(&admin, &src, 0, b"base-data").await;
    let (_, ws) = api
        .post(&op, "/workspaces", json!({ "source_volume_id": src }))
        .await;
    let wid = ws["id"].as_str().unwrap().to_string();
    let old = ws["volume_id"].as_str().unwrap().to_string();
    let expires = ws["expires_at_unix"].as_i64().unwrap();

    api.write(&op, &old, 0, b"CORRUPTED").await;
    api.write(&op, &old, 4096, b"junk").await;
    assert_eq!(api.read(&op, &old, 0, 9).await, b"CORRUPTED");

    let (st, after) = api
        .post(&op, &format!("/workspaces/{wid}/reset"), json!({}))
        .await;
    assert_eq!(st, 200, "{after}");
    let new = after["volume_id"].as_str().unwrap().to_string();
    assert_ne!(new, old, "a reset is a fresh clone");
    assert_eq!(after["state"], "active");
    assert_eq!(after["reset_count"], 1);
    assert_eq!(
        after["expires_at_unix"], expires,
        "a reset does not extend the lifetime"
    );
    assert_eq!(
        api.get(&admin, &format!("/volumes/{old}")).await.0,
        404,
        "the old clone is gone"
    );
    assert_eq!(
        api.read(&op, &new, 0, 9).await,
        b"base-data",
        "back to the base content"
    );
    assert_eq!(
        api.read(&op, &new, 4096, 4).await,
        vec![0u8; 4],
        "later writes are gone"
    );

    // And again: the base snapshot survives resets.
    api.write(&op, &new, 0, b"again").await;
    let (st, after) = api
        .post(&op, &format!("/workspaces/{wid}/reset"), json!({}))
        .await;
    assert_eq!(st, 200, "{after}");
    assert_eq!(after["reset_count"], 2);
    assert_eq!(
        api.read(&op, after["volume_id"].as_str().unwrap(), 0, 9)
            .await,
        b"base-data"
    );

    // The source is untouched throughout.
    assert_eq!(api.read(&op, &src, 0, 9).await, b"base-data");
}

#[tokio::test]
async fn expired_workspaces_are_garbage_collected() {
    let (addr, state) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    let snaps_before = snapshot_count(&api, &admin).await;

    let (st, short) = api
        .post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "ttl_secs": 60 }),
        )
        .await;
    assert_eq!(st, 201, "{short}");
    let (_, long) = api
        .post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "ttl_secs": 7200 }),
        )
        .await;
    let short_id = short["id"].as_str().unwrap().to_string();
    let short_vol = short["volume_id"].as_str().unwrap().to_string();
    let long_id = long["id"].as_str().unwrap().to_string();

    // Out-of-range lifetimes are refused up front.
    for ttl in [0, 59, -1, 99_999_999] {
        let (st, body) = api
            .post(
                &op,
                "/workspaces",
                json!({ "source_volume_id": src, "ttl_secs": ttl }),
            )
            .await;
        assert_eq!(st, 400, "ttl {ttl}: {body}");
    }

    // Nothing is due yet.
    let r = routes::workspaces::gc_once(&state, now()).await.unwrap();
    assert_eq!((r.discarded, r.failed), (0, 0));
    assert_eq!(
        api.get(&op, "/workspaces")
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Past the short one's expiry only it goes, with its clone and its snapshot.
    let r = routes::workspaces::gc_once(&state, now() + 61)
        .await
        .unwrap();
    assert_eq!((r.discarded, r.failed), (1, 0));
    assert_eq!(
        api.get(&op, &format!("/workspaces/{short_id}")).await.0,
        404
    );
    assert_eq!(
        api.get(&admin, &format!("/volumes/{short_vol}")).await.0,
        404
    );
    assert_eq!(api.get(&op, &format!("/workspaces/{long_id}")).await.0, 200);
    assert_eq!(
        snapshot_count(&api, &admin).await,
        snaps_before + 1,
        "only the long one's snapshot remains"
    );

    // A second pass is a no-op, and the long one goes when its time comes.
    let r = routes::workspaces::gc_once(&state, now() + 61)
        .await
        .unwrap();
    assert_eq!(r.discarded, 0);
    let r = routes::workspaces::gc_once(&state, now() + 7201)
        .await
        .unwrap();
    assert_eq!(r.discarded, 1);
    assert_eq!(snapshot_count(&api, &admin).await, snaps_before);
    assert_eq!(
        api.get(&admin, &format!("/volumes/{src}")).await.0,
        200,
        "the source is never collected"
    );

    // The expiry is audited as the GC's own action.
    let rows =
        atlas_inventory::audit::list(&state.pool, None, Some("workspace.expire"), None, None, 10)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
}

#[tokio::test]
async fn quota_size_cap_and_count_limits_refuse_without_leaking() {
    std::env::set_var("ATLAS_WORKSPACE_MAX_PER_TENANT", "2");
    let (addr, state) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    let snaps_before = snapshot_count(&api, &admin).await;
    let vols_before = volume_count(&api, &admin).await;

    // Size cap: a source bigger than max_bytes is refused before anything is created.
    let (st, body) = api
        .post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "max_bytes": MIB }),
        )
        .await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(
        api.post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "max_bytes": -1 })
        )
        .await
        .0,
        400
    );

    // Tenant volume quota: the source already uses the tenant's one volume slot.
    atlas_inventory::tenants::set_quota(&state.pool, "acme", 0, 1)
        .await
        .unwrap();
    let (st, body) = api
        .post(&op, "/workspaces", json!({ "source_volume_id": src }))
        .await;
    assert_eq!(st, 409, "{body}");
    assert!(body.to_string().contains("quota"), "{body}");
    // Tenant byte quota likewise.
    atlas_inventory::tenants::set_quota(&state.pool, "acme", 8 * MIB as i64, 0)
        .await
        .unwrap();
    let (st, body) = api
        .post(&op, "/workspaces", json!({ "source_volume_id": src }))
        .await;
    assert_eq!(st, 409, "{body}");
    assert!(body.to_string().contains("quota"), "{body}");

    // A refused create leaves nothing behind: no workspace, no snapshot, no clone.
    assert_eq!(
        api.get(&op, "/workspaces")
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(snapshot_count(&api, &admin).await, snaps_before);
    assert_eq!(volume_count(&api, &admin).await, vols_before);

    // Per-tenant workspace count (here 2): the third is refused until one is discarded.
    atlas_inventory::tenants::set_quota(&state.pool, "acme", 0, 0)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (st, ws) = api
            .post(&op, "/workspaces", json!({ "source_volume_id": src }))
            .await;
        assert_eq!(st, 201, "{ws}");
        ids.push(ws["id"].as_str().unwrap().to_string());
    }
    let (st, body) = api
        .post(&op, "/workspaces", json!({ "source_volume_id": src }))
        .await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(
        api.delete(&op, &format!("/workspaces/{}", ids[0])).await.0,
        200
    );
    assert_eq!(
        api.post(&op, "/workspaces", json!({ "source_volume_id": src }))
            .await
            .0,
        201
    );
}

#[tokio::test]
async fn tenants_cannot_see_or_touch_each_others_workspaces() {
    let (addr, _) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let a = token("agent-a", "operator", "tenant-a");
    let b = token("agent-b", "operator", "tenant-b");
    let src_a = api.volume(&admin, "tenant-a", "a-data", 8 * MIB).await;
    let (_, ws) = api
        .post(&a, "/workspaces", json!({ "source_volume_id": src_a }))
        .await;
    let wid = ws["id"].as_str().unwrap().to_string();
    let wvol = ws["volume_id"].as_str().unwrap().to_string();

    // Tenant B cannot clone A's volume or one of its snapshots, and cannot see, reset or discard A's workspace.
    let (st, _) = api
        .post(&b, "/workspaces", json!({ "source_volume_id": src_a }))
        .await;
    assert_eq!(st, 404);
    let (st, _) = api
        .post(
            &b,
            "/workspaces",
            json!({ "source_snapshot_id": ws["base_snapshot_id"] }),
        )
        .await;
    assert_eq!(st, 404);
    assert_eq!(api.get(&b, &format!("/workspaces/{wid}")).await.0, 404);
    assert_eq!(
        api.post(&b, &format!("/workspaces/{wid}/reset"), json!({}))
            .await
            .0,
        404
    );
    assert_eq!(api.delete(&b, &format!("/workspaces/{wid}")).await.0, 404);
    assert_eq!(
        api.get(&b, "/workspaces").await.1.as_array().unwrap().len(),
        0
    );
    assert_eq!(
        api.get(&a, "/workspaces").await.1.as_array().unwrap().len(),
        1
    );
    assert_eq!(
        api.get(&admin, "/workspaces")
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1,
        "admin sees every tenant"
    );
    // B's own workspace is attributed to B.
    let src_b = api.volume(&admin, "tenant-b", "b-data", 8 * MIB).await;
    let (st, wb) = api
        .post(&b, "/workspaces", json!({ "source_volume_id": src_b }))
        .await;
    assert_eq!(st, 201, "{wb}");
    assert_eq!(wb["tenant_id"], "tenant-b");
    assert_eq!(
        api.get(&b, "/workspaces").await.1.as_array().unwrap().len(),
        1
    );
    // Nothing of A's changed.
    assert_eq!(
        api.get(&a, &format!("/workspaces/{wid}")).await.1["volume_id"],
        wvol.as_str()
    );
}

#[tokio::test]
async fn viewers_read_but_cannot_change_and_only_native_sources_work() {
    let (addr, state) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let viewer = token("watcher", "viewer", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    let (_, ws) = api
        .post(&op, "/workspaces", json!({ "source_volume_id": src }))
        .await;
    let wid = ws["id"].as_str().unwrap().to_string();

    assert_eq!(
        api.get(&viewer, "/workspaces")
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(api.get(&viewer, &format!("/workspaces/{wid}")).await.0, 200);
    assert_eq!(
        api.post(&viewer, "/workspaces", json!({ "source_volume_id": src }))
            .await
            .0,
        403
    );
    assert_eq!(
        api.post(&viewer, &format!("/workspaces/{wid}/reset"), json!({}))
            .await
            .0,
        403
    );
    assert_eq!(
        api.delete(&viewer, &format!("/workspaces/{wid}")).await.0,
        403
    );

    // Exactly one source, and it must exist.
    assert_eq!(api.post(&op, "/workspaces", json!({})).await.0, 400);
    assert_eq!(
        api.post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": src, "source_snapshot_id": "snap_x" })
        )
        .await
        .0,
        400
    );
    assert_eq!(
        api.post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": "vol_nope" })
        )
        .await
        .0,
        404
    );

    // A Ceph volume is not a native one.
    let vol = atlas_api_types::StorageVolume {
        id: "vol_ceph_a".into(),
        cluster_id: None,
        pool_id: None,
        name: "ceph-a".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some("rbd:pool/vol_ceph_a".into()),
        size_bytes: MIB as i64,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("ceph-a".into()),
        storage_class_name: Some("zyvor-rbd".into()),
    };
    atlas_inventory::upsert_volume(&state.pool, "bkd_ceph_lab", "acme", &vol, None)
        .await
        .unwrap();
    let (st, body) = api
        .post(
            &op,
            "/workspaces",
            json!({ "source_volume_id": "vol_ceph_a" }),
        )
        .await;
    assert_eq!(st, 400, "{body}");
}

/// A workspace can also start from an existing snapshot; that snapshot is not Atlas's to delete.
#[tokio::test]
async fn a_workspace_from_a_named_snapshot_keeps_that_snapshot() {
    let (addr, _) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let op = token("agent-1", "operator", "acme");
    let src = api.volume(&admin, "acme", "dataset", 8 * MIB).await;
    api.write(&admin, &src, 0, b"v1").await;
    let (st, snap) = api
        .post(
            &op,
            &format!("/volumes/{src}/snapshots"),
            json!({ "name": "golden" }),
        )
        .await;
    assert_eq!(st, 201, "{snap}");
    let sid = snap["snapshot_id"].as_str().unwrap().to_string();
    api.write(&admin, &src, 0, b"v2").await; // the source moves on after the snapshot
    let before = snapshot_count(&api, &admin).await;

    let (st, ws) = api
        .post(&op, "/workspaces", json!({ "source_snapshot_id": sid }))
        .await;
    assert_eq!(st, 201, "{ws}");
    assert_eq!(ws["base_snapshot_id"], sid.as_str());
    assert_eq!(
        api.read(&op, ws["volume_id"].as_str().unwrap(), 0, 2).await,
        b"v1",
        "the snapshot's content, not the source's now"
    );
    assert_eq!(
        snapshot_count(&api, &admin).await,
        before,
        "no extra snapshot is taken"
    );

    let wid = ws["id"].as_str().unwrap();
    assert_eq!(api.delete(&op, &format!("/workspaces/{wid}")).await.0, 200);
    assert_eq!(
        snapshot_count(&api, &admin).await,
        before,
        "the named snapshot survives the workspace"
    );
}

/// Fire `n` creates at once (one task each) and return their HTTP statuses.
async fn create_concurrently(api: &Arc<Api>, tok: &str, body: Value, n: usize) -> Vec<u16> {
    let mut tasks = Vec::new();
    for _ in 0..n {
        let (api, tok, body) = (api.clone(), tok.to_string(), body.clone());
        tasks.push(tokio::spawn(async move {
            api.post(&tok, "/workspaces", body).await.0
        }));
    }
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

/// What a refused or failed create must not leave: only `active` rows, one clone and one snapshot per
/// workspace, nothing stuck in `creating`.
async fn assert_no_residue(
    api: &Api,
    admin: &str,
    snaps_before: usize,
    vols_before: usize,
) -> usize {
    let (_, list) = api.get(admin, "/workspaces").await;
    let rows = list.as_array().unwrap();
    assert!(
        rows.iter().all(|w| w["state"] == "active"),
        "no creating row is left behind: {list}"
    );
    assert_eq!(
        volume_count(api, admin).await,
        vols_before + rows.len(),
        "one clone per workspace"
    );
    assert_eq!(
        snapshot_count(api, admin).await,
        snaps_before + rows.len(),
        "one base snapshot per workspace"
    );
    rows.len()
}

#[tokio::test]
async fn racing_creates_never_exceed_the_owners_byte_budget() {
    let (addr, _) = spawn().await;
    let api = Arc::new(Api::new(addr));
    let admin = token("root", "admin", "global");
    // A budget of one 1 MiB workspace (below the per-tenant count of 2 that other tests in this
    // binary set process-wide, so the budget is the limit that bites): however the creates race,
    // at most one can be held.
    let agent = atlas_gateway::auth::mint_agent_token(
        SECRET,
        "agent-budget",
        "operator",
        "budg",
        3600,
        &[],
        None,
        Some(MIB as i64),
    )
    .unwrap()
    .0;
    let src = api.volume(&admin, "budg", "dataset", MIB).await;
    let snaps_before = snapshot_count(&api, &admin).await;
    let vols_before = volume_count(&api, &admin).await;

    let body = json!({ "source_volume_id": src, "max_bytes": MIB });
    let statuses = create_concurrently(&api, &agent, body.clone(), 8).await;
    assert!(
        statuses.iter().all(|s| *s == 201 || *s == 409),
        "{statuses:?}"
    );
    let admitted = statuses.iter().filter(|s| **s == 201).count();
    // Over-refusal under contention is allowed; over-admission is not.
    assert!(
        admitted <= 1,
        "budget exceeded: {admitted} admitted, {statuses:?}"
    );
    assert_eq!(
        assert_no_residue(&api, &admin, snaps_before, vols_before).await,
        admitted
    );

    // The refused creates released their reservation: the budget still holds exactly one.
    if admitted == 0 {
        let (st, ws) = api.post(&agent, "/workspaces", body.clone()).await;
        assert_eq!(st, 201, "create after the race: {ws}");
    }
    let (st, refused) = api.post(&agent, "/workspaces", body).await;
    assert_eq!(st, 409, "{refused}");
    assert!(refused.to_string().contains("budget"), "{refused}");
    assert_eq!(
        assert_no_residue(&api, &admin, snaps_before, vols_before).await,
        1
    );
}

#[tokio::test]
async fn racing_creates_never_exceed_the_tenant_workspace_count() {
    // Same value the other count test uses: the variable is process-wide.
    std::env::set_var("ATLAS_WORKSPACE_MAX_PER_TENANT", "2");
    let (addr, _) = spawn().await;
    let api = Arc::new(Api::new(addr));
    let admin = token("root", "admin", "global");
    let op = token("agent-count", "operator", "cnt");
    let src = api.volume(&admin, "cnt", "dataset", MIB).await;
    let snaps_before = snapshot_count(&api, &admin).await;
    let vols_before = volume_count(&api, &admin).await;

    let body = json!({ "source_volume_id": src });
    let statuses = create_concurrently(&api, &op, body.clone(), 8).await;
    assert!(
        statuses.iter().all(|s| *s == 201 || *s == 409),
        "{statuses:?}"
    );
    let admitted = statuses.iter().filter(|s| **s == 201).count();
    assert!(
        admitted <= 2,
        "count limit exceeded: {admitted} admitted, {statuses:?}"
    );
    assert_eq!(
        assert_no_residue(&api, &admin, snaps_before, vols_before).await,
        admitted
    );

    for _ in admitted..2 {
        assert_eq!(api.post(&op, "/workspaces", body.clone()).await.0, 201);
    }
    assert_eq!(api.post(&op, "/workspaces", body).await.0, 409);
    assert_eq!(
        assert_no_residue(&api, &admin, snaps_before, vols_before).await,
        2
    );
}

#[tokio::test]
async fn a_creating_row_counts_toward_the_budget_and_a_stale_one_is_reaped() {
    let (addr, state) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let agent = atlas_gateway::auth::mint_agent_token(
        SECRET,
        "agent-crash",
        "operator",
        "crash",
        3600,
        &[],
        None,
        Some(2 * MIB as i64),
    )
    .unwrap()
    .0;
    let src = api.volume(&admin, "crash", "dataset", MIB).await;
    let snaps_before = snapshot_count(&api, &admin).await;
    let vols_before = volume_count(&api, &admin).await;

    // A create that is part-way (its row is in, the clone is not): it holds 2 MiB of the budget.
    let t = now();
    let w = atlas_inventory::workspaces::Workspace {
        id: "ws_halfmade".into(),
        tenant_id: "crash".into(),
        owner: "agent-crash".into(),
        name: "half".into(),
        source_volume_id: src.clone(),
        base_snapshot_id: String::new(),
        owns_base_snapshot: true,
        volume_id: String::new(),
        max_bytes: 2 * MIB as i64,
        state: atlas_inventory::workspaces::CREATING.into(),
        reset_count: 0,
        last_error: None,
        created_at_unix: t,
        expires_at_unix: t + 3600,
    };
    atlas_inventory::workspaces::insert(&state.pool, &w)
        .await
        .unwrap();
    let (_, list) = api.get(&agent, "/workspaces").await;
    assert_eq!(list[0]["state"], "creating", "{list}");
    assert_eq!(list[0]["volume_id"], "");

    let body = json!({ "source_volume_id": src, "max_bytes": MIB });
    let (st, refused) = api.post(&agent, "/workspaces", body.clone()).await;
    assert_eq!(
        st, 409,
        "the half-made create's reservation counts: {refused}"
    );
    // It cannot be reset or discarded by hand while it is being made.
    assert_eq!(
        api.post(&agent, "/workspaces/ws_halfmade/reset", json!({}))
            .await
            .0,
        409
    );
    assert_eq!(api.delete(&agent, "/workspaces/ws_halfmade").await.0, 409);

    // A fresh one is left alone by the GC; once it is stale (creator gone) the GC removes it and
    // the budget is free again.
    let r = routes::workspaces::gc_once(&state, t + 60).await.unwrap();
    assert_eq!((r.discarded, r.failed), (0, 0));
    let r = routes::workspaces::gc_once(
        &state,
        t + atlas_inventory::workspaces::CREATING_STALE_SECS + 1,
    )
    .await
    .unwrap();
    assert_eq!((r.discarded, r.failed), (1, 0));
    assert_eq!(api.post(&agent, "/workspaces", body).await.0, 201);
    assert_eq!(
        assert_no_residue(&api, &admin, snaps_before, vols_before).await,
        1
    );
}

#[tokio::test]
async fn another_owners_reservation_does_not_count_and_a_refusal_leaves_no_row() {
    let (addr, state) = spawn().await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let agent = atlas_gateway::auth::mint_agent_token(
        SECRET,
        "agent-two",
        "operator",
        "adm",
        3600,
        &[],
        None,
        Some(MIB as i64),
    )
    .unwrap()
    .0;
    let src = api.volume(&admin, "adm", "dataset", MIB).await;
    let t = now();
    // A second agent's row would not count; the same owner's does.
    let mk = |id: &str, owner: &str| atlas_inventory::workspaces::Workspace {
        id: id.into(),
        tenant_id: "adm".into(),
        owner: owner.into(),
        name: id.into(),
        source_volume_id: src.clone(),
        base_snapshot_id: String::new(),
        owns_base_snapshot: false,
        volume_id: String::new(),
        max_bytes: MIB as i64,
        state: atlas_inventory::workspaces::CREATING.into(),
        reset_count: 0,
        last_error: None,
        created_at_unix: t,
        expires_at_unix: t + 3600,
    };
    atlas_inventory::workspaces::insert(&state.pool, &mk("ws_other_owner", "someone-else"))
        .await
        .unwrap();
    let body = json!({ "source_volume_id": src, "max_bytes": MIB });
    // Another owner's reservation does not eat this agent's budget.
    let (st, ws) = api.post(&agent, "/workspaces", body.clone()).await;
    assert_eq!(st, 201, "{ws}");
    // Now the budget is full; a refused create leaves exactly the rows that were there.
    let before = atlas_inventory::workspaces::list(&state.pool, None)
        .await
        .unwrap()
        .len();
    let (st, _) = api.post(&agent, "/workspaces", body).await;
    assert_eq!(st, 409);
    assert_eq!(
        atlas_inventory::workspaces::list(&state.pool, None)
            .await
            .unwrap()
            .len(),
        before
    );
}
