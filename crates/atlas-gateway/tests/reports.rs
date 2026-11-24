// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The read-only operations reports (`docs/REPORTS.md`) through the real router: seeded catalog
//! rows in, findings out, plus the validation and role gates. Fake driver, no infra.

use atlas_api_types::{StorageVolume, VolumeKind};
use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::{json, Value};

mod common;

const CEPH: &str = "bkd_ceph_lab";

async fn spawn(auth: bool) -> (String, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("reports").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "reports-test-secret-key-at-least-32-byte".into(),
        jwt_secret_previous: None,
        auth_required: auth,
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
        san_enable: false,
        san_server: None,
        san_targets: Vec::new(),
        san_driver_mode: atlas_common::config::DriverMode::Fake,
        cloudblock_enable: false,
        cloudblock_server: None,
        cloudblock_targets: Vec::new(),
        cloudblock_driver_mode: atlas_common::config::DriverMode::Fake,
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
    (format!("http://{addr}/api/atlas/v1"), pool)
}

async fn add_volume(pool: &sqlx::AnyPool, id: &str, tenant: &str, size: i64, state: &str) {
    let v = StorageVolume {
        id: id.into(),
        cluster_id: None,
        pool_id: None,
        name: format!("name-{id}"),
        kind: VolumeKind::Block,
        backend_native_id: Some(format!("native-{id}")),
        size_bytes: size,
        used_bytes: None,
        state: state.into(),
        health: Default::default(),
        kubernetes_namespace: None,
        pvc_name: None,
        storage_class_name: None,
    };
    atlas_inventory::upsert_volume(pool, CEPH, tenant, &v, None)
        .await
        .unwrap();
}

async fn add_snapshot(pool: &sqlx::AnyPool, id: &str, vol: &str, state: &str, age_days: i64) {
    atlas_inventory::snapshots::insert_snapshot(
        pool,
        id,
        "t",
        vol,
        &format!("n-{id}"),
        None,
        "crash",
        state,
    )
    .await
    .unwrap();
    let ts = atlas_inventory::now_rfc3339(chrono::Utc::now() - chrono::Duration::days(age_days));
    sqlx::query("UPDATE storage_snapshots SET created_at = $1 WHERE id = $2")
        .bind(ts)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn get(url: String) -> (u16, Value) {
    let r = reqwest::get(url).await.unwrap();
    let s = r.status().as_u16();
    (s, r.json().await.unwrap_or(Value::Null))
}

fn has(v: &Value, code: &str, subject: &str) -> bool {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == code && f["subject"] == subject)
}

