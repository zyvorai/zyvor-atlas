// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The pool of delta folders at work (`docs/NATIVE_DELTA_WRITES.md`, "How folding works"):
//! how many folds run at once, folds racing writes, truncates, snapshots, reclaim, leadership
//! changes, a full device and shutdown. Data is compared byte for byte with a model.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    engine::{DeltaConfig, NewNode},
    node::{Listeners, NativeNode, NodeConfig},
    raft::{CAPABILITIES, CAP_DELTA},
    run_folders, BlockStore, DataNodeServer, DeltaPoint, EngineConfig, FailureDomain, FileDevice,
    MetaBackend, NativeEngine, NativeError, Node, NodeType, RaftConfig, RaftError, RaftServer,
    RemoteDevice, Role, SetAttr, ROOT_INO,
};

const GRID: usize = 4096;
const TICK: Duration = Duration::from_millis(5);
const RAFT_TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(30);
/// Long enough for a folder that should not act to have done so.
const SETTLE: Duration = Duration::from_millis(300);

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(TICK);
    }
}

/// Holds or fails the device calls of one engine.
#[derive(Debug, Default)]
struct Gate {
    state: Mutex<Held>,
    released: Condvar,
    /// Writes fail, as on a full device.
    full: AtomicBool,
    /// This many reads panic.
    panics: AtomicUsize,
}

#[derive(Debug, Default)]
struct Held {
    /// Writes still to be held.
    arm: usize,
    waiting: usize,
    releases: u64,
}

impl Gate {
    /// The next `writes` writes wait for [`Self::release`].
    fn hold(&self, writes: usize) {
        self.state.lock().unwrap().arm = writes;
    }

    fn waiting(&self) -> usize {
        self.state.lock().unwrap().waiting
    }

    fn wait_for(&self, writes: usize) {
        until("writes to reach the gate", || self.waiting() >= writes);
    }

    fn release(&self) {
        let mut s = self.state.lock().unwrap();
        s.arm = 0;
        s.releases += 1;
        self.released.notify_all();
    }

    fn pass(&self) -> Result<(), NativeError> {
        if self.full.load(Ordering::SeqCst) {
            return Err(NativeError::Remote(
                "device full (injected by the test)".into(),
            ));
        }
        let mut s = self.state.lock().unwrap();
        if s.arm == 0 {
            return Ok(());
        }
        s.arm -= 1;
        s.waiting += 1;
        let seen = s.releases;
        while s.releases == seen {
            s = self.released.wait(s).unwrap();
        }
        s.waiting -= 1;
        Ok(())
    }
}

#[derive(Debug)]
struct Gated {
    inner: Arc<dyn BlockStore>,
    gate: Arc<Gate>,
}

impl BlockStore for Gated {
    fn append(&self, fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        self.gate.pass()?;
        self.inner.append(fence, data)
    }
    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.gate.pass()?;
        self.inner.write_at(fence, offset, data)
    }
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        let panics = self
            .gate
            .panics
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        assert!(panics.is_err(), "device failure injected by the test");
        self.inner.read_exact_at(offset, len)
    }
    fn len(&self) -> Result<u64, NativeError> {
        self.inner.len()
    }
    fn byte_granular(&self) -> Result<bool, NativeError> {
        self.inner.byte_granular()
    }
}

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

/// Four folders, every dirty cell due at once.
fn config(root: &Path) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = GRID;
    c.node_retry_after = Duration::from_millis(20);
    c.delta_writes = DeltaConfig {
        enabled: true,
        max_bytes: 1024,
        fold_after_deltas: 1,
        fold_interval_ms: 10,
        fold_concurrency: 4,
        gc_grace_ms: 0,
        ..DeltaConfig::default()
    };
    c
}

/// A local engine whose devices go through one gate.
fn gated(cfg: EngineConfig) -> (NativeEngine, Arc<Gate>) {
    let gate = Arc::new(Gate::default());
    let stores = (1..=3)
        .map(|i| {
            let path = cfg.root.join(format!("dev{i}"));
            let store = Gated {
                inner: Arc::new(FileDevice::open(path).unwrap()),
                gate: gate.clone(),
            };
            (node(i), Arc::new(store) as Arc<dyn BlockStore>)
        })
        .collect();
    let e = NativeEngine::open_with(cfg, stores, MetaBackend::Local).unwrap();
    (e, gate)
}

fn new_file(name: &str) -> NewNode {
    NewNode {
        name: name.into(),
        op_id: name.into(),
        kind: NodeType::File,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
    }
}

/// A file of filesystem `f` holding `cells` cells of 1s, and its model.
fn file(e: &NativeEngine, name: &str, cells: usize) -> (u64, Vec<u8>) {
    e.create_fs_as("f".into(), "f").unwrap();
    let ino = e.fs_mknode("f", ROOT_INO, new_file(name)).unwrap().ino;
    let want = vec![1; cells * GRID];
    e.write_file("f", ino, 0, &want).unwrap();
    (ino, want)
}

fn write(e: &NativeEngine, (ino, want): &mut (u64, Vec<u8>), at: usize, data: &[u8]) {
    e.write_file("f", *ino, at as u64, data).unwrap();
    if want.len() < at + data.len() {
        want.resize(at + data.len(), 0);
    }
    want[at..at + data.len()].copy_from_slice(data);
}

fn truncate(e: &NativeEngine, (ino, want): &mut (u64, Vec<u8>), size: usize) {
    let attr = SetAttr {
        size: Some(size as u64),
        ..SetAttr::default()
    };
    e.fs_setattr("f", *ino, attr).unwrap();
    want.resize(size, 0);
}

fn contents(e: &NativeEngine, fs: &str, ino: u64) -> Vec<u8> {
    let size = e.fs_getattr(fs, ino).unwrap().size;
    e.read_file(fs, ino, 0, size as usize).unwrap()
}

#[derive(Debug, Default)]
struct Attempts {
    ok: AtomicUsize,
    failed: AtomicUsize,
}

/// Stops the pool however the test ends, releasing anything it is held on, so that a failure
/// does not hang the test.
struct Finish<'a>(&'a AtomicBool, Option<&'a Gate>);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        if let Some(gate) = self.1 {
            gate.release();
        }
    }
}

/// Runs `f` beside the pool of folders of `e`.
fn with_pool<R>(e: &NativeEngine, gate: Option<&Gate>, f: impl FnOnce(&Attempts) -> R) -> R {
    let stop = AtomicBool::new(false);
    let attempts = Attempts::default();
    thread::scope(|s| {
        let _finish = Finish(&stop, gate);
        s.spawn(|| {
            run_folders(&[e], &stop, |ok| {
                let n = if ok { &attempts.ok } else { &attempts.failed };
                n.fetch_add(1, Ordering::SeqCst);
            })
        });
        f(&attempts)
    })
}

fn drained(e: &NativeEngine) -> bool {
    e.pending_deltas().unwrap() == (0, 0)
}

