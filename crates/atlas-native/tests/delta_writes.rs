// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Delta writes end to end (`docs/NATIVE_DELTA_WRITES.md`): a randomised model check with
//! reopens, leader failures around the proposal, the capability gate, and the interactions with
//! snapshots, replication, repair, GC, tiering, erasure coding and deduplication.

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    engine::{DeltaConfig, DeltaPoint, NewNode},
    metadata::{Catalog, MetaCommand},
    object::DirObjectStore,
    raft::CAP_DELTA,
    wal::Wal,
    BlockStore, DataNodeServer, DeviceBackend, EngineConfig, FailureDomain, FsOp, MetaBackend,
    NativeEngine, NativeError, Node, NodeType, RaftConfig, RaftError, RaftServer, RemoteDevice,
    SetAttr, TierPolicy, ROOT_INO,
};

const GRID: usize = 4096;
const TICK: Duration = Duration::from_millis(10);
/// The node's default Raft tick: with much shorter ones, an fsync on a busy disk outlasts an
/// election timeout and no leader holds.
const RAFT_TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(30);

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

/// Small limits, so folds, per-cell limits and reclaim all happen within a short test.
fn deltas() -> DeltaConfig {
    DeltaConfig {
        enabled: true,
        max_bytes: 1024,
        max_deltas_per_cell: 6,
        fold_after_deltas: 3,
        fold_after_ms: 20,
        fold_cells_per_pass: 3,
        gc_grace_ms: 0,
        ..DeltaConfig::default()
    }
}

fn config(root: &Path) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = GRID;
    c.delta_writes = deltas();
    c
}

fn local(cfg: EngineConfig) -> NativeEngine {
    NativeEngine::open(cfg, (1..=3).map(node).collect()).unwrap()
}

fn new_file(fs: &str, name: &str) -> NewNode {
    NewNode {
        name: name.into(),
        op_id: format!("{fs}/{name}"),
        kind: NodeType::File,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
    }
}

fn file(e: &NativeEngine, fs: &str, name: &str) -> u64 {
    e.fs_mknode(fs, ROOT_INO, new_file(fs, name)).unwrap().ino
}

fn truncate(e: &NativeEngine, fs: &str, ino: u64, size: u64) -> Result<(), NativeError> {
    let attr = SetAttr {
        size: Some(size),
        ..SetAttr::default()
    };
    e.fs_setattr(fs, ino, attr).map(|_| ())
}

/// A filesystem `f` holding one file of `len` bytes of 1s.
fn one_file(e: &NativeEngine, len: usize) -> u64 {
    e.create_fs_as("f".into(), "f").unwrap();
    let ino = file(e, "f", "a");
    e.write_file("f", ino, 0, &vec![1; len]).unwrap();
    ino
}

fn contents(e: &NativeEngine, fs: &str, ino: u64) -> Vec<u8> {
    let size = e.fs_getattr(fs, ino).unwrap().size;
    e.read_file(fs, ino, 0, size as usize).unwrap()
}

fn overwrite(model: &mut Vec<u8>, at: usize, data: &[u8]) {
    if model.len() < at + data.len() {
        model.resize(at + data.len(), 0);
    }
    model[at..at + data.len()].copy_from_slice(data);
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let seed = self.below(251) as u8;
        (0..len).map(|i| seed.wrapping_add(i as u8 / 7)).collect()
    }

    fn pick<'a, T>(&mut self, m: &'a BTreeMap<String, T>) -> Option<&'a String> {
        let n = self.below(m.len());
        m.keys().nth(n)
    }
}

type Tree = BTreeMap<String, (u64, Vec<u8>)>;

/// What the cluster should hold.
#[derive(Default)]
struct Model {
    filesystems: BTreeMap<String, Tree>,
    fs_snapshots: BTreeMap<String, (String, Tree)>,
    volumes: BTreeMap<String, Vec<u8>>,
    volume_snapshots: BTreeMap<String, Vec<u8>>,
}

impl Model {
    fn verify(&self, e: &NativeEngine) {
        let check_tree = |fs: &str, tree: &Tree| {
            let names: Vec<String> = e
                .fs_readdir(fs, ROOT_INO)
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .collect();
            assert_eq!(names, tree.keys().cloned().collect::<Vec<_>>(), "{fs}");
            for (name, (ino, want)) in tree {
                assert_eq!(&contents(e, fs, *ino), want, "{fs}/{name}");
            }
        };
        for (fs, tree) in &self.filesystems {
            check_tree(fs, tree);
        }
        for (snap, (fs, tree)) in &self.fs_snapshots {
            check_tree(&format!("{fs}@{snap}"), tree);
        }
        for (id, want) in &self.volumes {
            assert_eq!(&e.read(id, 0, want.len()).unwrap(), want, "volume {id}");
        }
        for (id, want) in &self.volume_snapshots {
            assert_eq!(&e.read_snapshot(id, 0, want.len()).unwrap(), want, "{id}");
        }
        e.check_delta_invariants().unwrap();
    }
}

