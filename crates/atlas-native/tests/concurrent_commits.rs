// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Concurrent commits (`docs/NATIVE_CONCURRENT_COMMITS.md`): randomised model checks, with the
//! option off and on and with delta writes off and on; races against partial overwrites,
//! truncates, unlinks, snapshots and deduplication; and leaders that stop part-way.

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    engine::NewNode, BlockStore, CommitPoint, DataNodeServer, DeltaConfig, EngineConfig,
    FailureDomain, MetaBackend, NativeEngine, NativeError, Node, NodeType, RaftConfig, RaftError,
    RaftServer, RemoteDevice, SetAttr, ROOT_INO,
};

const GRID: usize = 4096;
const TICK: Duration = Duration::from_millis(10);
/// With much shorter ticks an fsync on a busy disk outlasts an election timeout.
const RAFT_TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(30);
/// How long to watch for something that must not happen.
const QUIET: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy)]
struct Mode {
    concurrent: bool,
    delta: bool,
}

const MODES: [Mode; 4] = [
    Mode {
        concurrent: false,
        delta: false,
    },
    Mode {
        concurrent: true,
        delta: false,
    },
    Mode {
        concurrent: false,
        delta: true,
    },
    Mode {
        concurrent: true,
        delta: true,
    },
];

const ON: Mode = Mode {
    concurrent: true,
    delta: false,
};
const ON_WITH_DELTAS: Mode = Mode {
    concurrent: true,
    delta: true,
};

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

fn config(root: &Path, mode: Mode) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = GRID;
    c.concurrent_commits = mode.concurrent;
    // Small limits, so folds and reclaim happen within a short test.
    c.delta_writes = DeltaConfig {
        enabled: mode.delta,
        max_bytes: 1024,
        max_deltas_per_cell: 6,
        fold_after_deltas: 3,
        fold_after_ms: 20,
        fold_cells_per_pass: 3,
        gc_grace_ms: 0,
        ..DeltaConfig::default()
    };
    c
}

fn local(cfg: EngineConfig) -> NativeEngine {
    NativeEngine::open(cfg, (1..=3).map(node).collect()).unwrap()
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

fn file(e: &NativeEngine, name: &str) -> u64 {
    e.fs_mknode("f", ROOT_INO, new_file(name)).unwrap().ino
}

fn engine(root: &Path, mode: Mode) -> NativeEngine {
    let e = local(config(root, mode));
    e.create_fs_as("f".into(), "f").unwrap();
    e
}

fn truncate(e: &NativeEngine, ino: u64, size: usize) -> Result<(), NativeError> {
    let attr = SetAttr {
        size: Some(size as u64),
        ..SetAttr::default()
    };
    e.fs_setattr("f", ino, attr).map(|_| ())
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
        let seed = 1 + self.below(250) as u8;
        (0..len).map(|i| seed.wrapping_add(i as u8 / 7)).collect()
    }

    /// A write of every kind against a file of `size` bytes: new cells, whole cells, partial
    /// overwrites, appends and holes.
    fn write(&mut self, size: usize) -> (usize, Vec<u8>) {
        let (at, len) = match self.below(8) {
            // Whole cells.
            0 => (self.below(4) * GRID, (1 + self.below(2)) * GRID),
            // From the start of a cell, short of or past its end.
            1 | 2 => (self.below(4) * GRID, 1 + self.below(GRID + GRID / 2)),
            // A small file written whole.
            3 => (0, GRID / 2 + self.below(GRID)),
            // Past the end.
            4 => (size + self.below(GRID), 1 + self.below(700)),
            // Appending.
            5 => (size, 1 + self.below(700)),
            // Inside what exists.
            _ => (self.below(size.max(1)), 1 + self.below(900)),
        };
        // Files stay small however often they are appended to.
        let at = if at > 8 * GRID { at % (4 * GRID) } else { at };
        (at, self.bytes(len))
    }
}

/// Tells the other threads of a test to stop however it ends.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn maintain(e: &NativeEngine, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        e.delta_maintenance_once().unwrap();
        e.gc_once().unwrap();
        thread::sleep(Duration::from_millis(3));
    }
}

