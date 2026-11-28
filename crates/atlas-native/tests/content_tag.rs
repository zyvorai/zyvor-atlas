// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A file's content tag ([`NativeEngine::fs_content_tag`]): it moves with every change and never
//! comes back, which is what lets a mount keep cached data while it stays equal.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use atlas_native::{
    engine::{DeltaConfig, NewNode},
    EngineConfig, FailureDomain, NativeEngine, Node, NodeType, SetAttr, XattrMode, ROOT_INO,
};

const GRID: usize = 4096;

fn open_with(root: &Path, change: impl FnOnce(&mut EngineConfig)) -> NativeEngine {
    let nodes = (0..3)
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
        .collect();
    let mut cfg = EngineConfig::new(root);
    cfg.extent_bytes = GRID;
    change(&mut cfg);
    NativeEngine::open(cfg, nodes).unwrap()
}

fn open(root: &Path) -> NativeEngine {
    open_with(root, |_| {})
}

fn root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("atlas-native-tag-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn new_node(name: &str, kind: NodeType) -> NewNode {
    NewNode {
        name: name.into(),
        op_id: format!("op-{name}"),
        kind,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
    }
}

fn file(e: &NativeEngine, fs: &str, name: &str) -> u64 {
    e.fs_mknode(fs, ROOT_INO, new_node(name, NodeType::File))
        .unwrap()
        .ino
}

fn tag(e: &NativeEngine, fs: &str, ino: u64) -> String {
    e.fs_content_tag(fs, ino).unwrap().expect("a tag")
}

fn truncate(size: u64) -> SetAttr {
    SetAttr {
        size: Some(size),
        ..Default::default()
    }
}

/// Collects tags and fails on one seen before.
struct Seen(BTreeSet<String>);

impl Seen {
    fn new(first: String) -> Self {
        Self([first].into())
    }

    fn fresh(&mut self, tag: String, after: &str) {
        assert!(self.0.insert(tag), "the tag did not move after {after}");
    }
}

