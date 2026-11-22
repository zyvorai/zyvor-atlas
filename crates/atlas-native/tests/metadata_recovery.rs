// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use atlas_native::{EngineConfig, FailureDomain, NativeEngine, Node, PlacementPolicy};

fn nodes() -> Vec<Node> {
    vec![
        Node {
            id: "n1".into(),
            failure_domain: FailureDomain {
                zone: "z1".into(),
                rack: "r1".into(),
                host: "h1".into(),
            },
            free_bytes: 1 << 30,
            healthy: true,
        },
        Node {
            id: "n2".into(),
            failure_domain: FailureDomain {
                zone: "z1".into(),
                rack: "r2".into(),
                host: "h2".into(),
            },
            free_bytes: 1 << 30,
            healthy: true,
        },
        Node {
            id: "n3".into(),
            failure_domain: FailureDomain {
                zone: "z1".into(),
                rack: "r3".into(),
                host: "h3".into(),
            },
            free_bytes: 1 << 30,
            healthy: true,
        },
    ]
}

#[test]
fn wal_replays_after_reopen() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new(td.path());
    cfg.extent_bytes = 4096;
    cfg.placement = PlacementPolicy {
        replicas: 3,
        ..Default::default()
    };
    let vol;
    {
        let e = NativeEngine::open(cfg.clone(), nodes()).unwrap();
        vol = e.create_volume("v", 8192).unwrap();
        e.write(&vol, 0, b"hello atlas").unwrap();
        assert!(e.applied_index().unwrap() >= 2);
    }
    let e = NativeEngine::open(cfg, nodes()).unwrap();
    assert_eq!(e.read(&vol, 0, 11).unwrap(), b"hello atlas");
}

#[test]
fn snapshot_holds_extent_until_deleted() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = EngineConfig::new(td.path());
    cfg.extent_bytes = 4096;
    let e = NativeEngine::open(cfg, nodes()).unwrap();
    let v = e.create_volume("v", 8192).unwrap();
    e.write(&v, 0, b"before").unwrap();
    let s = e.create_snapshot(&v, "s1").unwrap();
    e.write(&v, 0, b"after!").unwrap();
    assert_eq!(e.read_snapshot(&s, 0, 6).unwrap(), b"before");
    assert_eq!(e.gc_once().unwrap().reclaimed, 0);
    e.delete_snapshot(&s).unwrap();
    assert_eq!(e.gc_once().unwrap().reclaimed, 1);
}

#[test]
fn rejected_command_is_not_logged() {
    let td = tempfile::tempdir().unwrap();
    let cfg = EngineConfig::new(td.path());
    {
        let e = NativeEngine::open(cfg.clone(), nodes()).unwrap();
        e.create_volume("v", 8192).unwrap();
        assert!(e.delete_volume("missing").is_err());
        assert_eq!(e.applied_index().unwrap(), 1);
    }
    let e = NativeEngine::open(cfg, nodes()).unwrap();
    assert_eq!(e.applied_index().unwrap(), 1);
}

#[test]
fn delete_volume_releases_live_extent() {
    let td = tempfile::tempdir().unwrap();
    let e = NativeEngine::open(EngineConfig::new(td.path()), nodes()).unwrap();
    let v = e.create_volume("v", 8192).unwrap();
    e.write(&v, 0, b"data").unwrap();
    e.delete_volume(&v).unwrap();
    let st = e.gc_once().unwrap();
    assert_eq!(st.candidates, 1);
    assert_eq!(st.reclaimed, 1);
}