/// Writes, truncates, deletes, snapshots, clones, GC and reopens from one thread, checked byte
/// for byte against a model.
fn sequential_run(mode: Mode, seed: u64, steps: usize) {
    let td = tempfile::tempdir().unwrap();
    let mut e = engine(td.path(), mode);
    let mut rng = Rng(seed);
    let mut files: BTreeMap<String, (u64, Vec<u8>)> = BTreeMap::new();
    let mut snapshots: BTreeMap<String, BTreeMap<String, (u64, Vec<u8>)>> = BTreeMap::new();
    e.create_volume_as("v".into(), "v", 8 * GRID as u64)
        .unwrap();
    let mut volume = vec![0u8; 8 * GRID];
    let verify = |e: &NativeEngine,
                  files: &BTreeMap<String, (u64, Vec<u8>)>,
                  snapshots: &BTreeMap<String, BTreeMap<String, (u64, Vec<u8>)>>,
                  volume: &[u8]| {
        for (name, (ino, want)) in files {
            assert_eq!(&contents(e, "f", *ino), want, "{mode:?} {name}");
        }
        for (id, tree) in snapshots {
            for (name, (ino, want)) in tree {
                let got = contents(e, &format!("f@{id}"), *ino);
                assert_eq!(&got, want, "{mode:?} {id}/{name}");
            }
        }
        assert_eq!(e.read("v", 0, volume.len()).unwrap(), volume, "{mode:?}");
        e.check_delta_invariants().unwrap();
    };
    for step in 0..steps {
        let names: Vec<String> = files.keys().cloned().collect();
        if names.len() < 3 {
            let name = format!("n{step}");
            let ino = file(&e, &name);
            files.insert(name, (ino, Vec::new()));
            continue;
        }
        let name = names[rng.below(names.len())].clone();
        match rng.below(20) {
            0..=9 => {
                let (ino, want) = files.get_mut(&name).unwrap();
                let (at, data) = rng.write(want.len());
                e.write_file("f", *ino, at as u64, &data).unwrap();
                overwrite(want, at, &data);
                assert_eq!(&contents(&e, "f", *ino), want, "{mode:?} step {step}");
            }
            10 => {
                let (at, data) = rng.write(volume.len() / 2);
                let at = at.min(volume.len() - data.len());
                e.write("v", at as u64, &data).unwrap();
                overwrite(&mut volume, at, &data);
            }
            11 | 12 => {
                let (ino, want) = files.get_mut(&name).unwrap();
                let size = rng.below(want.len() + GRID);
                truncate(&e, *ino, size).unwrap();
                want.resize(size, 0);
            }
            13 => {
                e.fs_unlink("f", ROOT_INO, &name).unwrap();
                files.remove(&name);
            }
            14 => {
                if snapshots.len() < 3 {
                    let id = format!("s{step}");
                    e.snapshot_fs_as(id.clone(), "f", &id).unwrap();
                    snapshots.insert(id, files.clone());
                }
            }
            15 => {
                if let Some(id) = snapshots.keys().next().cloned() {
                    e.delete_fs_snapshot(&id).unwrap();
                    snapshots.remove(&id);
                }
            }
            16 => {
                e.delta_maintenance_once().unwrap();
            }
            17 | 18 => {
                e.gc_once().unwrap();
            }
            _ => {
                if rng.below(3) == 0 {
                    drop(e);
                    e = local(config(td.path(), mode));
                } else {
                    e.repair_once().unwrap();
                }
                verify(&e, &files, &snapshots, &volume);
            }
        }
        if step % 50 == 0 {
            verify(&e, &files, &snapshots, &volume);
        }
    }
    verify(&e, &files, &snapshots, &volume);
    let committed = e.telemetry.snapshot().concurrent_commits;
    assert!(mode.concurrent || committed == 0);
}

#[test]
fn random_operations_match_a_model_in_every_mode() {
    for (i, mode) in MODES.into_iter().enumerate() {
        sequential_run(mode, 0x9e37_79b9_7f4a_7c15 + i as u64, 600);
        sequential_run(mode, 0x0123_4567_89ab_cdef + i as u64, 400);
    }
}