/// Random writes, truncates, renames, deletes, snapshots, clones, folds, GC and reopens, checked
/// byte for byte against a model.
fn random_run(seed: u64, steps: usize) {
    let td = tempfile::tempdir().unwrap();
    let mut e = local(config(td.path()));
    let mut m = Model::default();
    let mut rng = Rng(seed);
    e.create_fs_as("f".into(), "f").unwrap();
    let mut tree = Tree::new();
    for name in ["a", "b", "c"] {
        let ino = file(&e, "f", name);
        let data = rng.bytes(2 * GRID + 100);
        e.write_file("f", ino, 0, &data).unwrap();
        tree.insert(name.into(), (ino, data));
    }
    m.filesystems.insert("f".into(), tree);
    e.create_volume_as("v".into(), "v", 4 * GRID as u64)
        .unwrap();
    e.write("v", 0, &vec![9; 3 * GRID]).unwrap();
    let mut v = vec![9; 3 * GRID];
    v.resize(4 * GRID, 0);
    m.volumes.insert("v".into(), v);

    let (mut next, mut files) = (0, 0);
    let (mut delta_writes, mut folds) = (0, 0);
    for step in 0..steps {
        next += 1;
        let fs = rng.pick(&m.filesystems).unwrap().clone();
        let name = rng.pick(&m.filesystems[&fs]).cloned();
        match rng.below(20) {
            // Mostly small writes inside what exists, some large, some past the end.
            0..=7 => {
                let Some(name) = name else { continue };
                let (ino, want) = m.filesystems.get_mut(&fs).unwrap().get_mut(&name).unwrap();
                let len = match rng.below(10) {
                    0 => GRID + rng.below(2 * GRID),
                    1 => GRID,
                    _ => 1 + rng.below(700),
                };
                let mut at = match rng.below(10) {
                    0 => want.len() + rng.below(GRID),
                    _ => rng.below(want.len().max(1)),
                };
                if len == GRID {
                    at -= at % GRID;
                }
                let data = rng.bytes(len);
                e.write_file(&fs, *ino, at as u64, &data).unwrap();
                overwrite(want, at, &data);
                assert_eq!(&contents(&e, &fs, *ino), want, "step {step} {fs}/{name}");
            }
            8 | 9 => {
                let id = rng.pick(&m.volumes).unwrap().clone();
                let want = m.volumes.get_mut(&id).unwrap();
                let longest = if rng.below(6) == 0 { 2 * GRID } else { 600 };
                let len = 1 + rng.below(longest);
                let at = rng.below(want.len() - len);
                let data = rng.bytes(len);
                e.write(&id, at as u64, &data).unwrap();
                overwrite(want, at, &data);
            }
            10 => {
                let Some(name) = name else { continue };
                let (ino, want) = m.filesystems.get_mut(&fs).unwrap().get_mut(&name).unwrap();
                let size = rng.below(want.len() + GRID);
                truncate(&e, &fs, *ino, size as u64).unwrap();
                want.resize(size, 0);
            }
            // Rename, sometimes over another file.
            11 => {
                let Some(name) = name else { continue };
                let to = if rng.below(2) == 0 {
                    rng.pick(&m.filesystems[&fs]).unwrap().clone()
                } else {
                    format!("r{next}")
                };
                e.fs_rename(&fs, ROOT_INO, &name, ROOT_INO, &to).unwrap();
                let tree = m.filesystems.get_mut(&fs).unwrap();
                if to != name {
                    let moved = tree.remove(&name).unwrap();
                    tree.insert(to, moved);
                }
            }
            12 => {
                if m.filesystems[&fs].len() > 3 && rng.below(2) == 0 {
                    let name = name.unwrap();
                    e.fs_unlink(&fs, ROOT_INO, &name).unwrap();
                    m.filesystems.get_mut(&fs).unwrap().remove(&name);
                } else {
                    files += 1;
                    let name = format!("n{files}");
                    let ino = file(&e, &fs, &name);
                    let len = GRID + rng.below(GRID);
                    let data = rng.bytes(len);
                    e.write_file(&fs, ino, 0, &data).unwrap();
                    m.filesystems
                        .get_mut(&fs)
                        .unwrap()
                        .insert(name, (ino, data));
                }
            }
            13 => {
                if m.fs_snapshots.len() < 4 {
                    let id = format!("s{next}");
                    e.snapshot_fs_as(id.clone(), &fs, &id).unwrap();
                    m.fs_snapshots
                        .insert(id, (fs.clone(), m.filesystems[&fs].clone()));
                }
                if m.volume_snapshots.len() < 3 {
                    let id = format!("vs{next}");
                    let of = rng.pick(&m.volumes).unwrap().clone();
                    e.create_snapshot_as(id.clone(), &of, &id).unwrap();
                    m.volume_snapshots.insert(id, m.volumes[&of].clone());
                }
            }
            14 => {
                if let Some(s) = rng.pick(&m.fs_snapshots).cloned() {
                    if m.filesystems.len() < 3 {
                        let id = format!("c{next}");
                        e.clone_fs_as(id.clone(), &s, &id).unwrap();
                        m.filesystems.insert(id, m.fs_snapshots[&s].1.clone());
                    }
                }
                if let Some(s) = rng.pick(&m.volume_snapshots).cloned() {
                    if m.volumes.len() < 3 {
                        let id = format!("vc{next}");
                        e.clone_snapshot_as(id.clone(), &s, &id, None).unwrap();
                        m.volumes.insert(id, m.volume_snapshots[&s].clone());
                    }
                }
            }
            15 => {
                if let Some(s) = rng.pick(&m.fs_snapshots).cloned() {
                    e.delete_fs_snapshot(&s).unwrap();
                    m.fs_snapshots.remove(&s);
                }
                if let Some(s) = rng.pick(&m.volume_snapshots).cloned() {
                    e.delete_snapshot(&s).unwrap();
                    m.volume_snapshots.remove(&s);
                }
                if fs != "f" && rng.below(2) == 0 {
                    e.delete_fs(&fs).unwrap();
                    m.filesystems.remove(&fs);
                }
                if let Some(id) = rng.pick(&m.volumes).filter(|id| *id != "v").cloned() {
                    e.delete_volume(&id).unwrap();
                    m.volumes.remove(&id);
                }
            }
            16 | 17 => {
                e.delta_maintenance_once().unwrap();
            }
            18 => {
                e.gc_once().unwrap();
            }
            _ => {
                if rng.below(3) == 0 {
                    let stats = e.telemetry.snapshot();
                    delta_writes += stats.delta_writes;
                    folds += stats.delta_folds;
                    drop(e);
                    e = local(config(td.path()));
                    m.verify(&e);
                } else {
                    e.repair_once().unwrap();
                }
            }
        }
        if step % 40 == 0 {
            m.verify(&e);
        }
    }
    m.verify(&e);
    let stats = e.telemetry.snapshot();
    assert!(
        delta_writes + stats.delta_writes > steps as u64 / 20,
        "the run barely used deltas"
    );
    assert!(folds + stats.delta_folds > 0);
    e.fold_all_deltas().unwrap();
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
    e.gc_once().unwrap();
    m.verify(&e);
}

#[test]
fn random_operations_match_a_model_across_reopens() {
    random_run(0x9e37_79b9_7f4a_7c15, 700);
}

#[test]
fn random_operations_match_a_model_with_another_seed() {
    random_run(0x0123_4567_89ab_cdef, 500);
}

