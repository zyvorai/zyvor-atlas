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
    raft::CAP_DELTA,
    run_folders, BlockStore, DataNodeServer, EngineConfig, FailureDomain, FileDevice, MetaBackend,
    NativeEngine, NativeError, Node, NodeType, RaftConfig, RaftError, RaftServer, RemoteDevice,
    SetAttr, ROOT_INO,
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
    let mut folds = 0;
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
}

impl Cluster {
    fn start() -> Self {
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
        rcfg.capabilities = CAP_DELTA;
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
        assert_eq!(attempts.ok.load(Ordering::SeqCst), 0);
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