/// Writers put disjoint pieces into files and volumes nobody has written yet, so that they race
/// to create the same cells: every piece must be there.
#[test]
fn writers_creating_the_same_cells_keep_every_byte() {
    const WRITERS: usize = 6;
    for mode in MODES {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path(), mode);
        let mut rng = Rng(7);
        let mut done = Vec::new();
        for round in 0..60 {
            let ino = file(&e, &format!("r{round}"));
            let volume = format!("v{round}");
            e.create_volume_as(volume.clone(), &volume, 8 * GRID as u64)
                .unwrap();
            // Back to back pieces of random lengths: some share a cell, some cover whole ones.
            let mut at = 0;
            let pieces: Vec<(usize, Vec<u8>)> = (0..WRITERS)
                .map(|_| {
                    let len = match rng.below(4) {
                        0 => GRID,
                        1 => GRID + rng.below(200),
                        _ => 1 + rng.below(GRID),
                    };
                    let piece = (at, rng.bytes(len));
                    at += len + [0, 0, 100][rng.below(3)];
                    if rng.below(3) == 0 {
                        at = at.next_multiple_of(GRID);
                    }
                    piece
                })
                .collect();
            thread::scope(|s| {
                for (at, data) in &pieces {
                    let (e, volume) = (&e, &volume);
                    s.spawn(move || {
                        e.write_file("f", ino, *at as u64, data).unwrap();
                        let got = e.read_file("f", ino, *at as u64, data.len()).unwrap();
                        assert_eq!(&got, data, "{mode:?} round {round}");
                        if *at + data.len() <= 8 * GRID {
                            e.write(volume, *at as u64, data).unwrap();
                        }
                    });
                }
            });
            let mut want = Vec::new();
            let mut want_volume = vec![0; 8 * GRID];
            for (at, data) in &pieces {
                overwrite(&mut want, *at, data);
                if *at + data.len() <= 8 * GRID {
                    overwrite(&mut want_volume, *at, data);
                }
            }
            assert_eq!(contents(&e, "f", ino), want, "{mode:?} round {round}");
            assert_eq!(e.read(&volume, 0, 8 * GRID).unwrap(), want_volume);
            done.push((ino, want));
            if round % 7 == 0 {
                e.gc_once().unwrap();
                e.delta_maintenance_once().unwrap();
            }
        }
        for (ino, want) in done {
            assert_eq!(contents(&e, "f", ino), want, "{mode:?}");
        }
        e.check_delta_invariants().unwrap();
        let committed = e.telemetry.snapshot().concurrent_commits;
        assert_eq!(committed > 0, mode.concurrent, "{mode:?}: {committed}");
    }
}

/// A file's states, in the order its only writer made them.
#[derive(Default)]
struct History {
    states: Vec<Vec<u8>>,
    /// The newest state whose change has returned.
    acked: usize,
}

/// Each writer changes its own file while snapshots are taken and other threads fold, reclaim
/// and change modes. A write reads back at once, and a snapshot holds, for every file, a state
/// no older than the last one acknowledged before it was asked for and no newer than the last
/// one started before it returned.
#[test]
fn concurrent_writers_stay_linearizable_under_snapshots() {
    const WRITERS: usize = 5;
    for mode in MODES {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path(), mode);
        let inos: Vec<u64> = (0..WRITERS).map(|w| file(&e, &format!("w{w}"))).collect();
        let histories: Vec<Mutex<History>> = (0..WRITERS)
            .map(|_| {
                Mutex::new(History {
                    states: vec![Vec::new()],
                    acked: 0,
                })
            })
            .collect();
        let stop = AtomicBool::new(false);
        thread::scope(|s| {
            let _stop = StopOnDrop(&stop);
            s.spawn(|| maintain(&e, &stop));
            s.spawn(|| {
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    let attr = SetAttr {
                        mode: Some(0o600 + (i % 2) * 0o44),
                        ..SetAttr::default()
                    };
                    e.fs_setattr("f", inos[i as usize % WRITERS], attr).unwrap();
                    i += 1;
                    thread::sleep(Duration::from_millis(1));
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|w| {
                    let (e, ino, history) = (&e, inos[w], &histories[w]);
                    s.spawn(move || {
                        let mut rng = Rng(w as u64 * 1_000_003 + 11);
                        let mut want = Vec::new();
                        for i in 0..120 {
                            let cut = (rng.below(9) == 0).then(|| rng.below(want.len() + GRID));
                            let write = cut.is_none().then(|| rng.write(want.len()));
                            match (&cut, &write) {
                                (Some(size), _) => want.resize(*size, 0),
                                (None, Some((at, data))) => overwrite(&mut want, *at, data),
                                (None, None) => unreachable!(),
                            }
                            history.lock().unwrap().states.push(want.clone());
                            match (cut, write) {
                                (Some(size), _) => truncate(e, ino, size).unwrap(),
                                (None, Some((at, data))) => {
                                    e.write_file("f", ino, at as u64, &data).unwrap();
                                }
                                (None, None) => unreachable!(),
                            }
                            {
                                let mut h = history.lock().unwrap();
                                h.acked = h.states.len() - 1;
                            }
                            assert_eq!(contents(e, "f", ino), want, "{mode:?} {w} step {i}");
                        }
                    })
                })
                .collect();
            let mut taken = 0;
            while !writers.iter().all(|w| w.is_finished()) {
                let floors: Vec<usize> =
                    histories.iter().map(|h| h.lock().unwrap().acked).collect();
                let id = format!("s{taken}");
                e.snapshot_fs_as(id.clone(), "f", &id).unwrap();
                let ceilings: Vec<usize> = histories
                    .iter()
                    .map(|h| h.lock().unwrap().states.len())
                    .collect();
                for w in 0..WRITERS {
                    let got = contents(&e, &format!("f@{id}"), inos[w]);
                    let h = histories[w].lock().unwrap();
                    assert!(
                        h.states[floors[w]..ceilings[w]].contains(&got),
                        "{mode:?} snapshot {id} of file {w}: {} bytes match no state in {}..{}",
                        got.len(),
                        floors[w],
                        ceilings[w],
                    );
                }
                e.delete_fs_snapshot(&id).unwrap();
                taken += 1;
                thread::sleep(Duration::from_millis(5));
            }
            for w in writers {
                w.join().unwrap();
            }
            stop.store(true, Ordering::SeqCst);
        });
        e.check_delta_invariants().unwrap();
        let reopened = {
            drop(e);
            local(config(td.path(), mode))
        };
        for w in 0..WRITERS {
            let h = histories[w].lock().unwrap();
            assert_eq!(&contents(&reopened, "f", inos[w]), h.states.last().unwrap());
        }
    }
}

