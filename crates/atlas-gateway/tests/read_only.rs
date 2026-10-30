// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests for the read-only gateway surface, driven by the FakeCephDriver (no Ceph, no
//! Kubernetes). Verifies discovery populates inventory and that audit rows are written.

use std::net::SocketAddr;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};

mod common;

/// Spin up the gateway on an ephemeral port with the fake driver and a throwaway database.
async fn spawn() -> (SocketAddr, sqlx::AnyPool) {
    let database_url = common::fresh_database_url("read-only").await;

    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: "test-secret".into(),
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

/// Minted service-account tokens enforce RBAC end-to-end: an admin issues scoped tokens; an
/// operator token can create a volume, a viewer token cannot, and no token is rejected.
#[tokio::test]
async fn issued_tokens_enforce_rbac() {
    let secret = "auth-issue-test-secret-key";
    let base = format!("{}/api/atlas/v1", spawn_auth(secret).await);
    let c = client();

    // Bootstrap admin token (as the platform operator would mint offline with the shared secret).
    let (admin, _, _) =
        atlas_gateway::auth::mint_token(secret, "bootstrap", "admin", "global", 3600).unwrap();

    // Admin issues an operator token for a product service account.
    let issue = |role: &str| {
        c.post(format!("{base}/auth/tokens"))
            .bearer_auth(&admin)
            .json(&serde_json::json!({ "subject": "veyron", "role": role, "ttl_secs": 600 }))
            .send()
    };
    let op_resp = issue("operator").await.unwrap();
    assert_eq!(op_resp.status(), 201);
    let op_body: serde_json::Value = op_resp.json().await.unwrap();
    let op_token = op_body["token"].as_str().unwrap().to_string();
    assert_eq!(op_body["level"], 1);

    let vw_resp = issue("viewer").await.unwrap();
    let vw_token = vw_resp.json::<serde_json::Value>().await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // Tenant must match the operator token's own tenant ("global", since `issue("operator")`
    // above doesn't specify one — see `issue_token`'s default) now that `POST /volumes` rejects
    // a tenant-scoped actor creating a volume for a tenant other than its own (see volumes.rs).
    // This test is about role enforcement, not tenant enforcement (that's tenant_isolation.rs).
    let create = serde_json::json!({
        "tenant_id": "global", "name": "veyron-disk", "size_bytes": 1_073_741_824i64,
        "policy": "database", "kubernetes": { "namespace": "default" }
    });

    // Operator token → volume create accepted.
    let ok = c
        .post(format!("{base}/volumes"))
        .bearer_auth(&op_token)
        .json(&create)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 202, "operator should create volumes");

    // Viewer token → forbidden.
    let forbidden = c
        .post(format!("{base}/volumes"))
        .bearer_auth(&vw_token)
        .json(&create)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403, "viewer must not create volumes");

    // No token → unauthorized.
    let anon = c
        .post(format!("{base}/volumes"))
        .json(&create)
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401, "missing token is unauthorized");

    // A non-admin cannot mint tokens.
    let denied = c
        .post(format!("{base}/auth/tokens"))
        .bearer_auth(&op_token)
        .json(&serde_json::json!({ "subject": "x", "role": "admin" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403, "operator must not mint tokens");
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Insert a minimal source volume so snapshots can FK-reference it.
async fn seed_volume(pool: &sqlx::AnyPool, id: &str, name: &str) {
    let v = atlas_api_types::StorageVolume {
        id: id.into(),
        cluster_id: None,
        pool_id: None,
        name: name.into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: None,
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some(name.into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(pool, "bkd_ceph_lab", "t1", &v, None)
        .await
        .unwrap();
}

/// Spawn a gateway with auth enabled and the given JWT secret; returns its base URL.
async fn spawn_auth(secret: &str) -> String {
    let database_url = common::fresh_database_url("read-only-rbac").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: secret.into(),
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
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .unwrap();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn mint(secret: &str, role: &str) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
        + 3600;
    let claims = atlas_gateway::auth::Claims {
        sub: "u".into(),
        role: role.into(),
        exp,
        jti: String::new(),
        tenant_id: "global".into(),
        tools: Vec::new(),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn rbac_enforced_on_writes() {
    let secret = "rbac-test-secret";
    let base = spawn_auth(secret).await;
    // Tenant must match `mint()`'s hardcoded "global" tenant now that `POST /volumes` rejects a
    // tenant-scoped actor creating a volume for a different tenant (see volumes.rs). This test is
    // about role enforcement, not tenant enforcement (that's tenant_isolation.rs).
    let body = serde_json::json!({
        "tenant_id": "global", "name": "rbac-vol", "size_bytes": 1073741824_i64,
        "kind": "block", "policy": "database"
    });

    // No token → 401.
    let r = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Viewer token → 403 (read role can't create).
    let r = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .bearer_auth(mint(secret, "viewer"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);

    // Viewer CAN read.
    let r = client()
        .get(format!("{base}/api/atlas/v1/pools"))
        .bearer_auth(mint(secret, "viewer"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);

    // Operator token → 202.
    let r = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .bearer_auth(mint(secret, "storage.operator"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);

    // Operator CANNOT create a backend (admin-only) → 403.
    let r = client()
        .post(format!("{base}/api/atlas/v1/backends"))
        .bearer_auth(mint(secret, "storage.operator"))
        .json(&serde_json::json!({ "name": "b" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);

    // Admin CAN create a backend → 200.
    let r = client()
        .post(format!("{base}/api/atlas/v1/backends"))
        .bearer_auth(mint(secret, "storage.admin"))
        .json(&serde_json::json!({ "name": "b" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn health_and_version() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");

    let v: serde_json::Value = client()
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "ok");

    let v: serde_json::Value = client()
        .get(format!("{base}/version"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["name"], "atlas-gateway");
}

#[tokio::test]
async fn discovery_populates_inventory_and_audit() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // The Ceph backend is registered at startup.
    let backends: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/backends"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(backends.as_array().unwrap().len(), 1);
    assert_eq!(backends[0]["id"], "bkd_ceph_lab");

    // Trigger discovery.
    let resp = client()
        .post(format!(
            "{base}/api/atlas/v1/backends/bkd_ceph_lab/discover"
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["summary"]["pools"], 3);
    assert_eq!(body["summary"]["volumes"], 2);

    // Inventory is now populated.
    let pools: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/pools"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pools.as_array().unwrap().len(), 3);

    let volumes: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/volumes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(volumes.as_array().unwrap().len(), 2);

    let clusters: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/clusters"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clusters.as_array().unwrap().len(), 1);
    let cluster_id = clusters[0]["id"].as_str().unwrap();

    // Cluster health + capabilities resolve. FakeCephDriver's fixture simulates one down OSD
    // (osd.3) consistently across cluster()/osds()/ceph_status(), so this reads "warn" — matching
    // what GET /ceph/status reports for the same simulated cluster.
    let health: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/clusters/{cluster_id}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "warn");

    let caps: serde_json::Value = client()
        .get(format!(
            "{base}/api/atlas/v1/clusters/{cluster_id}/capabilities"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(caps["block"], true);
    assert_eq!(caps["object"], true);

    // Metrics summary aggregates capacity.
    let metrics: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/metrics/summary"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(metrics["clusters"], 1);
    assert_eq!(metrics["pools"], 3);

    // An audit row was written for the discovery request.
    let audits = atlas_inventory::audit::count_for_action(&pool, "backend.discover.requested")
        .await
        .unwrap();
    assert!(audits >= 1, "expected discovery audit row, got {audits}");
}

#[tokio::test]
async fn monitor_raises_and_resolves_alerts() {
    // The fake driver reports HEALTH_OK, so a plain discovery yields no alerts. We instead seed a
    // WARN cluster + a near-full pool + a down OSD and evaluate the rules directly.
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    let backend = atlas_api_types::StorageBackend {
        id: "bkd_ceph_lab".into(),
        name: "b".into(),
        backend_type: atlas_api_types::BackendType::Ceph,
        mode: atlas_api_types::BackendMode::ManagedRook,
        status: "active".into(),
        capabilities: Default::default(),
        connection_ref: None,
        cordoned: false,
    };
    atlas_inventory::upsert_backend(&pool, &backend)
        .await
        .unwrap();
    let discovery = atlas_api_types::DiscoveryResult {
        cluster: atlas_api_types::StorageCluster {
            id: "cls_1".into(),
            backend_id: "bkd_ceph_lab".into(),
            name: "lab".into(),
            native_fsid: None,
            health: atlas_api_types::Health::Warn,
            raw_capacity_bytes: Some(100),
            used_capacity_bytes: Some(90),
            available_capacity_bytes: Some(10),
        },
        pools: vec![atlas_api_types::StoragePool {
            id: "pool_1".into(),
            cluster_id: "cls_1".into(),
            name: "hot".into(),
            kind: "rbd".into(),
            device_class: None,
            replica_size: Some(1),
            used_bytes: Some(90),
            max_bytes: Some(100),
            health: atlas_api_types::Health::Warn,
        }],
        osds: vec![atlas_api_types::Osd {
            id: 7,
            cluster_id: "cls_1".into(),
            up: false,
            in_cluster: true,
            device_class: None,
            host: Some("n1".into()),
            used_bytes: None,
            capacity_bytes: None,
        }],
        volumes: vec![],
        health: atlas_api_types::StorageHealth {
            status: atlas_api_types::Health::Warn,
            summary: "WARN".into(),
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        },
    };
    atlas_inventory::upsert_discovery(&pool, "bkd_ceph_lab", &discovery, true)
        .await
        .unwrap();

    // Evaluate → expect 3 open alerts (cluster warn, pool 90% full, osd down).
    let ev: serde_json::Value = client()
        .post(format!("{base}/api/atlas/v1/alerts/evaluate"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ev["open_alerts"], 3);

    let open: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/alerts?state=open"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = open.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert!(arr.iter().any(|a| a["title"] == "OSD down"));
    assert!(arr.iter().any(|a| a["title"] == "Pool near full"));

    // Heal the cluster/pool/osd → re-evaluate → alerts resolved.
    let healthy = atlas_api_types::DiscoveryResult {
        cluster: atlas_api_types::StorageCluster {
            health: atlas_api_types::Health::Ok,
            ..discovery.cluster.clone()
        },
        pools: vec![atlas_api_types::StoragePool {
            used_bytes: Some(10),
            ..discovery.pools[0].clone()
        }],
        osds: vec![atlas_api_types::Osd {
            up: true,
            ..discovery.osds[0].clone()
        }],
        volumes: vec![],
        health: discovery.health.clone(),
    };
    atlas_inventory::upsert_discovery(&pool, "bkd_ceph_lab", &healthy, true)
        .await
        .unwrap();
    atlas_monitor::evaluate(&pool).await.unwrap();
    let still_open = atlas_inventory::alerts::count_open(&pool).await.unwrap();
    assert_eq!(still_open, 0, "alerts should resolve when conditions clear");
}

#[tokio::test]
async fn missing_volume_returns_404() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let resp = client()
        .get(format!("{base}/api/atlas/v1/volumes/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn storage_classes_without_cluster_returns_502() {
    // enable_k8s=false → no live driver → /storage-classes should 502, not panic.
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let resp = client()
        .get(format!("{base}/api/atlas/v1/storage-classes"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn kubernetes_pvcs_without_cluster_returns_502() {
    // Same "no live driver" contract as /storage-classes: degrade to 502, don't panic.
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let resp = client()
        .get(format!("{base}/api/atlas/v1/kubernetes/pvcs"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn kubernetes_pvs_without_cluster_returns_502() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let resp = client()
        .get(format!("{base}/api/atlas/v1/kubernetes/pvs"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn policies_are_listed() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let policies: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/policies"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = policies.as_array().unwrap();
    assert!(arr.iter().any(|p| p["intent"] == "database"));
    assert!(arr.iter().any(|p| p["storage_class"] == "zyvor-rbd-prod"));
}

#[tokio::test]
async fn create_volume_enqueues_job_and_reaches_terminal_state() {
    // No k8s driver in tests → the create job runs and terminates as `failed` (can't provision),
    // which still exercises the full async engine lifecycle: 202 → job → terminal state.
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");

    let body = serde_json::json!({
        "tenant_id": "tenant_acme",
        "name": "billing-db-root",
        "size_bytes": 2147483648_i64,
        "kind": "block",
        "policy": "database"
    });
    let resp = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: serde_json::Value = resp.json().await.unwrap();
    let job_id = accepted["job_id"].as_str().unwrap().to_string();
    assert_eq!(accepted["resource"]["storage_class"], "zyvor-rbd-prod");

    // Poll the job to a terminal state.
    let mut state = String::new();
    for _ in 0..50 {
        let job: serde_json::Value = client()
            .get(format!("{base}/api/atlas/v1/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        state = job["state"].as_str().unwrap_or_default().to_string();
        if state == "succeeded" || state == "failed" {
            // Without a cluster the driver reports it can't run the write path.
            if state == "failed" {
                assert!(job["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Kubernetes"));
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        state, "failed",
        "job should terminate failed without a cluster"
    );

    // The job also shows up in the list.
    let jobs: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(jobs.as_array().unwrap().iter().any(|j| j["id"] == job_id));
}

#[tokio::test]
async fn sse_job_watch_streams_to_terminal() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    // Create a job (fails without a cluster) and watch it via SSE.
    let created: serde_json::Value = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .json(&serde_json::json!({
            "tenant_id": "t", "name": "sse-vol", "size_bytes": 1073741824_i64, "kind": "block"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let job_id = created["job_id"].as_str().unwrap();

    let resp = client()
        .get(format!("{base}/api/atlas/v1/jobs/{job_id}/watch"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    // The stream closes once the job is terminal; the body carries the SSE frames.
    let body = resp.text().await.unwrap();
    assert!(body.contains("event:job") || body.contains("event: job"));
    assert!(
        body.contains("failed"),
        "expected terminal 'failed' in SSE body: {body}"
    );
}

#[tokio::test]
async fn create_volume_is_idempotent() {
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let body = serde_json::json!({
        "tenant_id": "t1", "name": "idem-vol", "size_bytes": 1073741824_i64, "kind": "block"
    });
    let j1: serde_json::Value = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let j2: serde_json::Value = client()
        .post(format!("{base}/api/atlas/v1/volumes"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        j1["job_id"], j2["job_id"],
        "same request → same job (idempotency)"
    );
}

#[tokio::test]
async fn expand_rejects_smaller_size() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");
    // Unknown volume → 404 (validated before enqueue).
    let resp = client()
        .post(format!("{base}/api/atlas/v1/volumes/nope/expand"))
        .json(&serde_json::json!({ "new_size_bytes": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // Seed a real volume (size_bytes = 1073741824 per seed_volume) so the size check itself,
    // not just the existence check, gets exercised.
    seed_volume(&pool, "vol_expand", "expand-vol").await;
    let expand = |new_size_bytes: i64| {
        let base = base.clone();
        async move {
            client()
                .post(format!("{base}/api/atlas/v1/volumes/vol_expand/expand"))
                .json(&serde_json::json!({ "new_size_bytes": new_size_bytes }))
                .send()
                .await
                .unwrap()
        }
    };

    // Strictly smaller → 400.
    let smaller = expand(1).await;
    assert_eq!(
        smaller.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "expand must reject a size smaller than the current size"
    );

    // Exactly the current size → also 400 (expand must grow, not no-op).
    let same = expand(1073741824).await;
    assert_eq!(
        same.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "expand must reject a size equal to the current size"
    );

    // Larger → 202 (accepted; the job itself fails later without a live k8s driver, which is out
    // of scope for this validation test).
    let larger = expand(2147483648).await;
    assert_eq!(
        larger.status(),
        reqwest::StatusCode::ACCEPTED,
        "expand must accept a strictly larger size"
    );
}

#[tokio::test]
async fn clone_requires_name_and_enqueues() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // Seed a source volume + snapshot (snapshots FK-reference their volume).
    seed_volume(&pool, "vol_src", "src-vol").await;
    atlas_inventory::snapshots::insert_snapshot(
        &pool,
        "snap_x",
        "t1",
        "vol_src",
        "snap-x-obj",
        None,
        "crash",
        "ready",
    )
    .await
    .unwrap();

    // Missing name → 400.
    let bad = client()
        .post(format!("{base}/api/atlas/v1/snapshots/snap_x/clone"))
        .json(&serde_json::json!({ "size_bytes": 1073741824_i64 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // With a name + size → 202 (job runs, fails without k8s, but the contract holds).
    let ok = client()
        .post(format!("{base}/api/atlas/v1/snapshots/snap_x/clone"))
        .json(&serde_json::json!({ "name": "clone-1", "size_bytes": 1073741824_i64 }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(body["resource"]["mode"], "clone");
    assert_eq!(body["resource"]["from_snapshot"], "snap_x");
}

#[tokio::test]
async fn bucket_create_enqueues_and_lists() {
    // No k8s driver in tests → the create job runs and terminates as `failed` (can't provision).
    // The inventory row is now only written once k8s has accepted the OBC create (dispatch-side,
    // not the HTTP handler) — a failed job must leave no row at all, matching `volume.create`'s
    // behavior, instead of a permanent orphan with `bucket_name: null`.
    // Explicit backend_id: RustFS is now the default backend when omitted, and isn't enabled in
    // this test's Config — this test specifically exercises the Ceph/Rook OBC path.
    let (addr, _pool) = spawn().await;
    let base = format!("http://{addr}");
    let resp = client()
        .post(format!("{base}/api/atlas/v1/buckets"))
        .json(&serde_json::json!({ "name": "backups-acme", "backend_id": "bkd_ceph_lab" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    let accepted: serde_json::Value = resp.json().await.unwrap();
    let job_id = accepted["job_id"].as_str().unwrap().to_string();

    let mut state = String::new();
    for _ in 0..50 {
        let job: serde_json::Value = client()
            .get(format!("{base}/api/atlas/v1/jobs/{job_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        state = job["state"].as_str().unwrap_or_default().to_string();
        if state == "succeeded" || state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(state, "failed");

    let buckets: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/buckets"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!buckets
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["name"] == "backups-acme"));
}

#[tokio::test]
async fn backup_requires_known_volume_and_bound_bucket() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // Unknown volume → 404.
    let r404 = client()
        .post(format!("{base}/api/atlas/v1/backup-jobs"))
        .json(&serde_json::json!({ "volume_id": "nope", "bucket_id": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r404.status(), reqwest::StatusCode::NOT_FOUND);

    // Seed a volume + a *pending* bucket → backup rejected (400) until the bucket binds.
    seed_volume(&pool, "vol_b", "vol-b").await;
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_1",
        "t1",
        "b1",
        "bkd_ceph_lab",
        "rook-ceph",
        "b1",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    let r400 = client()
        .post(format!("{base}/api/atlas/v1/backup-jobs"))
        .json(&serde_json::json!({ "volume_id": "vol_b", "bucket_id": "bkt_1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r400.status(), reqwest::StatusCode::BAD_REQUEST);

    // Bind the bucket → backup accepted (202).
    atlas_inventory::buckets::set_bound(
        &pool,
        "bkt_1",
        "b1-abc",
        "http://rgw.rook-ceph:80",
        "us-east-1",
        "b1",
    )
    .await
    .unwrap();
    let r202 = client()
        .post(format!("{base}/api/atlas/v1/backup-jobs"))
        .json(&serde_json::json!({ "volume_id": "vol_b", "bucket_id": "bkt_1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r202.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = r202.json().await.unwrap();
    assert!(body["resource"]["object_key"]
        .as_str()
        .unwrap()
        .starts_with("backups/vol_b/"));
}

#[tokio::test]
async fn restore_from_backup_enqueues() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // Unknown backup → 404.
    let r404 = client()
        .post(format!("{base}/api/atlas/v1/restore-jobs"))
        .json(&serde_json::json!({ "backup_id": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r404.status(), reqwest::StatusCode::NOT_FOUND);

    // Seed volume → snapshot → bound bucket → backup, then restore → 202.
    seed_volume(&pool, "vol_r", "vol-r").await;
    atlas_inventory::snapshots::insert_snapshot(
        &pool,
        "snap_r",
        "t1",
        "vol_r",
        "snap-r-obj",
        None,
        "app",
        "ready",
    )
    .await
    .unwrap();
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_r",
        "t1",
        "b",
        "bkd_ceph_lab",
        "rook-ceph",
        "b",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    atlas_inventory::buckets::set_bound(&pool, "bkt_r", "b-1", "http://rgw:80", "us-east-1", "b")
        .await
        .unwrap();
    atlas_inventory::backups::insert_backup(
        &pool,
        "bkp_r",
        "t1",
        "vol_r",
        Some("snap_r"),
        "bkt_r",
        "backups/vol_r/bkp_r.manifest.json",
        "manifest-v1",
        &serde_json::json!({ "backup_id": "bkp_r" }),
    )
    .await
    .unwrap();

    let r202 = client()
        .post(format!("{base}/api/atlas/v1/restore-jobs"))
        .json(&serde_json::json!({ "backup_id": "bkp_r", "name": "restored-vol" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r202.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = r202.json().await.unwrap();
    assert_eq!(body["resource"]["from_backup"], "bkp_r");
    assert_eq!(body["resource"]["pvc"], "restored-vol");
}

#[tokio::test]
async fn backup_delete_enqueues() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // Unknown backup → 404.
    let r = client()
        .delete(format!("{base}/api/atlas/v1/backups/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

    // Seed volume + bound bucket + backup, then DELETE → 202.
    seed_volume(&pool, "vol_bd", "vol-bd").await;
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_bd",
        "t1",
        "b",
        "bkd_ceph_lab",
        "rook-ceph",
        "b",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    atlas_inventory::buckets::set_bound(&pool, "bkt_bd", "b-1", "http://rgw:80", "us-east-1", "b")
        .await
        .unwrap();
    atlas_inventory::backups::insert_backup(
        &pool,
        "bkp_bd",
        "t1",
        "vol_bd",
        None,
        "bkt_bd",
        "backups/vol_bd/bkp_bd.manifest.json",
        "manifest-v1",
        &serde_json::json!({}),
    )
    .await
    .unwrap();

    let r = client()
        .delete(format!("{base}/api/atlas/v1/backups/bkp_bd"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["resource"]["backup_id"], "bkp_bd");
}

#[tokio::test]
async fn bucket_delete_guarded_by_backups() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // Unknown bucket → 404.
    let r = client()
        .delete(format!("{base}/api/atlas/v1/buckets/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

    // Empty bucket → 202.
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_empty",
        "t1",
        "e",
        "bkd_ceph_lab",
        "rook-ceph",
        "e",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    let r = client()
        .delete(format!("{base}/api/atlas/v1/buckets/bkt_empty"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);

    // Bucket with a backup → 409, force → 202.
    seed_volume(&pool, "vol_bk", "vol-bk").await;
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_used",
        "t1",
        "u",
        "bkd_ceph_lab",
        "rook-ceph",
        "u",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    atlas_inventory::backups::insert_backup(
        &pool,
        "bkp_u",
        "t1",
        "vol_bk",
        None,
        "bkt_used",
        "k",
        "manifest-v1",
        &serde_json::json!({}),
    )
    .await
    .unwrap();
    let blocked = client()
        .delete(format!("{base}/api/atlas/v1/buckets/bkt_used"))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), reqwest::StatusCode::CONFLICT);
    let forced = client()
        .delete(format!("{base}/api/atlas/v1/buckets/bkt_used?force=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(forced.status(), reqwest::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn backup_retention_prunes_old() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");
    seed_volume(&pool, "vol_ret", "vol-ret").await;
    atlas_inventory::buckets::insert_bucket(
        &pool,
        "bkt_ret",
        "t1",
        "b",
        "bkd_ceph_lab",
        "rook-ceph",
        "b",
        "zyvor-rgw-bucket",
    )
    .await
    .unwrap();
    atlas_inventory::buckets::set_bound(&pool, "bkt_ret", "b-1", "http://rgw:80", "us-east-1", "b")
        .await
        .unwrap();
    // Three pre-existing backups for the volume.
    for n in 0..3 {
        atlas_inventory::backups::insert_backup(
            &pool,
            &format!("bkp_ret{n}"),
            "t1",
            "vol_ret",
            None,
            "bkt_ret",
            &format!("backups/vol_ret/bkp_ret{n}.manifest.json"),
            "manifest-v1",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    // New backup with keep=2 → prune the 2 oldest (of the 4 total) via delete jobs.
    let r = client()
        .post(format!("{base}/api/atlas/v1/backup-jobs"))
        .json(&serde_json::json!({
            "volume_id": "vol_ret", "bucket_id": "bkt_ret", "keep": 2
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);

    let jobs: serde_json::Value = client()
        .get(format!("{base}/api/atlas/v1/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let deletes = jobs
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["job_type"] == "backup.delete")
        .count();
    assert_eq!(deletes, 2, "keep=2 with 4 backups should enqueue 2 deletes");
}

#[tokio::test]
async fn snapshot_delete_blocked_by_dependents() {
    let (addr, pool) = spawn().await;
    let base = format!("http://{addr}");

    // A snapshot with a dependent volume (as clone/restore would create).
    seed_volume(&pool, "vol_src", "src-vol").await;
    atlas_inventory::snapshots::insert_snapshot(
        &pool,
        "snap_dep",
        "t1",
        "vol_src",
        "snap-dep-obj",
        None,
        "crash",
        "ready",
    )
    .await
    .unwrap();
    let dependent = atlas_api_types::StorageVolume {
        id: "vol_clone".into(),
        cluster_id: None,
        pool_id: None,
        name: "clone-vol".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: None,
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("clone-vol".into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "t1", &dependent, None)
        .await
        .unwrap();
    atlas_inventory::set_volume_source_snapshot(&pool, "vol_clone", "snap_dep")
        .await
        .unwrap();

    // Delete without force → 409 Conflict.
    let blocked = client()
        .delete(format!("{base}/api/atlas/v1/snapshots/snap_dep"))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), reqwest::StatusCode::CONFLICT);

    // Delete with force → 202 Accepted.
    let forced = client()
        .delete(format!("{base}/api/atlas/v1/snapshots/snap_dep?force=true"))
        .send()
        .await
        .unwrap();
    assert_eq!(forced.status(), reqwest::StatusCode::ACCEPTED);
}

/// A discovery pass whose driver-derived id for a real volume differs from the id it was created
/// under (e.g. `POST /volumes` mints a random id; the real Ceph driver independently derives
/// `vol_{pool}_{image}` for the same RBD image) must resolve to the *existing* row via
/// `backend_native_id` instead of inserting a duplicate — otherwise the original row (the id every
/// label/quota/DR-mirror/schedule points at) goes untouched by the pass and gets pruned as stale on
/// the very next authoritative discovery. Verified live against a real cluster before this fix: a
/// PVC-created volume vanished from inventory entirely while its PVC stayed Bound on the cluster.
#[tokio::test]
async fn discovery_reconciles_id_by_backend_native_id_not_duplicate() {
    let (_addr, pool) = spawn().await;

    let backend = atlas_api_types::StorageBackend {
        id: "bkd_ceph_lab".into(),
        name: "b".into(),
        backend_type: atlas_api_types::BackendType::Ceph,
        mode: atlas_api_types::BackendMode::ManagedRook,
        status: "active".into(),
        capabilities: Default::default(),
        connection_ref: None,
        cordoned: false,
    };
    atlas_inventory::upsert_backend(&pool, &backend)
        .await
        .unwrap();

    // Simulate `POST /volumes`: a PVC-backed volume created with a random id and the real RBD
    // native id already resolved (this session's other fix — it used to be missing the "rbd:"
    // prefix that discovery uses).
    let created = atlas_api_types::StorageVolume {
        id: "vol_creation_time_random".into(),
        cluster_id: None,
        pool_id: None,
        name: "csi-vol-abc123".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some("rbd:rbd-nvme-prod/csi-vol-abc123".into()),
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("my-app-data".into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "t1", &created, None)
        .await
        .unwrap();

    // A discovery pass independently derives its own id for the same real image.
    let discovery = atlas_api_types::DiscoveryResult {
        cluster: atlas_api_types::StorageCluster {
            id: "cls_1".into(),
            backend_id: "bkd_ceph_lab".into(),
            name: "lab".into(),
            native_fsid: None,
            health: atlas_api_types::Health::Ok,
            raw_capacity_bytes: Some(100),
            used_capacity_bytes: Some(10),
            available_capacity_bytes: Some(90),
        },
        pools: vec![],
        osds: vec![],
        volumes: vec![atlas_api_types::StorageVolume {
            id: "vol_rbd-nvme-prod_csi-vol-abc123".into(),
            cluster_id: Some("cls_1".into()),
            pool_id: None,
            name: "csi-vol-abc123".into(),
            kind: atlas_api_types::VolumeKind::Block,
            backend_native_id: Some("rbd:rbd-nvme-prod/csi-vol-abc123".into()),
            size_bytes: 1073741824,
            used_bytes: Some(52428800),
            state: "available".into(),
            health: atlas_api_types::Health::Ok,
            kubernetes_namespace: Some("default".into()),
            pvc_name: Some("my-app-data".into()),
            storage_class_name: Some("zyvor-rbd-prod".into()),
        }],
        health: atlas_api_types::StorageHealth {
            status: atlas_api_types::Health::Ok,
            summary: "OK".into(),
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        },
    };
    atlas_inventory::upsert_discovery(&pool, "bkd_ceph_lab", &discovery, true)
        .await
        .unwrap();

    let all = atlas_inventory::list_volumes(&pool).await.unwrap();
    let matches: Vec<_> = all
        .iter()
        .filter(|v| v.backend_native_id.as_deref() == Some("rbd:rbd-nvme-prod/csi-vol-abc123"))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one row for this native id, got: {all:?}"
    );
    assert_eq!(
        matches[0].id, "vol_creation_time_random",
        "the original id must survive re-discovery, not get orphaned behind a duplicate"
    );
    assert_eq!(
        matches[0].used_bytes,
        Some(52428800),
        "discovery's fresh data must still land"
    );
}

/// The mirror-image race: a `VolumeCreate` job only calls `upsert_volume` once, at the very end,
/// after the PVC is already `Bound` — so a concurrent discovery tick can win the race and insert
/// its own row for the same `backend_native_id` first, under its own driver-derived id (discovery
/// finds nothing to reconcile with yet, since the create job hasn't inserted anything). Verified
/// live: `scripts/live/05-volume-lifecycle.sh` hit `UNIQUE constraint failed:
/// storage_volumes.backend_native_id` under this exact race against the real cluster.
/// `upsert_volume` must reconcile onto the *caller's* intended id — not just avoid erroring —
/// because the caller's response already promised that id and is about to insert a
/// `product_bindings` row referencing it.
#[tokio::test]
async fn create_volume_reconciles_onto_intended_id_after_discovery_race() {
    let (_addr, pool) = spawn().await;

    let backend = atlas_api_types::StorageBackend {
        id: "bkd_ceph_lab".into(),
        name: "b".into(),
        backend_type: atlas_api_types::BackendType::Ceph,
        mode: atlas_api_types::BackendMode::ManagedRook,
        status: "active".into(),
        capabilities: Default::default(),
        connection_ref: None,
        cordoned: false,
    };
    atlas_inventory::upsert_backend(&pool, &backend)
        .await
        .unwrap();

    // Discovery wins the race: it inserts a row for the real RBD image under its own derived id,
    // before the VolumeCreate job's own upsert_volume call runs.
    let raced_in = atlas_api_types::StorageVolume {
        id: "vol_rbd-nvme-prod_csi-vol-raced".into(),
        cluster_id: None,
        pool_id: None,
        name: "csi-vol-raced".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some("rbd:rbd-nvme-prod/csi-vol-raced".into()),
        size_bytes: 1073741824,
        used_bytes: Some(0),
        state: "available".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("live-race-vol".into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "global", &raced_in, None)
        .await
        .unwrap();

    // The VolumeCreate job now runs its own (losing) upsert_volume with the id the caller's
    // response already promised — must not error, must land on this id.
    let intended = atlas_api_types::StorageVolume {
        id: "vol_caller_promised_this_id".into(),
        cluster_id: None,
        pool_id: None,
        name: "live-race-vol".into(),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some("rbd:rbd-nvme-prod/csi-vol-raced".into()),
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some("live-race-vol".into()),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(
        &pool,
        "bkd_ceph_lab",
        "tnt_default",
        &intended,
        Some("development"),
    )
    .await
    .expect("must reconcile onto the intended id instead of erroring on the UNIQUE index");

    let all = atlas_inventory::list_volumes(&pool).await.unwrap();
    let matches: Vec<_> = all
        .iter()
        .filter(|v| v.backend_native_id.as_deref() == Some("rbd:rbd-nvme-prod/csi-vol-raced"))
        .collect();
    assert_eq!(matches.len(), 1, "expected exactly one row, got: {all:?}");
    assert_eq!(
        matches[0].id, "vol_caller_promised_this_id",
        "the id the caller's API response already promised must be the one that's live \
         (a product_bindings insert right after this uses that exact id)"
    );
    assert_eq!(
        matches[0].state, "bound",
        "create-time data must win, not be lost to the race"
    );
}
