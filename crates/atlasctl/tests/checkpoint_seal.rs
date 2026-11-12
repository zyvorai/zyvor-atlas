// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `atlasctl checkpoint seal` end to end: the real `atlasctl` binary against an in-process gateway
//! on the fixture atlas-native driver, with fake DCP, Megatron and Orbax directory layouts on a
//! temp dir standing in for the mounted filesystem. No cluster, no framework output.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
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
                    let db = dir.join(format!("atlasctl-ckpt-test-{}.db", std::process::id()));
                    let _ = std::fs::remove_file(&db);
                    let config = Config {
                        bind_addr: "127.0.0.1:0".into(),
                        grpc_addr: "127.0.0.1:0".into(),
                        database_url: format!("sqlite://{}?mode=rwc", db.display()),
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

fn new_fs_volume(name: &str) -> String {
    let o = ctl(&[
        "create-volume",
        name,
        "--kind",
        "filesystem",
        "--backend",
        NATIVE_BACKEND_ID,
    ]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    o.json()["volume_id"].as_str().unwrap().to_string()
}

fn snapshots_of(volume: &str) -> Vec<Value> {
    let o = ctl(&["snapshots"]);
    assert_eq!(o.code, 0);
    o.json()
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["volume_id"] == volume)
        .cloned()
        .collect()
}

fn names_of(volume: &str) -> Vec<String> {
    let mut v: Vec<String> = snapshots_of(volume)
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn touch(p: &Path, content: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// What a finished `torch.distributed.checkpoint` FileSystemWriter leaves in its directory.
fn fake_dcp(dir: &Path) {
    touch(&dir.join("__0_0.distcp"), "shard");
    touch(&dir.join("__1_0.distcp"), "shard");
    touch(&dir.join(".metadata"), "pickle");
}

/// A Megatron-LM save: `<save>/iter_NNNNNNN/...` and the tracker beside it.
fn fake_megatron(save: &Path, iteration: u64) {
    touch(
        &save
            .join(format!("iter_{iteration:07}"))
            .join("model_optim_rng.pt"),
        "w",
    );
    touch(
        &save.join("latest_checkpointed_iteration.txt"),
        &iteration.to_string(),
    );
}

fn seal<'a>(vol: &'a str, root: &'a Path, path: &'a str, extra: &[&'a str]) -> Out {
    let mut args = vec![
        "checkpoint",
        "seal",
        "--volume",
        vol,
        "--root",
        root.to_str().unwrap(),
        "--path",
        path,
    ];
    args.extend_from_slice(extra);
    ctl(&args)
}

#[test]
fn dcp_is_refused_until_metadata_lands_then_sealed_locked_and_idempotent() {
    let vol = new_fs_volume("ckpt-dcp");
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("runs/llama/step_100");
    touch(&dir.join("__0_0.distcp"), "shard");

    let o = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp"],
    );
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains(".metadata is missing"), "{}", o.stderr);
    assert_eq!(o.json()["refused"], true);
    assert!(
        snapshots_of(&vol).is_empty(),
        "a refused seal creates nothing"
    );

    fake_dcp(&dir);
    let o = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp", "--lock-days", "2"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let first = o.json();
    assert_eq!(first["sealed"], true);
    assert_eq!(first["created"], true);
    assert_eq!(first["lock"]["locked"], true);
    let name = first["snapshot_name"].as_str().unwrap();
    assert!(name.starts_with("ckpt.runs-llama.step-100."), "{name}");
    let id = first["snapshot_id"].as_str().unwrap().to_string();

    // The lock is the gateway's: deleting the snapshot is refused.
    let del = ctl(&["delete-snapshot", &id]);
    assert_ne!(
        del.code, 0,
        "a locked snapshot cannot be deleted: {}",
        del.stdout
    );
    assert_eq!(snapshots_of(&vol).len(), 1);

    // The gateway reports when the lock ends (about two days from now).
    let until = snapshots_of(&vol)[0]["locked_until"].as_i64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (until - now - 2 * 86_400).abs() < 120,
        "locked_until {until} vs now {now}"
    );

    // Re-run: same snapshot, no second one, and a shorter lock is not an error. The existing lock
    // already covers one day, so the lock call is skipped altogether.
    let again = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp", "--lock-days", "1"],
    );
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    assert_eq!(again.json()["created"], false);
    assert_eq!(again.json()["snapshot_id"], id.as_str());
    assert_eq!(again.json()["lock"]["skipped"], true, "{}", again.stdout);
    assert_eq!(again.json()["lock"]["retain_until_unix"], until);
    assert_eq!(snapshots_of(&vol).len(), 1);
    assert_eq!(snapshots_of(&vol)[0]["locked_until"], until, "unchanged");

    // A dry run shows the lock state and changes nothing.
    let dry = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp", "--lock-days", "1", "--dry-run"],
    );
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    assert_eq!(dry.json()["locked_until_unix"], until);
    assert!(
        dry.json()["lock"].as_str().unwrap().contains("already locked"),
        "{}",
        dry.stdout
    );
    let dry_long = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp", "--lock-days", "30", "--dry-run"],
    );
    assert!(
        dry_long.json()["lock"].as_str().unwrap().contains("would be locked"),
        "{}",
        dry_long.stdout
    );
    assert_eq!(snapshots_of(&vol)[0]["locked_until"], until, "dry run locks nothing");

    // A longer lock is a real extension: the call is made and the reported expiry moves out.
    let longer = seal(
        &vol,
        root.path(),
        "/runs/llama/step_100",
        &["--preset", "dcp", "--lock-days", "5"],
    );
    assert_eq!(longer.code, 0, "{}{}", longer.stdout, longer.stderr);
    assert!(longer.json()["lock"].get("skipped").is_none(), "{}", longer.stdout);
    let extended = snapshots_of(&vol)[0]["locked_until"].as_i64().unwrap();
    assert!(extended > until + 2 * 86_400, "{extended} vs {until}");
}