/// Another client cuts a file just past a writer's bytes, in the cell the writer is rewriting,
/// again and again: none of the writer's bytes may go.
#[test]
fn truncates_in_a_cell_being_written_keep_the_bytes_below_the_cut() {
    const OWNED: usize = 2 * GRID + 1000;
    for mode in MODES {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path(), mode);
        let ino = file(&e, "a");
        e.write_file("f", ino, 0, &[1; 10]).unwrap();
        let stop = AtomicBool::new(false);
        let want = thread::scope(|s| {
            let _stop = StopOnDrop(&stop);
            s.spawn(|| maintain(&e, &stop));
            s.spawn(|| {
                let mut rng = Rng(99);
                while !stop.load(Ordering::SeqCst) {
                    let at = OWNED + 100 + rng.below(1000);
                    e.write_file("f", ino, at as u64, &rng.bytes(200)).unwrap();
                    truncate(&e, ino, OWNED + rng.below(100)).unwrap();
                }
            });
            let writer = s.spawn(|| {
                let mut rng = Rng(5);
                let mut want = vec![1; 10];
                want.resize(OWNED, 0);
                for i in 0..200 {
                    let (at, len) = match rng.below(3) {
                        0 => (rng.below(3) * GRID, 1000),
                        1 => (rng.below(2) * GRID, GRID),
                        _ => {
                            let at = rng.below(OWNED - 600);
                            (at, 1 + rng.below(600))
                        }
                    };
                    let len = len.min(OWNED - at);
                    let data = rng.bytes(len);
                    e.write_file("f", ino, at as u64, &data).unwrap();
                    overwrite(&mut want, at, &data);
                    let got = e.read_file("f", ino, at as u64, len).unwrap();
                    assert_eq!(got, data, "{mode:?} write {i}");
                }
                want
            });
            let want = writer.join().unwrap();
            stop.store(true, Ordering::SeqCst);
            want
        });
        truncate(&e, ino, OWNED).unwrap();
        assert_eq!(contents(&e, "f", ino), want, "{mode:?}");
        truncate(&e, ino, OWNED + 1500).unwrap();
        let got = contents(&e, "f", ino);
        assert!(got[OWNED..].iter().all(|b| *b == 0), "{mode:?}");
        e.check_delta_invariants().unwrap();
    }
}

/// Holds the first concurrent write to reach `point` until released.
struct Held {
    reached: Receiver<()>,
    release: Sender<()>,
}

impl Held {
    fn at(e: &NativeEngine, point: CommitPoint) -> Self {
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let taken = AtomicBool::new(false);
        e.set_commit_hook(Some(Arc::new(move |p| {
            if p == point && !taken.swap(true, Ordering::SeqCst) {
                reached_tx.send(()).unwrap();
                // A dropped sender releases it too, so a failed test does not hang.
                let _ = release_rx.lock().unwrap().recv_timeout(WAIT);
            }
        })));
        Self { reached, release }
    }

    /// Writes `data` at `at` of `ino`, runs `meanwhile` while that write is held with its data
    /// placed, and returns what the write came to.
    fn write(
        self,
        e: &NativeEngine,
        ino: u64,
        at: usize,
        data: &[u8],
        meanwhile: impl FnOnce(),
    ) -> Result<(), NativeError> {
        thread::scope(|s| {
            let release = self.release;
            let write = s.spawn(move || e.write_file("f", ino, at as u64, data).map(|_| ()));
            self.reached.recv_timeout(WAIT).expect("the write to place");
            meanwhile();
            release.send(()).unwrap();
            write.join().unwrap()
        })
    }
}

fn conflicts(e: &NativeEngine) -> u64 {
    e.telemetry.snapshot().concurrent_conflicts
}