#[test]
fn every_change_moves_the_tag_and_undoing_one_does_not_bring_it_back() {
    let root = root("changes");
    // Deduplication makes a rewrite of earlier bytes point at an earlier extent again.
    let e = open_with(&root, |c| c.dedup = true);
    e.create_fs_as("f".into(), "fs").unwrap();
    let (a, b) = (file(&e, "f", "a"), file(&e, "f", "b"));
    let other = e.write_file("f", b, 0, &[2; GRID]).map(|_| tag(&e, "f", b));
    let other = other.unwrap();
    let mut seen = Seen::new(tag(&e, "f", a));
    let one = vec![1u8; 2 * GRID];
    e.write_file("f", a, 0, &one).unwrap();
    seen.fresh(tag(&e, "f", a), "the first write");
    let after = tag(&e, "f", a);
    e.read_file("f", a, 0, 2 * GRID).unwrap();
    e.fs_getattr("f", a).unwrap();
    assert_eq!(tag(&e, "f", a), after, "reading moved the tag");

    e.write_file("f", a, 0, &[9; GRID]).unwrap();
    seen.fresh(tag(&e, "f", a), "an overwrite");
    e.write_file("f", a, 0, &one[..GRID]).unwrap();
    seen.fresh(tag(&e, "f", a), "writing the old bytes back");
    e.write_file("f", a, 10, b"part").unwrap();
    seen.fresh(tag(&e, "f", a), "a partial overwrite");
    e.fs_setattr("f", a, truncate(GRID as u64)).unwrap();
    seen.fresh(tag(&e, "f", a), "a truncate");
    e.fs_setattr("f", a, truncate(2 * GRID as u64)).unwrap();
    seen.fresh(tag(&e, "f", a), "extending with a hole");
    let times = e.fs_getattr("f", a).unwrap();
    e.write_file("f", a, 0, b"x").unwrap();
    e.fs_setattr(
        "f",
        a,
        SetAttr {
            mtime_ns: Some(times.mtime_ns),
            atime_ns: Some(times.atime_ns),
            ..Default::default()
        },
    )
    .unwrap();
    seen.fresh(tag(&e, "f", a), "a write with its times put back");
    e.fs_setxattr("f", a, "user.k", b"v", XattrMode::Set)
        .unwrap();
    seen.fresh(tag(&e, "f", a), "an extended attribute");
    e.fs_link("f", a, ROOT_INO, "a2").unwrap();
    seen.fresh(tag(&e, "f", a), "a hard link");
    e.fs_rename("f", ROOT_INO, "a2", ROOT_INO, "a3").unwrap();
    seen.fresh(tag(&e, "f", a), "a rename");
    assert_eq!(tag(&e, "f", b), other, "another file's changes moved it");

    // A rename over a file leaves the moved inode's tag to its new name.
    e.fs_rename("f", ROOT_INO, "b", ROOT_INO, "a").unwrap();
    let found = e.fs_lookup("f", ROOT_INO, "a").unwrap();
    assert_eq!(found.ino, b);
    assert_ne!(tag(&e, "f", b), tag(&e, "f", a));

    assert_eq!(e.fs_content_tag("f", ROOT_INO).unwrap(), None);
    assert!(e.fs_content_tag("f", 999).is_err());
    assert!(e.fs_content_tag("nope", a).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_delta_write_and_its_fold_move_the_tag() {
    let root = root("delta");
    let e = open_with(&root, |c| {
        c.delta_writes = DeltaConfig {
            enabled: true,
            max_bytes: 1024,
            ..DeltaConfig::default()
        }
    });
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = file(&e, "f", "a");
    e.write_file("f", a, 0, &[1; GRID]).unwrap();
    let mut seen = Seen::new(tag(&e, "f", a));
    e.write_file("f", a, 100, b"delta").unwrap();
    seen.fresh(tag(&e, "f", a), "a delta write");
    e.write_file("f", a, 100, &[1; 5]).unwrap();
    seen.fresh(tag(&e, "f", a), "a delta that writes the old bytes back");
    let pending = tag(&e, "f", a);
    if e.fold_all_deltas().unwrap() > 0 {
        assert_ne!(tag(&e, "f", a), pending);
    }
    assert_eq!(e.read_file("f", a, 0, GRID).unwrap(), [1; GRID]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_tree_replaced_under_the_same_id_has_new_tags_and_they_survive_a_reopen() {
    let root = root("replace");
    let e = open(&root);
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = file(&e, "f", "a");
    e.write_file("f", a, 0, b"original").unwrap();
    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    let live = tag(&e, "f", a);
    let frozen = tag(&e, "f@s", a);
    let attr = e.fs_getattr("f", a).unwrap();
    let mut seen = Seen::new(live);
    e.write_file("f", a, 0, b"changed!").unwrap();
    seen.fresh(tag(&e, "f", a), "a write");
    assert_eq!(tag(&e, "f@s", a), frozen);

    // Restore by replacing the filesystem with a clone of the snapshot: the inode, its extents
    // and its times are all as they were when `live` was read.
    e.delete_fs("f").unwrap();
    e.clone_fs_as("f".into(), "s", "fs").unwrap();
    assert_eq!(e.fs_getattr("f", a).unwrap(), attr);
    seen.fresh(tag(&e, "f", a), "a clone under the same id");
    let cloned = tag(&e, "f", a);
    e.delete_fs("f").unwrap();
    e.clone_fs_as("f".into(), "s", "fs").unwrap();
    seen.fresh(tag(&e, "f", a), "a second clone under the same id");
    let again = tag(&e, "f", a);
    assert_ne!(again, cloned);

    // Replayed from the log, then loaded from the catalog store.
    drop(e);
    let e = open(&root);
    assert_eq!(tag(&e, "f", a), again);
    assert_eq!(tag(&e, "f@s", a), frozen);
    e.checkpoint().unwrap();
    drop(e);
    let e = open(&root);
    assert_eq!(tag(&e, "f", a), again);
    assert_eq!(tag(&e, "f@s", a), frozen);

    // A snapshot id used again is another snapshot, even of the same tree.
    e.delete_fs_snapshot("s").unwrap();
    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    assert_ne!(tag(&e, "f@s", a), frozen);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn replicas_have_no_tag_and_a_revert_does_not_bring_tags_back() {
    let root = root("revert");
    let e = open(&root);
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = file(&e, "f", "a");
    e.write_file("f", a, 0, b"at the snapshot").unwrap();
    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    let at_snapshot = tag(&e, "f", a);
    e.write_file("f", a, 0, b"after it").unwrap();
    let mut seen = Seen::new(at_snapshot);
    seen.fresh(tag(&e, "f", a), "a write");

    // Failback: revert to the snapshot, receive for a while, promote again.
    e.replica_demote("f", "s".into(), "source".into()).unwrap();
    assert_eq!(e.fs_content_tag("f", a).unwrap(), None);
    // A snapshot of a replica is as frozen as any.
    assert!(e.fs_content_tag("f@s", a).unwrap().is_some());
    e.replica_promote("f", false).unwrap();
    assert_eq!(
        e.read_file("f", a, 0, 64).unwrap(),
        b"at the snapshot",
        "reverted"
    );
    seen.fresh(tag(&e, "f", a), "a revert to the snapshot");

    // An empty replica too.
    e.create_replica("r".into(), "r", GRID as u64).unwrap();
    assert_eq!(e.fs_content_tag("r", ROOT_INO).unwrap(), None);
    let _ = std::fs::remove_dir_all(root);
}
