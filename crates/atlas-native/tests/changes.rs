// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The change listing of a cloned filesystem through the engine: real writes, paging, cursors,
//! checkpoints and reopen, concurrent changes and the refusals. The classification itself is
//! tested against a model in `src/engine/changes/tests.rs`.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use atlas_native::{
    engine::{ChangeKind, ChangeLimits, ChangedField, FsChange, NewNode},
    metadata::MetaError,
    DeltaConfig, EngineConfig, FailureDomain, NativeEngine, NativeError, Node, NodeType, SetAttr,
    ROOT_INO,
};

const GRID: usize = 4096;

fn root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("atlas-native-changes-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn config(root: &Path) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = GRID;
    // Checkpoint often, so listings read paged live filesystems.
    c.wal_compact_after = 8;
    c
}

fn open(cfg: EngineConfig) -> NativeEngine {
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
    NativeEngine::open(cfg, nodes).unwrap()
}

fn mk(e: &NativeEngine, fs: &str, parent: u64, name: &str, kind: NodeType) -> u64 {
    e.fs_mknode(
        fs,
        parent,
        NewNode {
            name: name.into(),
            op_id: uuid::Uuid::new_v4().to_string(),
            kind,
            target: None,
            rdev: 0,
            mode: 0o644,
            create_mode: None,
            uid: 1000,
            gid: 1000,
        },
    )
    .unwrap()
    .ino
}

fn lines(changes: &[FsChange]) -> Vec<(ChangeKind, &str)> {
    changes.iter().map(|c| (c.change, c.path.as_str())).collect()
}

/// Filesystem `ws`, cloned from snapshot `s` of `src`: `/dir/file` and `/f0`..`/f9`. Listings are
/// kept per filesystem id for the whole process, so tests that page use their own.
fn forked(e: &NativeEngine, ws: &str) -> (u64, u64) {
    e.create_fs_as("src".into(), "src").unwrap();
    let dir = mk(e, "src", ROOT_INO, "dir", NodeType::Dir);
    let file = mk(e, "src", dir, "file", NodeType::File);
    e.write_file("src", file, 0, &[1; 3 * GRID]).unwrap();
    for i in 0..10 {
        mk(e, "src", ROOT_INO, &format!("f{i}"), NodeType::File);
    }
    e.snapshot_fs_as("s".into(), "src", "s").unwrap();
    e.clone_fs_as(ws.into(), "s", ws).unwrap();
    (dir, file)
}

fn unsupported<T: std::fmt::Debug>(r: Result<T, NativeError>, says: &str) {
    match r {
        Err(NativeError::Metadata(MetaError::Unsupported(m))) => assert!(m.contains(says), "{m}"),
        other => panic!("wanted a refusal saying {says:?}, got {other:?}"),
    }
}