#[test]
fn writes_of_one_new_cell_take_turns() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path(), ON);
    let ino = file(&e, "a");
    let hold = Held::at(&e, CommitPoint::Placed);
    let other = &e;
    thread::scope(|s| {
        let mut second = None;
        hold.write(&e, ino, 0, &[1; 100], || {
            let write = s.spawn(move || other.write_file("f", ino, 200, &[2; 100]));
            thread::sleep(QUIET);
            assert!(!write.is_finished(), "did not wait for its turn");
            assert_eq!(e.fs_getattr("f", ino).unwrap().size, 0);
            second = Some(write);
        })
        .unwrap();
        second.unwrap().join().unwrap().unwrap();
    });
    let mut want = vec![1; 100];
    want.resize(200, 0);
    want.resize(300, 2);
    assert_eq!(contents(&e, "f", ino), want);
    assert_eq!(conflicts(&e), 0);
}

#[test]
fn a_held_write_delays_neither_other_cells_nor_other_files() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path(), ON);
    let (a, b) = (file(&e, "a"), file(&e, "b"));
    e.write_file("f", b, 0, &[9; 2 * GRID]).unwrap();
    let hold = Held::at(&e, CommitPoint::Placed);
    hold.write(&e, a, 0, &[1; 100], || {
        // Each of these returns while the first write is held.
        e.write_file("f", a, GRID as u64, &[2; 100]).unwrap();
        e.write_file("f", b, 10, &[3; 10]).unwrap();
        truncate(&e, b, 100).unwrap();
        let attr = SetAttr {
            mode: Some(0o600),
            ..SetAttr::default()
        };
        e.fs_setattr("f", a, attr).unwrap();
    })
    .unwrap();
    let mut want = vec![1; 100];
    want.resize(GRID, 0);
    want.resize(GRID + 100, 2);
    assert_eq!(contents(&e, "f", a), want);
    assert_eq!(contents(&e, "f", b)[10..20], [3; 10]);
    assert_eq!(e.fs_getattr("f", a).unwrap().mode, 0o600);
}

#[test]
fn a_cell_that_grew_meanwhile_is_merged_not_overwritten() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path(), ON);
    let ino = file(&e, "a");
    e.write_file("f", ino, 0, &[1; 100]).unwrap();
    let hold = Held::at(&e, CommitPoint::Placed);
    // 200 bytes cover the 100 there are, until a partial write makes it 300.
    hold.write(&e, ino, 0, &[2; 200], || {
        e.write_file("f", ino, 250, &[3; 50]).unwrap();
    })
    .unwrap();
    let mut want = vec![2; 200];
    want.resize(250, 0);
    want.resize(300, 3);
    assert_eq!(contents(&e, "f", ino), want);
    assert_eq!(conflicts(&e), 1);
    e.gc_once().unwrap();
    assert_eq!(contents(&e, "f", ino), want);
}

#[test]
fn a_write_racing_a_truncate_lands_after_it() {
    for mode in [ON, ON_WITH_DELTAS] {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path(), mode);
        let ino = file(&e, "a");
        e.write_file("f", ino, 0, &vec![1; 2 * GRID]).unwrap();
        let hold = Held::at(&e, CommitPoint::Placed);
        hold.write(&e, ino, GRID, &vec![2; GRID], || {
            truncate(&e, ino, 10).unwrap();
        })
        .unwrap();
        let mut want = vec![1; 10];
        want.resize(GRID, 0);
        want.resize(2 * GRID, 2);
        assert_eq!(contents(&e, "f", ino), want, "{mode:?}");
        e.check_delta_invariants().unwrap();
    }
}

#[test]
fn a_write_to_a_file_or_volume_deleted_meanwhile_fails() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path(), ON);
    let ino = file(&e, "a");
    let hold = Held::at(&e, CommitPoint::Placed);
    let result = hold.write(&e, ino, 0, &[1; 100], || {
        e.fs_unlink("f", ROOT_INO, "a").unwrap();
    });
    assert!(result.is_err(), "{result:?}");

    e.create_volume_as("v".into(), "v", GRID as u64).unwrap();
    let hold = Held::at(&e, CommitPoint::Placed);
    thread::scope(|s| {
        let write = s.spawn(|| e.write("v", 0, &[1; 100]));
        hold.reached.recv_timeout(WAIT).unwrap();
        e.delete_volume("v").unwrap();
        hold.release.send(()).unwrap();
        assert!(write.join().unwrap().is_err());
    });
    e.gc_once().unwrap();
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_snapshot_taken_while_a_write_is_held_never_changes() {
    for mode in [ON, ON_WITH_DELTAS] {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path(), mode);
        let ino = file(&e, "a");
        e.write_file("f", ino, 0, &[1; GRID]).unwrap();
        let hold = Held::at(&e, CommitPoint::Placed);
        hold.write(&e, ino, 0, &[2; GRID], || {
            // With delta writes on this is a delta on the cell, which the snapshot folds.
            e.write_file("f", ino, 8, &[3; 8]).unwrap();
            e.snapshot_fs_as("s".into(), "f", "s").unwrap();
        })
        .unwrap();
        let mut old = vec![1; GRID];
        old[8..16].fill(3);
        assert_eq!(contents(&e, "f@s", ino), old, "{mode:?}");
        assert_eq!(contents(&e, "f", ino), vec![2; GRID], "{mode:?}");
        e.fold_all_deltas().unwrap();
        e.gc_once().unwrap();
        assert_eq!(contents(&e, "f@s", ino), old, "{mode:?}");
        e.check_delta_invariants().unwrap();
    }
}