#[test]
fn folds_of_different_cells_run_together_up_to_the_limit() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 10);
    for cell in 0..10 {
        write(&e, &mut a, cell * GRID + 3, &[2; 8]);
    }
    gate.hold(usize::MAX);
    with_pool(&e, Some(&gate), |attempts| {
        // Four folds, each writing its new base to three replicas, and no fifth.
        gate.wait_for(4 * 3);
        thread::sleep(SETTLE);
        assert_eq!(gate.waiting(), 4 * 3);
        assert_eq!(contents(&e, "f", a.0), a.1);
        gate.release();
        until("the pool to drain", || drained(&e));
        until("every fold to be reported", || {
            attempts.ok.load(Ordering::SeqCst) == 10
        });
        assert_eq!(attempts.failed.load(Ordering::SeqCst), 0);
    });
    assert_eq!(e.telemetry.snapshot().delta_folds, 10);
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn one_cell_is_never_folded_twice_at_once() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 1);
    for i in 0..3 {
        write(&e, &mut a, 10 * i, &[2; 8]);
    }
    gate.hold(3);
    with_pool(&e, Some(&gate), |_| {
        gate.wait_for(3);
        // Due again, with three idle folders and a caller wanting to fold it.
        write(&e, &mut a, 100, &[3; 8]);
        thread::scope(|s| {
            let _release = Finish(&AtomicBool::new(false), Some(&gate));
            let all = s.spawn(|| e.fold_all_deltas().unwrap());
            thread::sleep(SETTLE);
            assert!(!all.is_finished(), "folded beside the fold in progress");
            assert_eq!(e.telemetry.snapshot().delta_folds, 0);
            assert_eq!(e.pending_deltas().unwrap(), (1, 4));
            assert_eq!(contents(&e, "f", a.0), a.1);
            gate.release();
        });
        until("the pool to drain", || drained(&e));
    });
    // The held fold kept the delta appended meanwhile; one more folded it.
    assert_eq!(e.telemetry.snapshot().delta_folds, 2);
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_snapshot_waits_for_a_fold_in_progress() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 2);
    write(&e, &mut a, 7, &[2; 8]);
    gate.hold(3);
    with_pool(&e, Some(&gate), |_| {
        gate.wait_for(3);
        thread::scope(|s| {
            let _release = Finish(&AtomicBool::new(false), Some(&gate));
            let snapshot = s.spawn(|| e.snapshot_fs_as("s".into(), "f", "s"));
            thread::sleep(SETTLE);
            assert!(
                !snapshot.is_finished(),
                "the snapshot did not wait for the fold"
            );
            gate.release();
            snapshot.join().unwrap().unwrap();
        });
    });
    assert_eq!(e.telemetry.snapshot().delta_folds, 1);
    assert_eq!(contents(&e, "f@s", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_cell_changed_during_its_fold_keeps_the_change() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 2);
    let inos: Vec<u64> = ["b", "c"]
        .iter()
        .map(|n| e.fs_mknode("f", ROOT_INO, new_file(n)).unwrap().ino)
        .collect();
    let mut b = (inos[0], vec![1; GRID]);
    e.write_file("f", b.0, 0, &b.1).unwrap();
    e.write_file("f", inos[1], 0, &[1; GRID]).unwrap();
    write(&e, &mut a, GRID + 200, &[2; 8]);
    write(&e, &mut b, 20, &[2; 8]);
    e.write_file("f", inos[1], 20, &[2; 8]).unwrap();
    // All three folds held while their cells are cut, rewritten and removed.
    gate.hold(3 * 3);
    with_pool(&e, Some(&gate), |attempts| {
        gate.wait_for(3 * 3);
        truncate(&e, &mut a, GRID + 100);
        truncate(&e, &mut a, 2 * GRID);
        write(&e, &mut b, 0, &[5; GRID]);
        e.fs_unlink("f", ROOT_INO, "c").unwrap();
        gate.release();
        until("the three folds to end", || {
            attempts.ok.load(Ordering::SeqCst) + attempts.failed.load(Ordering::SeqCst) == 3
        });
        assert_eq!(attempts.failed.load(Ordering::SeqCst), 0);
    });
    // None could commit.
    assert_eq!(e.telemetry.snapshot().delta_folds, 0);
    assert!(drained(&e));
    // Space freed and taken again: nothing of the rejected folds is in the way.
    e.gc_once().unwrap();
    for i in 0..6 {
        write(&e, &mut a, i * 700, &[i as u8 + 10; GRID]);
        e.gc_once().unwrap();
    }
    assert_eq!(contents(&e, "f", a.0), a.1);
    assert_eq!(contents(&e, "f", b.0), b.1);
    e.check_delta_invariants().unwrap();
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

/// Writers of small, whole-cell and cross-cell writes and truncates, beside the pool, reclaim
/// and snapshots, over several reopens. Every write reads back, every snapshot completes while
/// the writes go on and never changes, and the data matches the writers' models.
#[test]
fn writers_snapshots_truncates_and_reclaim_race_the_pool() {
    const WRITERS: usize = 4;
    const CELLS: usize = 3;
    let td = tempfile::tempdir().unwrap();
    let cfg = || {
        let mut c = config(td.path());
        c.delta_writes.max_deltas_per_cell = 6;
        c.delta_writes.fold_after_deltas = 2;
        c.delta_writes.fold_after_ms = 5;
        c.delta_writes.fold_interval_ms = 1;
        c.delta_writes.gc_grace_ms = 5;
        c
    };
    let mut files: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut volume = vec![1u8; CELLS * GRID];
    {
        let (e, _) = gated(cfg());
        e.create_fs_as("f".into(), "f").unwrap();
        for w in 0..WRITERS {
            let new = new_file(&format!("w{w}"));
            let ino = e.fs_mknode("f", ROOT_INO, new).unwrap().ino;
            files.push((ino, vec![0; CELLS * GRID]));
            e.write_file("f", ino, 0, &files[w].1).unwrap();
        }
        e.create_volume_as("v".into(), "v", volume.len() as u64)
            .unwrap();
        e.write("v", 0, &volume).unwrap();
    }
    // Snapshots with the contents they must keep.
    let mut taken: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    let mut volume_taken: Vec<(String, Vec<u8>)> = Vec::new();
    let (mut folds, mut batches) = (0, 0);
    for round in 0..3u64 {
        let (e, _) = gated(cfg());
        let stop = AtomicBool::new(false);
        // Writers share it; the snapshots that are compared with the models take it alone.
        let pause = RwLock::new(());
        let models: Vec<Mutex<(u64, Vec<u8>)>> = files.drain(..).map(Mutex::new).collect();
        let volume_model = Mutex::new(std::mem::take(&mut volume));
        with_pool(&e, None, |attempts| {
            thread::scope(|s| {
                let _stop = Finish(&stop, None);
                let (e, stop, pause) = (&e, &stop, &pause);
                for (w, model) in models.iter().enumerate() {
                    s.spawn(move || {
                        let mut rng = Rng(round * 131 + w as u64 * 77 + 1);
                        while !stop.load(Ordering::SeqCst) {
                            let _run = pause.read().unwrap();
                            let mut m = model.lock().unwrap();
                            let fill = rng.below(251) as u8;
                            let (at, len) = match rng.below(20) {
                                0 => (rng.below(CELLS) * GRID, GRID),
                                1 => (rng.below(CELLS * GRID - 1500), 1500),
                                2 => {
                                    let size = rng.below(CELLS * GRID);
                                    truncate(e, &mut m, size);
                                    truncate(e, &mut m, CELLS * GRID);
                                    (size, CELLS * GRID - size)
                                }
                                _ => {
                                    let len = 1 + rng.below(600);
                                    (rng.below(CELLS * GRID - len), len)
                                }
                            };
                            if len != CELLS * GRID - at {
                                write(e, &mut m, at, &vec![fill; len]);
                            }
                            let got = e.read_file("f", m.0, at as u64, len).unwrap();
                            assert_eq!(got, m.1[at..at + len], "writer {w}");
                        }
                    });
                }
                let volume_model = &volume_model;
                s.spawn(move || {
                    let mut rng = Rng(round + 999);
                    while !stop.load(Ordering::SeqCst) {
                        let _run = pause.read().unwrap();
                        let mut m = volume_model.lock().unwrap();
                        let len = 1 + rng.below(600);
                        let at = rng.below(m.len() - len);
                        let data = vec![rng.below(251) as u8; len];
                        e.write("v", at as u64, &data).unwrap();
                        m[at..at + len].copy_from_slice(&data);
                        assert_eq!(e.read("v", at as u64, len).unwrap(), data);
                    }
                });
                s.spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        e.reclaim_if_pressed().unwrap();
                        e.gc_once().unwrap();
                        thread::sleep(Duration::from_millis(2));
                    }
                });
                for i in 0..8 {
                    thread::sleep(Duration::from_millis(30));
                    let id = format!("s{round}-{i}");
                    let view = format!("f@{id}");
                    // Every other one with the writers stopped, so it can be checked against
                    // the models; the rest while they write.
                    let paused = (i % 2 == 0).then(|| pause.write().unwrap());
                    let start = Instant::now();
                    e.snapshot_fs_as(id.clone(), "f", &id).unwrap();
                    e.create_snapshot_as(format!("v{id}"), "v", &id).unwrap();
                    assert!(start.elapsed() < WAIT);
                    let seen: Vec<Vec<u8>> = models
                        .iter()
                        .enumerate()
                        .map(|(w, model)| {
                            if paused.is_some() {
                                let m = model.lock().unwrap();
                                assert_eq!(contents(e, &view, m.0), m.1, "{view} writer {w}");
                                m.1.clone()
                            } else {
                                let entry = e.fs_lookup(&view, ROOT_INO, &format!("w{w}")).unwrap();
                                contents(e, &view, entry.ino)
                            }
                        })
                        .collect();
                    let vid = format!("v{id}");
                    let seen_volume = e.read_snapshot(&vid, 0, CELLS * GRID).unwrap();
                    if paused.is_some() {
                        assert_eq!(seen_volume, *volume_model.lock().unwrap(), "{vid}");
                    }
                    taken.push((view, seen));
                    volume_taken.push((vid, seen_volume));
                }
            });
            assert_eq!(attempts.failed.load(Ordering::SeqCst), 0);
        });
        folds += e.telemetry.snapshot().delta_folds;
        batches += e.telemetry.fold_batch.snapshot().count();
        files = models
            .into_iter()
            .map(|m| m.into_inner().unwrap())
            .collect();
        volume = volume_model.into_inner().unwrap();
        for (ino, want) in &files {
            assert_eq!(&contents(&e, "f", *ino), want, "round {round}");
        }
        assert_eq!(e.read("v", 0, volume.len()).unwrap(), volume);
        e.check_delta_invariants().unwrap();
    }
    assert!(folds > 20, "only {folds} folds");
    assert!(batches > 0, "no fold was committed in a batch");
    let (e, _) = gated(cfg());
    with_pool(&e, None, |_| until("the pool to drain", || drained(&e)));
    e.gc_once().unwrap();
    for (view, seen) in &taken {
        for ((ino, _), want) in files.iter().zip(seen) {
            assert_eq!(&contents(&e, view, *ino), want, "{view} changed");
        }
    }
    for (id, want) in &volume_taken {
        assert_eq!(&e.read_snapshot(id, 0, want.len()).unwrap(), want, "{id}");
    }
    for (ino, want) in &files {
        assert_eq!(&contents(&e, "f", *ino), want);
    }
    assert_eq!(e.read("v", 0, volume.len()).unwrap(), volume);
    e.check_delta_invariants().unwrap();
}

