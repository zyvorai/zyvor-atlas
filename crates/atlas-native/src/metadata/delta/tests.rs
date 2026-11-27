// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Delta commands against the catalog alone: rejection leaves it untouched, replay and reload
//! reach the same state, and every path that drops an extent map drops its deltas too.

use serde_json::Value;

use super::*;
use crate::{
    metadata::{MetaCommand, ReplicaRef},
    namespace::{FsOp, NodeType, ReplicaInode, SetAttr, ROOT_INO},
    raft::Message,
    raft_snapshot::{Incoming, Outgoing, SnapshotChunk},
    store::{load_checkpoint, CatalogStore, CATALOG_STORE},
};

const GRID: u64 = 1024;

/// An extent on two nodes at device offset `at`.
fn extent(id: &str, at: u64, len: usize, delta: bool) -> ExtentRef {
    ExtentRef {
        id: id.into(),
        logical_offset: 0,
        payload_len: len,
        raw_len: 0,
        checksum: [7; 32],
        content: None,
        replicas: ["n1", "n2"]
            .iter()
            .map(|n| ReplicaRef {
                node_id: n.to_string(),
                device_index: 0,
                offset: at,
            })
            .collect(),
        ec: None,
        created_ms: 0,
        object: None,
        delta,
    }
}

fn volume(id: &str) -> DeltaTarget {
    DeltaTarget::Volume { id: id.into() }
}

fn file(fs: &str, ino: u64) -> DeltaTarget {
    DeltaTarget::File { fs: fs.into(), ino }
}

fn item(target: DeltaTarget, cell: u64, at: u64, id: &str, len: usize) -> DeltaItem {
    // Device offsets far from the bases, distinct per id.
    let dev = 1 << 20 | id.bytes().fold(0u64, |h, b| h * 131 + u64::from(b)) << 12;
    DeltaItem {
        target,
        cell,
        at,
        extent: extent(id, dev, len, true),
        now_ns: 50,
    }
}

fn volume_deltas(items: Vec<DeltaItem>) -> MetaCommand {
    MetaCommand::AppendDeltas { items }
}

fn file_deltas(items: Vec<DeltaItem>) -> MetaCommand {
    MetaCommand::Fs {
        op: FsOp::AppendDeltas { items },
    }
}

fn fs(op: FsOp) -> MetaCommand {
    MetaCommand::Fs { op }
}

fn mknod(name: &str, node_type: NodeType) -> MetaCommand {
    fs(FsOp::Mknode {
        fs: "f".into(),
        parent: ROOT_INO,
        name: name.into(),
        op_id: name.into(),
        node_type,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
        now_ns: 2,
    })
}

fn install_file(ino: u64, cell: u64, ext: ExtentRef) -> MetaCommand {
    fs(FsOp::InstallFileExtent {
        fs: "f".into(),
        ino,
        logical_offset: cell,
        size: cell + ext.len() as u64,
        extent: ext,
        now_ns: 3,
        replica: false,
    })
}

fn truncate(ino: u64, size: u64) -> MetaCommand {
    fs(FsOp::SetAttr {
        fs: "f".into(),
        ino,
        attr: SetAttr {
            size: Some(size),
            ..Default::default()
        },
        now_ns: 60,
    })
}

fn snapshot_fs(id: &str) -> MetaCommand {
    fs(FsOp::SnapshotFs {
        id: id.into(),
        fs: "f".into(),
        name: id.into(),
        now_ns: 70,
    })
}

fn snapshot_volume(id: &str) -> MetaCommand {
    MetaCommand::CreateSnapshot {
        id: id.into(),
        volume_id: "v".into(),
        name: id.into(),
    }
}

fn fold(
    target: DeltaTarget,
    cell: u64,
    base: &str,
    consumed: &[&str],
    new: ExtentRef,
) -> MetaCommand {
    MetaCommand::FoldCell {
        cell: DeltaCell { target, cell },
        base: base.into(),
        consumed: consumed.iter().map(|s| s.to_string()).collect(),
        extent: new,
    }
}

