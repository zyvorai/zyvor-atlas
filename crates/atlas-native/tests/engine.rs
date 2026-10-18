// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use atlas_native::{EngineConfig, FailureDomain, NativeEngine, Node};

fn test_root(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("atlas-native-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn nodes() -> Vec<Node> {
    (0..3)
        .map(|i| Node {
            id: format!("n{i}"),
            failure_domain: FailureDomain {
                zone: "z1".into(),
                rack: format!("r{i}"),
                host: format!("h{i}"),
            },
            free_bytes: 1 << 30,
            healthy: true,
        })
        .collect()
}

#[test]
fn replicated_write_and_read_round_trip() {
    let root = test_root("rw");
    let e = NativeEngine::open(EngineConfig::new(&root), nodes()).unwrap();
    let v = e.create_volume("train", 16 * 1024 * 1024).unwrap();
    let payload = vec![0x5a; 4096];
    e.write(&v, 0, &payload).unwrap();
    assert_eq!(e.read(&v, 0, payload.len()).unwrap(), payload);
    assert_eq!(e.telemetry.snapshot().writes, 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn snapshot_is_copy_on_write_metadata() {
    let root = test_root("snap");
    let e = NativeEngine::open(EngineConfig::new(&root), nodes()).unwrap();
    let v = e.create_volume("train", 16 * 1024 * 1024).unwrap();
    e.write(&v, 0, b"before").unwrap();
    let s = e.create_snapshot(&v, "s1").unwrap();
    e.write(&v, 0, b"after!").unwrap();
    assert_eq!(e.read(&v, 0, 6).unwrap(), b"after!");
    assert_eq!(e.read_snapshot(&s, 0, 6).unwrap(), b"before");
    let _ = std::fs::remove_dir_all(root);
}

fn small_extents(root: &std::path::Path) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = 16;
    c
}

/// Byte-for-byte model of the volume: every write is checked against a plain buffer.
#[test]
fn unaligned_and_cross_extent_io_matches_a_flat_buffer() {
    let root = test_root("unaligned");
    let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
    let size = 100u64;
    let v = e.create_volume("v", size).unwrap();
    let mut model = vec![0u8; size as usize];

    assert_eq!(
        e.read(&v, 0, size as usize).unwrap(),
        model,
        "unwritten reads zero"
    );
    let writes: &[(u64, usize, u8)] = &[
        (5, 3, 1),   // inside one extent, nothing there yet
        (0, 16, 2),  // whole extent over the partial one
        (10, 30, 3), // spans three extents, partial at both ends
        (47, 2, 4),  // straddles an extent boundary
        (90, 10, 5), // last, short extent of the volume
        (33, 1, 6),  // single byte inside an existing extent
        (0, 100, 7), // everything
        (31, 40, 8),
    ];
    for &(off, len, b) in writes {
        e.write(&v, off, &vec![b; len]).unwrap();
        model[off as usize..off as usize + len].fill(b);
        assert_eq!(
            e.read(&v, 0, size as usize).unwrap(),
            model,
            "after write at {off}"
        );
    }
    for (off, len) in [(0, 1), (15, 2), (17, 50), (99, 1), (3, 97), (100, 0)] {
        assert_eq!(
            e.read(&v, off, len).unwrap(),
            &model[off as usize..off as usize + len],
            "read {off}+{len}"
        );
    }
    assert!(e.read(&v, 90, 11).is_err(), "read past the end");
    assert!(e.write(&v, 99, b"xy").is_err(), "write past the end");

    let s = e.create_snapshot(&v, "s").unwrap();
    e.write(&v, 20, &[9u8; 20]).unwrap();
    assert_eq!(e.read_snapshot(&s, 0, size as usize).unwrap(), model);
    assert!(e.read_snapshot(&s, 0, size as usize + 1).is_err());

    // Every extent stays on the grid: one per 16-byte cell, none overlapping.
    let extents = e.volumes().unwrap()[0].extents;
    assert_eq!(extents, 7);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn partial_overwrite_survives_reopen() {
    let root = test_root("partial-reopen");
    let v = {
        let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
        let v = e.create_volume("v", 64).unwrap();
        e.write(&v, 0, &[1u8; 64]).unwrap();
        e.write(&v, 12, b"hello world").unwrap();
        v
    };
    let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
    let mut want = vec![1u8; 64];
    want[12..23].copy_from_slice(b"hello world");
    assert_eq!(e.read(&v, 0, 64).unwrap(), want);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn clone_shares_extents_until_written_and_outlives_its_source() {
    let root = test_root("clone");
    let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
    let v = e.create_volume("src", 64).unwrap();
    e.write(&v, 0, &[1u8; 64]).unwrap();
    let s = e.create_snapshot(&v, "s").unwrap();
    e.write(&v, 0, &[2u8; 16]).unwrap();

    let c = e.clone_snapshot(&s, "copy", None).unwrap();
    assert_eq!(e.read(&c, 0, 64).unwrap(), vec![1u8; 64]);
    let info = |id: &str| {
        e.volumes()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap()
    };
    assert_eq!(info(&c).size_bytes, 64);
    assert_eq!(info(&c).extents, 4);

    // Writes on either side stay on that side.
    e.write(&c, 20, b"clone").unwrap();
    assert_eq!(&e.read(&c, 20, 5).unwrap(), b"clone");
    assert_eq!(e.read(&v, 16, 16).unwrap(), vec![1u8; 16]);
    assert_eq!(e.read_snapshot(&s, 0, 64).unwrap(), vec![1u8; 64]);

    // A larger clone reads zeros past the snapshot's end.
    let big = e.clone_snapshot(&s, "big", Some(128)).unwrap();
    let mut want = vec![1u8; 64];
    want.resize(128, 0);
    assert_eq!(e.read(&big, 0, 128).unwrap(), want);
    assert!(e.clone_snapshot(&s, "small", Some(32)).is_err());
    assert!(e.clone_snapshot("nope", "x", None).is_err());

    // Dropping the source and snapshot leaves the clones' shared extents alone.
    e.delete_snapshot(&s).unwrap();
    e.delete_volume(&v).unwrap();
    e.gc_once().unwrap();
    let mut want = vec![1u8; 64];
    want[20..25].copy_from_slice(b"clone");
    assert_eq!(e.read(&c, 0, 64).unwrap(), want);
    assert_eq!(e.read(&big, 0, 64).unwrap(), vec![1u8; 64]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resize_grows_and_never_shrinks() {
    let root = test_root("resize");
    let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
    let v = e.create_volume("v", 40).unwrap();
    e.write(&v, 32, &[7u8; 8]).unwrap();
    assert!(e.write(&v, 40, b"x").is_err());
    e.resize_volume(&v, 100).unwrap();
    e.write(&v, 38, &[8u8; 10]).unwrap();
    let mut want = vec![0u8; 100];
    want[32..38].fill(7);
    want[38..48].fill(8);
    assert_eq!(e.read(&v, 0, 100).unwrap(), want);
    assert!(e.resize_volume(&v, 50).is_err(), "shrink refused");
    assert!(e.resize_volume("nope", 200).is_err());
    assert_eq!(e.volumes().unwrap()[0].size_bytes, 100);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn creates_with_a_client_id_are_idempotent() {
    let root = test_root("idem");
    let e = NativeEngine::open(small_extents(&root), nodes()).unwrap();
    let v = e.create_volume_as("vol-1".into(), "v", 64).unwrap();
    assert_eq!(e.create_volume_as("vol-1".into(), "v", 64).unwrap(), v);
    assert!(e.create_volume_as("vol-1".into(), "other", 64).is_err());
    assert_eq!(e.volumes().unwrap().len(), 1);

    e.write(&v, 0, &[4u8; 16]).unwrap();
    let s = e.create_snapshot_as("snap-1".into(), &v, "s").unwrap();
    e.create_snapshot_as("snap-1".into(), &v, "s").unwrap();
    assert!(e.create_snapshot_as("snap-1".into(), &v, "t").is_err());
    let c = e
        .clone_snapshot_as("clone-1".into(), &s, "c", None)
        .unwrap();
    e.clone_snapshot_as("clone-1".into(), &s, "c", None)
        .unwrap();
    assert_eq!(e.volumes().unwrap().len(), 2);

    // The repeats took no extra extent references: dropping everything reclaims every extent.
    e.delete_volume(&c).unwrap();
    e.delete_snapshot(&s).unwrap();
    e.delete_volume(&v).unwrap();
    assert_eq!(e.gc_once().unwrap().reclaimed, 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn locked_snapshot_refuses_delete_and_survives_gc() {
    let root = test_root("worm");
    let e = NativeEngine::open(EngineConfig::new(&root), nodes()).unwrap();
    let v = e.create_volume("vault", 16 * 1024 * 1024).unwrap();
    e.write(&v, 0, &[7u8; 64]).unwrap();
    let s = e.create_snapshot(&v, "immutable").unwrap();

    let until = e.lock_snapshot(&s, 3600).unwrap();
    assert!(e.delete_snapshot(&s).is_err());

    // The source going away must not take a locked snapshot's data with it.
    e.delete_volume(&v).unwrap();
    e.gc_once().unwrap();
    assert_eq!(e.read_snapshot(&s, 0, 64).unwrap(), vec![7u8; 64]);

    // A lock only extends: a shorter retention is refused, a longer one is accepted.
    assert!(e.lock_snapshot(&s, 10).is_err());
    assert!(e.lock_snapshot(&s, 7200).unwrap() > until);
    assert!(e.delete_snapshot(&s).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn lapsed_lock_allows_delete() {
    let root = test_root("worm-lapsed");
    let e = NativeEngine::open(EngineConfig::new(&root), nodes()).unwrap();
    let v = e.create_volume("v", 16 * 1024 * 1024).unwrap();
    let s = e.create_snapshot(&v, "s").unwrap();
    e.lock_snapshot(&s, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2100));
    e.delete_snapshot(&s).unwrap();
    let _ = std::fs::remove_dir_all(root);
}
