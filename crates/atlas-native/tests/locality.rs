// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Dataset locality: per-node and per-host bytes of a tree, and pinning a copy onto hosts.

use atlas_native::{
    engine::NewNode, metadata::MetaError, EngineConfig, FailureDomain, Locality, NativeEngine,
    NativeError, Node, NodeType, ROOT_INO,
};

/// Five nodes on five hosts across three racks, three replicas, 16-byte extents.
fn engine() -> (NativeEngine, tempfile::TempDir) {
    let td = tempfile::tempdir().unwrap();
    let nodes = (0..5)
        .map(|i| Node {
            id: format!("n{i}"),
            failure_domain: FailureDomain {
                zone: "z1".into(),
                rack: format!("r{}", i % 3),
                host: format!("h{i}"),
            },
            free_bytes: 1 << 30,
            healthy: true,
        })
        .collect();
    let mut cfg = EngineConfig::new(td.path());
    cfg.extent_bytes = 16;
    (NativeEngine::open(cfg, nodes).unwrap(), td)
}

fn mk(e: &NativeEngine, parent: u64, name: &str, kind: NodeType) -> u64 {
    e.fs_mknode(
        "f",
        parent,
        NewNode {
            name: name.into(),
            op_id: format!("op-{parent}-{name}"),
            kind,
            target: None,
            rdev: 0,
            mode: 0o755,
            create_mode: None,
            uid: 0,
            gid: 0,
        },
    )
    .unwrap()
    .ino
}

fn data(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

fn host<'a>(l: &'a Locality, h: &str) -> &'a atlas_native::HostLocality {
    l.hosts.iter().find(|x| x.host == h).unwrap()
}

/// /ds/{a, b, sub/c} plus /other, written as 16-byte extents.
fn dataset(e: &NativeEngine) -> Vec<(u64, Vec<u8>)> {
    e.create_fs_as("f".into(), "fs").unwrap();
    let ds = mk(e, ROOT_INO, "ds", NodeType::Dir);
    let sub = mk(e, ds, "sub", NodeType::Dir);
    let mut files = Vec::new();
    for (parent, name, seed, len) in [
        (ds, "a", 1, 100),
        (ds, "b", 2, 40),
        (sub, "c", 3, 64),
        (ROOT_INO, "other", 4, 48),
    ] {
        let ino = mk(e, parent, name, NodeType::File);
        let d = data(seed, len);
        e.write_file("f", ino, 0, &d).unwrap();
        files.push((ino, d));
    }
    files
}

#[test]
fn locality_counts_each_extent_once_per_node_and_host() {
    let (e, _td) = engine();
    dataset(&e);
    let l = e.fs_locality("f", "/ds", 1_000_000).unwrap();
    // a: 7 extents, b: 3, c: 4.
    assert_eq!((l.files, l.dirs, l.extents, l.bytes), (3, 2, 14, 204));
    assert_eq!((l.tiered_bytes, l.erasure_coded_bytes), (0, 0));
    assert!(!l.truncated);
    assert_eq!(l.nodes.len(), 5);
    assert_eq!(l.hosts.len(), 5);
    // Three replicas on distinct hosts: every byte is on three nodes and three hosts.
    assert_eq!(l.nodes.iter().map(|n| n.bytes).sum::<u64>(), 3 * 204);
    assert_eq!(l.hosts.iter().map(|h| h.bytes).sum::<u64>(), 3 * 204);
    assert!(l.nodes.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    for h in &l.hosts {
        assert!((h.local_fraction - h.bytes as f64 / 204.0).abs() < 1e-9);
    }

    let file = e.fs_locality("f", "ds/sub/c", 1_000_000).unwrap();
    assert_eq!((file.files, file.dirs, file.bytes), (1, 0, 64));
    let root = e.fs_locality("f", "/", 1_000_000).unwrap();
    assert_eq!((root.files, root.dirs, root.bytes), (4, 3, 252));
    let cut = e.fs_locality("f", "/", 2).unwrap();
    assert!(cut.truncated && cut.files + cut.dirs == 2);
}

#[test]
fn pin_moves_one_copy_of_every_extent_onto_the_hosts() {
    let (e, _td) = engine();
    let files = dataset(&e);
    let before = e.fs_locality("f", "/ds", 1_000_000).unwrap();
    let already = host(&before, "h4").bytes;
    assert!(already < 204, "test needs extents not yet on h4");

    let mut after: Option<String> = None;
    let (mut moved, mut local, mut calls) = (0, 0, 0);
    loop {
        let r = e
            .fs_pin("f", "/ds", &["h4".into()], after.as_deref(), 3, 1_000_000)
            .unwrap();
        calls += 1;
        assert_eq!(r.deferred, 0);
        moved += r.moved;
        local += r.local;
        after = r.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(moved + local, 14);
    assert!(moved > 0 && calls == 5);

    let l = e.fs_locality("f", "/ds", 1_000_000).unwrap();
    assert_eq!(host(&l, "h4").bytes, 204);
    assert_eq!(host(&l, "h4").local_fraction, 1.0);
    // Still three copies on three distinct hosts.
    assert_eq!(l.nodes.iter().map(|n| n.bytes).sum::<u64>(), 3 * 204);
    assert_eq!(l.hosts.iter().map(|h| h.bytes).sum::<u64>(), 3 * 204);
    // The data reads back unchanged, and the extents outside the tree were not touched.
    for (ino, d) in &files {
        assert_eq!(&e.read_file("f", *ino, 0, 4096).unwrap(), d);
    }
    let other = e.fs_locality("f", "/other", 1_000_000).unwrap();
    assert_eq!(other.hosts.iter().map(|h| h.bytes).sum::<u64>(), 3 * 48);

    // Pinning again is a no-op.
    let again = e
        .fs_pin("f", "/ds", &["h4".into()], None, 100, 1_000_000)
        .unwrap();
    assert_eq!((again.moved, again.local, again.next), (0, 14, None));
    // Scrub finds nothing to repair after the moves.
    assert_eq!(e.repair_once().unwrap().replicas_repaired, 0);
}

#[test]
fn pin_and_locality_reject_bad_requests() {
    let (e, _td) = engine();
    dataset(&e);
    let pin = |hosts: &[&str], path: &str| {
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        e.fs_pin("f", path, &hosts, None, 10, 1_000_000)
    };
    assert!(matches!(pin(&[], "/ds"), Err(NativeError::Invalid(_))));
    assert!(matches!(
        pin(&["nowhere"], "/ds"),
        Err(NativeError::Invalid(_))
    ));
    assert!(matches!(
        pin(&["h1"], "/ds/../other"),
        Err(NativeError::Invalid(_))
    ));
    assert!(matches!(
        e.fs_locality("f", "/missing", 10),
        Err(NativeError::Metadata(MetaError::NotFound(_)))
    ));
}
