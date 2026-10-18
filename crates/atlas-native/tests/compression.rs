// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! LZ4-compressed extents: space saved, byte-exact reads (whole, partial overwrite, snapshots),
//! erasure-coded payloads, repair, and incompressible data left alone.

use std::path::Path;

use atlas_native::{ec::EcScheme, EngineConfig, FailureDomain, NativeEngine, Node, NodeType};

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

fn engine(root: &Path, compression: bool, erasure: Option<&str>) -> NativeEngine {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = EXTENT;
    c.compression = compression;
    c.erasure = erasure.map(|s| s.parse::<EcScheme>().unwrap());
    c.erasure_min_bytes = 4096;
    NativeEngine::open(c, (1..=6).map(node).collect()).unwrap()
}

fn used(e: &NativeEngine) -> u64 {
    (1..=6)
        .map(|i| e.device_len(&format!("n{i}")).unwrap())
        .sum()
}

fn text(n: usize) -> Vec<u8> {
    b"ts=2026-10-18T10:00:00Z level=info component=atlas msg=\"extent written\"\n"
        .iter()
        .copied()
        .cycle()
        .take(n)
        .collect()
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
    let p = std::env::temp_dir().join(format!("atlas-compress-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Bytes the devices hold after writing `data` to a fresh volume, and what a read returns.
fn stored(compression: bool, erasure: Option<&str>, data: &[u8]) -> (u64, Vec<u8>) {
    let r = root("stored");
    let e = engine(&r, compression, erasure);
    let v = e.create_volume("v", 4 << 20).unwrap();
    let before = used(&e);
    e.write(&v, 0, data).unwrap();
    let got = e.read(&v, 0, data.len()).unwrap();
    let after = used(&e);
    let _ = std::fs::remove_dir_all(r);
    (after - before, got)
}

#[test]
fn compressible_data_takes_less_space_and_reads_back_exactly() {
    let data = text(8 * EXTENT);
    let (plain, got_plain) = stored(false, None, &data);
    let (packed, got_packed) = stored(true, None, &data);
    assert_eq!(got_plain, data);
    assert_eq!(got_packed, data);
    assert!(
        packed * 4 < plain,
        "text should shrink well over 4x: {packed} vs {plain}"
    );
}

#[test]
fn erasure_coded_payloads_shrink_and_read_back_exactly() {
    let data = text(8 * EXTENT);
    let (plain, _) = stored(false, Some("4+2"), &data);
    let (packed, got) = stored(true, Some("4+2"), &data);
    assert_eq!(got, data);
    assert!(packed * 4 < plain, "{packed} vs {plain}");
}

#[test]
fn incompressible_data_is_stored_as_is() {
    let data = random(11, 4 * EXTENT);
    let (plain, _) = stored(false, None, &data);
    let (packed, got) = stored(true, None, &data);
    assert_eq!(got, data);
    assert_eq!(packed, plain, "random data must not be compressed or grow");
}

#[test]
fn partial_overwrites_snapshots_and_clones_stay_byte_exact() {
    let r = root("overwrite");
    let e = engine(&r, true, None);
    let v = e.create_volume("v", 4 << 20).unwrap();
    let mut model = text(3 * EXTENT);
    e.write(&v, 0, &model).unwrap();

    // Overwrite across an extent boundary with data that does not compress.
    let noise = random(5, 5000);
    let at = EXTENT - 2000;
    e.write(&v, at as u64, &noise).unwrap();
    model[at..at + noise.len()].copy_from_slice(&noise);
    assert_eq!(e.read(&v, 0, model.len()).unwrap(), model);

    let snap = e.create_snapshot(&v, "s").unwrap();
    e.write(&v, 10, b"changed after the snapshot").unwrap();
    assert_eq!(e.read_snapshot(&snap, 0, model.len()).unwrap(), model);
    let clone = e.clone_snapshot(&snap, "c", None).unwrap();
    assert_eq!(e.read(&clone, 0, model.len()).unwrap(), model);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn compressed_extents_survive_reopen_and_scrub_clean() {
    let r = root("reopen");
    let data = text(2 * EXTENT);
    let v = {
        let e = engine(&r, true, None);
        let v = e.create_volume("v", 4 << 20).unwrap();
        e.write(&v, 0, &data).unwrap();
        v
    };
    // Reopened with compression off: existing compressed extents must still read.
    let e = engine(&r, false, None);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    let stats = e.repair_once().unwrap();
    assert_eq!(
        (stats.replicas_repaired, stats.unrecoverable),
        (0, 0),
        "{stats:?}"
    );
    assert!(stats.extents_checked > 0);
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn filesystem_files_compress_too() {
    let r = root("fs");
    let e = engine(&r, true, None);
    e.create_fs_as("f".into(), "fs").unwrap();
    let ino = e
        .fs_mknode(
            "f",
            1,
            atlas_native::engine::NewNode {
                name: "log".into(),
                op_id: "op".into(),
                kind: NodeType::File,
                target: None,
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap()
        .ino;
    let data = text(3 * EXTENT);
    let before = used(&e);
    e.write_file("f", ino, 0, &data).unwrap();
    assert!(used(&e) - before < (data.len() as u64 * 3) / 4);
    assert_eq!(e.read_file("f", ino, 0, data.len()).unwrap(), data);
    let _ = std::fs::remove_dir_all(r);
}
