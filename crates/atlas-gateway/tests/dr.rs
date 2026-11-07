// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 cross-cluster DR — hardened control-plane. Register a peer, enable RBD mirroring on a
//! volume, and fail over (promote/demote) with transition guards, preflight, and a confirm-gated
//! failover runbook. Fake driver: `rbd mirror` CLI is skipped so jobs succeed; live mirroring still
//! needs a second Ceph cluster (see docs/DR.md).

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("dr").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "dr-test-secret-at-least-32-bytes-long!!".into(),
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
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state");
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, pool)
}

#[tokio::test]
async fn dr_peer_mirror_and_failover() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    // Register a mirroring peer.
    let peer = c
        .post(format!("{base}/dr/peers"))
        .json(&json!({ "name": "dc2", "cluster_fsid": "fsid-2", "secret_ref": "dc2-bootstrap" }))
        .send()
        .await
        .unwrap();
    assert_eq!(peer.status(), 201);
    let peer_body: Value = peer.json().await.unwrap();
    let peer_id = peer_body["id"].as_str().unwrap().to_string();

    // A direct-RBD volume to mirror.
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, backend_native_id)
         VALUES ('v1', 't', 'bkd_ceph_lab', 'db', 'block', 1073741824, 'bound', 'rbd:nvme/img1')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Enable mirroring with a real peer id.
    let en = c
        .post(format!(
            "{base}/volumes/v1/mirror?mode=snapshot&peer={peer_id}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(en.status(), 202);
    let mirror_id = en.json::<Value>().await.unwrap()["resource"]["mirror_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mirror = |c: &reqwest::Client| {
        let base = base.clone();
        let c = c.clone();
        let mid = mirror_id.clone();
        async move {
            let list: Value = c
                .get(format!("{base}/dr/mirrors"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            list.as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == json!(mid))
                .cloned()
                .unwrap()
        }
    };
    let m = mirror(&c).await;
    assert_eq!(m["role"], "primary");
    assert_eq!(m["state"], "enabled");
    assert_eq!(m["pool"], "nvme");
    assert_eq!(m["image"], "img1");

    // Unknown peer on enable → 400.
    let bad = c
        .post(format!("{base}/volumes/v1/mirror?mode=snapshot&peer=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // Failover drill: demote → secondary, then promote → primary.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/demote"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(mirror(&c).await["role"], "secondary");
    let st: Value = c
        .get(format!("{base}/dr/mirrors/{mirror_id}/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["role"], "secondary");
    assert_eq!(st["promote_ready"], true);
    assert!(st["live"].is_null(), "fake mode has no live rbd status");
    // Demoting again is a conflict.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/demote"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );

    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/promote?force=yes"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!(
            "{base}/dr/mirrors/{mirror_id}/promote?force=0&peer_replayed=1"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        202
    );
    assert_eq!(mirror(&c).await["role"], "primary");
    let st: Value = c
        .get(format!("{base}/dr/mirrors/{mirror_id}/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["promote_ready"], false);
    assert_eq!(st["promote_blocker"], "this site's copy is primary");
    // Promoting an already-primary without force → 409.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/promote"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    // Force promote allowed for split-brain drills.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/promote?force=1"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );

    // Promoting an unknown mirror → 404.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/nope/promote"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Preflight + confirm-gated failover runbook.
    let pre: Value = c
        .get(format!("{base}/dr/preflight"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pre["ready"], true);
    assert_eq!(pre["peers"], 1);

    // Demote again so failover has a secondary to promote.
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{mirror_id}/demote"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    let fo = c
        .post(format!("{base}/dr/failover"))
        .json(&json!({ "mirror_id": mirror_id, "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(fo.status(), 202);
    assert_eq!(mirror(&c).await["role"], "primary");
    assert!(mirror(&c).await["last_failover_at"].as_str().is_some());

    // Unconfirmed failover rejected.
    assert_eq!(
        c.post(format!("{base}/dr/failover"))
            .json(&json!({ "mirror_id": mirror_id, "confirm": false }))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );

    // RPO stamp.
    let rpo = c
        .post(format!("{base}/dr/mirrors/{mirror_id}/rpo"))
        .json(&json!({ "rpo_seconds": 45 }))
        .send()
        .await
        .unwrap();
    assert_eq!(rpo.status(), 200);
    assert_eq!(mirror(&c).await["rpo_seconds"], 45);

    // DR status summarizes the posture.
    let status: Value = c
        .get(format!("{base}/dr/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["peers"], 1);
    assert_eq!(status["mirrors"], 1);
    assert_eq!(status["primary"], 1);
    assert_eq!(status["verified"], false);
    assert_eq!(status["dataplane_verified"], false);
    assert_eq!(status["control_plane_ready"], true);

    // Preflight always reports dataplane unverified; warnings carry the honesty note.
    let pre2: Value = c
        .get(format!("{base}/dr/preflight"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pre2["dataplane_verified"], false);
    assert!(!pre2["warnings"].as_array().unwrap().is_empty());

    // Peer site: register the replicated copy as secondary, then promote it.
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, backend_native_id)
         VALUES ('v2', 't', 'bkd_ceph_lab', 'db2', 'block', 1073741824, 'bound', 'rbd:nvme/img2')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        c.post(format!("{base}/volumes/v2/mirror?role=replica"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let sec = c
        .post(format!(
            "{base}/volumes/v2/mirror?mode=snapshot&peer={peer_id}&role=secondary"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(sec.status(), 201);
    let sec_body: Value = sec.json().await.unwrap();
    assert_eq!(sec_body["role"], "secondary");
    let sec_id = sec_body["mirror_id"].as_str().unwrap().to_string();
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{sec_id}/demote"))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        c.post(format!("{base}/dr/failover"))
            .json(&json!({ "mirror_id": sec_id, "confirm": true }))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );

    // Resync discards the local copy, so it is only allowed on a secondary.
    let resync = |c: &reqwest::Client| {
        let url = format!("{base}/dr/mirrors/{sec_id}/resync");
        let c = c.clone();
        async move { c.post(url).send().await.unwrap().status() }
    };
    assert_eq!(resync(&c).await, 409);
    assert_eq!(
        c.post(format!("{base}/dr/mirrors/{sec_id}/demote"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(resync(&c).await, 202);
    assert_eq!(
        mirror_role(&c, &base, &sec_id).await,
        ("secondary".into(), "enabled".into())
    );
}

async fn mirror_role(c: &reqwest::Client, base: &str, id: &str) -> (String, String) {
    let list: Value = c
        .get(format!("{base}/dr/mirrors"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let m = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == json!(id))
        .unwrap();
    (
        m["role"].as_str().unwrap().into(),
        m["state"].as_str().unwrap().into(),
    )
}

async fn insert_rbd_volume(pool: &sqlx::AnyPool, id: &str, native: &str) {
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, backend_native_id)
         VALUES ($1, 't', 'bkd_ceph_lab', $1, 'block', 1073741824, 'bound', $2)",
    )
    .bind(id)
    .bind(native)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn pool_mode_mirroring_requires_journal() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();
    let peer = c
        .post(format!("{base}/dr/peers"))
        .json(&json!({ "name": "dc2" }))
        .send()
        .await
        .unwrap();
    assert_eq!(peer.status(), 201);
    insert_rbd_volume(&pool, "pv1", "rbd:nvme/pimg").await;

    let get: Value = c
        .get(format!("{base}/dr/pools/nvme/mirroring"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get["mode"], "image");

    let bad = c
        .put(format!("{base}/dr/pools/nvme/mirroring"))
        .json(&json!({ "mode": "everything" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let set = c
        .put(format!("{base}/dr/pools/nvme/mirroring"))
        .json(&json!({ "mode": "pool" }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 200);
    let get: Value = c
        .get(format!("{base}/dr/pools/nvme/mirroring"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(get["mode"], "pool");

    // Pool mode mirrors journaling images only.
    let snap = c
        .post(format!("{base}/volumes/pv1/mirror?mode=snapshot"))
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 400);
    let en = c
        .post(format!("{base}/volumes/pv1/mirror?mode=journal"))
        .send()
        .await
        .unwrap();
    assert_eq!(en.status(), 202);
    let body: Value = en.json().await.unwrap();
    assert_eq!(body["resource"]["pool_mode"], "pool");
    let job: Value = c
        .get(format!("{base}/jobs/{}", body["job_id"].as_str().unwrap()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(job["job_type"], "rbd.mirror");
    let mirrors: Value = c
        .get(format!("{base}/dr/mirrors"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mirrors[0]["mode"], "journal");
}

#[tokio::test]
async fn volume_group_snapshot_and_rollback() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();
    insert_rbd_volume(&pool, "g1", "rbd:nvme/db-data").await;
    insert_rbd_volume(&pool, "g2", "rbd:nvme/db-wal").await;
    insert_rbd_volume(&pool, "g3", "pvc:ns/claim").await;

    let bad = c
        .post(format!("{base}/volume-groups"))
        .json(&json!({ "name": "db/1", "volume_ids": ["g1"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    // A request cannot size the handler's work: more than 256 members is refused before any lookup.
    let ids: Vec<String> = (0..257).map(|i| format!("big{i}")).collect();
    let too_many = c
        .post(format!("{base}/volume-groups"))
        .json(&json!({ "name": "huge", "volume_ids": ids }))
        .send()
        .await
        .unwrap();
    assert_eq!(too_many.status(), 400);
    let not_rbd = c
        .post(format!("{base}/volume-groups"))
        .json(&json!({ "name": "db", "volume_ids": ["g1", "g3"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(not_rbd.status(), 400);

    let created = c
        .post(format!("{base}/volume-groups"))
        .json(&json!({ "name": "db", "volume_ids": ["g1", "g2"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let g: Value = created.json().await.unwrap();
    let gid = g["id"].as_str().unwrap().to_string();
    assert_eq!(g["pool"], "nvme");

    let again = c
        .post(format!("{base}/volume-groups"))
        .json(&json!({ "name": "other", "volume_ids": ["g2"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409, "an image can be in one group");

    let snap = c
        .post(format!("{base}/volume-groups/{gid}/snapshots"))
        .json(&json!({ "name": "cp1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 201);
    let dup = c
        .post(format!("{base}/volume-groups/{gid}/snapshots"))
        .json(&json!({ "name": "cp1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 409);

    let detail: Value = c
        .get(format!("{base}/volume-groups/{gid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["members"].as_array().unwrap().len(), 2);
    assert_eq!(detail["members"][0]["rbd"], "nvme/db-data");
    assert_eq!(detail["snapshots"][0]["name"], "cp1");

    let unconfirmed = c
        .post(format!("{base}/volume-groups/{gid}/snapshots/cp1/rollback"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(unconfirmed.status(), 400);
    let rb = c
        .post(format!("{base}/volume-groups/{gid}/snapshots/cp1/rollback"))
        .json(&json!({ "confirm": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(rb.status(), 202);
    let rb: Value = rb.json().await.unwrap();
    let job: Value = c
        .get(format!("{base}/jobs/{}", rb["job_id"].as_str().unwrap()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(job["job_type"], "rbd.group_rollback");

    let del = c
        .delete(format!("{base}/volume-groups/{gid}/snapshots/cp1"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let del = c
        .delete(format!("{base}/volume-groups/{gid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let list: Value = c
        .get(format!("{base}/volume-groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list.as_array().unwrap().is_empty());
}
