// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Group commit on [`FileDevice`]: concurrent writers share syncs. The tests hold, fail and
//! count syncs through [`DeviceSync`].
//!
//! Benchmark against a sync per write (what the device did before):
//! `cargo test --release -p atlas-native --test group_commit -- --ignored --nocapture`
//!
//! Environment:
//! - `ATLAS_BENCH_DIR`: directory for the files (default: the system temp dir). Point it at the
//!   filesystem under test; only its own `atlas-bench-*` subdirectories are created and removed.
//! - `BENCH_OPS` (default 192): writes per run, split between the writers.
//! - `BENCH_SIZES_KIB` (default `4,64,1024`), `BENCH_ROUNDS` (default 3).
//! - `BENCH_FILES` (default 1): devices the writers are spread over round-robin, each with its
//!   own group commit.

use std::{
    fs::{File, OpenOptions},
    io,
    net::TcpListener,
    os::unix::fs::FileExt,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{channel, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread::{self, ScopedJoinHandle, ThreadId},
    time::{Duration, Instant},
};

use atlas_native::{
    device::{DeviceSync, FileDevice},
    BlockStore, DataNodeServer, NativeError, RemoteDevice,
};

const WAIT: Duration = Duration::from_secs(10);
/// How long to watch for something that must not happen.
const QUIET: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum Verdict {
    Succeed,
    Fail,
    Panic,
}

/// A sync that has started and waits to be told how it ends.
struct Held(Sender<Verdict>);

impl Held {
    fn succeed(self) {
        self.0.send(Verdict::Succeed).unwrap();
    }
    fn fail(self) {
        self.0.send(Verdict::Fail).unwrap();
    }
    fn panic(self) {
        self.0.send(Verdict::Panic).unwrap();
    }
}

#[derive(Debug)]
struct Gate(Sender<Held>);

impl DeviceSync for Gate {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        let (tx, rx) = channel();
        self.0
            .send(Held(tx))
            .map_err(|_| io::Error::other("nobody is watching the syncs"))?;
        // Longer than any wait in the tests, so a test has already failed by then; succeeding
        // lets its writers finish instead of hanging it.
        match rx.recv_timeout(WAIT * 2) {
            Ok(Verdict::Succeed) | Err(RecvTimeoutError::Timeout) => file.sync_data(),
            Ok(Verdict::Fail) => Err(io::Error::other("injected sync failure")),
            Ok(Verdict::Panic) => panic!("injected sync panic"),
            Err(e @ RecvTimeoutError::Disconnected) => Err(io::Error::other(e)),
        }
    }
}

/// The syncs a gated device starts, in order.
struct Syncs(Receiver<Held>);

impl Syncs {
    fn next(&self) -> Held {
        self.0.recv_timeout(WAIT).expect("a sync to start")
    }
    fn none(&self) {
        assert!(
            self.0.recv_timeout(QUIET).is_err(),
            "a sync started that should not have"
        );
    }
}

fn gated(path: &Path) -> (FileDevice, Syncs) {
    let (tx, rx) = channel();
    let device = FileDevice::open_with_sync(path, Box::new(Gate(tx))).unwrap();
    (device, Syncs(rx))
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn assert_sync_failure<T: std::fmt::Debug>(result: Result<T, NativeError>, cause: &str) {
    match result {
        Err(NativeError::Io(e)) => {
            let message = e.to_string();
            assert!(message.contains("device sync failed"), "{message}");
            assert!(message.contains(cause), "{message}");
        }
        other => panic!("expected a sync failure, got {other:?}"),
    }
}

fn join<T>(h: ScopedJoinHandle<'_, T>) -> T {
    wait_for("a writer to be released", || h.is_finished());
    h.join().unwrap()
}

#[derive(Debug, Clone, Default)]
struct Recorder(Arc<Mutex<Vec<ThreadId>>>);

impl DeviceSync for Recorder {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        self.0.lock().unwrap().push(thread::current().id());
        file.sync_data()
    }
}