#[test]
fn writes_are_listed_and_pages_describe_one_instant() {
    let root = root("pages");
    let e = open(config(&root));
    let (dir, file) = forked(&e, "w");
    let empty = e.fs_changes("w", None, 10).unwrap();
    assert_eq!((empty.total, empty.next.as_deref()), (0, None));
    assert_eq!(empty.base_snapshot, "s");
    assert_eq!(empty.fs, "w");

    // The source moving on is not the workspace's change.
    e.write_file("src", file, 0, b"source").unwrap();
    mk(&e, "src", ROOT_INO, "source-only", NodeType::File);

    e.write_file("w", file, 100, b"workspace").unwrap();
    for i in 0..4 {
        mk(&e, "w", dir, &format!("new{i}"), NodeType::File);
    }
    e.fs_unlink("w", ROOT_INO, "f3").unwrap();
    e.fs_rename("w", ROOT_INO, "f4", dir, "was-f4").unwrap();

    let all = e.fs_changes("w", None, 100).unwrap();
    assert!(all.next.is_none() && all.as_of_index > 0 && all.computed_unix_ms > 0);
    assert_eq!(
        lines(&all.changes),
        [
            (ChangeKind::Modified, "/dir/file"),
            (ChangeKind::Created, "/dir/new0"),
            (ChangeKind::Created, "/dir/new1"),
            (ChangeKind::Created, "/dir/new2"),
            (ChangeKind::Created, "/dir/new3"),
            (ChangeKind::Renamed, "/dir/was-f4"),
            (ChangeKind::Deleted, "/f3"),
        ]
    );
    assert_eq!(all.total, 7);
    let s = all.summary;
    assert_eq!((s.created, s.modified, s.deleted, s.renamed), (4, 1, 1, 1));
    let edit = &all.changes[0];
    assert!(edit.changed.contains(&ChangedField::Content));
    assert!(!edit.changed.contains(&ChangedField::Size), "{edit:?}");
    assert_eq!(edit.size, Some(3 * GRID as u64));

    // Pages of two, with the workspace changing between them.
    let first = e.fs_changes("w", None, 2).unwrap();
    let mut got = first.changes.clone();
    let mut next = first.next.clone();
    let mut pages = 1;
    while let Some(cursor) = next {
        mk(&e, "w", ROOT_INO, &format!("late{pages}"), NodeType::File);
        let page = e.fs_changes("w", Some(&cursor), 2).unwrap();
        assert_eq!(
            (page.total, page.as_of_index),
            (first.total, first.as_of_index)
        );
        got.extend(page.changes);
        next = page.next;
        pages += 1;
    }
    assert_eq!(pages, 4);
    assert_eq!(got, all.changes);
    // A new listing sees what was written meanwhile.
    assert_eq!(e.fs_changes("w", None, 100).unwrap().total, 10);

    // A cursor belongs to one filesystem.
    let cursor = first.next.unwrap();
    e.clone_fs_as("w2".into(), "s", "w2").unwrap();
    unsupported(e.fs_changes("w2", Some(&cursor), 2), "cursor");
    unsupported(e.fs_changes("w", Some("0123456789abcdef.2"), 2), "cursor");
    for bad in ["", "x", ".", "abc.x", "abc.-1"] {
        assert!(
            matches!(
                e.fs_changes("w", Some(bad), 2),
                Err(NativeError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    // An offset past the end is an empty last page.
    let (id, _) = cursor.split_once('.').unwrap();
    let past = e
        .fs_changes("w", Some(&format!("{id}.999999")), 2)
        .unwrap();
    assert!(past.changes.is_empty() && past.next.is_none());

    // Reopened, the live filesystems are paged from the store and the answer is the same.
    let before = e.fs_changes("w", None, 100).unwrap();
    drop(e);
    let e = open(config(&root));
    let after = e.fs_changes("w", None, 100).unwrap();
    assert_eq!(after.changes, before.changes);

    // A deleted filesystem has no listing, cached or not.
    let paged = e.fs_changes("w", None, 1).unwrap();
    e.delete_fs("w").unwrap();
    for cursor in [None, paged.next.as_deref()] {
        assert!(matches!(
            e.fs_changes("w", cursor, 1),
            Err(NativeError::Metadata(MetaError::NotFound(_)))
        ));
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn only_a_clone_with_its_snapshot_can_be_listed() {
    let root = root("refusals");
    let e = open(config(&root));
    forked(&e, "r");
    unsupported(e.fs_changes("src", None, 10), "not cloned");
    assert!(matches!(
        e.fs_changes("r@s", None, 10),
        Err(NativeError::Invalid(_))
    ));
    assert!(matches!(
        e.fs_changes("nope", None, 10),
        Err(NativeError::Metadata(MetaError::NotFound(_)))
    ));
    let limits = ChangeLimits {
        max_inodes: 3,
        ..ChangeLimits::default()
    };
    assert!(matches!(
        e.fs_changes_within("r", None, 10, limits),
        Err(NativeError::Metadata(MetaError::TooBig(_)))
    ));
    mk(&e, "r", ROOT_INO, "new", NodeType::File);
    let paged = {
        mk(&e, "r", ROOT_INO, "new2", NodeType::File);
        e.fs_changes("r", None, 1).unwrap()
    };
    e.delete_fs_snapshot("s").unwrap();
    unsupported(e.fs_changes("r", None, 10), "has been deleted");
    // A listing computed while the snapshot existed can still be read to its end.
    let rest = e.fs_changes("r", paged.next.as_deref(), 10).unwrap();
    assert_eq!(rest.changes.len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn small_overwrites_held_as_deltas_are_content_changes() {
    let root = root("deltas");
    let mut cfg = config(&root);
    cfg.delta_writes = DeltaConfig {
        enabled: true,
        max_bytes: 1024,
        max_deltas_per_cell: 6,
        fold_after_deltas: 1000,
        fold_after_ms: 600_000,
        ..DeltaConfig::default()
    };
    let e = open(cfg);
    let (_, file) = forked(&e, "d");
    e.write_file("d", file, 7, b"tiny").unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 1);
    let got = e.fs_changes("d", None, 10).unwrap().changes;
    assert_eq!(lines(&got), [(ChangeKind::Modified, "/dir/file")]);
    assert!(got[0].changed.contains(&ChangedField::Content));
    // Folded by the snapshot, the answer is the same.
    e.snapshot_fs_as("s2".into(), "d", "s2").unwrap();
    assert_eq!(e.fs_changes("d", None, 10).unwrap().changes, got);
    let _ = std::fs::remove_dir_all(root);
}

/// Stops the writer thread however the listing side ends.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A file renamed back and forth while it is listed: every listing shows it once, wherever it was
/// at that instant.
#[test]
fn a_listing_is_one_instant_of_a_changing_filesystem() {
    let root = root("concurrent");
    let e = open(config(&root));
    let (dir, file) = forked(&e, "c");
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut round = 0;
            while !stop.load(Ordering::Relaxed) {
                e.fs_rename("c", dir, "file", ROOT_INO, "moved").unwrap();
                e.fs_rename("c", ROOT_INO, "moved", dir, "file").unwrap();
                // Dirty other inodes so checkpoints keep happening.
                let name = format!("f{}", round % 10);
                let ino = e.fs_lookup("c", ROOT_INO, &name).unwrap().ino;
                e.fs_setattr(
                    "c",
                    ino,
                    SetAttr {
                        mode: Some(0o600),
                        ..Default::default()
                    },
                )
                .unwrap();
                round += 1;
            }
        });
        let _stop = StopOnDrop(&stop);
        for _ in 0..200 {
            let got = e.fs_changes("c", None, 100).unwrap().changes;
            let mine: Vec<&FsChange> = got.iter().filter(|c| c.ino == file).collect();
            assert!(mine.len() <= 1, "{mine:?}");
            if let Some(c) = mine.first() {
                match c.change {
                    ChangeKind::Renamed => {
                        assert_eq!(c.path, "/moved");
                        assert_eq!(c.from.as_deref(), Some("/dir/file"));
                    }
                    ChangeKind::Modified => assert_eq!(c.changed, [ChangedField::Ctime]),
                    other => panic!("{other:?}: {c:?}"),
                }
            }
            assert!(
                got.iter()
                    .all(|c| c.ino == file || c.change == ChangeKind::Modified),
                "{got:?}"
            );
        }
    });
    let _ = std::fs::remove_dir_all(root);
}