#[test]
fn the_pool_folds_everything_once_delta_writes_are_off() {
    let td = tempfile::tempdir().unwrap();
    let mut a = {
        let (e, _) = gated(config(td.path()));
        let mut a = file(&e, "a", 20);
        for cell in 0..20 {
            write(&e, &mut a, cell * GRID + cell, &[2; 8]);
        }
        a
    };
    let mut off = config(td.path());
    off.delta_writes.enabled = false;
    // Neither threshold would make these cells due.
    off.delta_writes.fold_after_deltas = 1000;
    off.delta_writes.fold_after_ms = 3_600_000;
    let (e, _) = gated(off);
    assert_eq!(e.pending_deltas().unwrap(), (20, 20));
    with_pool(&e, None, |_| until("the pool to drain", || drained(&e)));
    write(&e, &mut a, 9, &[3; 8]);
    assert_eq!(e.telemetry.snapshot().delta_writes, 0);
    assert_eq!(e.telemetry.fold_batch.snapshot().count(), 0);
    assert_eq!(contents(&e, "f", a.0), a.1);
}

#[test]
fn stalled_folds_do_not_stall_reclaim() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    cfg.delta_writes.max_pending_bytes = 200_000;
    cfg.delta_writes.fold_headroom_bytes = 10_000;
    let (e, gate) = gated(cfg);
    let mut a = file(&e, "a", 12);
    for cell in 0..4 {
        write(&e, &mut a, cell * GRID + 3, &[2; 8]);
    }
    gate.hold(4 * 3);
    with_pool(&e, Some(&gate), |_| {
        gate.wait_for(4 * 3);
        assert!(e.reclaim_if_pressed().unwrap().is_none(), "room to spare");
        // Eight dead 4 KiB cells on three replicas: over half the room.
        for cell in 4..12 {
            write(&e, &mut a, cell * GRID, &[3; GRID]);
        }
        let st = e.reclaim_if_pressed().unwrap().expect("room is short");
        assert_eq!(st.reclaimed, 8);
        assert_eq!(gate.waiting(), 4 * 3);
        gate.release();
        until("the pool to drain", || drained(&e));
    });
    assert_eq!(contents(&e, "f", a.0), a.1);
}