#[test]
fn a_small_overwrite_is_a_delta_and_reads_as_the_new_bytes() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    let ino = one_file(&e, 2 * GRID);
    let before = e.fs_getattr("f", ino).unwrap();
    e.write_file("f", ino, 100, &[2; 200]).unwrap();
    e.write_file("f", ino, 200, &[3; 10]).unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 2);
    assert_eq!(e.pending_deltas().unwrap(), (1, 2));
    let mut want = vec![1; 2 * GRID];
    overwrite(&mut want, 100, &[2; 200]);
    overwrite(&mut want, 200, &[3; 10]);
    assert_eq!(contents(&e, "f", ino), want);
    assert_eq!(e.read_file("f", ino, 205, 3).unwrap(), [3, 3, 3]);
    let after = e.fs_getattr("f", ino).unwrap();
    assert_eq!((after.size, after.blocks), (before.size, before.blocks));
    assert!(after.mtime_ns > before.mtime_ns);

    // Not eligible: a whole cell, a write across cells, past the end, or too large.
    e.write_file("f", ino, 0, &[4; GRID]).unwrap();
    e.write_file("f", ino, GRID as u64 - 2, &[5; 4]).unwrap();
    e.write_file("f", ino, 2 * GRID as u64 - 2, &[6; 4])
        .unwrap();
    e.write_file("f", ino, 10, &[7; 2000]).unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 2);
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
    overwrite(&mut want, 0, &[4; GRID]);
    overwrite(&mut want, GRID - 2, &[5; 4]);
    overwrite(&mut want, 2 * GRID - 2, &[6; 4]);
    overwrite(&mut want, 10, &[7; 2000]);
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn a_cell_at_its_delta_limit_is_rewritten_with_them() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    let ino = one_file(&e, GRID);
    let mut want = vec![1; GRID];
    for i in 0..20u8 {
        e.write_file("f", ino, u64::from(i) * 8, &[i; 8]).unwrap();
        overwrite(&mut want, usize::from(i) * 8, &[i; 8]);
        assert!(e.pending_deltas().unwrap().1 <= 6);
    }
    assert!(e.telemetry.snapshot().delta_fallbacks > 0);
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn over_the_pending_cap_writes_take_the_old_path() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    // Room for about three 512-byte deltas on three replicas.
    cfg.delta_writes.max_pending_bytes = 10_000;
    cfg.delta_writes.fold_headroom_bytes = 4_000;
    let e = local(cfg);
    let ino = one_file(&e, 4 * GRID);
    let mut want = vec![1; 4 * GRID];
    for i in 0..24u8 {
        let at = usize::from(i % 4) * GRID + usize::from(i);
        e.write_file("f", ino, at as u64, &[i; 512]).unwrap();
        overwrite(&mut want, at, &[i; 512]);
        e.gc_once().unwrap();
    }
    let stats = e.telemetry.snapshot();
    assert!(
        stats.delta_writes > 0 && stats.delta_fallbacks > 0,
        "{stats:?}"
    );
    assert!(e.catalog_copy().unwrap().delta_totals.pending_bytes <= 6_000);
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn snapshots_fold_first_and_a_retried_snapshot_succeeds() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    let ino = one_file(&e, GRID);
    e.write_file("f", ino, 1, &[2; 4]).unwrap();
    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
    e.write_file("f", ino, 9, &[3; 4]).unwrap();
    // The same snapshot again: still the old one.
    e.snapshot_fs_as("s".into(), "f", "s").unwrap();
    let mut old = vec![1; GRID];
    overwrite(&mut old, 1, &[2; 4]);
    assert_eq!(contents(&e, "f@s", ino), old);
    e.clone_fs_as("c".into(), "s", "c").unwrap();
    assert_eq!(contents(&e, "c", ino), old);

    e.create_volume_as("v".into(), "v", GRID as u64).unwrap();
    e.write("v", 0, &[1; GRID]).unwrap();
    e.write("v", 5, &[2; 5]).unwrap();
    e.create_snapshot_as("vs".into(), "v", "vs").unwrap();
    e.write("v", 20, &[3; 5]).unwrap();
    e.create_snapshot_as("vs".into(), "v", "vs").unwrap();
    let mut old = vec![1; GRID];
    overwrite(&mut old, 5, &[2; 5]);
    assert_eq!(e.read_snapshot("vs", 0, GRID).unwrap(), old);
    e.check_delta_invariants().unwrap();
}

/// Snapshots taken while writers keep adding deltas never see one, never change afterwards,
/// and no write or snapshot fails.
#[test]
fn snapshots_and_truncates_race_writers() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    e.create_fs_as("f".into(), "f").unwrap();
    let inos: Vec<u64> = (0..4)
        .map(|i| {
            let ino = file(&e, "f", &format!("w{i}"));
            e.write_file("f", ino, 0, &vec![0; 3 * GRID]).unwrap();
            ino
        })
        .collect();
    let stop = AtomicBool::new(false);
    let (taken, last) = thread::scope(|s| {
        let _stop = StopOnDrop(&stop);
        let writers: Vec<_> = inos
            .iter()
            .map(|&ino| {
                let (e, stop) = (&e, &stop);
                s.spawn(move || {
                    let mut want = vec![0u8; 3 * GRID];
                    let mut rng = Rng(ino * 77 + 1);
                    while !stop.load(Ordering::SeqCst) {
                        // The first two cells; the third belongs to the truncating below.
                        let at = rng.below(2 * GRID - 64);
                        let data = rng.bytes(64);
                        e.write_file("f", ino, at as u64, &data).unwrap();
                        overwrite(&mut want, at, &data);
                        assert_eq!(e.read_file("f", ino, at as u64, 64).unwrap(), data);
                    }
                    want
                })
            })
            .collect();
        let maintenance = s.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                e.delta_maintenance_once().unwrap();
                e.gc_once().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut taken = Vec::new();
        for i in 0..12 {
            let id = format!("s{i}");
            e.snapshot_fs_as(id.clone(), "f", &id).unwrap();
            let view = format!("f@{id}");
            let seen: Vec<Vec<u8>> = inos.iter().map(|&n| contents(&e, &view, n)).collect();
            taken.push((view, seen));
            // A delta on the third cell, then a cut inside that cell, and grow back.
            e.write_file("f", inos[1], 2 * GRID as u64 + 5, &[i; 9])
                .unwrap();
            truncate(&e, "f", inos[1], 2 * GRID as u64 + 7).unwrap();
            truncate(&e, "f", inos[1], 3 * GRID as u64).unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        maintenance.join().unwrap();
        let last: Vec<Vec<u8>> = writers.into_iter().map(|w| w.join().unwrap()).collect();
        (taken, last)
    });
    for (view, seen) in taken {
        for (ino, want) in inos.iter().zip(seen) {
            assert_eq!(contents(&e, &view, *ino), want, "{view} changed");
        }
    }
    for (i, (ino, want)) in inos.iter().zip(last).enumerate() {
        let got = contents(&e, "f", *ino);
        assert_eq!(got[..2 * GRID], want[..2 * GRID], "writer {i}");
        if i == 1 {
            assert_eq!(got[2 * GRID + 5..2 * GRID + 7], [11, 11]);
            assert!(got[2 * GRID + 7..].iter().all(|b| *b == 0));
        }
    }
    e.check_delta_invariants().unwrap();
}