/// Inodes 2 (`a`, two cells) and 3 (`b`, one cell) in filesystem `f`, inode 4 a directory, and
/// volume `v` with two cells.
fn base_log() -> Vec<MetaCommand> {
    vec![
        MetaCommand::CreateVolume {
            id: "v".into(),
            name: "v".into(),
            size_bytes: 4 * GRID,
        },
        MetaCommand::InstallExtents {
            volume_id: "v".into(),
            extents: [("v0", 0), ("v1", GRID)]
                .iter()
                .map(|(id, cell)| ExtentRef {
                    logical_offset: *cell,
                    ..extent(id, 100_000 + cell, GRID as usize, false)
                })
                .collect(),
            shared: Vec::new(),
        },
        fs(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: Some(GRID),
        }),
        mknod("a", NodeType::File),
        mknod("b", NodeType::File),
        mknod("d", NodeType::Dir),
        install_file(2, 0, extent("a0", 0, GRID as usize, false)),
        install_file(2, GRID, extent("a1", 2048, 512, false)),
        install_file(3, 0, extent("b0", 4096, GRID as usize, false)),
    ]
}

#[derive(Default)]
struct T {
    c: Catalog,
    index: u64,
}

impl T {
    fn new() -> Self {
        let mut t = Self::default();
        for cmd in base_log() {
            t.ok(cmd);
        }
        t
    }

    fn apply(&mut self, cmd: &MetaCommand) -> Result<Vec<ExtentId>, MetaError> {
        self.index += 1;
        let r = self.c.apply_committed(1, self.index, cmd);
        self.c.check_delta_invariants().unwrap();
        r
    }

    fn ok(&mut self, cmd: MetaCommand) -> Vec<ExtentId> {
        self.apply(&cmd).unwrap_or_else(|e| panic!("{cmd:?}: {e}"))
    }

    /// `cmd` is rejected and nothing but the applied index moves, derived totals included.
    fn rejected(&mut self, cmd: MetaCommand) -> MetaError {
        let before = state(&self.c);
        let totals = self.c.delta_totals;
        let err = self.apply(&cmd).expect_err("must be rejected");
        assert_eq!(state(&self.c), before, "{cmd:?} changed the catalog");
        assert_eq!(self.c.delta_totals, totals);
        // And as a proposal would validate it: `apply` itself leaves the catalog alone.
        let mut direct = self.c.clone();
        direct
            .apply(1, self.index + 1, &cmd)
            .expect_err("must be rejected");
        assert_eq!(state(&direct), before);
        err
    }

    fn ids(&self, target: &DeltaTarget, cell: u64) -> Vec<String> {
        self.c
            .cell_state(target, cell)
            .unwrap()
            .deltas
            .iter()
            .map(|d| d.id.clone())
            .collect()
    }

    fn dead(&self, id: &str) -> bool {
        self.c.extents[id].refs == 0
    }
}

/// The catalog as JSON, without the position in the log.
fn state(c: &Catalog) -> Value {
    let mut v = serde_json::to_value(c).unwrap();
    v["applied_index"] = Value::Null;
    v["current_term"] = Value::Null;
    v
}

#[test]
fn appends_keep_size_usage_and_bases_and_stamp_file_times() {
    let mut t = T::new();
    let usage = t.c.filesystem("f").unwrap().usage;
    t.ok(file_deltas(vec![
        item(file("f", 2), 0, 10, "d1", 100),
        item(file("f", 2), GRID, 0, "d2", 512),
        item(file("f", 3), 0, 1000, "d3", 24),
        item(file("f", 2), 0, 10, "d4", 4),
    ]));
    t.ok(volume_deltas(vec![item(volume("v"), GRID, 5, "d5", 8)]));
    assert_eq!(t.ids(&file("f", 2), 0), ["d1", "d4"]);
    assert_eq!(t.ids(&volume("v"), GRID), ["d5"]);
    let f = t.c.filesystem("f").unwrap();
    assert_eq!(f.usage, usage);
    let a = f.inode(2).unwrap();
    assert_eq!((a.size(), a.mtime_ns, a.ctime_ns), (GRID + 512, 50, 50));
    assert_eq!(t.c.dirty.len(), 4);
    assert_eq!(t.c.delta_totals.pending, 5);
    assert_eq!(t.c.delta_totals.pending_bytes, 2 * (100 + 512 + 24 + 4 + 8));
    assert!(t.c.fs_is_dirty("f") && t.c.volume_is_dirty("v"));
    assert!(!t.c.fs_is_dirty("g") && !t.c.volume_is_dirty("w"));
}