#[test]
fn a_full_device_slows_the_folders_and_they_finish_once_there_is_room() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 40);
    for cell in 0..40 {
        write(&e, &mut a, cell * GRID + 3, &[2; 8]);
    }
    gate.full.store(true, Ordering::SeqCst);
    with_pool(&e, Some(&gate), |attempts| {
        thread::sleep(Duration::from_secs(2));
        let failed = attempts.failed.load(Ordering::SeqCst);
        // Four folders, pausing 10 ms, then twice as long each time: about eight attempts each
        // in two seconds, where pausing only 10 ms would make hundreds.
        assert!((4..=60).contains(&failed), "{failed} attempts");
        assert_eq!(attempts.ok.load(Ordering::SeqCst), 0);
        assert_eq!(e.pending_deltas().unwrap(), (40, 40));
        assert_eq!(contents(&e, "f", a.0), a.1);
        gate.full.store(false, Ordering::SeqCst);
        until("the pool to drain", || drained(&e));
    });
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_folder_survives_a_fold_that_panics() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    cfg.delta_writes.fold_concurrency = 1;
    let (e, gate) = gated(cfg);
    let mut a = file(&e, "a", 3);
    for cell in 0..3 {
        write(&e, &mut a, cell * GRID + 3, &[2; 8]);
    }
    // The only folder reads a cell's base on its own thread.
    gate.panics.store(1, Ordering::SeqCst);
    with_pool(&e, Some(&gate), |attempts| {
        until("the pool to drain", || drained(&e));
        assert_eq!(attempts.failed.load(Ordering::SeqCst), 1);
    });
    assert_eq!(contents(&e, "f", a.0), a.1);
}

#[test]
fn stopping_the_pool_waits_for_the_folds_in_progress_and_no_longer() {
    let td = tempfile::tempdir().unwrap();
    let (e, gate) = gated(config(td.path()));
    let mut a = file(&e, "a", 1);
    write(&e, &mut a, 3, &[2; 8]);
    gate.hold(3);
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let _finish = Finish(&stop, Some(&gate));
        let pool = s.spawn(|| run_folders(&[&e], &stop, |_| {}));
        gate.wait_for(3);
        stop.store(true, Ordering::SeqCst);
        thread::sleep(SETTLE);
        assert!(!pool.is_finished(), "returned with a fold in progress");
        gate.release();
        until("the pool to stop", || pool.is_finished());
    });
    assert!(drained(&e), "the fold in progress was abandoned");
    assert_eq!(contents(&e, "f", a.0), a.1);

    // Idle folders asleep for a long interval stop at once.
    drop(e);
    let mut cfg = config(td.path());
    cfg.delta_writes.fold_interval_ms = 600_000;
    let (e, _) = gated(cfg);
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let pool = s.spawn(|| run_folders(&[&e], &stop, |_| {}));
        thread::sleep(SETTLE);
        let start = Instant::now();
        stop.store(true, Ordering::SeqCst);
        pool.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
    });
}

#[derive(Clone)]
struct Meta {
    engine: Arc<NativeEngine>,
    raft: Arc<RaftServer>,
    gate: Arc<Gate>,
}

/// Three metadata replicas over three data nodes, each replica with its own gate.
struct Cluster {
    td: tempfile::TempDir,
    data: Vec<DataNodeServer>,
    data_addrs: Vec<SocketAddr>,
    addrs: BTreeMap<String, SocketAddr>,
    metas: BTreeMap<String, Option<Meta>>,
    /// What each replica advertises when opened, if not this build's capabilities.
    capabilities: BTreeMap<String, u64>,
    compact_after: u64,
}

impl Cluster {
    fn start() -> Self {
        Self::start_with(BTreeMap::new(), 1024)
    }

    fn start_with(capabilities: BTreeMap<String, u64>, compact_after: u64) -> Self {
        let td = tempfile::tempdir().unwrap();
        let data: Vec<DataNodeServer> = (1..=3)
            .map(|i| {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                let root = td.path().join(format!("dn{i}"));
                DataNodeServer::start(format!("n{i}"), root, l).unwrap()
            })
            .collect();
        let listeners: BTreeMap<String, TcpListener> = (1..=3)
            .map(|i| (format!("m{i}"), TcpListener::bind("127.0.0.1:0").unwrap()))
            .collect();
        let mut c = Self {
            td,
            data_addrs: data.iter().map(DataNodeServer::local_addr).collect(),
            data,
            addrs: listeners
                .iter()
                .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
                .collect(),
            metas: BTreeMap::new(),
            capabilities,
            compact_after,
        };
        for (id, l) in listeners {
            c.open(&id, l);
        }
        c
    }