#[test]
fn a_delta_appended_while_a_whole_write_is_held_is_replaced_with_its_cell() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path(), ON_WITH_DELTAS);
    let ino = file(&e, "a");
    e.write_file("f", ino, 0, &[1; GRID]).unwrap();
    let hold = Held::at(&e, CommitPoint::Placed);
    hold.write(&e, ino, 0, &[2; GRID], || {
        e.write_file("f", ino, 8, &[3; 8]).unwrap();
        assert_eq!(e.pending_deltas().unwrap(), (1, 1));
    })
    .unwrap();
    assert_eq!(contents(&e, "f", ino), vec![2; GRID]);
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_share_that_went_stale_is_placed_afresh() {
    let td = tempfile::tempdir().unwrap();
    let mut cfg = config(td.path(), ON);
    cfg.dedup = true;
    let e = local(cfg);
    e.create_fs_as("f".into(), "f").unwrap();
    let (a, b) = (file(&e, "a"), file(&e, "b"));
    let data = Rng(3).bytes(GRID);
    e.write_file("f", a, 0, &data).unwrap();
    let hold = Held::at(&e, CommitPoint::Placed);
    hold.write(&e, b, 0, &data, || {
        e.fs_unlink("f", ROOT_INO, "a").unwrap();
        e.gc_once().unwrap();
    })
    .unwrap();
    assert_eq!(contents(&e, "f", b), data);
    e.gc_once().unwrap();
    assert_eq!(contents(&e, "f", b), data);
    e.check_delta_invariants().unwrap();
}

struct DataNodes {
    servers: Vec<DataNodeServer>,
}

impl DataNodes {
    fn start(root: &Path) -> Self {
        let servers = (1..=3)
            .map(|i| {
                let dir = root.join(format!("n{i}"));
                let device = atlas_native::FileDevice::open(dir.join("d.data")).unwrap();
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                DataNodeServer::start_devices(
                    format!("n{i}"),
                    &dir,
                    vec![Arc::new(device)],
                    listener,
                    None,
                )
                .unwrap()
            })
            .collect();
        Self { servers }
    }

    fn stores(&self) -> Vec<(Node, Arc<dyn BlockStore>)> {
        self.servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let remote = RemoteDevice::new(s.local_addr(), WAIT);
                (node(i + 1), Arc::new(remote) as Arc<dyn BlockStore>)
            })
            .collect()
    }
}

#[derive(Clone)]
struct Meta {
    engine: Arc<NativeEngine>,
    raft: Arc<RaftServer>,
}

/// Three metadata replicas with concurrent commits over three data nodes.
struct Cluster {
    td: tempfile::TempDir,
    dns: DataNodes,
    addrs: BTreeMap<String, SocketAddr>,
    metas: BTreeMap<String, Option<Meta>>,
}