/// One writer keeps rewriting a range across two cells (a read-modify-write under the lock)
/// while others write deltas to the same cells beside it, with folds running: no acknowledged
/// write of one is ever undone by another.
#[test]
fn concurrent_rewrites_and_deltas_of_the_same_cells_keep_every_write() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    cfg.delta_writes.max_deltas_per_cell = 64;
    let e = local(cfg);
    let ino = one_file(&e, 2 * GRID);
    // (offset, length) of each writer's own range; the first is too long to be a delta.
    let ranges = [
        (GRID - 800, 1600),
        (0, 64),
        (600, 64),
        (GRID + 900, 64),
        (GRID + 2000, 64),
    ];
    let stop = AtomicBool::new(false);
    let last: Vec<Vec<u8>> = thread::scope(|s| {
        let folder = s.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                e.delta_maintenance_once().unwrap();
                e.gc_once().unwrap();
                thread::sleep(Duration::from_millis(2));
            }
        });
        let writers: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(w, &(at, len))| {
                let e = &e;
                s.spawn(move || {
                    let mut last = Vec::new();
                    for i in 0..150usize {
                        last = vec![(w * 40 + i % 40 + 2) as u8; len];
                        e.write_file("f", ino, at as u64, &last).unwrap();
                        let got = e.read_file("f", ino, at as u64, len).unwrap();
                        assert_eq!(got, last, "writer {w} lost write {i}");
                    }
                    last
                })
            })
            .collect();
        let results: Vec<_> = writers.into_iter().map(|w| w.join()).collect();
        stop.store(true, Ordering::SeqCst);
        folder.join().unwrap();
        results.into_iter().map(|r| r.unwrap()).collect()
    });
    let got = contents(&e, "f", ino);
    for (&(at, len), want) in ranges.iter().zip(last) {
        assert_eq!(got[at..at + len], want[..]);
    }
    let stats = e.telemetry.snapshot();
    assert!(stats.delta_writes > 100, "{stats:?}");
    e.check_delta_invariants().unwrap();
}

/// Tells the other threads of a test to stop however it ends, so a failure cannot leave them
/// running and the test hung.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Holds the caller until `writers` deltas are on the data nodes, and a little longer so they
/// all reach the commit queue.
fn gather(replicated: &AtomicUsize, writers: usize) {
    let deadline = Instant::now() + WAIT;
    while replicated.load(Ordering::SeqCst) < writers {
        assert!(Instant::now() < deadline, "writers never replicated");
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(300));
}

/// Writers share commands, and a write whose file went away meanwhile does not take the others
/// down with it.
#[test]
fn a_batch_leaves_out_stale_writes_and_commits_the_rest() {
    let td = tempfile::tempdir().unwrap();
    let e = Arc::new(local(config(td.path())));
    e.create_fs_as("f".into(), "f").unwrap();
    let inos: Vec<u64> = (0..6)
        .map(|i| {
            let ino = file(&e, "f", &format!("w{i}"));
            e.write_file("f", ino, 0, &[1; GRID]).unwrap();
            ino
        })
        .collect();
    let batches = Arc::new(AtomicUsize::new(0));
    let (n, engine) = (batches.clone(), e.clone());
    let replicated = AtomicUsize::new(0);
    e.set_delta_hook(Some(Arc::new(move |point| {
        if point == DeltaPoint::Replicated {
            replicated.fetch_add(1, Ordering::SeqCst);
        }
        if point != DeltaPoint::BeforePropose {
            return;
        }
        match n.fetch_add(1, Ordering::SeqCst) {
            // Hold the first batch while the others queue behind it.
            0 => gather(&replicated, 6),
            1 => engine.fs_unlink("f", ROOT_INO, "w5").unwrap(),
            _ => {}
        }
    })));
    let applied = e.applied_index().unwrap();
    let results: Vec<Result<(), NativeError>> = thread::scope(|s| {
        let first = {
            let (e, ino) = (&e, inos[0]);
            s.spawn(move || e.write_file("f", ino, 4, &[2; 4]).map(|_| ()))
        };
        while batches.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        let rest: Vec<_> = inos[1..]
            .iter()
            .map(|&ino| {
                let e = &e;
                s.spawn(move || e.write_file("f", ino, 4, &[2; 4]).map(|_| ()))
            })
            .collect();
        std::iter::once(first)
            .chain(rest)
            .map(|h| h.join().unwrap())
            .collect()
    });
    e.set_delta_hook(None);
    for (i, r) in results.iter().enumerate().take(5) {
        assert!(r.is_ok(), "writer {i}: {r:?}");
        let got = e.read_file("f", inos[i], 4, 4).unwrap();
        assert_eq!(got, [2; 4]);
    }
    assert!(results[5].is_err(), "its file is gone");
    assert_eq!(e.telemetry.snapshot().delta_writes, 5);
    // Two delta commands and the unlink, not one command per writer.
    assert_eq!(e.applied_index().unwrap() - applied, 3);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_panic_while_committing_leaves_every_waiter_with_an_unknown_outcome() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    e.create_fs_as("f".into(), "f").unwrap();
    let inos: Vec<u64> = (0..4)
        .map(|i| {
            let ino = file(&e, "f", &format!("w{i}"));
            e.write_file("f", ino, 0, &[1; GRID]).unwrap();
            ino
        })
        .collect();
    let batches = Arc::new(AtomicUsize::new(0));
    let n = batches.clone();
    let replicated = AtomicUsize::new(0);
    e.set_delta_hook(Some(Arc::new(move |point| {
        if point == DeltaPoint::Replicated {
            replicated.fetch_add(1, Ordering::SeqCst);
        }
        if point != DeltaPoint::BeforePropose {
            return;
        }
        match n.fetch_add(1, Ordering::SeqCst) {
            0 => gather(&replicated, 4),
            1 => panic!("committer failure injected by the test"),
            _ => {}
        }
    })));
    let results: Vec<thread::Result<Result<(), NativeError>>> = thread::scope(|s| {
        let first = {
            let (e, ino) = (&e, inos[0]);
            s.spawn(move || e.write_file("f", ino, 4, &[2; 4]).map(|_| ()))
        };
        while batches.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        let rest: Vec<_> = inos[1..]
            .iter()
            .map(|&ino| {
                let e = &e;
                s.spawn(move || e.write_file("f", ino, 4, &[2; 4]).map(|_| ()))
            })
            .collect();
        std::iter::once(first)
            .chain(rest)
            .map(|h| h.join())
            .collect()
    });
    assert!(matches!(&results[0], Ok(Ok(()))));
    let panicked = results[1..].iter().filter(|r| r.is_err()).count();
    let unknown = results[1..]
        .iter()
        .filter(|r| matches!(r, Ok(Err(NativeError::Unknown(_)))))
        .count();
    assert_eq!((panicked, unknown), (1, 2));
    for ino in &inos[1..] {
        assert_eq!(e.read_file("f", *ino, 4, 4).unwrap(), [1; 4]);
    }
    // The next writer commits as usual.
    e.write_file("f", inos[1], 4, &[3; 4]).unwrap();
    assert_eq!(e.read_file("f", inos[1], 4, 4).unwrap(), [3; 4]);
}