#[tokio::test]
async fn snapshot_hygiene_endpoint() {
    let (base, pool) = spawn(false).await;
    let (st, empty) = get(format!("{base}/snapshots/hygiene")).await;
    assert_eq!(st, 200);
    assert_eq!(empty["summary"]["findings"], 0);

    add_volume(&pool, "v1", "acme", 1 << 30, "ready").await;
    add_snapshot(&pool, "s-old", "v1", "ready", 200).await;
    add_snapshot(&pool, "s-new", "v1", "ready", 1).await;
    add_snapshot(&pool, "s-bad", "v1", "failed", 1).await;
    add_snapshot(&pool, "s-lock", "v1", "ready", 300).await;
    let soon = chrono::Utc::now().timestamp() + 2 * 86_400;
    atlas_inventory::snapshots::set_locked_until(&pool, "s-lock", soon)
        .await
        .unwrap();

    let (st, r) = get(format!("{base}/snapshots/hygiene")).await;
    assert_eq!(st, 200);
    assert_eq!(r["can_execute"], false);
    assert!(has(&r, "OLD_SNAPSHOT", "s-old"));
    assert!(has(&r, "FAILED_SNAPSHOT", "s-bad"));
    assert!(has(&r, "LOCK_EXPIRING", "s-lock"));
    assert!(
        !has(&r, "OLD_SNAPSHOT", "s-lock"),
        "a locked snapshot is not reclaimable"
    );
    assert!(!r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["subject"] == "s-new"));
    assert_eq!(r["summary"]["actively_locked"], 1);
    assert_eq!(r["busiest_volumes"][0]["snapshots"], 4);

    // Thresholds are honoured and validated.
    let (_, r) = get(format!(
        "{base}/snapshots/hygiene?stale_days=1000&max_per_volume=3"
    ))
    .await;
    assert!(!has(&r, "OLD_SNAPSHOT", "s-old"));
    assert!(has(&r, "OVER_RETAINED", "v1"));
    for bad in [
        "stale_days=0",
        "max_per_volume=-1",
        "lock_warn_days=9999",
        "stuck_minutes=0",
    ] {
        assert_eq!(
            get(format!("{base}/snapshots/hygiene?{bad}")).await.0,
            400,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn quota_headroom_endpoint() {
    let (base, pool) = spawn(false).await;
    add_volume(&pool, "v1", "acme", 900, "ready").await;
    add_volume(&pool, "v2", "free", 5_000, "ready").await;
    let c = reqwest::Client::new();
    let put = c
        .put(format!("{base}/tenants/acme/quota"))
        .json(&json!({ "max_bytes": 1000, "max_volumes": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);

    let (st, r) = get(format!("{base}/tenants/quota-headroom")).await;
    assert_eq!(st, 200);
    let t = r["tenants"].as_array().unwrap();
    assert_eq!(t[0]["tenant_id"], "acme");
    assert_eq!(t[0]["status"], "warning");
    assert_eq!(t[0]["bytes"]["headroom"], 100);
    let free = t.iter().find(|x| x["tenant_id"] == "free").unwrap();
    assert_eq!(free["status"], "unlimited");

    let (_, r) = get(format!(
        "{base}/tenants/quota-headroom?warn_percent=50&critical_percent=85"
    ))
    .await;
    assert_eq!(r["tenants"][0]["status"], "critical");
    for bad in [
        "warn_percent=95&critical_percent=90",
        "critical_percent=101",
        "warn_percent=0",
        "warn_percent=NaN",
    ] {
        assert_eq!(
            get(format!("{base}/tenants/quota-headroom?{bad}")).await.0,
            400,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn schedule_coverage_endpoint() {
    let (base, pool) = spawn(false).await;
    add_volume(&pool, "v-prot", "t", 1, "ready").await;
    add_volume(&pool, "v-bare", "t", 1, "ready").await;
    add_volume(&pool, "v-off", "t", 1, "ready").await;
    atlas_inventory::schedules::insert(
        &pool, "sch-a", "t", "v-prot", "snapshot", None, "manifest", 3600, 1,
    )
    .await
    .unwrap();
    atlas_inventory::schedules::insert(
        &pool, "sch-b", "t", "v-off", "snapshot", None, "manifest", 3600, 7,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE snapshot_schedules SET enabled = 0 WHERE id = $1")
        .bind("sch-b")
        .execute(&pool)
        .await
        .unwrap();
    // A schedule whose next run is hours in the past (scheduler not ticking).
    sqlx::query("UPDATE snapshot_schedules SET next_run_at = $1 WHERE id = $2")
        .bind(atlas_inventory::now_rfc3339(
            chrono::Utc::now() - chrono::Duration::hours(5),
        ))
        .bind("sch-a")
        .execute(&pool)
        .await
        .unwrap();

    let (st, r) = get(format!("{base}/schedules/coverage")).await;
    assert_eq!(st, 200);
    assert!(has(&r, "UNSCHEDULED_VOLUME", "v-bare"));
    assert!(
        has(&r, "UNSCHEDULED_VOLUME", "v-off"),
        "a disabled schedule is no protection"
    );
    assert!(has(&r, "SCHEDULE_DISABLED", "sch-b"));
    assert!(has(&r, "STALLED_SCHEDULE", "sch-a"));
    assert!(has(&r, "LOW_RETENTION", "sch-a"));
    assert!(!has(&r, "UNSCHEDULED_VOLUME", "v-prot"));
    assert_eq!(r["summary"]["scheduled"], 1);
    assert_eq!(r["summary"]["unscheduled"], 2);

    let (_, r) = get(format!("{base}/schedules/coverage?min_keep=1")).await;
    assert!(!has(&r, "LOW_RETENTION", "sch-a"));
    assert_eq!(
        get(format!("{base}/schedules/coverage?min_keep=0")).await.0,
        400
    );
    assert_eq!(
        get(format!("{base}/schedules/coverage?overdue_intervals=0"))
            .await
            .0,
        400
    );
}

#[tokio::test]
async fn ceph_health_explain_endpoint() {
    let (base, _pool) = spawn(false).await;
    let (st, all) = get(format!("{base}/ceph/health-explain")).await;
    assert_eq!(st, 200);
    assert!(all["checks"].as_array().unwrap().len() >= 20);

    let (_, one) = get(format!("{base}/ceph/health-explain?codes=osd_down,MADE_UP")).await;
    assert_eq!(one["checks"][0]["code"], "OSD_DOWN");
    assert_eq!(one["unknown"], json!(["MADE_UP"]));
    assert_eq!(
        get(format!("{base}/ceph/health-explain?codes=bad%20code"))
            .await
            .0,
        400
    );

    // `ceph health detail -f json` shape.
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{base}/ceph/health-explain"))
        .json(&json!({ "status": "HEALTH_WARN",
                       "checks": { "SLOW_OPS": { "severity": "HEALTH_WARN" }, "PG_DAMAGED": { "severity": "HEALTH_ERR" } } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r: Value = r.json().await.unwrap();
    assert_eq!(r["checks"][0]["code"], "PG_DAMAGED", "data risk first");
    assert_eq!(r["checks"][1]["code"], "SLOW_OPS");
    for body in [json!({}), json!({ "codes": [] }), json!({ "codes": [1] })] {
        let r = c
            .post(format!("{base}/ceph/health-explain"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{body}");
    }
}

/// Auth on: no token is 401 and a viewer is 403 on every report; an operator gets 200.
#[tokio::test]
async fn reports_need_the_operator_role() {
    let (base, _pool) = spawn(true).await;
    let c = reqwest::Client::new();
    let paths = [
        "/snapshots/hygiene",
        "/tenants/quota-headroom",
        "/schedules/coverage",
        "/ceph/health-explain",
    ];
    for p in paths {
        assert_eq!(
            c.get(format!("{base}{p}")).send().await.unwrap().status(),
            401,
            "{p}"
        );
    }
    let login = |u: &str, pw: &str| {
        let c = c.clone();
        let url = format!("{base}/auth/login");
        let body = json!({ "username": u, "password": pw });
        async move {
            c.post(url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let admin = login("admin", "Admin@321").await;
    let admin_jwt = admin["token"].as_str().unwrap().to_string();
    for (user, role) in [("viewer1", "viewer"), ("oper1", "operator")] {
        let r = c
            .post(format!("{base}/auth/users"))
            .bearer_auth(&admin_jwt)
            .json(&json!({ "username": user, "password": "SomePass123", "role": role }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "create {user}: {}", r.status());
    }
    let viewer = login("viewer1", "SomePass123").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let oper = login("oper1", "SomePass123").await["token"]
        .as_str()
        .unwrap()
        .to_string();
    for p in paths {
        let v = c
            .get(format!("{base}{p}"))
            .bearer_auth(&viewer)
            .send()
            .await
            .unwrap();
        assert_eq!(v.status(), 403, "viewer {p}");
        let o = c
            .get(format!("{base}{p}"))
            .bearer_auth(&oper)
            .send()
            .await
            .unwrap();
        assert_eq!(o.status(), 200, "operator {p}");
    }
    let post = c
        .post(format!("{base}/ceph/health-explain"))
        .bearer_auth(&viewer)
        .json(&json!({ "codes": ["OSD_DOWN"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 403);
}