impl Cluster {
    fn start() -> Self {
        let td = tempfile::tempdir().unwrap();
        let dns = DataNodes::start(td.path());
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
            c.open(&id, l);
        }
        c
    }

    fn root(&self, id: &str) -> PathBuf {
        self.td.path().join(id)
    }

    fn open(&mut self, id: &str, listener: TcpListener) {
        let mut peers = self.addrs.clone();
        peers.remove(id);
        let rcfg = RaftConfig::new(
            id,
            peers.keys().cloned().collect(),
            self.root(id).join("raft"),
        );
        let raft = Arc::new(RaftServer::start(rcfg, listener, peers, RAFT_TICK).unwrap());
        let engine = NativeEngine::open_with(
            config(&self.root(id).join("engine"), ON),
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
            m.engine.set_commit_hook(None);
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

    /// Runs `op` on each live replica; the first that accepts it, or why none did.
    fn once<T>(
        &self,
        op: &mut impl FnMut(&NativeEngine) -> Result<T, NativeError>,
    ) -> Result<(String, T), String> {
        let mut last = String::new();
        for (id, m) in self.live() {
            match op(&m.engine) {
                Ok(v) => return Ok((id.clone(), v)),
                Err(NativeError::Raft(
                    e @ (RaftError::NotLeader { .. }
                    | RaftError::LeadershipLost { .. }
                    | RaftError::TermChanged { .. }
                    | RaftError::Timeout { .. }
                    | RaftError::Shutdown),
                )) => last = e.to_string(),
                Err(e @ (NativeError::Fenced { .. } | NativeError::Unknown(_))) => {
                    last = e.to_string()
                }
                Err(other) => panic!("{id}: {other}"),
            }
        }
        Err(last)
    }

    fn on_leader<T>(
        &self,
        mut op: impl FnMut(&NativeEngine) -> Result<T, NativeError>,
    ) -> (String, T) {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.once(&mut op) {
                Ok(done) => return done,
                Err(last) => assert!(Instant::now() < deadline, "no leader accepted: {last}"),
            }
            thread::sleep(TICK);
        }
    }

    fn engine(&self, id: &str) -> Arc<NativeEngine> {
        self.metas[id].as_ref().unwrap().engine.clone()
    }

    fn raft(&self, id: &str) -> Arc<RaftServer> {
        self.metas[id].as_ref().unwrap().raft.clone()
    }

    /// A filesystem with file `a` holding 100 bytes of 1s: the leader and the inode.
    fn with_file(&self) -> (String, u64) {
        self.on_leader(|e| e.create_fs_as("f".into(), "f"));
        let (_, attr) = self.on_leader(|e| e.fs_mknode("f", ROOT_INO, new_file("a")));
        let (leader, _) = self.on_leader(|e| e.write_file("f", attr.ino, 0, &[1; 100]));
        (leader, attr.ino)
    }

    /// Every live replica applies the same catalog and reads the same bytes of each file.
    fn converged(&self, inos: &[u64]) -> Vec<Vec<u8>> {
        let (leader, want) = self.on_leader(|e| {
            e.ensure_leader()?;
            inos.iter()
                .map(|&ino| e.read_file("f", ino, 0, 1 << 20))
                .collect::<Result<Vec<_>, _>>()
        });
        let applied = self.engine(&leader).applied_index().unwrap();
        let deadline = Instant::now() + WAIT;
        for (id, m) in self.live() {
            while m.engine.applied_index().unwrap() < applied {
                assert!(Instant::now() < deadline, "{id} did not catch up");
                thread::sleep(TICK);
            }
            for (&ino, want) in inos.iter().zip(&want) {
                assert_eq!(&m.engine.read_file("f", ino, 0, 1 << 20).unwrap(), want);
            }
            m.raft.catalog().unwrap().check_delta_invariants().unwrap();
        }
        want
    }
}

#[test]
fn a_leader_stopped_before_proposing_changes_nothing() {
    let mut c = Cluster::start();
    let (leader, ino) = c.with_file();
    let raft = c.raft(&leader);
    c.engine(&leader)
        .set_commit_hook(Some(Arc::new(move |point| {
            if point == CommitPoint::Placed {
                raft.halt();
            }
        })));
    let err = c.engine(&leader).write_file("f", ino, 0, &[2; 200]);
    assert!(err.is_err());
    c.kill(&leader);
    assert_eq!(c.converged(&[ino])[0], vec![1; 100]);
    c.on_leader(|e| e.write_file("f", ino, 0, &[3; 150]));
    c.restart(&leader);
    assert_eq!(c.converged(&[ino])[0], vec![3; 150]);
}

fn stopped_after_proposing(committed: bool) {
    let mut c = Cluster::start();
    let (leader, ino) = c.with_file();
    let raft = c.raft(&leader);
    c.engine(&leader)
        .set_commit_hook(Some(Arc::new(move |point| {
            if point == CommitPoint::Proposed {
                if committed {
                    let size = |raft: &RaftServer| {
                        let size = |c: &atlas_native::Catalog| {
                            c.filesystem("f").unwrap().inode(ino).unwrap().size()
                        };
                        raft.with_catalog(size).unwrap()
                    };
                    let deadline = Instant::now() + WAIT;
                    while size(&raft) != 200 {
                        assert!(Instant::now() < deadline, "the write never committed");
                        thread::sleep(TICK);
                    }
                }
                raft.halt();
            }
        })));
    let err = c.engine(&leader).write_file("f", ino, 0, &[2; 200]);
    assert!(err.is_err(), "acknowledged by a stopped leader");
    c.kill(&leader);
    let first = c.converged(&[ino]).remove(0);
    if committed {
        assert_eq!(first, vec![2; 200]);
    } else {
        assert!(first == vec![1; 100] || first == vec![2; 200]);
    }
    c.on_leader(|e| e.write_file("f", ino, 0, &[2; 200]));
    c.on_leader(|e| e.write_file("f", ino, 12, &[3; 2]));
    c.restart(&leader);
    let mut want = vec![2; 200];
    want[12..14].fill(3);
    assert_eq!(c.converged(&[ino])[0], want);
}