#[test]
fn dead_extents_wait_out_the_grace_period_and_are_reclaimed_in_one_command() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    cfg.delta_writes.gc_grace_ms = 300;
    let e = local(cfg);
    let ino = one_file(&e, 4 * GRID);
    for i in 0..4u64 {
        e.write_file("f", ino, i * GRID as u64 + 1, &[2; 8])
            .unwrap();
    }
    assert_eq!(e.fold_all_deltas().unwrap(), 4);
    let st = e.gc_once().unwrap();
    // Four old bases and four deltas, none ripe yet.
    assert_eq!((st.candidates, st.reclaimed), (8, 0));
    thread::sleep(Duration::from_millis(350));
    let applied = e.applied_index().unwrap();
    let st = e.gc_once().unwrap();
    assert_eq!((st.candidates, st.reclaimed), (8, 8));
    assert_eq!(e.applied_index().unwrap() - applied, 1);
    assert_eq!(e.catalog_copy().unwrap().delta_totals.dead_bytes, 0);
}

#[test]
fn deltas_are_plain_replicas_never_shared_or_tiered() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(&td.path().join("e"));
    cfg.erasure = Some("2+1".parse().unwrap());
    cfg.erasure_min_bytes = 1;
    cfg.placement.replicas = 3;
    cfg.dedup = true;
    cfg.compression = true;
    cfg.objects = Some(Arc::new(
        DirObjectStore::new(td.path().join("objects")).unwrap(),
    ));
    let e = NativeEngine::open(cfg, (1..=3).map(node).collect()).unwrap();
    let ino = one_file(&e, 2 * GRID);
    // The same bytes twice: dedup would share them.
    e.write_file("f", ino, 8, &[5; 600]).unwrap();
    e.write_file("f", ino, GRID as u64 + 8, &[5; 600]).unwrap();
    let c = e.catalog_copy().unwrap();
    let found: Vec<_> = c.extents.values().filter(|m| m.extent.delta).collect();
    assert_eq!(found.len(), 2);
    for m in &found {
        assert!(m.extent.ec.is_none() && m.extent.raw_len == 0 && m.refs == 1);
        assert_eq!(m.extent.replicas.len(), 3);
    }
    let policy = TierPolicy {
        cold_after: Duration::ZERO,
        min_bytes: 0,
    };
    for id in e.cold_extents(&policy).unwrap() {
        assert!(!c.extents[&id].extent.delta);
    }
    assert_eq!(e.tier_extent(&found[0].extent.id).unwrap(), None);
    e.tier_once(&policy, |_| {}, || false).unwrap();
    let mut want = vec![1; 2 * GRID];
    overwrite(&mut want, 8, &[5; 600]);
    overwrite(&mut want, GRID + 8, &[5; 600]);
    // A read over a tiered base with a delta on it, then a fold of it.
    assert_eq!(contents(&e, "f", ino), want);
    e.fold_all_deltas().unwrap();
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn a_cell_with_deltas_hides_its_replicas_from_direct_readers() {
    let td = tempfile::tempdir().unwrap();
    let e = local(config(td.path()));
    let ino = one_file(&e, 2 * GRID);
    e.write_file("f", ino, 8, &[5; 8]).unwrap();
    let layout = e.file_layout("f", ino, 0, 2 * GRID).unwrap();
    assert!(layout.extents[0].replicas.is_empty() && layout.extents[0].ec.is_none());
    assert_eq!(layout.extents[1].replicas.len(), 3);
    e.fold_all_deltas().unwrap();
    let layout = e.file_layout("f", ino, 0, 2 * GRID).unwrap();
    assert_eq!(layout.extents[0].replicas.len(), 3);
}

/// Turned off, deltas written earlier are still read and get folded; with the mode never on,
/// no new command reaches the log.
#[test]
fn the_mode_can_be_turned_off_and_emits_nothing_when_never_on() {
    let td = tempfile::tempdir().unwrap();
    let off = |root: &Path| {
        let mut cfg = config(root);
        cfg.delta_writes.enabled = false;
        cfg.wal_compact_after = 0;
        cfg
    };
    let never = td.path().join("never");
    {
        let e = local(off(&never));
        let ino = one_file(&e, 2 * GRID);
        e.write_file("f", ino, 8, &[5; 8]).unwrap();
        e.snapshot_fs_as("s".into(), "f", "s").unwrap();
        e.fs_unlink("f", ROOT_INO, "a").unwrap();
        e.delete_fs_snapshot("s").unwrap();
        assert_eq!(e.delta_maintenance_once().unwrap(), 0);
        assert!(e.gc_once().unwrap().reclaimed > 0);
    }
    let wal = Wal::open(never.join("wal")).unwrap();
    let log = wal.replay::<MetaCommand>().unwrap();
    assert!(log.len() > 5);
    for r in log {
        let new = matches!(
            r.command,
            MetaCommand::AppendDeltas { .. }
                | MetaCommand::FoldCell { .. }
                | MetaCommand::ReclaimExtents { .. }
                | MetaCommand::Fs {
                    op: FsOp::AppendDeltas { .. }
                }
        );
        assert!(!new, "{:?}", r.command);
    }

    let once = td.path().join("once");
    let ino = {
        let e = local(config(&once));
        let ino = one_file(&e, 2 * GRID);
        e.write_file("f", ino, 8, &[5; 8]).unwrap();
        ino
    };
    let e = local(off(&once));
    assert_eq!(e.pending_deltas().unwrap(), (1, 1));
    assert_eq!(&contents(&e, "f", ino)[8..16], [5; 8]);
    e.write_file("f", ino, GRID as u64 + 3, &[7; 8]).unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 0);
    assert_eq!(e.delta_maintenance_once().unwrap(), 1);
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
    e.write_file("f", ino, 40, &[6; 8]).unwrap();
    let got = contents(&e, "f", ino);
    assert_eq!((&got[8..16], &got[40..48]), (&[5; 8][..], &[6; 8][..]));
}