#[test]
fn megatron_tracker_gates_each_iteration_and_keep_last_prunes_old_unlocked_ones() {
    let vol = new_fs_volume("ckpt-megatron");
    let root = tempfile::tempdir().unwrap();
    let save = root.path().join("runs/gpt");
    // A snapshot nobody made through `checkpoint seal` is never pruned.
    let manual = ctl(&["snapshot-volume", &vol, "--name", "manual-before-run"]);
    assert_eq!(manual.code, 0, "{}", manual.stdout);

    for it in [100u64, 200, 300] {
        fake_megatron(&save, it);
        let path = format!("/runs/gpt/iter_{it:07}");
        let o = seal(
            &vol,
            root.path(),
            &path,
            &[
                "--preset",
                "megatron",
                "--lock-days",
                "0",
                "--keep-last",
                "2",
            ],
        );
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    }
    let names = names_of(&vol);
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.iter().any(|n| n == "manual-before-run"));
    assert!(
        !names.iter().any(|n| n.contains("iter-0000100")),
        "oldest was pruned: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("iter-0000200"))
            && names.iter().any(|n| n.contains("iter-0000300"))
    );

    // The tracker holds only the latest save: iteration 200 can no longer be sealed from it.
    let o = seal(
        &vol,
        root.path(),
        "/runs/gpt/iter_0000200",
        &["--preset", "megatron", "--lock-days", "0"],
    );
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("holds only the latest"), "{}", o.stderr);
}

