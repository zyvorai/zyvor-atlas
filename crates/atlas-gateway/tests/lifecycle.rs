// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 volume lifecycle: orphan GC surfaces backups whose source volume is gone, and per-image
//! QoS enqueues a throttle job. Fake driver — the QoS job fails without a real `rbd`, but the
//! enqueue contract + validation are exercised; orphan detection is pure-DB and fully verified.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let db = format!(
        "{}/atlas-lifecycle-{}-{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
    );
    let _ = std::fs::remove_file(&db);
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url: format!("sqlite://{db}?mode=rwc"),
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "lifecycle-test-secret-at-least-32-bytes".into(),
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

/// A backup whose source volume no longer exists is reported as an orphan; a live-volume backup isn't.
#[tokio::test]
async fn orphan_backups_are_reported() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt1",
        "t",
        "b",
        "bkd_ceph_lab",
        "ns",
        "obc",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    // A live volume + its backup (not an orphan).
    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state)
         VALUES ('live_vol', 't', 'bkd_ceph_lab', 'live', 'block', 1073741824, 'bound')",
    )
    .execute(&pool)
    .await
    .unwrap();
    atlas_inventory::backups::insert_backup(
        &pool,
        "bk_live",
        "t",
        "live_vol",
        None,
        "bkt1",
        "k/live",
        "manifest-v1",
        &json!({}),
    )
    .await
    .unwrap();
    // A backup whose source volume is gone (orphan).
    atlas_inventory::backups::insert_backup(
        &pool,
        "bk_orphan",
        "t",
        "gone_vol",
        None,
        "bkt1",
        "k/orphan",
        "manifest-v1",
        &json!({}),
    )
    .await
    .unwrap();

    let orphans: Value = c
        .get(format!("{base}/maintenance/orphans"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        orphans["count"], 1,
        "exactly one orphan expected: {orphans}"
    );
    let ids: Vec<&str> = orphans["orphan_backups"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["bk_orphan"]);
}