/// A source with deltas replicates through its snapshots, and the replica takes no delta.
#[test]
fn replication_ships_folded_snapshots() {
    let td = tempfile::tempdir().unwrap();
    let source = local(config(&td.path().join("source")));
    let target = local(config(&td.path().join("target")));
    let ino = one_file(&source, 3 * GRID);
    let grid = source.filesystems().unwrap()[0].extent_bytes;
    target.create_replica("r".into(), "r", grid).unwrap();
    let mut from: Option<String> = None;
    let mut want = vec![1; 3 * GRID];
    for (round, to) in ["s1", "s2", "s3"].into_iter().enumerate() {
        for k in 0..3 {
            let at = k * GRID + 10 * round;
            let data = [round as u8 + 2; 30];
            source.write_file("f", ino, at as u64, &data).unwrap();
            overwrite(&mut want, at, &data);
        }
        source.snapshot_fs_as(to.into(), "f", to).unwrap();
        let mut after = 0;
        loop {
            let page = source.fs_diff(to, from.as_deref(), after, 16).unwrap();
            let parts = page.inodes.iter().map(|i| i.inode.clone()).collect();
            target
                .replica_apply("r", from.clone(), to.into(), parts, page.removed.clone())
                .unwrap();
            for i in &page.inodes {
                for &(off, len) in &i.data {
                    let view = format!("f@{to}");
                    let data = source
                        .read_file(&view, i.inode.ino, off, len as usize)
                        .unwrap();
                    target.replica_write("r", i.inode.ino, off, &data).unwrap();
                }
            }
            match page.next {
                Some(n) => after = n,
                None => break,
            }
        }
        let local_id = format!("r-{to}");
        let next_ino = source.fs_diff(to, None, 0, 1).unwrap().next_ino;
        target
            .replica_commit("r", from.clone(), to.into(), local_id, next_ino)
            .unwrap();
        assert_eq!(contents(&target, "r", ino), want, "{to}");
        from = Some(to.into());
    }
    assert!(source.telemetry.snapshot().delta_writes > 0);
    assert_eq!(target.telemetry.snapshot().delta_writes, 0);
    assert!(target.write_file("r", ino, 8, &[9; 8]).is_err());
    // Promoted, it takes deltas like any filesystem; demoted again, they are dropped.
    target.replica_promote("r", false).unwrap();
    target.write_file("r", ino, 8, &[9; 8]).unwrap();
    assert_eq!(target.telemetry.snapshot().delta_writes, 1);
    target
        .replica_demote("r", "r-s3".into(), "s3".into())
        .unwrap();
    assert_eq!(target.pending_deltas().unwrap(), (0, 0));
    assert_eq!(contents(&target, "r", ino), want);
    target.check_delta_invariants().unwrap();
}

struct DataNodes {
    servers: Vec<Option<DataNodeServer>>,
    addrs: Vec<SocketAddr>,
}

impl DataNodes {
    fn start(root: &Path, n: usize) -> Self {
        let servers: Vec<_> = (1..=n)
            .map(|i| {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                DataNodeServer::start(format!("n{i}"), root.join(format!("dn{i}")), l).unwrap()
            })
            .collect();
        Self {
            addrs: servers.iter().map(DataNodeServer::local_addr).collect(),
            servers: servers.into_iter().map(Some).collect(),
        }
    }

    fn stores(&self) -> Vec<(Node, Arc<dyn BlockStore>)> {
        self.addrs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let d = RemoteDevice::new(*a, Duration::from_secs(5));
                (node(i + 1), Arc::new(d) as Arc<dyn BlockStore>)
            })
            .collect()
    }
}

#[test]
fn scrub_and_repair_cover_deltas() {
    let td = tempfile::tempdir().unwrap();
    let mut dns = DataNodes::start(td.path(), 5);
    let mut cfg = config(&td.path().join("meta"));
    cfg.node_retry_after = Duration::from_millis(100);
    let e = NativeEngine::open_with(cfg, dns.stores(), MetaBackend::Local).unwrap();
    let ino = one_file(&e, 4 * GRID);
    let mut want = vec![1; 4 * GRID];
    for i in 0..8u8 {
        let at = usize::from(i % 4) * GRID + usize::from(i) * 9;
        e.write_file("f", ino, at as u64, &[i; 100]).unwrap();
        overwrite(&mut want, at, &[i; 100]);
    }
    assert_eq!(e.telemetry.snapshot().delta_writes, 8);
    for lost in [0, 1] {
        dns.servers[lost].take().unwrap().shutdown();
        assert_eq!(contents(&e, "f", ino), want);
        let st = e.repair_once().unwrap();
        assert_eq!(st.unrecoverable, 0, "{st:?}");
    }
    let c = e.catalog_copy().unwrap();
    for m in c.extents.values().filter(|m| m.refs > 0) {
        let on: Vec<&str> = m
            .extent
            .replicas
            .iter()
            .map(|r| r.node_id.as_str())
            .collect();
        assert!(!on.contains(&"n1") && !on.contains(&"n2"), "{on:?}");
    }
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn block_aligned_devices_take_no_deltas() {
    let td = tempfile::tempdir().unwrap();
    let servers: Vec<_> = (1..=3)
        .map(|i| {
            let root = td.path().join(format!("dn{i}"));
            let device = atlas_native::open_store(
                &root.join("dev.data"),
                DeviceBackend::Aligned,
                &root.join("hwm"),
            )
            .unwrap();
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            DataNodeServer::start_devices(format!("n{i}"), &root, vec![device], l, None).unwrap()
        })
        .collect();
    let stores = servers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let d = RemoteDevice::new(s.local_addr(), Duration::from_secs(5));
            (node(i + 1), Arc::new(d) as Arc<dyn BlockStore>)
        })
        .collect();
    let e = NativeEngine::open_with(config(&td.path().join("meta")), stores, MetaBackend::Local)
        .unwrap();
    let ino = one_file(&e, GRID);
    e.write_file("f", ino, 8, &[5; 8]).unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 0);
    assert_eq!(&contents(&e, "f", ino)[8..16], [5; 8]);
}

#[derive(Clone)]
struct Meta {
    engine: Arc<NativeEngine>,
    raft: Arc<RaftServer>,
}

/// Three metadata replicas over three data nodes.
struct Cluster {
    td: tempfile::TempDir,
    dns: DataNodes,
    addrs: BTreeMap<String, SocketAddr>,
    metas: BTreeMap<String, Option<Meta>>,
}

impl Cluster {
    fn start(capabilities: impl Fn(&str) -> u64) -> Self {
        let td = tempfile::tempdir().unwrap();
        let dns = DataNodes::start(td.path(), 3);
        let listeners: BTreeMap<String, TcpListener> = (1..=3)
            .map(|i| (format!("m{i}"), TcpListener::bind("127.0.0.1:0").unwrap()))
            .collect();
        let addrs = listeners
            .iter()
            .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
            .collect();
        let mut c = Self {
            td,
            dns,
            addrs,
            metas: BTreeMap::new(),
        };
        for (id, l) in listeners {
            c.open(&id, l, capabilities(&id));
        }
        c
    }

    fn root(&self, id: &str) -> PathBuf {
        self.td.path().join(id)
    }

