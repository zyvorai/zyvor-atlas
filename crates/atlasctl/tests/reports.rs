// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! `atlasctl` report commands (`snapshot-hygiene`, `quota-headroom`, `schedule-coverage`,
//! `ceph-health-explain`) end to end: the real binary against an in-process gateway.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::OnceLock;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use serde_json::Value;

/// One gateway for the whole test binary, on its own runtime thread; every test makes its own
/// filesystem volume, so they do not see each other's snapshots.
fn gateway() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let dir = std::env::temp_dir();
                    let db = dir.join(format!("atlasctl-reports-test-{}.db", std::process::id()));
                    let _ = std::fs::remove_file(&db);
                    let config = Config {
                        bind_addr: "127.0.0.1:0".into(),
                        grpc_addr: "127.0.0.1:0".into(),
                        database_url: format!("sqlite://{}?mode=rwc", db.display()),
                        ceph_driver_mode: CephDriverMode::Fake,
                        kubeconfig_path: None,
                        jwt_secret: "reports-cli-secret-at-least-32-bytes!!".into(),
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
                    let app = routes::router(state);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    axum::serve(listener, app).await.unwrap();
                });
        });
        format!("http://{}", rx.recv().unwrap())
    })
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.stdout))
    }
}

fn ctl(args: &[&str]) -> Out {
    let o = Command::new(env!("CARGO_BIN_EXE_atlasctl"))
        .args(["--base-url", gateway()])
        .args(args)
        .output()
        .unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[test]
fn ceph_health_explain_by_codes_by_file_and_all() {
    let one = ctl(&["ceph-health-explain", "--codes", "osd_down,NOPE"]);
    assert_eq!(one.code, 0, "{}{}", one.stdout, one.stderr);
    let j = one.json();
    assert_eq!(j["checks"][0]["code"], "OSD_DOWN");
    assert_eq!(j["unknown"], serde_json::json!(["NOPE"]));

    let all = ctl(&["ceph-health-explain"]).json();
    assert!(all["checks"].as_array().unwrap().len() >= 20);

    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("health.json");
    std::fs::write(
        &f,
        r#"{"status":"HEALTH_ERR","checks":{"PG_DAMAGED":{},"MON_CLOCK_SKEW":{}}}"#,
    )
    .unwrap();
    let from_file = ctl(&["ceph-health-explain", "--file", f.to_str().unwrap()]);
    assert_eq!(
        from_file.code, 0,
        "{}{}",
        from_file.stdout, from_file.stderr
    );
    assert_eq!(from_file.json()["checks"][0]["code"], "PG_DAMAGED");

    let missing = ctl(&["ceph-health-explain", "--file", "/nonexistent/health.json"]);
    assert_ne!(missing.code, 0);
}

#[test]
fn report_commands_hit_the_endpoints() {
    let h = ctl(&["snapshot-hygiene", "--stale-days", "30"]);
    assert_eq!(h.code, 0, "{}{}", h.stdout, h.stderr);
    assert_eq!(h.json()["assumptions"]["stale_days"], 30);

    let q = ctl(&[
        "quota-headroom",
        "--warn-percent",
        "60",
        "--critical-percent",
        "90",
    ]);
    assert_eq!(q.code, 0, "{}{}", q.stdout, q.stderr);
    assert_eq!(q.json()["assumptions"]["warn_percent"], 60.0);

    let s = ctl(&["schedule-coverage", "--min-keep", "3"]);
    assert_eq!(s.code, 0, "{}{}", s.stdout, s.stderr);
    assert_eq!(s.json()["assumptions"]["min_keep"], 3);
}

#[test]
fn bad_thresholds_exit_nonzero() {
    assert_ne!(ctl(&["snapshot-hygiene", "--stale-days", "0"]).code, 0);
    assert_ne!(
        ctl(&[
            "quota-headroom",
            "--warn-percent",
            "95",
            "--critical-percent",
            "90"
        ])
        .code,
        0
    );
    assert_ne!(ctl(&["schedule-coverage", "--min-keep", "0"]).code, 0);
}