#[test]
fn a_rejected_batch_changes_nothing() {
    let mut t = T::new();
    t.ok(file_deltas(vec![item(file("f", 2), 0, 0, "d1", 8)]));
    let good = || item(file("f", 2), 0, 16, "good", 8);
    let mut not_delta = item(file("f", 2), 0, 0, "x", 8);
    not_delta.extent.delta = false;
    let mut compressed = item(file("f", 2), 0, 0, "x", 8);
    compressed.extent.raw_len = 16;
    let mut tiered = item(file("f", 2), 0, 0, "x", 8);
    tiered.extent.replicas.clear();
    tiered.extent.object = Some("k".into());
    let bad = vec![
        // Unknown filesystem, inode, a directory, a cell with no extent.
        item(file("nope", 2), 0, 0, "x", 8),
        item(file("f", 99), 0, 0, "x", 8),
        item(file("f", 4), 0, 0, "x", 8),
        item(file("f", 2), 2 * GRID, 0, "x", 8),
        // Past the base extent (a1 is 512 bytes), past the file, offset overflow.
        item(file("f", 2), GRID, 505, "x", 8),
        item(file("f", 2), 0, u64::MAX, "x", 8),
        // Existing id (a base, a delta), the same id twice, bad extents.
        item(file("f", 2), 0, 0, "a0", 8),
        item(file("f", 2), 0, 0, "d1", 8),
        item(file("f", 2), 0, 0, "good", 8),
        item(file("f", 2), 0, 0, "x", 0),
        not_delta,
        compressed,
        tiered,
        // A volume item in the file command.
        item(volume("v"), 0, 0, "x", 8),
    ];
    for b in bad {
        t.rejected(file_deltas(vec![good(), b]));
    }
    t.rejected(file_deltas(Vec::new()));
    t.rejected(volume_deltas(vec![good()]));
    t.rejected(volume_deltas(vec![
        item(volume("v"), 0, 0, "ok", 8),
        item(volume("v"), 3 * GRID, 0, "x", 8),
    ]));
}

/// A base left reaching past the end of its file (replication can leave one) takes no delta
/// beyond the end: the file's size never changes on a delta write.
#[test]
fn a_delta_must_end_within_the_file_not_only_within_its_base() {
    let mut t = T::new();
    // Shrink the recorded size under the base, as a replica increment can.
    let inode = t.c.fs_mut("f").unwrap().inode_mut(3).unwrap();
    if let InodeKind::File { size, .. } = &mut inode.kind {
        *size = 100;
    }
    t.c.fs_mut("f").unwrap().usage = None;
    t.c.fill_usage().unwrap();
    t.rejected(file_deltas(vec![item(file("f", 3), 0, 96, "x", 8)]));
    t.ok(file_deltas(vec![item(file("f", 3), 0, 92, "ok", 8)]));
}

#[test]
fn replicas_refuse_the_file_command() {
    let mut t = T::new();
    t.ok(snapshot_fs("s"));
    t.ok(fs(FsOp::ReplicaDemote {
        fs: "f".into(),
        snapshot: "s".into(),
        base: "remote".into(),
    }));
    let err = t.rejected(file_deltas(vec![item(file("f", 2), 0, 0, "x", 8)]));
    assert!(matches!(err, MetaError::ReadOnly(_)), "{err}");
}