#[test]
fn a_leader_stopped_while_the_command_replicates_does_not_acknowledge() {
    stopped_after_proposing(false);
}

#[test]
fn a_leader_stopped_after_the_commit_does_not_acknowledge() {
    stopped_after_proposing(true);
}

#[test]
fn a_proposal_that_cannot_commit_keeps_its_space_until_its_fate_is_known() {
    let mut c = Cluster::start();
    let (leader, ino) = c.with_file();
    // Free space for the data to be placed in.
    for _ in 0..3 {
        c.on_leader(|e| e.write_file("f", ino, 0, &[1; 100]));
    }
    c.on_leader(|e| e.gc_once());
    let followers: Vec<(String, Arc<RaftServer>)> = c
        .metas
        .keys()
        .filter(|id| **id != leader)
        .map(|id| (id.clone(), c.raft(id)))
        .collect();
    let cut = followers.clone();
    c.engine(&leader)
        .set_commit_hook(Some(Arc::new(move |point| {
            if point == CommitPoint::Placed {
                cut.iter().for_each(|(_, r)| r.halt());
            }
        })));
    let err = c.engine(&leader).write_file("f", ino, 0, &[2; 100]);
    assert!(err.is_err());
    c.engine(&leader).set_commit_hook(None);
    drop(followers);
    for id in c.metas.keys().cloned().collect::<Vec<_>>() {
        if id != leader {
            c.restart(&id);
        }
    }
    // Writes elsewhere while the unknown command may or may not still commit.
    for i in 1..6u8 {
        let at = u64::from(i) * GRID as u64;
        c.on_leader(|e| e.write_file("f", ino, at, &[i + 10; 100]));
    }
    let got = c.converged(&[ino]).remove(0);
    assert!(got[..100] == [1; 100] || got[..100] == [2; 100]);
    for i in 1..6usize {
        assert_eq!(got[i * GRID..i * GRID + 100], [i as u8 + 10; 100]);
    }
}

/// Writers of their own files keep writing while the leader is stopped again and again. A write
/// that fails is repeated until it succeeds; each one acknowledged must stay.
#[test]
fn concurrent_writers_keep_every_acknowledged_write_across_leader_changes() {
    const WRITERS: usize = 4;
    let c = RwLock::new(Cluster::start());
    c.read().unwrap().with_file();
    let inos: Vec<u64> = (0..WRITERS)
        .map(|w| {
            let c = c.read().unwrap();
            let new = new_file(&format!("w{w}"));
            c.on_leader(|e| e.fs_mknode("f", ROOT_INO, new.clone()))
                .1
                .ino
        })
        .collect();
    let stop = AtomicBool::new(false);
    let mut committed = 0;
    let models: Vec<Vec<u8>> = thread::scope(|s| {
        let _stop = StopOnDrop(&stop);
        let writers: Vec<_> = inos
            .iter()
            .map(|&ino| {
                let (c, stop) = (&c, &stop);
                s.spawn(move || {
                    let mut rng = Rng(ino * 31 + 3);
                    let mut want = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        let (at, data) = rng.write(want.len());
                        let deadline = Instant::now() + WAIT;
                        loop {
                            let done = c.read().unwrap().once(&mut |e: &NativeEngine| {
                                e.write_file("f", ino, at as u64, &data)?;
                                e.ensure_leader()?;
                                e.read_file("f", ino, at as u64, data.len())
                            });
                            match done {
                                Ok((_, got)) => {
                                    assert_eq!(got, data, "inode {ino}");
                                    break;
                                }
                                Err(last) => {
                                    assert!(Instant::now() < deadline, "{last}");
                                    thread::sleep(TICK);
                                }
                            }
                        }
                        overwrite(&mut want, at, &data);
                    }
                    want
                })
            })
            .collect();
        // Counted before a replica is restarted, which forgets them.
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(500));
            let (leader, raft) = {
                let c = c.read().unwrap();
                let (leader, ()) = c.on_leader(|e| e.ensure_leader());
                let raft = c.raft(&leader);
                (leader, raft)
            };
            let counted = c.read().unwrap().engine(&leader).telemetry.snapshot();
            committed += counted.concurrent_commits;
            // Stopped first, so that writers waiting on it let go of the cluster.
            raft.halt();
            drop(raft);
            c.write().unwrap().restart(&leader);
            c.read().unwrap().on_leader(|e| {
                e.ensure_leader()?;
                e.gc_once()
            });
        }
        stop.store(true, Ordering::SeqCst);
        writers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let c = c.into_inner().unwrap();
    let got = c.converged(&inos);
    for (w, (got, want)) in got.iter().zip(&models).enumerate() {
        assert_eq!(got, want, "writer {w}");
    }
    assert!(committed > 0, "no leader committed concurrently");
}