/// No flusher hand-off and no batching delay: each write of a lone writer runs exactly one sync,
/// itself, before returning.
#[test]
fn a_lone_writer_runs_its_own_sync_for_each_write() {
    let td = tempfile::tempdir().unwrap();
    let syncs = Recorder::default();
    let d = FileDevice::open_with_sync(td.path().join("dev"), Box::new(syncs.clone())).unwrap();
    for i in 0..10usize {
        assert_eq!(d.append(b"abcd").unwrap(), 4 * i as u64);
        assert_eq!(syncs.0.lock().unwrap().len(), 2 * i + 1);
        d.write_at(0, b"wxyz").unwrap();
        assert_eq!(syncs.0.lock().unwrap().len(), 2 * i + 2);
        assert_eq!(d.unsynced_writes(), 0);
    }
    let me = thread::current().id();
    assert!(syncs.0.lock().unwrap().iter().all(|t| *t == me));
}

#[test]
fn a_writer_is_released_only_by_a_sync_that_started_after_its_write() {
    let td = tempfile::tempdir().unwrap();
    let (d, syncs) = gated(&td.path().join("dev"));
    let d = &d;
    thread::scope(|s| {
        let a = s.spawn(move || d.append(b"aaaa"));
        let first = syncs.next();
        let b = s.spawn(move || d.write_at(0, b"bb"));
        wait_for("b's write to return", || d.unsynced_writes() == 2);
        assert_eq!(d.read_exact_at(0, 4).unwrap(), b"bbaa");
        syncs.none();

        first.succeed();
        assert_eq!(join(a).unwrap(), 0);
        // The first sync started before b wrote, so b needs another.
        let second = syncs.next();
        thread::sleep(QUIET);
        assert!(!b.is_finished(), "released by an earlier sync");
        second.succeed();
        join(b).unwrap();
        syncs.none();
        assert_eq!(d.unsynced_writes(), 0);
    });
}

#[test]
fn writes_that_return_during_a_sync_share_the_next_one() {
    let td = tempfile::tempdir().unwrap();
    let (d, syncs) = gated(&td.path().join("dev"));
    let d = &d;
    thread::scope(|s| {
        let first_writer = s.spawn(move || d.append(&[0; 100]));
        let first = syncs.next();
        let rest: Vec<_> = (1..32u8)
            .map(|i| s.spawn(move || (i, d.append(&[i; 100]))))
            .collect();
        wait_for("the writes to return", || d.unsynced_writes() == 32);
        first.succeed();
        join(first_writer).unwrap();
        let second = syncs.next();
        thread::sleep(QUIET);
        assert!(rest.iter().all(|h| !h.is_finished()));
        second.succeed();
        for h in rest {
            let (i, offset) = join(h);
            assert_eq!(d.read_exact_at(offset.unwrap(), 100).unwrap(), [i; 100]);
        }
        syncs.none();
        assert_eq!(d.len().unwrap(), 3200);
    });
}

#[test]
fn a_failed_sync_fails_every_waiter_and_poisons_the_device() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let (d, syncs) = gated(&path);
    let d = &d;
    thread::scope(|s| {
        let a = s.spawn(move || d.append(b"aaaa"));
        let first = syncs.next();
        let b = s.spawn(move || d.append(b"bbbb"));
        let c = s.spawn(move || d.write_at(0, b"cc"));
        wait_for("b and c to write", || d.unsynced_writes() == 3);
        first.succeed();
        // Covered by a successful sync: durable, whatever happens next.
        assert_eq!(join(a).unwrap(), 0);

        let second = syncs.next();
        let late = s.spawn(move || d.append(b"dddd"));
        wait_for("the late write", || d.unsynced_writes() == 3);
        second.fail();
        assert_sync_failure(join(b), "injected sync failure");
        assert_sync_failure(join(c), "injected sync failure");
        // Not covered by the failed sync, but a later success could not be trusted either.
        assert_sync_failure(join(late), "injected sync failure");
        syncs.none();

        let len = d.len().unwrap();
        assert_sync_failure(d.append(b"eeee"), "injected sync failure");
        assert_sync_failure(d.write_at(0, b"ff"), "injected sync failure");
        syncs.none();
        assert_eq!(d.len().unwrap(), len, "a poisoned device wrote");
        assert_eq!(d.read_exact_at(2, 2).unwrap(), b"aa");
    });
}