#[test]
fn fold_is_compare_and_swap_on_the_base_and_the_oldest_deltas() {
    let mut t = T::new();
    for id in ["d1", "d2", "d3"] {
        t.ok(file_deltas(vec![item(file("f", 2), 0, 0, id, 8)]));
    }
    let new = |id: &str, len| extent(id, 1 << 30, len, false);
    let target = || file("f", 2);
    let g = GRID as usize;
    t.rejected(fold(target(), 0, "a1", &["d1"], new("n", g)));
    t.rejected(fold(target(), 0, "a0", &[], new("n", g)));
    t.rejected(fold(target(), 0, "a0", &["d2"], new("n", g)));
    t.rejected(fold(target(), 0, "a0", &["d1", "d3"], new("n", g)));
    t.rejected(fold(
        target(),
        0,
        "a0",
        &["d1", "d2", "d3", "d4"],
        new("n", g),
    ));
    t.rejected(fold(target(), 0, "a0", &["d1"], new("n", g - 1)));
    t.rejected(fold(target(), 0, "a0", &["d1"], new("b0", g)));
    t.rejected(fold(
        target(),
        0,
        "a0",
        &["d1"],
        extent("n", 1 << 30, g, true),
    ));
    t.rejected(fold(target(), GRID, "a1", &["d1"], new("n", 512)));

    let before = t.c.filesystem("f").unwrap().clone();
    let inode = before.inode(2).unwrap();
    let gc = t.ok(fold(target(), 0, "a0", &["d1", "d2"], new("n", g)));
    assert_eq!(gc, ["a0", "d1", "d2"]);
    // The delta appended after the folder looked stays on the new base.
    assert_eq!(t.ids(&target(), 0), ["d3"]);
    let after = t.c.filesystem("f").unwrap();
    let now = after.inode(2).unwrap();
    assert_eq!(
        (now.mtime_ns, now.ctime_ns, now.size()),
        (inode.mtime_ns, inode.ctime_ns, inode.size())
    );
    assert_eq!(after.usage, before.usage);
    // The same fold again finds the base changed.
    t.rejected(fold(target(), 0, "a0", &["d3"], new("n2", g)));
    t.ok(fold(target(), 0, "n", &["d3"], new("n2", g)));
    assert!(t.c.dirty.is_empty());
    assert_eq!(t.c.delta_totals.pending, 0);
}

#[test]
fn batched_reclaim_is_all_or_nothing() {
    let mut t = T::new();
    t.ok(file_deltas(vec![
        item(file("f", 2), 0, 0, "d1", 8),
        item(file("f", 2), 0, 8, "d2", 8),
    ]));
    t.ok(fold(
        file("f", 2),
        0,
        "a0",
        &["d1", "d2"],
        extent("n", 1 << 30, 1024, false),
    ));
    assert_eq!(t.c.delta_totals.dead_bytes, 2 * (1024 + 16));
    let reclaim = |ids: &[&str]| MetaCommand::ReclaimExtents {
        extent_ids: ids.iter().map(|s| s.to_string()).collect(),
    };
    t.rejected(reclaim(&["d1", "n"]));
    t.rejected(reclaim(&["d1", "missing"]));
    t.rejected(reclaim(&["d1", "d1"]));
    let free = t.c.free.total_bytes();
    t.ok(reclaim(&["a0", "d1", "d2"]));
    assert_eq!(t.c.free.total_bytes() - free, 2 * (1024 + 16));
    assert_eq!(t.c.delta_totals.dead_bytes, 0);
}

#[test]
fn snapshots_wait_for_folds_but_a_retried_one_succeeds() {
    let mut t = T::new();
    t.ok(snapshot_fs("s"));
    t.ok(snapshot_volume("vs"));
    t.ok(file_deltas(vec![item(file("f", 2), 0, 0, "d1", 8)]));
    t.ok(volume_deltas(vec![item(volume("v"), 0, 0, "d2", 8)]));
    assert!(matches!(t.rejected(snapshot_fs("s2")), MetaError::Busy(_)));
    assert!(matches!(
        t.rejected(snapshot_volume("vs2")),
        MetaError::Busy(_)
    ));
    t.ok(snapshot_fs("s"));
    t.ok(snapshot_volume("vs"));
}