    fn open(&mut self, id: &str, listener: TcpListener) {
        let peers: BTreeMap<String, SocketAddr> = self
            .addrs
            .iter()
            .filter(|(p, _)| *p != id)
            .map(|(p, a)| (p.clone(), *a))
            .collect();
        let root = self.td.path().join(id);
        let mut rcfg = RaftConfig::new(id, peers.keys().cloned().collect(), root.join("raft"));
        rcfg.capabilities = self.capabilities.get(id).copied().unwrap_or(CAPABILITIES);
        rcfg.compact_after = self.compact_after;
        let raft = Arc::new(RaftServer::start(rcfg, listener, peers, RAFT_TICK).unwrap());
        let gate = Arc::new(Gate::default());
        let stores = self
            .data_addrs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let store = Gated {
                    inner: Arc::new(RemoteDevice::new(*a, Duration::from_secs(5))),
                    gate: gate.clone(),
                };
                (node(i + 1), Arc::new(store) as Arc<dyn BlockStore>)
            })
            .collect();
        let engine = NativeEngine::open_with(
            config(&root.join("engine")),
            stores,
            MetaBackend::Raft {
                server: raft.clone(),
                timeout: Duration::from_secs(3),
            },
        )
        .unwrap();
        self.metas.insert(
            id.into(),
            Some(Meta {
                engine: Arc::new(engine),
                raft,
                gate,
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

    fn restart(&mut self, id: &str) {
        self.kill(id);
        let deadline = Instant::now() + WAIT;
        let listener = loop {
            match TcpListener::bind(self.addrs[id]) {
                Ok(l) => break l,
                Err(e) => assert!(Instant::now() < deadline, "rebind {id}: {e}"),
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.open(id, listener);
    }

    fn live(&self) -> impl Iterator<Item = (&String, &Meta)> {
        self.metas
            .iter()
            .filter_map(|(id, m)| m.as_ref().map(|m| (id, m)))
    }

    /// Writes the data nodes refused for carrying an old term.
    fn fenced_writes(&self) -> f64 {
        self.data
            .iter()
            .map(|d| {
                sum_of(
                    &d.render_metrics().unwrap(),
                    "atlas_native_data_fenced_writes_total",
                )
            })
            .sum()
    }

    fn meta(&self, id: &str) -> Meta {
        self.metas[id].clone().unwrap()
    }

    /// Runs `op` on each live replica but `not` until one (the leader) accepts it.
    fn on_leader<T>(
        &self,
        not: &str,
        mut op: impl FnMut(&NativeEngine) -> Result<T, NativeError>,
    ) -> (String, T) {
        let deadline = Instant::now() + WAIT;
        let mut last = String::new();
        loop {
            for (id, m) in self.live().filter(|(id, _)| *id != not) {
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

    /// A leader that writes deltas, with a file of three cells of 1s and a delta on two.
    fn with_deltas(&self) -> (String, (u64, Vec<u8>)) {
        self.on_leader("", |e| e.create_fs_as("f".into(), "f"));
        let (_, attr) = self.on_leader("", |e| e.fs_mknode("f", ROOT_INO, new_file("a")));
        let mut want = vec![1; 3 * GRID];
        self.on_leader("", |e| e.write_file("f", attr.ino, 0, &want));
        until("every replica to advertise deltas", || {
            self.live()
                .any(|(_, m)| m.engine.delta_supported().unwrap_or(false))
        });
        let mut leader = String::new();
        for cell in 0..2 {
            let at = cell * GRID + 8;
            leader = self
                .on_leader("", |e| e.write_file("f", attr.ino, at as u64, &[2; 8]))
                .0;
            want[at..at + 8].fill(2);
        }
        assert_eq!(self.meta(&leader).engine.pending_deltas().unwrap(), (2, 2));
        (leader, (attr.ino, want))
    }

    /// Every live replica applies the same catalog and reads `want`.
    fn converged(&self, ino: u64, want: &[u8]) {
        let (leader, got) = self.on_leader("", |e| {
            e.ensure_leader()?;
            e.read_file("f", ino, 0, 1 << 20)
        });
        assert_eq!(got, want, "leader {leader}");
        let applied = self.meta(&leader).engine.applied_index().unwrap();
        until("replicas to apply the log", || {
            self.live()
                .all(|(_, m)| m.engine.applied_index().unwrap() >= applied)
        });
        for (id, m) in self.live() {
            let got = m.engine.read_file("f", ino, 0, 1 << 20).unwrap();
            assert_eq!(got, want, "{id}");
            m.raft.catalog().unwrap().check_delta_invariants().unwrap();
        }
    }
}

/// The leader is cut off with folds half written; its successor writes the same cells. The old
/// folds' writes are refused by their term, and followers never fold.
#[test]
fn a_deposed_leaders_folds_are_fenced_off() {
    let mut c = Cluster::start();
    let (old, mut a) = c.with_deltas();
    let (ino, deposed) = (a.0, c.meta(&old));
    for (id, m) in c.live().filter(|(id, _)| **id != old) {
        with_pool(&m.engine, None, |attempts| {
            thread::sleep(SETTLE);
            let all = attempts.ok.load(Ordering::SeqCst) + attempts.failed.load(Ordering::SeqCst);
            assert_eq!(all, 0, "follower {id} folded");
        });
    }
    deposed.gate.hold(2 * 3);
    with_pool(&deposed.engine, Some(&deposed.gate), |attempts| {
        deposed.gate.wait_for(2 * 3);
        deposed.raft.halt();
        // The successor rewrites both cells being folded.
        for cell in 0..2 {
            let data = vec![cell as u8 + 5; GRID];
            c.on_leader(&old, |e| {
                e.write_file("f", ino, (cell * GRID) as u64, &data)
            });
            a.1[cell * GRID..(cell + 1) * GRID].copy_from_slice(&data);
        }
        assert_eq!(c.fenced_writes(), 0.0);
        deposed.gate.release();
        until("both old folds to fail", || {
            attempts.failed.load(Ordering::SeqCst) >= 2
        });
        assert_eq!(attempts.ok.load(Ordering::SeqCst), 0);
    });
    assert!(c.fenced_writes() > 0.0, "the old writes were accepted");
    assert_eq!(deposed.engine.telemetry.snapshot().delta_folds, 0);
    drop(deposed);
    c.kill(&old);
    c.converged(ino, &a.1);
    c.restart(&old);
    c.converged(ino, &a.1);
}

/// The leader loses its quorum with folds half written: they cannot commit, whether or not they
/// were proposed, and nothing changes in what is read, then or after the quorum returns.
#[test]
fn folds_that_cannot_commit_leave_the_data_as_it_was() {
    let mut c = Cluster::start();
    let (leader, mut a) = c.with_deltas();
    let (ino, cut) = (a.0, c.meta(&leader));
    let followers: Vec<String> = c
        .metas
        .keys()
        .filter(|id| **id != leader)
        .cloned()
        .collect();
    cut.gate.hold(2 * 3);
    with_pool(&cut.engine, Some(&cut.gate), |attempts| {
        cut.gate.wait_for(2 * 3);
        for id in &followers {
            c.meta(id).raft.halt();
        }
        cut.gate.release();
        until("both folds to fail", || {
            attempts.failed.load(Ordering::SeqCst) >= 2
        });
        // Once it has stepped down, its folders start nothing more.
        until("the leader to step down", || {
            cut.raft.status().unwrap().role != Role::Leader
        });
        let all = || attempts.ok.load(Ordering::SeqCst) + attempts.failed.load(Ordering::SeqCst);
        thread::sleep(SETTLE);
        let before = all();
        thread::sleep(SETTLE);
        assert_eq!(all(), before, "folded without leading");
        assert_eq!(attempts.ok.load(Ordering::SeqCst), 0);
        assert_eq!(cut.engine.pending_deltas().unwrap(), (2, 2));
    });
    drop(cut);
    for id in &followers {
        c.restart(id);
    }
    c.converged(ino, &a.1);
    // Writes that take space meanwhile do not land on anything the undecided folds hold.
    for i in 0..8usize {
        let at = (i % 3) * GRID + i * 40;
        let data = vec![i as u8 + 20; 900];
        c.on_leader("", |e| e.write_file("f", ino, at as u64, &data));
        a.1[at..at + 900].copy_from_slice(&data);
    }
    let (new_leader, ()) = c.on_leader("", |e| e.ensure_leader());
    let engine = c.meta(&new_leader).engine;
    with_pool(&engine, None, |_| {
        until("the pool to drain", || drained(&engine))
    });
    c.converged(ino, &a.1);
}

/// Holds the committer of fold batches until released (also when dropped, so that a failing
/// test does not hang), and counts the rewrites handed over meanwhile.
#[derive(Default)]
struct Latch {
    open: AtomicBool,
    rewritten: AtomicUsize,
    proposals: AtomicUsize,
}

struct HeldCommitter<'a>(&'a NativeEngine, Arc<Latch>);

/// Opens a [`Latch`] when dropped.
struct Release(Arc<Latch>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.open.store(true, Ordering::SeqCst);
    }
}

impl<'a> HeldCommitter<'a> {
    fn new(e: &'a NativeEngine) -> Self {
        let latch = Arc::new(Latch::default());
        let l = latch.clone();
        e.set_delta_hook(Some(Arc::new(move |point| match point {
            DeltaPoint::FoldReplicated => {
                l.rewritten.fetch_add(1, Ordering::SeqCst);
            }
            DeltaPoint::FoldBeforePropose => {
                l.proposals.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + 2 * WAIT;
                while !l.open.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(TICK);
                }
            }
            _ => {}
        })));
        Self(e, latch)
    }

    fn rewritten(&self) -> usize {
        self.1.rewritten.load(Ordering::SeqCst)
    }

    /// Until `n` rewrites are on the data nodes and a batch is waiting to be proposed.
    fn wait_for(&self, n: usize) {
        until("rewrites to wait for the committer", || {
            self.rewritten() >= n && self.1.proposals.load(Ordering::SeqCst) > 0
        });
    }

    fn release(&self) {
        drop(self.releaser());
    }

    /// For a scope that waits for the committer: releases it also if the scope panics.
    fn releaser(&self) -> Release {
        Release(self.1.clone())
    }
}

impl Drop for HeldCommitter<'_> {
    fn drop(&mut self) {
        self.release();
        self.0.set_delta_hook(None);
    }
}

/// A local engine with a file of `cells` cells, each with a delta on it.
fn dirty_file(cfg: EngineConfig, cells: usize) -> (NativeEngine, (u64, Vec<u8>)) {
    let (e, _) = gated(cfg);
    let mut a = file(&e, "a", cells);
    for cell in 0..cells {
        write(&e, &mut a, cell * GRID + 3, &[2; 8]);
    }
    (e, a)
}

/// While a batch is being committed, rewritten cells queue up for the next one, which takes all
/// of them. The folders stop once the queue is full.
#[test]
fn folds_that_wait_share_the_next_batch_up_to_the_limit() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path());
    cfg.delta_writes.fold_batch_cells = 3;
    let (e, a) = dirty_file(cfg, 20);
    let held = HeldCommitter::new(&e);
    with_pool(&e, None, |attempts| {
        let _release = held.releaser();
        // One to three cells in the batch being committed, three queued, and four folders each
        // waiting to queue one more.
        held.wait_for(1 + 3 + 4);
        thread::sleep(SETTLE);
        assert!(held.rewritten() <= 3 + 3 + 4, "{}", held.rewritten());
        assert_eq!(e.telemetry.snapshot().delta_folds, 0);
        assert_eq!(contents(&e, "f", a.0), a.1);
        held.release();
        until("the pool to drain", || drained(&e));
        until("every fold to be reported", || {
            attempts.ok.load(Ordering::SeqCst) == 20
        });
        assert_eq!(attempts.failed.load(Ordering::SeqCst), 0);
    });
    let batches = e.telemetry.fold_batch.snapshot();
    assert_eq!(batches.sum, 20);
    assert!(batches.count() < 20, "no folds shared a command");
    // Nothing over the limit of 3 (the bucket up to 4 is the third).
    assert_eq!(batches.buckets[..3].iter().sum::<u64>(), batches.count());
    assert_eq!(e.telemetry.snapshot().delta_folds, 20);
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_batch_keeps_the_changes_made_while_it_waited() {
    let td = tempfile::tempdir().unwrap();
    let (e, mut a) = dirty_file(config(td.path()), 6);
    let held = HeldCommitter::new(&e);
    with_pool(&e, None, |attempts| {
        let _release = held.releaser();
        held.wait_for(6);
        // Cut away, rewritten whole, appended to; the rest are left alone.
        truncate(&e, &mut a, 5 * GRID);
        write(&e, &mut a, 0, &[5; GRID]);
        write(&e, &mut a, GRID + 100, &[6; 8]);
        held.release();
        until("the pool to drain", || drained(&e));
        until("every fold to be reported", || {
            attempts.ok.load(Ordering::SeqCst) == 7
        });
        assert_eq!(attempts.failed.load(Ordering::SeqCst), 0);
    });
    let t = e.telemetry.snapshot();
    // The cut and the rewritten cell lost; the appended one folded, and then again.
    assert_eq!((t.delta_folds, t.delta_fold_conflicts), (5, 2));
    e.gc_once().unwrap();
    for i in 0..6 {
        write(&e, &mut a, i * 700, &[i as u8 + 10; GRID]);
        e.gc_once().unwrap();
    }
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_snapshot_waits_for_a_queued_fold() {
    let td = tempfile::tempdir().unwrap();
    let (e, a) = dirty_file(config(td.path()), 6);
    let held = HeldCommitter::new(&e);
    with_pool(&e, None, |_| {
        let _release = held.releaser();
        held.wait_for(6);
        thread::scope(|s| {
            let _release = held.releaser();
            let snapshot = s.spawn(|| {
                let r = e.snapshot_fs_as("s".into(), "f", "s");
                (r, e.pending_deltas().unwrap())
            });
            thread::sleep(SETTLE);
            let early = snapshot.is_finished();
            held.release();
            assert!(!early, "the snapshot did not wait for the queued folds");
            let (taken, pending) = snapshot.join().unwrap();
            taken.unwrap();
            assert_eq!(pending, (0, 0));
        });
    });
    assert_eq!(e.telemetry.snapshot().delta_folds, 6);
    assert_eq!(contents(&e, "f@s", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn stopping_the_pool_commits_what_is_queued() {
    let td = tempfile::tempdir().unwrap();
    let (e, a) = dirty_file(config(td.path()), 6);
    let held = HeldCommitter::new(&e);
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let _finish = Finish(&stop, None);
        let _release = held.releaser();
        let pool = s.spawn(|| run_folders(&[&e], &stop, |_| {}));
        held.wait_for(6);
        stop.store(true, Ordering::SeqCst);
        thread::sleep(SETTLE);
        let early = pool.is_finished();
        held.release();
        assert!(!early, "returned with folds queued");
        until("the pool to stop", || pool.is_finished());
    });
    assert!(drained(&e), "queued folds were abandoned");
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_committer_survives_a_batch_that_panics() {
    let td = tempfile::tempdir().unwrap();
    let (e, a) = dirty_file(config(td.path()), 8);
    let panicked = Arc::new(AtomicBool::new(false));
    let once = panicked.clone();
    e.set_delta_hook(Some(Arc::new(move |point| {
        if point == DeltaPoint::FoldBeforePropose && !once.swap(true, Ordering::SeqCst) {
            panic!("the committer failed (injected by the test)");
        }
    })));
    with_pool(&e, None, |attempts| {
        until("the pool to drain", || drained(&e));
        assert!(attempts.failed.load(Ordering::SeqCst) >= 1);
    });
    assert!(panicked.load(Ordering::SeqCst));
    assert_eq!(contents(&e, "f", a.0), a.1);
    e.check_delta_invariants().unwrap();
}

/// Runs the pools of every live replica beside `f`; only the leader's folds.
fn with_pools<R>(c: &Cluster, f: impl FnOnce() -> R) -> R {
    let engines: Vec<Arc<NativeEngine>> = c.live().map(|(_, m)| m.engine.clone()).collect();
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let _finish = Finish(&stop, None);
        for e in &engines {
            let stop = &stop;
            s.spawn(move || run_folders(&[e], stop, |_| {}));
        }
        f()
    })
}

/// Folds everything pending, and checks that the replicas agree on `want`.
fn drain_and_converge(c: &Cluster, ino: u64, want: &[u8]) {
    with_pools(c, || {
        until("the pools to drain", || {
            c.live().any(|(_, m)| {
                m.engine.ensure_leader().is_ok() && m.engine.pending_deltas().unwrap() == (0, 0)
            })
        })
    });
    c.converged(ino, want);
}

impl Cluster {
    /// A leader with a file of `cells` cells and a delta on each.
    fn with_dirty_cells(&self, cells: usize) -> (String, (u64, Vec<u8>)) {
        let (_, (ino, mut want)) = self.with_deltas();
        self.on_leader("", |e| {
            e.write_file("f", ino, 3 * GRID as u64, &vec![1; (cells - 3) * GRID])
        });
        want.resize(cells * GRID, 1);
        let deadline = Instant::now() + WAIT;
        loop {
            for cell in 0..cells {
                let at = cell * GRID + 40;
                self.on_leader("", |e| e.write_file("f", ino, at as u64, &[3; 8]));
                want[at..at + 8].fill(3);
            }
            let (leader, pending) = self.on_leader("", |e| {
                e.ensure_leader()?;
                e.pending_deltas()
            });
            if pending.0 == cells {
                return (leader, (ino, want));
            }
            // A change of leader made some writes rewrite their cell instead.
            assert!(Instant::now() < deadline, "{pending:?} of {cells} dirty");
        }
    }

    /// Small and whole-cell writes that take whatever space is free, with reclaim between.
    fn churn(&self, ino: u64, want: &mut [u8]) {
        for i in 0..10usize {
            let (at, len) = match i % 3 {
                0 => ((i % 4) * GRID, GRID),
                _ => (i * 700 + 11, 900),
            };
            let data = vec![i as u8 + 20; len];
            self.on_leader("", |e| e.write_file("f", ino, at as u64, &data));
            want[at..at + len].copy_from_slice(&data);
            self.on_leader("", |e| e.gc_once());
        }
    }
}

/// The leader is lost when its batched fold reaches `point`; `committed` waits for the command
/// to commit first. Whatever became of it, every replica reads the same bytes as before, and
/// later writes and folds keep them.
fn leader_lost_during_a_batch(point: DeltaPoint, committed: bool) {
    const CELLS: usize = 8;
    let mut c = Cluster::start();
    let (leader, mut a) = c.with_dirty_cells(CELLS);
    let ino = a.0;
    let old = c.meta(&leader);
    until("every replica to advertise batches", || {
        old.engine.fold_batch_supported().unwrap_or(false)
    });
    let pending = old.engine.pending_deltas().unwrap();
    assert_eq!(pending.0, CELLS);
    let reached = Arc::new(AtomicBool::new(false));
    let (flag, raft) = (reached.clone(), old.raft.clone());
    old.engine.set_delta_hook(Some(Arc::new(move |at| {
        if at != point || flag.swap(true, Ordering::SeqCst) {
            return;
        }
        if committed {
            // Until this replica has applied a fold, so the batch has committed.
            let deadline = Instant::now() + WAIT;
            while raft.catalog().unwrap().dirty.len() == CELLS && Instant::now() < deadline {
                thread::sleep(TICK);
            }
        }
        raft.halt();
    })));
    with_pool(&old.engine, None, |attempts| {
        until("a fold to fail", || {
            attempts.failed.load(Ordering::SeqCst) >= 1
        });
        assert!(reached.load(Ordering::SeqCst));
        // A batch committed before its leader died is reported as unknown, not as done. (After
        // a rewrite, another folder's may already have been committed.)
        if point != DeltaPoint::FoldReplicated {
            assert_eq!(attempts.ok.load(Ordering::SeqCst), 0);
        }
    });
    drop(old);
    c.kill(&leader);
    c.converged(ino, &a.1);
    if committed {
        let (new, ()) = c.on_leader("", |e| e.ensure_leader());
        assert!(c.meta(&new).engine.pending_deltas().unwrap().0 < CELLS);
    }
    c.churn(ino, &mut a.1);
    c.converged(ino, &a.1);
    drain_and_converge(&c, ino, &a.1);
    c.restart(&leader);
    c.churn(ino, &mut a.1);
    drain_and_converge(&c, ino, &a.1);
}

#[test]
fn a_leader_lost_after_rewriting_a_cell_loses_no_data() {
    leader_lost_during_a_batch(DeltaPoint::FoldReplicated, false);
}

#[test]
fn a_leader_lost_before_proposing_a_batch_loses_no_data() {
    leader_lost_during_a_batch(DeltaPoint::FoldBeforePropose, false);
}

#[test]
fn a_leader_lost_while_a_batch_replicates_loses_no_data() {
    leader_lost_during_a_batch(DeltaPoint::FoldProposed, false);
}

#[test]
fn a_leader_lost_after_a_batch_commits_loses_no_data() {
    leader_lost_during_a_batch(DeltaPoint::FoldProposed, true);
}

/// A replica advertising only what a build without batched folds does gets single folds.
#[test]
fn batches_wait_until_every_metadata_node_decodes_them() {
    let older = BTreeMap::from([("m3".to_string(), CAP_DELTA)]);
    let mut c = Cluster::start_with(older, 1024);
    let (_, mut a) = c.with_dirty_cells(6);
    let ino = a.0;
    // Time enough for every follower to have answered.
    thread::sleep(SETTLE);
    let batches = |c: &Cluster| -> u64 {
        c.live()
            .map(|(_, m)| m.engine.telemetry.fold_batch.snapshot().count())
            .sum()
    };
    let folds = |c: &Cluster| -> u64 {
        c.live()
            .map(|(_, m)| m.engine.telemetry.snapshot().delta_folds)
            .sum()
    };
    for (id, m) in c.live() {
        assert!(!m.engine.fold_batch_supported().unwrap(), "{id}");
    }
    drain_and_converge(&c, ino, &a.1);
    assert!(folds(&c) >= 6);
    assert_eq!(batches(&c), 0);

    c.capabilities.clear();
    c.restart("m3");
    for cell in 0..6 {
        let at = cell * GRID + 80;
        c.on_leader("", |e| e.write_file("f", ino, at as u64, &[4; 8]));
        a.1[at..at + 8].fill(4);
    }
    until("a leader that batches", || {
        c.live()
            .any(|(_, m)| m.engine.fold_batch_supported().unwrap_or(false))
    });
    drain_and_converge(&c, ino, &a.1);
    assert!(batches(&c) > 0);
}

/// A replica that was away while batches were committed and the log compacted catches up from a
/// snapshot, and applies the batches that follow it.
#[test]
fn a_replica_catches_up_from_a_snapshot_taken_after_batches() {
    let mut c = Cluster::start_with(BTreeMap::new(), 4);
    let (leader, mut a) = c.with_dirty_cells(6);
    let ino = a.0;
    until("every replica to advertise batches", || {
        c.meta(&leader)
            .engine
            .fold_batch_supported()
            .unwrap_or(false)
    });
    c.converged(ino, &a.1);
    let away = c.metas.keys().find(|id| **id != leader).unwrap().clone();
    let had = c.meta(&away).engine.applied_index().unwrap();
    c.kill(&away);
    for _ in 0..2 {
        drain_and_converge(&c, ino, &a.1);
        c.churn(ino, &mut a.1);
    }
    let batches: u64 = c
        .live()
        .map(|(_, m)| m.engine.telemetry.fold_batch.snapshot().count())
        .sum();
    assert!(batches > 0);
    c.restart(&away);
    c.converged(ino, &a.1);
    let snapshot = sum_of(
        &c.meta(&away).raft.render_metrics().unwrap(),
        "atlas_native_raft_snapshot_index",
    );
    assert!(
        snapshot > had as f64,
        "caught up from the log, not a snapshot"
    );
    c.churn(ino, &mut a.1);
    drain_and_converge(&c, ino, &a.1);
}

/// Random small writes, whole-cell writes and truncates while every replica's pool runs, with
/// the leader lost in the middle of each round, compared with a model.
#[test]
fn random_writes_and_batched_folds_survive_leader_changes() {
    const CELLS: usize = 6;
    let mut c = Cluster::start();
    let (_, mut a) = c.with_dirty_cells(CELLS);
    let ino = a.0;
    let mut rng = Rng(20_261_128);
    let mut batches = 0;
    for round in 0..4 {
        let (leader, ()) = c.on_leader("", |e| e.ensure_leader());
        with_pools(&c, || {
            for step in 0..40 {
                if step == 15 + round * 5 {
                    c.meta(&leader).raft.halt();
                }
                match rng.below(12) {
                    0 => {
                        let size = rng.below(CELLS * GRID);
                        for size in [size, CELLS * GRID] {
                            let attr = SetAttr {
                                size: Some(size as u64),
                                ..SetAttr::default()
                            };
                            c.on_leader("", |e| e.fs_setattr("f", ino, attr.clone()));
                            a.1.resize(size, 0);
                        }
                    }
                    n => {
                        let len = if n == 1 { GRID } else { 1 + rng.below(600) };
                        let mut at = rng.below(CELLS * GRID - len);
                        if len == GRID {
                            at -= at % GRID;
                        }
                        let data = vec![rng.below(251) as u8; len];
                        // An unknown outcome is retried: the same bytes at the same place.
                        c.on_leader("", |e| e.write_file("f", ino, at as u64, &data));
                        a.1[at..at + len].copy_from_slice(&data);
                    }
                }
                if step % 8 == 0 {
                    c.on_leader("", |e| e.gc_once());
                }
            }
        });
        batches += c
            .live()
            .map(|(_, m)| m.engine.telemetry.fold_batch.snapshot().count())
            .sum::<u64>();
        c.restart(&leader);
        c.converged(ino, &a.1);
    }
    assert!(batches > 0, "no fold was committed in a batch");
    drain_and_converge(&c, ino, &a.1);
}

fn http(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = std::str::from_utf8(&out[9..12]).unwrap().parse().unwrap();
    (status, out[split + 4..].to_vec())
}

/// The value of a metric on `/metrics`.
fn metric(addr: SocketAddr, name: &str) -> f64 {
    let (_, body) = http(addr, "GET", "/metrics", b"");
    sum_of(&String::from_utf8(body).unwrap(), name)
}

/// The sum of a metric's samples in Prometheus text.
fn sum_of(text: &str, name: &str) -> f64 {
    text.lines()
        .filter(|l| {
            l.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(['{', ' ']))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// A node runs the folders and the early reclaim on their own threads, and shuts them down with
/// folds pending.
#[test]
fn a_node_folds_reclaims_and_shuts_down_with_a_backlog() {
    let td = tempfile::tempdir().unwrap();
    let bind = |at: &str| TcpListener::bind(at).unwrap();
    let (data, meta) = (bind("127.0.0.1:0"), bind("127.0.0.1:0"));
    let data_addr = data.local_addr().unwrap().to_string();
    let meta_addr = meta.local_addr().unwrap().to_string();
    drop((data, meta));
    let config = || -> NodeConfig {
        serde_json::from_value(serde_json::json!({
            "node_id": "n",
            "data_dir": td.path(),
            "http_listen": "127.0.0.1:0",
            "data_node": { "listen": data_addr },
            "metadata": {
                "listen": meta_addr,
                "peers": {},
                "data_nodes": [{ "id": "n", "addr": data_addr }],
                "replicas": 1,
                "extent_bytes": GRID,
                "tick_ms": 10,
                "repair_interval_secs": 0,
                "gc_interval_secs": 0,
                "delta_writes": {
                    "enabled": true,
                    "fold_after_deltas": 1,
                    "fold_interval_ms": 10,
                    "fold_concurrency": 3,
                    "max_pending_bytes": 200_000,
                    "fold_headroom_bytes": 10_000,
                    "gc_grace_ms": 0,
                },
            },
        }))
        .unwrap()
    };
    // The data and Raft ports are fixed in the config, so a restart finds them again.
    let start = || {
        let deadline = Instant::now() + WAIT;
        loop {
            match NativeNode::start_with(config(), Listeners::default()) {
                Ok(n) => return n,
                Err(e) => assert!(Instant::now() < deadline, "{e}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
    };
    let mut n = start();
    let addr = n.http_addr();
    let mut volume = String::new();
    until("a leader", || {
        let body = br#"{"name":"vol","size_bytes":262144}"#;
        let (st, b) = http(addr, "POST", "/v1/volumes", body);
        if st == 201 {
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
            volume = v["id"].as_str().unwrap().to_string();
        }
        st == 201
    });
    let mut want = vec![1u8; 64 * GRID];
    let put = |addr: SocketAddr, at: usize, data: &[u8]| {
        let path = format!("/v1/volumes/{volume}/data?offset={at}");
        let (st, b) = http(addr, "PUT", &path, data);
        assert_eq!(st, 204, "{}", String::from_utf8_lossy(&b));
    };
    put(addr, 0, &want);
    for cell in 0..40 {
        let at = cell * GRID + 8;
        put(addr, at, &[2; 8]);
        want[at..at + 8].fill(2);
    }
    until("the node to fold everything", || {
        metric(addr, "atlas_native_delta_pending") == 0.0
    });
    assert!(metric(addr, "atlas_native_delta_writes_total") > 0.0);
    assert!(metric(addr, "atlas_native_fold_runs_total") > 0.0);
    assert_eq!(metric(addr, "atlas_native_fold_errors_total"), 0.0);
    // No periodic GC is configured: the reclaim thread did it.
    until("an early reclaim", || {
        metric(addr, "atlas_native_gc_runs_total") > 0.0
    });
    for cell in 0..64 {
        let at = cell * GRID + 100;
        put(addr, at, &[3; 8]);
        want[at..at + 8].fill(3);
    }
    let stopping = Instant::now();
    n.shutdown();
    assert!(stopping.elapsed() < Duration::from_secs(20));
    drop(n);

    let n = start();
    let addr = n.http_addr();
    let path = format!("/v1/volumes/{volume}/data?offset=0&len={}", want.len());
    until("the data to be served again", || {
        let (st, got) = http(addr, "GET", &path, b"");
        if st == 200 {
            assert_eq!(got, want);
        }
        st == 200
    });
    until("the node to fold the rest", || {
        metric(addr, "atlas_native_delta_pending") == 0.0
    });
}