#[test]
fn writers_that_arrive_during_a_failing_sync_do_not_retry_it() {
    let td = tempfile::tempdir().unwrap();
    let (d, syncs) = gated(&td.path().join("dev"));
    let d = &d;
    thread::scope(|s| {
        let a = s.spawn(move || d.append(b"aaaa"));
        let first = syncs.next();
        let b = s.spawn(move || d.append(b"bbbb"));
        wait_for("b to write", || d.unsynced_writes() == 2);
        first.fail();
        assert_sync_failure(join(a), "injected sync failure");
        assert_sync_failure(join(b), "injected sync failure");
        syncs.none();
    });
}

#[test]
fn a_panicking_sync_poisons_the_device_instead_of_stranding_writers() {
    let td = tempfile::tempdir().unwrap();
    let (d, syncs) = gated(&td.path().join("dev"));
    let d = &d;
    thread::scope(|s| {
        let a = s.spawn(move || d.append(b"aaaa"));
        let first = syncs.next();
        let b = s.spawn(move || d.append(b"bbbb"));
        wait_for("b to write", || d.unsynced_writes() == 2);
        first.panic();
        assert_sync_failure(join(a), "sync panicked");
        assert_sync_failure(join(b), "sync panicked");
        assert_sync_failure(d.append(b"cccc"), "sync panicked");
        syncs.none();
    });
}

#[test]
fn reopening_appends_after_everything_in_the_file() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let (d, syncs) = gated(&path);
    thread::scope(|s| {
        let a = s.spawn(|| d.append(b"aaaa"));
        syncs.next().succeed();
        join(a).unwrap();
        let b = s.spawn(|| d.append(b"bbbbbb"));
        syncs.next().fail();
        assert!(join(b).is_err());
    });
    drop(d);
    // As before group commit: the end is the file's length, including an unacknowledged tail.
    let d = FileDevice::open(&path).unwrap();
    assert_eq!(d.len().unwrap(), 10);
    assert_eq!(d.append(b"c").unwrap(), 10);
    d.write_at(0, b"dd").unwrap();
    assert_eq!(d.read_exact_at(0, 11).unwrap(), b"ddaabbbbbbc");
}

fn gated_node(root: &Path) -> (DataNodeServer, Arc<FileDevice>, Syncs) {
    let (d, syncs) = gated(&root.join("nvme0.data"));
    let d = Arc::new(d);
    let server = DataNodeServer::start_devices(
        "n1",
        root,
        vec![d.clone() as Arc<dyn BlockStore>],
        TcpListener::bind("127.0.0.1:0").unwrap(),
        None,
    )
    .unwrap();
    (server, d, syncs)
}

/// A client of `group` that has already made its first write at term 1.
fn client(server: &DataNodeServer, group: u32, syncs: &Syncs) -> RemoteDevice {
    let c = RemoteDevice::new(server.local_addr(), WAIT * 3).in_group(group);
    thread::scope(|s| {
        let h = s.spawn(|| c.append(1, b"prime"));
        syncs.next().succeed();
        join(h).unwrap();
    });
    c
}

#[test]
fn a_data_node_shares_one_sync_between_metadata_groups() {
    let td = tempfile::tempdir().unwrap();
    let (server, d, syncs) = gated_node(td.path());
    let (g0, g1) = (client(&server, 0, &syncs), client(&server, 1, &syncs));
    let (g0, g1) = (&g0, &g1);
    thread::scope(|s| {
        let a = s.spawn(move || g0.append(1, b"a"));
        let first = syncs.next();
        let b = s.spawn(move || g0.append(1, b"b"));
        let c = s.spawn(move || g1.append(1, b"c"));
        wait_for("both groups to write", || d.unsynced_writes() == 3);
        first.succeed();
        join(a).unwrap();
        syncs.next().succeed();
        join(b).unwrap();
        join(c).unwrap();
        syncs.none();
    });
}

