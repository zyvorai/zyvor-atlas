// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use atlas_native::{
    engine::NewNode, metadata::MetaError, EngineConfig, FailureDomain, FsQuota, NativeEngine,
    NativeError, Node, NodeType, SetAttr, ROOT_INO,
};

fn engine(name: &str) -> (NativeEngine, std::path::PathBuf) {
    let root =
        std::env::temp_dir().join(format!("atlas-native-fs-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    (open(&root), root)
}

fn open(root: &std::path::Path) -> NativeEngine {
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
    cfg.extent_bytes = 16;
    NativeEngine::open(cfg, nodes).unwrap()
}

fn node(name: &str, kind: NodeType) -> NewNode {
    NewNode {
        name: name.into(),
        op_id: format!("op-{name}"),
        kind,
        target: (kind == NodeType::Symlink).then(|| "target".into()),
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 1000,
        gid: 1000,
    }
}

fn is_quota<T>(r: Result<T, NativeError>) -> bool {
    matches!(r, Err(NativeError::Metadata(MetaError::Quota(_))))
}

fn truncate(size: u64) -> SetAttr {
    SetAttr {
        size: Some(size),
        ..Default::default()
    }
}

/// Every write and truncate is mirrored into a plain buffer and the whole file compared.
#[test]
fn file_io_matches_a_flat_buffer() {
    let (e, root) = engine("model");
    e.create_fs_as("f".into(), "fs").unwrap();
    let ino = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    let mut model: Vec<u8> = Vec::new();
    let mut seed = 0x9e37_79b9_u64;
    let mut next = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    for step in 0..200u8 {
        if next(5) == 0 {
            let size = next(120);
            e.fs_setattr("f", ino, truncate(size)).unwrap();
            model.resize(size as usize, 0);
        } else {
            let off = next(100);
            let data = vec![step; 1 + next(40) as usize];
            let a = e.write_file("f", ino, off, &data).unwrap();
            let end = off as usize + data.len();
            if model.len() < end {
                model.resize(end, 0);
            }
            model[off as usize..end].copy_from_slice(&data);
            assert_eq!(a.size, model.len() as u64);
        }
        let got = e.read_file("f", ino, 0, 4096).unwrap();
        assert_eq!(got, model, "after step {step}");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn holes_and_end_of_file() {
    let (e, root) = engine("holes");
    e.create_fs_as("f".into(), "fs").unwrap();
    let ino = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    let a = e.write_file("f", ino, 100, b"xy").unwrap();
    assert_eq!(a.size, 102);
    // Only the extent holding the two bytes is allocated.
    assert_eq!(a.blocks, 1);
    let data = e.read_file("f", ino, 0, 200).unwrap();
    assert_eq!(data.len(), 102);
    assert!(data[..100].iter().all(|b| *b == 0));
    assert_eq!(&data[100..], b"xy");
    assert!(e.read_file("f", ino, 102, 10).unwrap().is_empty());
    assert!(e.read_file("f", ino, 500, 10).unwrap().is_empty());

    // Truncating into an extent and growing again must not resurrect the cut bytes.
    e.write_file("f", ino, 0, &[7u8; 16]).unwrap();
    e.fs_setattr("f", ino, truncate(4)).unwrap();
    e.fs_setattr("f", ino, truncate(16)).unwrap();
    let data = e.read_file("f", ino, 0, 16).unwrap();
    assert_eq!(&data[..4], &[7u8; 4]);
    assert!(data[4..].iter().all(|b| *b == 0));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn removed_and_truncated_data_is_reclaimed() {
    let (e, root) = engine("gc");
    e.create_fs_as("f".into(), "fs").unwrap();
    let ino = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    e.write_file("f", ino, 0, &[1u8; 64]).unwrap();
    e.fs_setattr("f", ino, truncate(16)).unwrap();
    assert_eq!(e.gc_once().unwrap().reclaimed, 3);
    e.fs_unlink("f", ROOT_INO, "a").unwrap();
    assert_eq!(e.gc_once().unwrap().reclaimed, 1);
    assert!(matches!(
        e.fs_getattr("f", ino),
        Err(NativeError::Metadata(_))
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn namespace_survives_reopen_and_snapshots_are_frozen() {
    let (e, root) = engine("reopen");
    e.create_fs_as("f".into(), "fs").unwrap();
    let d = e
        .fs_mknode("f", ROOT_INO, node("d", NodeType::Dir))
        .unwrap()
        .ino;
    let a = e.fs_mknode("f", d, node("a", NodeType::File)).unwrap().ino;
    let l = e.fs_mknode("f", d, node("l", NodeType::Symlink)).unwrap();
    assert_eq!(l.size, 6);
    // A retried create returns the same inode.
    assert_eq!(
        e.fs_mknode("f", d, node("a", NodeType::File)).unwrap().ino,
        a
    );
    e.write_file("f", a, 0, b"old contents").unwrap();
    e.snapshot_fs_as("s1".into(), "f", "s1").unwrap();
    e.write_file("f", a, 0, b"NEW").unwrap();
    e.fs_rename("f", d, "a", ROOT_INO, "moved").unwrap();
    assert!(matches!(
        e.write_file("f@s1", a, 0, b"x"),
        Err(NativeError::ReadOnly(_))
    ));
    drop(e);

    let e = open(&root);
    let names: Vec<String> = e
        .fs_readdir("f", ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["d", "moved"]);
    assert_eq!(e.fs_readlink("f", l.ino).unwrap(), "target");
    assert_eq!(e.fs_lookup("f@s1", d, "a").unwrap().ino, a);
    assert_eq!(e.read_file("f@s1", a, 0, 64).unwrap(), b"old contents");
    assert_eq!(e.read_file("f", a, 0, 64).unwrap(), b"NEW contents");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn quotas_are_enforced_on_writes_and_survive_reopen() {
    let (e, root) = engine("quota");
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    let quota = FsQuota {
        max_bytes: Some(32),
        max_inodes: Some(2),
    };
    e.fs_set_quota("f", quota).unwrap();
    // Two 16-byte extents fill it; a partial rewrite of one stays within it.
    e.write_file("f", a, 0, &[1; 32]).unwrap();
    e.write_file("f", a, 4, b"abcd").unwrap();
    assert!(is_quota(e.write_file("f", a, 32, b"x")));
    assert!(is_quota(e.fs_mknode(
        "f",
        ROOT_INO,
        node("b", NodeType::File)
    )));
    assert_eq!(e.fs_statfs("f").unwrap().quota, Some(quota));
    assert!(matches!(
        e.fs_set_quota("f@s", FsQuota::default()),
        Err(NativeError::ReadOnly(_))
    ));
    e.checkpoint().unwrap();
    drop(e);

    let e = open(&root);
    assert_eq!(e.fs_quota("f").unwrap(), Some(quota));
    assert!(is_quota(e.write_file("f", a, 32, b"x")));
    let mut data = vec![1; 32];
    data[4..8].copy_from_slice(b"abcd");
    assert_eq!(e.read_file("f", a, 0, 64).unwrap(), data);
    e.fs_set_quota("f", FsQuota::default()).unwrap();
    assert_eq!(e.fs_quota("f").unwrap(), None);
    e.write_file("f", a, 32, b"x").unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn snapshot_reads_and_clones_are_isolated() {
    let (e, root) = engine("clone");
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    e.write_file("f", a, 0, b"version one, long").unwrap();
    e.snapshot_fs_as("s1".into(), "f", "s1").unwrap();
    e.clone_fs_as("c".into(), "s1", "clone").unwrap();
    e.write_file("f", a, 0, b"VERSION TWO").unwrap();
    e.write_file("c", a, 8, b"CLONE").unwrap();
    assert_eq!(e.read_file("f", a, 0, 64).unwrap(), b"VERSION TWO, long");
    assert_eq!(e.read_file("f@s1", a, 0, 64).unwrap(), b"version one, long");
    assert_eq!(e.read_file("c", a, 0, 64).unwrap(), b"version CLONElong");
    e.delete_fs_snapshot("s1").unwrap();
    e.delete_fs("c").unwrap();
    assert!(e.gc_once().unwrap().reclaimed > 0);
    assert_eq!(e.read_file("f", a, 0, 64).unwrap(), b"VERSION TWO, long");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn filesystem_grid_is_its_own_and_survives_snapshot_and_clone() {
    let (e, root) = engine("grid");
    // The cluster grid in these tests is 16 bytes.
    assert!(e.create_fs_with("big".into(), "big", Some(32)).is_err());
    assert!(e.create_fs_with("zero".into(), "zero", Some(0)).is_err());
    e.create_fs_with("f".into(), "fs", Some(4)).unwrap();
    e.create_fs_as("d".into(), "default").unwrap();
    let grid = |id: &str| {
        e.filesystems()
            .unwrap()
            .into_iter()
            .find(|f| f.id == id)
            .unwrap()
            .extent_bytes
    };
    assert_eq!(grid("f"), 4);
    assert_eq!(grid("d"), 16, "the default never exceeds the cluster grid");

    let ino = e
        .fs_mknode("f", ROOT_INO, node("a", NodeType::File))
        .unwrap()
        .ino;
    let mut model = vec![0u8; 0];
    for (off, len, b) in [
        (0u64, 23usize, 1u8),
        (5, 1, 2),
        (3, 6, 3),
        (30, 2, 4),
        (17, 9, 5),
    ] {
        e.write_file("f", ino, off, &vec![b; len]).unwrap();
        let end = off as usize + len;
        if model.len() < end {
            model.resize(end, 0);
        }
        model[off as usize..end].fill(b);
    }
    assert_eq!(e.read_file("f", ino, 0, 64).unwrap(), model);
    assert_eq!(e.read_file("f", ino, 6, 7).unwrap(), model[6..13]);
    e.fs_setattr("f", ino, truncate(10)).unwrap();
    e.fs_setattr("f", ino, truncate(14)).unwrap();
    model.truncate(10);
    model.resize(14, 0);
    assert_eq!(e.read_file("f", ino, 0, 64).unwrap(), model);

    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    e.clone_fs_as("c".into(), "s", "c").unwrap();
    assert_eq!(grid("c"), 4);
    e.write_file("c", ino, 1, b"zz").unwrap();
    assert_eq!(e.read_file("f@s", ino, 0, 64).unwrap(), model);
    assert_eq!(e.read_file("f", ino, 0, 64).unwrap(), model);
    model[1..3].copy_from_slice(b"zz");
    assert_eq!(e.read_file("c", ino, 0, 64).unwrap(), model);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn locked_fs_snapshot_refuses_delete_and_only_extends() {
    let (e, root) = engine("fs-worm");
    e.create_fs_as("f".into(), "fs").unwrap();
    e.snapshot_fs_as("s1".into(), "f", "s1").unwrap();

    let until = e.lock_fs_snapshot("s1", 3600).unwrap();
    assert!(e.delete_fs_snapshot("s1").is_err());
    assert!(e.lock_fs_snapshot("s1", 10).is_err());
    assert!(e.lock_fs_snapshot("s1", 7200).unwrap() > until);
    assert!(e.delete_fs_snapshot("s1").is_err());
    assert!(e.lock_fs_snapshot("nope", 60).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn filesystem_activity_counts_writes_renames_deletes_and_entropy() {
    let (e, root) = engine("activity");
    e.create_fs_as("f".into(), "fs").unwrap();
    let a = e.fs_mknode("f", ROOT_INO, node("a", NodeType::File)).unwrap().ino;
    let b = e.fs_mknode("f", ROOT_INO, node("b", NodeType::File)).unwrap().ino;

    // Near-random 8 KiB (what encrypted data looks like) into `a`, zeros into `b`.
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    let random: Vec<u8> = (0..8192)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    e.write_file("f", a, 0, &random).unwrap();
    e.write_file("f", b, 0, &[0u8; 4096]).unwrap();
    e.fs_rename("f", ROOT_INO, "a", ROOT_INO, "a.locked").unwrap();
    e.fs_unlink("f", ROOT_INO, "b").unwrap();

    let s = e.fs_activity.samples("f");
    let (mut files, mut renamed, mut deleted, mut bytes, mut ratio) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for w in &s {
        files += w.files_modified;
        renamed += w.files_renamed;
        deleted += w.files_deleted;
        bytes += w.bytes_written;
        ratio = f64::max(ratio, w.high_entropy_ratio);
    }
    // The run may straddle a window boundary, so check totals across the windows.
    assert_eq!((renamed, deleted, bytes), (1.0, 1.0, 12288.0));
    assert!(files >= 2.0);
    assert!(ratio > 0.5, "random data should read as high entropy: {ratio}");
    assert!(e.fs_activity.samples("other").is_empty() || e.fs_activity.samples("other")[0].bytes_written == 0.0);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn gateway_temp_file_renames_do_not_count_as_mass_renaming() {
    let (e, root) = engine("activity-internal");
    e.create_fs_as("f".into(), "fs").unwrap();
    for i in 0..20 {
        let tmp = format!(".atlas_s3_tmp_{i}");
        let ino = e.fs_mknode("f", ROOT_INO, node(&tmp, NodeType::File)).unwrap().ino;
        e.write_file("f", ino, 0, b"object body").unwrap();
        e.fs_rename("f", ROOT_INO, &tmp, ROOT_INO, &format!("obj{i}")).unwrap();
    }
    let renamed: f64 = e.fs_activity.samples("f").iter().map(|w| w.files_renamed).sum();
    let modified: f64 = e.fs_activity.samples("f").iter().map(|w| w.files_modified).sum();
    assert_eq!(renamed, 0.0);
    assert!(modified >= 20.0, "S3-style writes are still seen as writes");
    let _ = std::fs::remove_dir_all(root);
}