#[test]
fn truncate_is_refused_only_inside_a_cell_with_deltas() {
    let mut t = T::new();
    t.ok(file_deltas(vec![item(file("f", 2), GRID, 100, "d1", 8)]));
    // Cutting inside the dirty cell is retryable-refused.
    assert!(matches!(
        t.rejected(truncate(2, GRID + 200)),
        MetaError::Busy(_)
    ));
    // Growing, and cutting inside a clean cell below it, work: the dirty cell goes whole.
    t.ok(truncate(2, 8 * GRID));
    let gc = t.ok(truncate(2, 100));
    assert!(gc.contains(&"a1".to_string()) && gc.contains(&"d1".to_string()));
    assert!(t.c.dirty.is_empty());

    // Cutting exactly at a cell boundary needs no fold either.
    t.ok(file_deltas(vec![item(file("f", 3), 0, 0, "d2", 8)]));
    t.ok(truncate(3, 0));
    assert!(t.dead("d2"));
}

/// Every command that replaces or removes an extent map releases the deltas on it.
#[test]
fn every_way_of_dropping_an_extent_drops_its_deltas() {
    let dirty = || {
        let mut t = T::new();
        t.ok(file_deltas(vec![
            item(file("f", 2), 0, 0, "d1", 8),
            item(file("f", 3), 0, 0, "d2", 8),
        ]));
        t.ok(volume_deltas(vec![item(volume("v"), 0, 0, "d3", 8)]));
        t
    };
    let g = GRID as usize;
    let new = |id: &str, cell: u64| ExtentRef {
        logical_offset: cell,
        ..extent(id, 1 << 30, g, false)
    };
    let cases: Vec<(MetaCommand, Vec<&str>)> = vec![
        // The four install commands.
        (
            MetaCommand::InstallExtent {
                volume_id: "v".into(),
                logical_offset: 0,
                extent: new("n", 0),
            },
            vec!["d3"],
        ),
        (
            MetaCommand::InstallExtents {
                volume_id: "v".into(),
                extents: vec![new("n", 0)],
                shared: Vec::new(),
            },
            vec!["d3"],
        ),
        (install_file(2, 0, new("n", 0)), vec!["d1"]),
        (
            fs(FsOp::InstallFileExtents {
                fs: "f".into(),
                ino: 2,
                extents: vec![new("n", 0)],
                size: GRID,
                now_ns: 3,
                replica: false,
                shared: Vec::new(),
            }),
            vec!["d1"],
        ),
        (
            MetaCommand::DeleteVolume {
                volume_id: "v".into(),
            },
            vec!["d3"],
        ),
        (fs(FsOp::DeleteFs { fs: "f".into() }), vec!["d1", "d2"]),
        (
            fs(FsOp::Unlink {
                fs: "f".into(),
                parent: ROOT_INO,
                name: "a".into(),
                now_ns: 4,
            }),
            vec!["d1"],
        ),
        // Rename over an existing file.
        (
            fs(FsOp::Rename {
                fs: "f".into(),
                parent: ROOT_INO,
                name: "a".into(),
                new_parent: ROOT_INO,
                new_name: "b".into(),
                now_ns: 4,
            }),
            vec!["d2"],
        ),
        (truncate(2, 0), vec!["d1"]),
    ];
    for (cmd, gone) in cases {
        let mut t = dirty();
        let gc = t.ok(cmd.clone());
        for id in ["d1", "d2", "d3"] {
            let expected = gone.contains(&id);
            assert_eq!(t.dead(id), expected, "{cmd:?}: {id}");
            assert_eq!(gc.contains(&id.to_string()), expected, "{cmd:?}: {id}");
        }
    }
}