/// Writers hold their group's fence lock shared while they wait for a sync, and a higher term
/// waits for it exclusively. The group commit needs no fence lock, so the sync completes and the
/// term is raised after it.
#[test]
fn a_waiting_fence_raise_does_not_block_group_commit() {
    let td = tempfile::tempdir().unwrap();
    let (server, d, syncs) = gated_node(td.path());
    let (g0, g1) = (client(&server, 0, &syncs), client(&server, 1, &syncs));
    let (g0, g1) = (&g0, &g1);
    thread::scope(|s| {
        let leader = s.spawn(move || g0.append(1, b"a"));
        let first = syncs.next();
        let follower = s.spawn(move || g0.append(1, b"b"));
        let other = s.spawn(move || g1.append(1, b"c"));
        wait_for("the writes", || d.unsynced_writes() == 3);
        let raise = s.spawn(move || g0.append(2, b"d"));
        thread::sleep(QUIET);
        assert_eq!(
            d.unsynced_writes(),
            3,
            "the new term wrote before the old term's writes finished"
        );
        first.succeed();
        let writers = [leader, follower, other, raise];
        // The new term's write lands in the second sync or in one of its own.
        wait_for("every writer", || {
            if let Ok(held) = syncs.0.recv_timeout(Duration::from_millis(5)) {
                held.succeed();
            }
            writers.iter().all(ScopedJoinHandle::is_finished)
        });
        for w in writers {
            w.join().unwrap().unwrap();
        }
    });
    assert_eq!(server.group_fence(0), 2);
    assert!(matches!(
        g0.append(1, b"e"),
        Err(NativeError::Fenced { current: 2 })
    ));
}

// --- Benchmark ---

/// `FileDevice` as it was before group commit: every write runs its own sync.
struct SyncPerWrite {
    end: Mutex<u64>,
    file: File,
}

impl SyncPerWrite {
    fn open(path: &Path) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let end = file.metadata().unwrap().len();
        Self {
            end: Mutex::new(end),
            file,
        }
    }

    fn append(&self, data: &[u8]) {
        let off = {
            let mut end = self.end.lock().unwrap();
            let off = *end;
            *end += data.len() as u64;
            off
        };
        self.file.write_all_at(data, off).unwrap();
        self.file.sync_data().unwrap();
    }

    fn write_at(&self, offset: u64, data: &[u8]) {
        assert!(offset + data.len() as u64 <= self.file.metadata().unwrap().len());
        self.file.write_all_at(data, offset).unwrap();
        self.file.sync_data().unwrap();
    }
}

#[derive(Debug)]
struct Counting(Arc<AtomicU64>);

impl DeviceSync for Counting {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        file.sync_data()
    }
}

enum Dev {
    Before(SyncPerWrite),
    After(FileDevice),
}

struct Run {
    ops_per_s: f64,
    p50_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    syncs: u64,
}

#[derive(Clone, Copy)]
struct Case {
    size: usize,
    overwrite: bool,
    writers: usize,
    ops: usize,
    files: usize,
}

