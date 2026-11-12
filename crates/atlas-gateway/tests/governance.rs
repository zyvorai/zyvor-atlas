// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Day-2 governance: token revocation + bootstrap admin bearer. A minted token works until its
//! `jti` is revoked, after which the auth middleware rejects it (401) before its TTL expires.
//! Revocation is admin-only. Auth on.

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

mod common;

async fn spawn_auth(secret: &str) -> String {
    spawn_auth_with(secret, None).await
}

async fn spawn_auth_with(secret: &str, bootstrap: Option<&str>) -> String {
    let database_url = common::fresh_database_url("governance").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: secret.into(),
        jwt_secret_previous: None,
        auth_required: true,
        bootstrap_admin_token: bootstrap.map(|s| s.to_string()),
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
    .unwrap();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn revoked_token_is_rejected() {
    let secret = "gov-test-secret-key-at-least-32-bytes!!";
    let base = format!("{}/api/atlas/v1", spawn_auth(secret).await);
    let c = reqwest::Client::new();
    let (admin, _, _) =
        atlas_gateway::auth::mint_token(secret, "boot", "admin", "global", 3600).unwrap();

    // Admin issues two operator tokens; the response carries the jti used for revocation.
    let issue = |c: &reqwest::Client, admin: &str| {
        let base = base.clone();
        let admin = admin.to_string();
        let c = c.clone();
        async move {
            c.post(format!("{base}/auth/tokens"))
                .bearer_auth(&admin)
                .json(&serde_json::json!({ "subject": "svc", "role": "operator", "ttl_secs": 600 }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let t1 = issue(&c, &admin).await;
    let t2 = issue(&c, &admin).await;
    let (op1, jti1) = (t1["token"].as_str().unwrap(), t1["jti"].as_str().unwrap());
    let op2 = t2["token"].as_str().unwrap();

    // The token works before revocation.
    let ok = c
        .get(format!("{base}/alerts"))
        .bearer_auth(op1)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // A non-admin cannot revoke.
    let denied = c
        .post(format!("{base}/auth/tokens/{jti1}/revoke"))
        .bearer_auth(op2)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403, "operator must not revoke tokens");

    // Admin revokes token 1.
    let rev = c
        .post(format!("{base}/auth/tokens/{jti1}/revoke"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(rev.status(), 200);

    // Token 1 is now rejected (401) even though it hasn't expired; token 2 still works.
    let after = c
        .get(format!("{base}/alerts"))
        .bearer_auth(op1)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401, "revoked token must be rejected");
    let still = c
        .get(format!("{base}/alerts"))
        .bearer_auth(op2)
        .send()
        .await
        .unwrap();
    assert_eq!(still.status(), 200, "an unrevoked token keeps working");

    // The deny-list lists the revoked jti.
    let revoked: Value = c
        .get(format!("{base}/auth/tokens/revoked"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        revoked.as_array().unwrap().iter().any(|r| r["jti"] == jti1),
        "revoked jti should be listed"
    );
}

#[tokio::test]
async fn bootstrap_admin_token_can_mint_jwts() {
    let secret = "gov-test-secret-key-at-least-32-bytes!!";
    let boot = "one-shot-bootstrap-admin-token-xyz";
    let base = format!("{}/api/atlas/v1", spawn_auth_with(secret, Some(boot)).await);
    let c = reqwest::Client::new();

    let minted: Value = c
        .post(format!("{base}/auth/tokens"))
        .bearer_auth(boot)
        .json(&serde_json::json!({ "subject": "ops", "role": "admin", "ttl_secs": 600 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let jwt = minted["token"]
        .as_str()
        .expect("bootstrap mint returns a JWT");

    let ok = c
        .get(format!("{base}/alerts"))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "JWT minted via bootstrap must work");

    let bad = c
        .get(format!("{base}/alerts"))
        .bearer_auth("wrong-bootstrap")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
}

#[tokio::test]
async fn console_password_login_mints_admin_jwt() {
    let secret = "gov-test-secret-key-at-least-32-bytes!!";
    let base = format!("{}/api/atlas/v1", spawn_auth(secret).await);
    let c = reqwest::Client::new();

    let bad = c
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "admin", "password": "wrong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    let minted: Value = c
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "admin", "password": "Admin@321" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let jwt = minted["token"].as_str().expect("login returns a JWT");
    assert_eq!(minted["role"], "admin");

    let ok = c
        .get(format!("{base}/alerts"))
        .bearer_auth(jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200, "JWT from password login must work");
}

#[tokio::test]
async fn admin_can_create_user_with_privilege_and_they_can_login() {
    let secret = "gov-test-secret-key-at-least-32-bytes!!";
    let base = format!("{}/api/atlas/v1", spawn_auth(secret).await);
    let c = reqwest::Client::new();

    let admin: Value = c
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "admin", "password": "Admin@321" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let admin_jwt = admin["token"].as_str().unwrap();

    let created = c
        .post(format!("{base}/auth/users"))
        .bearer_auth(admin_jwt)
        .json(&serde_json::json!({
            "username": "ops",
            "password": "OpsPass99",
            "role": "operator"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{}", created.text().await.unwrap());
    let body: Value = created.json().await.unwrap();
    assert_eq!(body["role"], "operator");
    assert_eq!(body["level"], 1);

    let login: Value = c
        .post(format!("{base}/auth/login"))
        .json(&serde_json::json!({ "username": "ops", "password": "OpsPass99" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(login["role"], "operator");
    let ops_jwt = login["token"].as_str().unwrap();

    // Operator cannot create users.
    let denied = c
        .post(format!("{base}/auth/users"))
        .bearer_auth(ops_jwt)
        .json(&serde_json::json!({
            "username": "other",
            "password": "OtherPass1",
            "role": "viewer"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);

    let listed: Value = c
        .get(format!("{base}/auth/users"))
        .bearer_auth(admin_jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["username"] == "ops" && u["role"] == "operator"),
        "ops user should be listed"
    );
}