/// Demote and promote revert a tree to a snapshot, and deleting a snapshot releases only what
/// it holds (never the live tree's deltas).
#[test]
fn reverts_and_snapshot_deletes_keep_the_delta_books_straight() {
    let mut t = T::new();
    t.ok(snapshot_fs("s"));
    t.ok(snapshot_fs("other"));
    t.ok(file_deltas(vec![item(file("f", 2), 0, 0, "d1", 8)]));
    t.ok(fs(FsOp::DeleteFsSnapshot {
        id: "other".into(),
        now_unix: 0,
    }));
    assert!(!t.dead("d1"));
    t.ok(fs(FsOp::ReplicaDemote {
        fs: "f".into(),
        snapshot: "s".into(),
        base: "remote".into(),
    }));
    assert!(t.dead("d1") && t.c.dirty.is_empty());

    // A replica never accepts deltas; should one ever hold some, an increment that cuts,
    // clears or removes the file, and a promotion that reverts it, still release them.
    let plant = |t: &mut T, ino: u64, id: &str| {
        let e = extent(id, 1 << 25, 8, true);
        t.c.add_extent_ref(&e);
        let inode = t.c.fs_mut("f").unwrap().inode_mut(ino).unwrap();
        if let InodeKind::File { deltas, .. } = &mut inode.kind {
            deltas.insert(
                0,
                vec![DeltaRef {
                    id: id.into(),
                    at: 0,
                }],
            );
        }
        t.c.dirty.insert(
            DeltaCell {
                target: file("f", ino),
                cell: 0,
            },
            1,
        );
        t.c.check_delta_invariants().unwrap();
    };
    let increment = |inodes: Vec<ReplicaInode>, removed: Vec<u64>| {
        fs(FsOp::ReplicaApply {
            fs: "f".into(),
            from: Some("remote".into()),
            to: "next".into(),
            inodes,
            removed,
        })
    };
    let changed = |size: u64, holes: Vec<u64>| ReplicaInode {
        ino: 2,
        node_type: NodeType::File,
        mode: 0o644,
        uid: 0,
        gid: 0,
        nlink: 1,
        atime_ns: 1,
        mtime_ns: 1,
        ctime_ns: 1,
        xattrs: Default::default(),
        size,
        holes,
        parent: 0,
        entries: Vec::new(),
        target: None,
        rdev: 0,
    };
    plant(&mut t, 2, "hole");
    t.ok(increment(vec![changed(GRID + 512, vec![0])], Vec::new()));
    assert!(t.dead("hole"));
    plant(&mut t, 3, "removed");
    t.ok(increment(Vec::new(), vec![3]));
    assert!(t.dead("removed"));
    // Promotion with a partial increment reverts to the last complete one.
    let inode = t.c.fs_mut("f").unwrap().inode_mut(2).unwrap();
    if let InodeKind::File { extents, .. } = &mut inode.kind {
        extents.insert(0, "b0".into());
    }
    t.c.extents.get_mut("b0").unwrap().refs += 1;
    plant(&mut t, 2, "cut");
    t.ok(increment(vec![changed(0, Vec::new())], Vec::new()));
    assert!(t.dead("cut"));
    t.ok(fs(FsOp::ReplicaPromote {
        fs: "f".into(),
        force: false,
    }));
}

/// A log with every delta command, including rejected ones.
fn delta_log() -> Vec<MetaCommand> {
    let mut log = base_log();
    let n = |id: &str, len: usize| extent(id, 1 << 30, len, false);
    log.extend([
        file_deltas(vec![
            item(file("f", 2), 0, 0, "d1", 8),
            item(file("f", 3), 0, 8, "d2", 8),
        ]),
        volume_deltas(vec![item(volume("v"), 0, 0, "d3", 8)]),
        snapshot_fs("rejected"),
        file_deltas(vec![item(file("f", 2), 0, 4, "d4", 8)]),
        fold(file("f", 2), 0, "a0", &["d1"], n("n1", 1024)),
        fold(file("f", 2), 0, "a0", &["d4"], n("lost", 1024)),
        truncate(3, 100),
        file_deltas(vec![item(file("f", 3), 0, 2000, "bad", 8)]),
        fold(file("f", 3), 0, "b0", &["d2"], n("n2", 1024)),
        truncate(3, 100),
        MetaCommand::ReclaimExtents {
            extent_ids: vec!["a0".into(), "d1".into()],
        },
        MetaCommand::MarkExtentReclaimed {
            extent_id: "d2".into(),
        },
        file_deltas(vec![item(file("f", 3), 0, 1, "d5", 8)]),
        fold(volume("v"), 0, "v0", &["d3"], n("n3", 1024)),
        snapshot_volume("vs"),
        volume_deltas(vec![item(volume("v"), GRID, 9, "d6", 8)]),
    ]);
    log
}