fn run(base: &Path, case: Case, group: bool) -> Run {
    let td = tempfile::Builder::new()
        .prefix("atlas-bench-")
        .tempdir_in(base)
        .unwrap();
    let per_writer = case.ops / case.writers;
    let ops = per_writer * case.writers;
    let data: Vec<u8> = (0..case.size).map(|i| (i * 13 % 251) as u8).collect();
    let syncs = Arc::new(AtomicU64::new(0));
    // Slots each file holds before an overwrite run.
    let slots = ops.div_ceil(case.files);
    let devices: Vec<Dev> = (0..case.files)
        .map(|i| {
            let path = td.path().join(format!("dev{i}"));
            if case.overwrite {
                let f = File::create(&path).unwrap();
                for slot in 0..slots {
                    f.write_all_at(&data, (slot * case.size) as u64).unwrap();
                }
                f.sync_all().unwrap();
            }
            if group {
                let counting = Box::new(Counting(syncs.clone()));
                Dev::After(FileDevice::open_with_sync(&path, counting).unwrap())
            } else {
                Dev::Before(SyncPerWrite::open(&path))
            }
        })
        .collect();
    File::open(td.path()).unwrap().sync_all().unwrap();

    let start = Instant::now();
    let mut lat: Vec<f64> = thread::scope(|s| {
        let handles: Vec<_> = (0..case.writers)
            .map(|w| {
                let (devices, data) = (&devices, &data);
                s.spawn(move || {
                    (0..per_writer)
                        .map(|i| {
                            let op = i * case.writers + w;
                            let device = &devices[w % case.files];
                            // A scattered but repeatable slot; 7919 is prime to `slots`.
                            let slot = (op / case.files * 7919) % slots;
                            let offset = (slot * case.size) as u64;
                            let t = Instant::now();
                            match (device, case.overwrite) {
                                (Dev::Before(d), false) => d.append(data),
                                (Dev::Before(d), true) => d.write_at(offset, data),
                                (Dev::After(d), false) => {
                                    d.append(data).unwrap();
                                }
                                (Dev::After(d), true) => d.write_at(offset, data).unwrap(),
                            }
                            t.elapsed().as_secs_f64() * 1e3
                        })
                        .collect::<Vec<f64>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(f64::total_cmp);
    let pct = |p: f64| lat[((p / 100.0 * lat.len() as f64).ceil() as usize).max(1) - 1];
    Run {
        ops_per_s: ops as f64 / secs,
        p50_ms: pct(50.0),
        p99_ms: pct(99.0),
        max_ms: pct(100.0),
        syncs: if group {
            syncs.load(Ordering::Relaxed)
        } else {
            ops as u64
        },
    }
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    (v[(v.len() - 1) / 2] + v[v.len() / 2]) / 2.0
}

#[test]
#[ignore]
fn group_commit_bench() {
    let base = std::env::var_os("ATLAS_BENCH_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let ops = env("BENCH_OPS", 192);
    let rounds = env("BENCH_ROUNDS", 3);
    let files = env("BENCH_FILES", 1);
    let sizes: Vec<usize> = std::env::var("BENCH_SIZES_KIB")
        .unwrap_or_else(|_| "4,64,1024".into())
        .split(',')
        .map(|s| s.trim().parse::<usize>().unwrap() << 10)
        .collect();
    println!(
        "group commit bench: dir {}, {ops} writes per run, {rounds} rounds, {files} file(s)",
        base.display()
    );

    let mut cases = Vec::new();
    for &size in &sizes {
        for overwrite in [false, true] {
            for writers in [1, 8, 32] {
                cases.push(Case {
                    size,
                    overwrite,
                    writers,
                    ops,
                    files,
                });
            }
        }
    }
    let mut results: Vec<[Vec<Run>; 2]> = cases.iter().map(|_| Default::default()).collect();
    for round in 0..rounds {
        for (case, result) in cases.iter().zip(&mut results) {
            // Alternate which goes first, so neither always follows the other's writeback.
            for group in [round % 2 == 1, round % 2 == 0] {
                let r = run(&base, *case, group);
                println!(
                    "run round={round} files={files} size_kib={} mode={} writers={} impl={} \
                     ops_per_s={:.1} p50_ms={:.2} p99_ms={:.2} max_ms={:.2} syncs={}",
                    case.size >> 10,
                    if case.overwrite {
                        "overwrite"
                    } else {
                        "append"
                    },
                    case.writers,
                    if group { "after" } else { "before" },
                    r.ops_per_s,
                    r.p50_ms,
                    r.p99_ms,
                    r.max_ms,
                    r.syncs,
                );
                result[usize::from(group)].push(r);
            }
        }
    }

    println!("\nmedians of {rounds} rounds (before -> after), {files} file(s)");
    println!(
        "| size KiB | mode | writers | ops/s | change | p50 ms | p99 ms | syncs (of writes) |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for (case, [before, after]) in cases.iter().zip(&results) {
        let of = |runs: &[Run], f: fn(&Run) -> f64| median(runs.iter().map(f).collect());
        let (b, a) = (of(before, |r| r.ops_per_s), of(after, |r| r.ops_per_s));
        println!(
            "| {} | {} | {} | {b:.1} -> {a:.1} | {:+.1}% | {:.2} -> {:.2} | {:.2} -> {:.2} | \
             {:.0} -> {:.0} |",
            case.size >> 10,
            if case.overwrite {
                "overwrite"
            } else {
                "append"
            },
            case.writers,
            (a / b - 1.0) * 100.0,
            of(before, |r| r.p50_ms),
            of(after, |r| r.p50_ms),
            of(before, |r| r.p99_ms),
            of(after, |r| r.p99_ms),
            of(before, |r| r.syncs as f64),
            of(after, |r| r.syncs as f64),
        );
    }
}