/// Per-image QoS enqueues a throttle job and validates its inputs.
#[tokio::test]
async fn qos_enqueues_and_validates() {
    let base = format!("http://{}/api/atlas/v1", spawn().await.0);
    let c = reqwest::Client::new();

    // Valid: an IOPS cap → 202 with the limits echoed.
    let ok = c
        .post(format!("{base}/rbd-images/nvme/img1/qos?iops=1000"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 202);
    let body: Value = ok.json().await.unwrap();
    assert_eq!(body["resource"]["iops_limit"], 1000);
    assert!(body["job_id"].is_string());

    // No limits → 400; negative → 400.
    assert_eq!(
        c.post(format!("{base}/rbd-images/nvme/img1/qos"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/rbd-images/nvme/img1/qos?bps=-1"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

/// `GET /osds/ok-to-stop` answers from the Ceph driver and validates its input.
#[tokio::test]
async fn osd_ok_to_stop_answers_and_validates() {
    let base = format!("http://{}/api/atlas/v1", spawn().await.0);
    let c = reqwest::Client::new();
    let get = |q: &str| c.get(format!("{base}/osds/ok-to-stop{q}")).send();

    // The fixture cluster already has osd.3 down on node02: stopping osd.2 adds no new failure
    // domain, stopping anything on another host does.
    let ok: Value = get("?ids=2").await.unwrap().json().await.unwrap();
    assert_eq!(ok["ok_to_stop"], true, "{ok}");
    assert_eq!(ok["osd_ids"], json!([2]));
    assert_eq!(ok["detail"]["num_not_ok_pgs"], 0);

    let no: Value = get("?ids=0,1").await.unwrap().json().await.unwrap();
    assert_eq!(no["ok_to_stop"], false, "{no}");
    assert!(no["detail"]["num_not_ok_pgs"].as_i64().unwrap() > 0);

    // Unknown OSD -> 404; bad or missing ids -> 400.
    assert_eq!(get("?ids=99").await.unwrap().status(), 404);
    for bad in ["", "?ids=", "?ids=x", "?ids=-1"] {
        assert_eq!(get(bad).await.unwrap().status(), 400, "{bad}");
    }
}

/// `GET /ceph/devices` lists the drives and flags the ones predicted to fail soon.
#[tokio::test]
async fn ceph_devices_flag_drives_predicted_to_fail() {
    let base = format!("http://{}/api/atlas/v1", spawn().await.0);
    let c = reqwest::Client::new();
    let body: Value = c
        .get(format!("{base}/ceph/devices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["devices"].as_array().unwrap().len(), 6, "{body}");
    let s = &body["summary"];
    assert_eq!(s["total"], 6);
    assert_eq!(s["with_prediction"], 2);
    assert_eq!(s["without_prediction"], 4);
    // Only the drive whose predicted window has passed is at risk; the one dated 2099 is not.
    let at_risk = s["at_risk"].as_array().unwrap();
    assert_eq!(at_risk.len(), 1, "{s}");
    assert_eq!(at_risk[0]["devid"], "SAMSUNG_MZQL23T8HCLS_S64E0002");
    assert_eq!(at_risk[0]["already_due"], true);
    assert_eq!(at_risk[0]["daemons"][0], "osd.3");
}

/// Resize-down is opt-in (allow_shrink), and pool migration validates its destination.
#[tokio::test]
async fn resize_down_and_migrate() {
    let base = format!("http://{}/api/atlas/v1", spawn().await.0);
    let c = reqwest::Client::new();

    // Omitting allow_shrink defaults to false — the opt-in itself must default closed.
    let default_shrink = c
        .post(format!("{base}/rbd-images/nvme/img1/resize"))
        .json(&json!({ "size_bytes": 1073741824i64 }))
        .send()
        .await
        .unwrap();
    assert_eq!(default_shrink.status(), 202);
    assert_eq!(
        default_shrink.json::<Value>().await.unwrap()["resource"]["allow_shrink"],
        false,
        "allow_shrink must default to false when omitted"
    );

    // Shrink with allow_shrink → 202, echoed.
    let shrink = c
        .post(format!("{base}/rbd-images/nvme/img1/resize"))
        .json(&json!({ "size_bytes": 1073741824i64, "allow_shrink": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(shrink.status(), 202);
    assert_eq!(
        shrink.json::<Value>().await.unwrap()["resource"]["allow_shrink"],
        true
    );

    // Migrate to another pool → 202; same pool or missing dest → 400.
    assert_eq!(
        c.post(format!("{base}/rbd-images/nvme/img1/migrate?dest_pool=hdd"))
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(
        c.post(format!(
            "{base}/rbd-images/nvme/img1/migrate?dest_pool=nvme"
        ))
        .send()
        .await
        .unwrap()
        .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/rbd-images/nvme/img1/migrate"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

/// `/volumes/{id}/schedule` + `/schedules` + `/schedules/{id}`: create, list, and delete a
/// protection schedule end-to-end. A PVC-backed volume is required (see the comment on
/// `create_schedule` — a raw non-PVC volume would create a schedule whose timer advances forever
/// with no job ever enqueued), so the fixture volume sets `pvc_name`.
#[tokio::test]
async fn schedule_create_list_delete() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}/api/atlas/v1");
    let c = reqwest::Client::new();

    sqlx::query(
        "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, pvc_name)
         VALUES ('vol_sched', 't', 'bkd_ceph_lab', 'sched-src', 'block', 1073741824, 'bound', 'pvc-sched-src')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Create: valid body → 201, echoing the volume/interval/kind.
    let created = c
        .post(format!("{base}/volumes/vol_sched/schedule"))
        .json(&json!({ "interval_secs": 3600, "keep": 3 }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let sched: Value = created.json().await.unwrap();
    assert_eq!(sched["volume_id"], "vol_sched");
    assert_eq!(sched["kind"], "snapshot");
    let sched_id = sched["id"].as_str().unwrap().to_string();

    // Validation: interval_secs <= 0 → 400; unknown kind → 400; unknown volume → 404.
    assert_eq!(
        c.post(format!("{base}/volumes/vol_sched/schedule"))
            .json(&json!({ "interval_secs": 0 }))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/volumes/vol_sched/schedule"))
            .json(&json!({ "interval_secs": 3600, "kind": "bogus" }))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        c.post(format!("{base}/volumes/does-not-exist/schedule"))
            .json(&json!({ "interval_secs": 3600 }))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // List: filtered by volume_id, the created schedule comes back.
    let listed: Value = c
        .get(format!("{base}/schedules?volume_id=vol_sched"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = listed.as_array().unwrap();
    assert!(items.iter().any(|s| s["id"] == sched_id));

    // Delete: known id → 200; repeat delete of the same id → 404 (already gone).
    assert_eq!(
        c.delete(format!("{base}/schedules/{sched_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        c.delete(format!("{base}/schedules/{sched_id}"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}