/// The same log reaches the same state whether it is applied straight through, with a reload
/// from the catalog store after every command, or from a snapshot streamed to a follower halfway.
#[test]
fn replay_reload_and_snapshot_transfer_reach_the_same_state() {
    let log = delta_log();
    let apply = |c: &mut Catalog, i: usize| {
        let _ = c.apply_committed(1, i as u64 + 1, &log[i]);
    };
    let mut straight = Catalog::default();
    (0..log.len()).for_each(|i| apply(&mut straight, i));
    straight.check_delta_invariants().unwrap();
    assert!(!straight.dirty.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let mut store = CatalogStore::open(dir.path().join(CATALOG_STORE)).unwrap();
    let mut reloaded = Catalog::default();
    for i in 0..log.len() {
        apply(&mut reloaded, i);
        store.checkpoint(&mut reloaded).unwrap();
        if i % 3 == 0 {
            drop(reloaded);
            drop(store);
            store = CatalogStore::open(dir.path().join(CATALOG_STORE)).unwrap();
            reloaded = load_checkpoint(dir.path(), &store).unwrap();
            reloaded.check_delta_invariants().unwrap();
        }
    }
    assert_eq!(state(&reloaded), state(&straight));
    assert_eq!(reloaded.delta_totals, straight.delta_totals);
    assert_eq!(reloaded.dirty, straight.dirty);

    for half in [log.len() - 1, log.len() - 5] {
        let mut leader = Catalog::default();
        (0..half).for_each(|i| apply(&mut leader, i));
        assert!(!leader.dirty.is_empty());
        let follower = tempfile::tempdir().unwrap();
        let wire = |o: &Outgoing| -> SnapshotChunk {
            let msg = Message::InstallSnapshot {
                term: 1,
                index: o.index,
                seq: o.seq,
                chunk: Box::new(o.chunk.clone()),
            };
            match serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap() {
                Message::InstallSnapshot { chunk, .. } => *chunk,
                other => panic!("{other:?}"),
            }
        };
        let mut out = Outgoing::start(leader.clone(), 0);
        let SnapshotChunk::Header { catalog } = wire(&out) else {
            panic!("a transfer starts with the header");
        };
        let mut inc = Incoming::start(follower.path(), out.index, *catalog).unwrap();
        loop {
            out.advance(0).unwrap();
            if out.is_done() {
                break;
            }
            inc.stage(wire(&out)).unwrap();
        }
        let path = follower.path().join(CATALOG_STORE);
        std::fs::rename(inc.finish(), &path).unwrap();
        let store = CatalogStore::open(&path).unwrap();
        let mut installed = load_checkpoint(follower.path(), &store).unwrap();
        assert_eq!(installed.dirty, leader.dirty);
        assert_eq!(installed.delta_totals, leader.delta_totals);
        (half..log.len()).for_each(|i| apply(&mut installed, i));
        installed.check_delta_invariants().unwrap();
        assert_eq!(state(&installed), state(&straight));
    }
}

/// A catalog that never held a delta encodes exactly as before, so older builds read it.
#[test]
fn nothing_new_is_encoded_without_deltas() {
    let t = T::new();
    let json = serde_json::to_string(&t.c).unwrap();
    for word in ["\"delta\"", "\"deltas\"", "\"dirty\""] {
        assert!(!json.contains(word), "{word} in {json}");
    }
    for cmd in base_log() {
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains("delta"), "{json}");
    }
    // The delta commands survive the log encoding.
    for cmd in delta_log() {
        let json = serde_json::to_vec(&cmd).unwrap();
        let back: MetaCommand = serde_json::from_slice(&json).unwrap();
        assert_eq!(serde_json::to_vec(&back).unwrap(), json);
    }
}