#[test]
fn keep_last_never_deletes_a_locked_snapshot_and_dry_run_changes_nothing() {
    let vol = new_fs_volume("ckpt-prune");
    let root = tempfile::tempdir().unwrap();
    for s in 1..=3 {
        touch(&root.path().join(format!("runs/r/s{s}/DONE")), "");
    }
    // s1 is locked, s2 and s3 are not.
    let o = seal(
        &vol,
        root.path(),
        "/runs/r/s1",
        &["--marker", "DONE", "--lock-days", "3"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let s1 = o.json()["snapshot_id"].as_str().unwrap().to_string();
    for s in [2, 3] {
        let o = seal(
            &vol,
            root.path(),
            &format!("/runs/r/s{s}"),
            &["--marker", "DONE", "--lock-days", "0"],
        );
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    }
    assert_eq!(snapshots_of(&vol).len(), 3);

    let dry = seal(
        &vol,
        root.path(),
        "/runs/r/s3",
        &["--marker", "DONE", "--keep-last", "1", "--dry-run"],
    );
    assert_eq!(dry.code, 0, "{}{}", dry.stdout, dry.stderr);
    let plan = dry.json();
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["snapshot"], "exists, would be reused");
    assert_eq!(plan["would_try_to_delete"].as_array().unwrap().len(), 2);
    assert_eq!(snapshots_of(&vol).len(), 3, "dry run deletes nothing");

    let o = seal(
        &vol,
        root.path(),
        "/runs/r/s3",
        &["--marker", "DONE", "--lock-days", "0", "--keep-last", "1"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let out = o.json();
    assert_eq!(out["pruned"].as_array().unwrap().len(), 1, "{out}");
    let retained = out["retained"].as_array().unwrap();
    assert_eq!(retained.len(), 1, "{out}");
    assert_eq!(retained[0]["id"], s1.as_str());
    let left = names_of(&vol);
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().any(|n| n.contains(".s1.")) && left.iter().any(|n| n.contains(".s3.")));
}

#[test]
fn explicit_marker_orbax_assume_complete_and_bad_inputs() {
    let vol = new_fs_volume("ckpt-misc");
    let root = tempfile::tempdir().unwrap();
    touch(&root.path().join("c/step5/weights.bin"), "w");

    let o = seal(&vol, root.path(), "/c/step5", &["--marker", "_SUCCESS"]);
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    touch(&root.path().join("c/step5/_SUCCESS"), "");
    let o = seal(
        &vol,
        root.path(),
        "/c/step5",
        &["--marker", "_SUCCESS", "--lock-days", "0"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.json()["lock"]["locked"], false);

    // Orbax: an uncommitted temp directory is refused even with the marker file inside.
    touch(
        &root
            .path()
            .join("o/7.orbax-checkpoint-tmp/commit_success.txt"),
        "ok",
    );
    let o = seal(
        &vol,
        root.path(),
        "/o/7.orbax-checkpoint-tmp",
        &["--preset", "orbax"],
    );
    assert_eq!(o.code, 3, "{}{}", o.stdout, o.stderr);
    touch(&root.path().join("o/7/commit_success.txt"), "ok");
    let o = seal(
        &vol,
        root.path(),
        "/o/7",
        &["--preset", "orbax", "--lock-days", "0"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);

    // Without a mount the caller vouches for the checkpoint.
    let o = ctl(&[
        "checkpoint",
        "seal",
        "--volume",
        &vol,
        "--path",
        "/nowhere/s1",
        "--assume-complete",
        "--lock-days",
        "0",
    ]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);

    // No completion rule, no --root, a block volume, a missing volume and --keep-last 0 all fail.
    let none = ctl(&[
        "checkpoint",
        "seal",
        "--volume",
        &vol,
        "--path",
        "/c/step5",
        "--root",
        "/tmp",
    ]);
    assert_eq!(none.code, 2, "clap usage error: {}", none.stderr);
    let noroot = ctl(&[
        "checkpoint",
        "seal",
        "--volume",
        &vol,
        "--path",
        "/c/step5",
        "--marker",
        "x",
    ]);
    assert_eq!(noroot.code, 2, "{}", noroot.stderr);
    let zero = seal(
        &vol,
        root.path(),
        "/c/step5",
        &["--marker", "_SUCCESS", "--keep-last", "0"],
    );
    assert_eq!(zero.code, 1, "{}", zero.stderr);
    let missing = seal(
        "vol_native_fs_nope",
        root.path(),
        "/c/step5",
        &["--marker", "_SUCCESS"],
    );
    assert_eq!(missing.code, 1, "{}", missing.stderr);
    assert!(missing.stderr.contains("not found"), "{}", missing.stderr);
    let blk = ctl(&["create-volume", "blk", "--backend", NATIVE_BACKEND_ID]);
    let bid = blk.json()["volume_id"].as_str().unwrap().to_string();
    let o = seal(&bid, root.path(), "/c/step5", &["--marker", "_SUCCESS"]);
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("only filesystem volumes"), "{}", o.stderr);
}
