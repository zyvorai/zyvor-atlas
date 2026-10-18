// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Extent deduplication: identical content is stored once, copy-on-write keeps writers
//! isolated, space comes back when the last reference goes, and a share whose extent was freed
//! is rejected by the catalog.

use std::path::Path;

use atlas_native::{
    metadata::{Catalog, ExtentRef, MetaCommand, ReplicaRef},
    EngineConfig, FailureDomain, NativeEngine, Node,
};

const EXTENT: usize = 64 << 10;

fn node(i: usize) -> Node {
    Node {
        id: format!("n{i}"),
        failure_domain: FailureDomain {
            zone: "z1".into(),
            rack: format!("r{i}"),
            host: format!("h{i}"),
        },
        free_bytes: 1 << 30,
        healthy: true,
    }
}

fn engine(root: &Path, dedup: bool, compression: bool) -> NativeEngine {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = EXTENT;
    c.dedup = dedup;
    c.compression = compression;
    NativeEngine::open(c, (1..=3).map(node).collect()).unwrap()
}

fn used(e: &NativeEngine) -> u64 {
    (1..=3)
        .map(|i| e.device_len(&format!("n{i}")).unwrap())
        .sum()
}

fn random(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn root(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("atlas-dedup-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn identical_writes_are_stored_once_and_read_back_exactly() {
    let r = root("once");
    let e = engine(&r, true, false);
    let data = random(1, 4 * EXTENT);
    let a = e.create_volume("a", 4 << 20).unwrap();
    let b = e.create_volume("b", 4 << 20).unwrap();
    e.write(&a, 0, &data).unwrap();
    let after_first = used(&e);
    e.write(&b, 0, &data).unwrap();
    assert_eq!(
        used(&e),
        after_first,
        "the second copy must add no device bytes"
    );
    assert_eq!(e.read(&a, 0, data.len()).unwrap(), data);
    assert_eq!(e.read(&b, 0, data.len()).unwrap(), data);

    let t = e.telemetry.snapshot();
    assert_eq!(t.dedup_hits, 4);
    assert_eq!(t.dedup_saved_bytes, (4 * EXTENT) as u64);
    let s = e.data_reduction().unwrap();
    assert_eq!(s.extents, 4);
    assert_eq!(s.shared_extents, 4);
    assert_eq!(s.referenced_bytes, 2 * s.logical_bytes);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn repeated_cells_inside_one_write_share_one_extent() {
    let r = root("batch");
    let e = engine(&r, true, false);
    let cell = random(2, EXTENT);
    let mut data = Vec::new();
    for _ in 0..4 {
        data.extend_from_slice(&cell);
    }
    let v = e.create_volume("v", 4 << 20).unwrap();
    let before = used(&e);
    e.write(&v, 0, &data).unwrap();
    // One extent on three nodes, not four.
    assert_eq!(used(&e) - before, 3 * EXTENT as u64);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    assert_eq!(e.data_reduction().unwrap().extents, 1);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn writers_stay_isolated_and_space_returns_after_the_last_reference() {
    let r = root("cow");
    let e = engine(&r, true, false);
    let data = random(3, 2 * EXTENT);
    let a = e.create_volume("a", 4 << 20).unwrap();
    let b = e.create_volume("b", 4 << 20).unwrap();
    e.write(&a, 0, &data).unwrap();
    e.write(&b, 0, &data).unwrap();

    // Changing one side never changes the other.
    e.write(&a, 100, b"only in a").unwrap();
    let mut want_a = data.clone();
    want_a[100..109].copy_from_slice(b"only in a");
    assert_eq!(e.read(&a, 0, data.len()).unwrap(), want_a);
    assert_eq!(e.read(&b, 0, data.len()).unwrap(), data);

    // Dropping a leaves b's shared extent alone; dropping b as well frees everything.
    e.delete_volume(&a).unwrap();
    e.gc_once().unwrap();
    assert_eq!(e.read(&b, 0, data.len()).unwrap(), data);
    e.delete_volume(&b).unwrap();
    e.gc_once().unwrap();
    assert_eq!(e.data_reduction().unwrap().extents, 0);

    // The hint still names a freed extent: new data must be stored afresh, not shared.
    let c = e.create_volume("c", 4 << 20).unwrap();
    e.write(&c, 0, &data).unwrap();
    assert_eq!(e.read(&c, 0, data.len()).unwrap(), data);
    assert_eq!(e.data_reduction().unwrap().shared_extents, 0);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn dedup_works_together_with_compression_and_snapshots() {
    let r = root("both");
    let e = engine(&r, true, true);
    let text: Vec<u8> = b"level=info msg=\"extent written\" component=atlas\n"
        .iter()
        .copied()
        .cycle()
        .take(3 * EXTENT)
        .collect();
    let a = e.create_volume("a", 4 << 20).unwrap();
    let b = e.create_volume("b", 4 << 20).unwrap();
    e.write(&a, 0, &text).unwrap();
    let one = used(&e);
    e.write(&b, 0, &text).unwrap();
    assert_eq!(used(&e), one);
    assert!(one < (text.len() as u64 * 3) / 8, "compressed once: {one}");
    let snap = e.create_snapshot(&b, "s").unwrap();
    e.write(&b, 0, b"changed").unwrap();
    assert_eq!(e.read_snapshot(&snap, 0, text.len()).unwrap(), text);
    assert_eq!(e.read(&a, 0, text.len()).unwrap(), text);
    let s = e.data_reduction().unwrap();
    assert!(s.compressed_extents >= 1 && s.stored_bytes < s.logical_bytes);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn dedup_off_stores_every_copy() {
    let r = root("off");
    let e = engine(&r, false, false);
    let data = random(4, 2 * EXTENT);
    let a = e.create_volume("a", 4 << 20).unwrap();
    let b = e.create_volume("b", 4 << 20).unwrap();
    e.write(&a, 0, &data).unwrap();
    let one = used(&e);
    e.write(&b, 0, &data).unwrap();
    assert!(used(&e) >= 2 * one - 4096);
    assert_eq!(e.telemetry.snapshot().dedup_hits, 0);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn a_share_of_a_missing_or_changed_extent_is_rejected_by_the_catalog() {
    let rep = |n: &str| ReplicaRef {
        node_id: n.into(),
        device_index: 0,
        offset: 0,
    };
    let ext = |checksum: u8| ExtentRef {
        id: "e".into(),
        logical_offset: 0,
        payload_len: 4096,
        raw_len: 0,
        checksum: [checksum; 32],
        content: None,
        replicas: vec![rep("a"), rep("b")],
        ec: None,
        created_ms: 0,
        object: None,
    };
    let mut c = Catalog::default();
    c.apply(
        1,
        1,
        &MetaCommand::CreateVolume {
            id: "v".into(),
            name: "v".into(),
            size_bytes: 8192,
        },
    )
    .unwrap();
    let share = |extents: Vec<ExtentRef>| MetaCommand::InstallExtents {
        volume_id: "v".into(),
        extents,
        shared: vec!["e".into()],
    };
    // Nothing named "e" exists yet.
    assert!(c.apply(1, 2, &share(vec![ext(1)])).is_err());
    // Install it for real, then share it: accepted, and the second reference counts.
    c.apply(
        1,
        3,
        &MetaCommand::InstallExtents {
            volume_id: "v".into(),
            extents: vec![ext(1)],
            shared: vec![],
        },
    )
    .unwrap();
    let mut second = ext(1);
    second.logical_offset = 4096;
    c.apply(1, 4, &share(vec![second])).unwrap();
    assert_eq!(c.extents.get("e").unwrap().refs, 2);
    // Same id but different bytes: refused, and nothing changed.
    let mut other = ext(9);
    other.logical_offset = 8192;
    assert!(c.apply(1, 5, &share(vec![other])).is_err());
    assert_eq!(c.extents.get("e").unwrap().refs, 2);
}

#[test]
fn shared_extents_read_correctly_at_different_offsets_and_in_files() {
    let r = root("offsets");
    let e = engine(&r, true, false);
    let cell = random(8, EXTENT);

    // The same bytes at cell 0 of one volume and cell 3 of another.
    let a = e.create_volume("a", 1 << 20).unwrap();
    let b = e.create_volume("b", 1 << 20).unwrap();
    e.write(&a, 0, &cell).unwrap();
    e.write(&b, (3 * EXTENT) as u64, &cell).unwrap();
    assert_eq!(e.read(&b, (3 * EXTENT) as u64, EXTENT).unwrap(), cell);
    assert_eq!(e.read(&a, 0, EXTENT).unwrap(), cell);
    assert_eq!(e.read(&b, 0, EXTENT).unwrap(), vec![0u8; EXTENT]);
    let snap = e.create_snapshot(&b, "s").unwrap();
    assert_eq!(
        e.read_snapshot(&snap, (3 * EXTENT) as u64, EXTENT).unwrap(),
        cell
    );

    // And in two files at different offsets.
    e.create_fs_as("f".into(), "fs").unwrap();
    let mk = |name: &str| {
        e.fs_mknode(
            "f",
            1,
            atlas_native::engine::NewNode {
                name: name.into(),
                op_id: format!("op-{name}"),
                kind: atlas_native::NodeType::File,
                target: None,
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap()
        .ino
    };
    let (x, y) = (mk("x"), mk("y"));
    e.write_file("f", x, 0, &cell).unwrap();
    e.write_file("f", y, (2 * EXTENT) as u64, &cell).unwrap();
    assert_eq!(
        e.read_file("f", y, (2 * EXTENT) as u64, EXTENT).unwrap(),
        cell
    );
    assert_eq!(e.read_file("f", x, 0, EXTENT).unwrap(), cell);
    assert_eq!(e.data_reduction().unwrap().extents, 1);
    let _ = std::fs::remove_dir_all(r);
}