    fn open(&mut self, id: &str, listener: TcpListener, capabilities: u64) {
        let peers: BTreeMap<String, SocketAddr> = self
            .addrs
            .iter()
            .filter(|(p, _)| *p != id)
            .map(|(p, a)| (p.clone(), *a))
            .collect();
        let mut rcfg = RaftConfig::new(
            id,
            peers.keys().cloned().collect(),
            self.root(id).join("raft"),
        );
        rcfg.capabilities = capabilities;
        let raft = Arc::new(RaftServer::start(rcfg, listener, peers, RAFT_TICK).unwrap());
        let engine = NativeEngine::open_with(
            config(&self.root(id).join("engine")),
            self.dns.stores(),
            MetaBackend::Raft {
                server: raft.clone(),
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        self.metas.insert(
            id.into(),
            Some(Meta {
                engine: Arc::new(engine),
                raft,
            }),
        );
    }

    /// Stops a replica as a crash would: it says nothing more.
    fn kill(&mut self, id: &str) {
        if let Some(m) = self.metas.insert(id.into(), None).flatten() {
            m.raft.halt();
            m.engine.set_delta_hook(None);
        }
    }

    fn restart(&mut self, id: &str, capabilities: u64) {
        self.kill(id);
        let deadline = Instant::now() + WAIT;
        let listener = loop {
            match TcpListener::bind(self.addrs[id]) {
                Ok(l) => break l,
                Err(e) => assert!(Instant::now() < deadline, "rebind {id}: {e}"),
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.open(id, listener, capabilities);
    }

    fn live(&self) -> impl Iterator<Item = (&String, &Meta)> {
        self.metas
            .iter()
            .filter_map(|(id, m)| m.as_ref().map(|m| (id, m)))
    }

    /// Runs `op` on each live replica until one (the leader) accepts it.
    fn on_leader<T>(
        &self,
        mut op: impl FnMut(&NativeEngine) -> Result<T, NativeError>,
    ) -> (String, T) {
        let deadline = Instant::now() + WAIT;
        let mut last = String::new();
        loop {
            for (id, m) in self.live() {
                match op(&m.engine) {
                    Ok(v) => return (id.clone(), v),
                    Err(NativeError::Raft(
                        e @ (RaftError::NotLeader { .. }
                        | RaftError::LeadershipLost { .. }
                        | RaftError::TermChanged { .. }
                        | RaftError::Timeout { .. }
                        | RaftError::Shutdown),
                    )) => last = e.to_string(),
                    Err(e @ NativeError::Fenced { .. }) => last = e.to_string(),
                    Err(e @ NativeError::Unknown(_)) => last = e.to_string(),
                    Err(other) => panic!("{id}: {other}"),
                }
            }
            assert!(Instant::now() < deadline, "no leader accepted: {last}");
            thread::sleep(TICK);
        }
    }

    /// The leader, with a file of `len` bytes of 1s every replica has, once deltas are allowed.
    fn with_file(&self, len: usize) -> (String, u64) {
        self.on_leader(|e| e.create_fs_as("f".into(), "f"));
        let (_, ino) = self.on_leader(|e| e.fs_mknode("f", ROOT_INO, new_file("f", "a")));
        let ino = ino.ino;
        let (leader, _) = self.on_leader(|e| e.write_file("f", ino, 0, &vec![1; len]));
        (leader, ino)
    }

    fn engine(&self, id: &str) -> &Arc<NativeEngine> {
        &self.metas[id].as_ref().unwrap().engine
    }

    fn raft(&self, id: &str) -> Arc<RaftServer> {
        self.metas[id].as_ref().unwrap().raft.clone()
    }

    fn wait(&self, what: &str, f: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(TICK);
        }
    }

    /// Waits until a leader has heard every replica advertise deltas, and returns it.
    fn wait_supported(&self) -> String {
        let supported = |c: &Self| {
            c.live()
                .find(|(_, m)| m.engine.delta_supported().unwrap_or(false))
                .map(|(id, _)| id.clone())
        };
        self.wait("every replica to advertise deltas", |c| {
            supported(c).is_some()
        });
        supported(self).expect("a leader supports deltas")
    }

    /// Every live replica applies the same catalog and reads the same bytes.
    fn converged(&self, ino: u64) -> Vec<u8> {
        let (leader, want) = self.on_leader(|e| {
            e.ensure_leader()?;
            e.read_file("f", ino, 0, 1 << 20)
        });
        let applied = self.engine(&leader).applied_index().unwrap();
        self.wait("replicas to apply the log", |c| {
            c.live()
                .all(|(_, m)| m.engine.applied_index().unwrap() >= applied)
        });
        for (id, m) in self.live() {
            assert_eq!(
                m.engine.read_file("f", ino, 0, 1 << 20).unwrap(),
                want,
                "{id}"
            );
            let c: Catalog = m.raft.catalog().unwrap();
            c.check_delta_invariants().unwrap();
        }
        want
    }
}

#[test]
fn deltas_wait_until_every_metadata_node_advertises_them() {
    let mut c = Cluster::start(|id| if id == "m3" { 0 } else { CAP_DELTA });
    let (leader, ino) = c.with_file(GRID);
    // Time enough for every follower to have answered.
    thread::sleep(Duration::from_millis(500));
    assert!(!c.engine(&leader).delta_supported().unwrap_or(false));
    c.on_leader(|e| e.write_file("f", ino, 8, &[2; 8]));
    for (_, m) in c.live() {
        assert_eq!(m.engine.telemetry.snapshot().delta_writes, 0);
    }
    assert_eq!(c.converged(ino)[8..16], [2; 8]);

    c.restart("m3", CAP_DELTA);
    c.wait_supported();
    let (leader, _) = c.on_leader(|e| e.write_file("f", ino, 24, &[3; 8]));
    assert_eq!(c.engine(&leader).telemetry.snapshot().delta_writes, 1);
    assert_eq!(c.converged(ino)[24..32], [3; 8]);

    // A node that stops advertising closes the gate again; pending deltas still fold.
    let old = c.metas.keys().find(|id| **id != leader).unwrap().clone();
    c.restart(&old, 0);
    let (leader, ()) = c.on_leader(|e| e.ensure_leader());
    c.wait("the gate to close", |c| {
        !c.engine(&leader).delta_supported().unwrap()
    });
    c.on_leader(|e| e.snapshot_fs_as("s".into(), "f", "s"));
    let written = |c: &Cluster| -> u64 {
        c.live()
            .map(|(_, m)| m.engine.telemetry.snapshot().delta_writes)
            .sum()
    };
    let before = written(&c);
    c.on_leader(|e| e.write_file("f", ino, 40, &[4; 8]));
    assert_eq!(written(&c), before);
    assert_eq!(c.converged(ino)[40..48], [4; 8]);
}

#[test]
fn a_leader_killed_before_proposing_loses_only_that_write() {
    let mut c = Cluster::start(|_| CAP_DELTA);
    let (_, ino) = c.with_file(2 * GRID);
    let leader = c.wait_supported();
    let raft = c.raft(&leader);
    let reached = Arc::new(AtomicBool::new(false));
    let flag = reached.clone();
    c.engine(&leader)
        .set_delta_hook(Some(Arc::new(move |point| {
            if point == DeltaPoint::BeforePropose {
                flag.store(true, Ordering::SeqCst);
                raft.halt();
            }
        })));
    let err = c
        .engine(&leader)
        .write_file("f", ino, 8, &[2; 8])
        .unwrap_err();
    assert!(reached.load(Ordering::SeqCst), "{err}");
    // Nothing was proposed, so the engine knows the write did not happen.
    assert!(!matches!(err, NativeError::Unknown(_)), "{err}");
    c.kill(&leader);
    let (new_leader, got) = c.on_leader(|e| {
        e.ensure_leader()?;
        e.read_file("f", ino, 0, 2 * GRID)
    });
    assert_ne!(new_leader, leader);
    assert_eq!(got, vec![1; 2 * GRID]);
    c.on_leader(|e| e.write_file("f", ino, 16, &[3; 8]));
    c.restart(&leader, CAP_DELTA);
    let got = c.converged(ino);
    assert_eq!((&got[8..16], &got[16..24]), (&[1; 8][..], &[3; 8][..]));
}

/// The command reached the log: whether it commits is up to the next leader, and the writer is
/// told it cannot know. `committed` waits for it to commit before the leader dies.
fn killed_after_proposing(committed: bool) {
    let mut c = Cluster::start(|_| CAP_DELTA);
    let (_, ino) = c.with_file(2 * GRID);
    let leader = c.wait_supported();
    let raft = c.raft(&leader);
    c.engine(&leader)
        .set_delta_hook(Some(Arc::new(move |point| {
            if point == DeltaPoint::Proposed {
                if committed {
                    // Until this replica has applied the delta, so it has committed.
                    let deadline = Instant::now() + WAIT;
                    while raft.catalog().unwrap().dirty.is_empty() {
                        assert!(Instant::now() < deadline, "the delta never committed");
                        thread::sleep(TICK);
                    }
                }
                raft.halt();
            }
        })));
    let err = c
        .engine(&leader)
        .write_file("f", ino, 8, &[2; 8])
        .unwrap_err();
    assert!(matches!(err, NativeError::Unknown(_)), "{err}");
    c.kill(&leader);
    let first = c.converged(ino);
    if committed {
        assert_eq!(first[8..16], [2; 8]);
    } else {
        assert!(first[8..16] == [1; 8] || first[8..16] == [2; 8]);
    }
    // The client retries the same write; later writes land on top.
    c.on_leader(|e| e.write_file("f", ino, 8, &[2; 8]));
    c.on_leader(|e| e.write_file("f", ino, 12, &[3; 2]));
    c.restart(&leader, CAP_DELTA);
    let got = c.converged(ino);
    assert_eq!(got[8..16], [2, 2, 2, 2, 3, 3, 2, 2]);
}

#[test]
fn a_leader_killed_while_the_command_replicates_reports_an_unknown_outcome() {
    killed_after_proposing(false);
}

#[test]
fn an_applied_write_whose_leader_died_reports_an_unknown_outcome() {
    killed_after_proposing(true);
}

#[test]
fn a_proposal_that_cannot_commit_is_unknown_and_its_space_is_not_reused_wrongly() {
    let mut c = Cluster::start(|_| CAP_DELTA);
    let (_, ino) = c.with_file(2 * GRID);
    let leader = c.wait_supported();
    // Free space for the delta to be placed in.
    for _ in 0..3 {
        c.on_leader(|e| e.write_file("f", ino, 0, &[1; GRID]));
    }
    c.on_leader(|e| {
        thread::sleep(Duration::from_millis(20));
        e.gc_once()
    });
    let followers: Vec<(String, Arc<RaftServer>)> = c
        .metas
        .keys()
        .filter(|id| **id != leader)
        .map(|id| (id.clone(), c.raft(id)))
        .collect();
    let cut = followers.clone();
    c.engine(&leader)
        .set_delta_hook(Some(Arc::new(move |point| {
            if point == DeltaPoint::BeforePropose {
                cut.iter().for_each(|(_, r)| r.halt());
            }
        })));
    let err = c
        .engine(&leader)
        .write_file("f", ino, 8, &[2; 600])
        .unwrap_err();
    assert!(matches!(err, NativeError::Unknown(_)), "{err}");
    c.engine(&leader).set_delta_hook(None);
    drop(followers);
    for id in c.metas.keys().cloned().collect::<Vec<_>>() {
        if id != leader {
            c.restart(&id, CAP_DELTA);
        }
    }
    // Writes elsewhere while the unknown command may or may not still commit.
    for i in 0..6u8 {
        c.on_leader(|e| e.write_file("f", ino, GRID as u64 + u64::from(i) * 700, &[i + 10; 600]));
    }
    let got = c.converged(ino);
    assert!(got[8..608] == [1; 600] || got[8..608] == [2; 600]);
    for i in 0..5usize {
        assert_eq!(
            got[GRID + i * 700..GRID + i * 700 + 600],
            [i as u8 + 10; 600]
        );
    }
}

/// A replica that leads again must not hand out space it saw as free before others allocated it.
#[test]
fn writes_stay_intact_while_leadership_moves_and_comes_back() {
    let mut c = Cluster::start(|_| CAP_DELTA);
    let (first, ino) = c.with_file(3 * GRID);
    let mut want = vec![1; 3 * GRID];
    let mut rng = Rng(42);
    let mut led_again = false;
    let mut left = false;
    let mut delta_writes = 0;
    for round in 0..24 {
        // A new leader writes deltas only once every replica has answered it.
        let leader = c.wait_supported();
        left |= leader != first;
        led_again |= left && leader == first;
        let before = c.engine(&leader).telemetry.snapshot().delta_writes;
        for _ in 0..6 {
            let at = rng.below(3 * GRID - 700);
            let len = 1 + rng.below(700);
            let data = rng.bytes(len);
            // An unknown outcome is retried: the same bytes at the same place.
            c.on_leader(|e| e.write_file("f", ino, at as u64, &data));
            overwrite(&mut want, at, &data);
        }
        delta_writes += c.engine(&leader).telemetry.snapshot().delta_writes - before;
        c.on_leader(|e| {
            e.ensure_leader()?;
            e.delta_maintenance_once()
        });
        c.on_leader(|e| e.gc_once());
        assert_eq!(c.converged(ino), want, "round {round}");
        if led_again && round >= 6 {
            break;
        }
        let (leader, ()) = c.on_leader(|e| e.ensure_leader());
        c.restart(&leader, CAP_DELTA);
    }
    assert!(led_again, "{first} never led again");
    // Most rounds' writes were deltas, in several terms.
    assert!(delta_writes > 12, "only {delta_writes} delta writes");
}
