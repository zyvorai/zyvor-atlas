// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Failures to make data durable, with the sync call replaced by one that reports errors as
//! Linux does: one caller is told, and other calls on the same open file return 0 although the
//! data never reached the disk. The same behaviour on a real kernel, through a device that
//! fails writes, is in `tests/writeback_errors.rs`.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    thread::{self, ScopedJoinHandle},
    time::{Duration, Instant},
};

use crate::{
    device::{DataSync, DeviceSync, FileDevice},
    ec::EcScheme,
    engine::{DirectCellDone, DirectCommit, DirectReplicaDone, NewNode},
    metadata::MetaCommand,
    node::{
        DataNodeRole, DataNodeSpec, DeviceConfig, Listeners, MetadataRole, NativeNode, NodeConfig,
    },
    object::DirObjectStore,
    raw::{AlignedBuf, AlignedIo, PreadIo, RawDevice},
    rebuild::{RebuildConfig, Rebuilder},
    wal::{Wal, WalRecord},
    BlockStore, DataNodeServer, DeviceBackend, DeviceRestart, EngineConfig, Envelope,
    FailureDomain, GrantKey, Message, MetaBackend, NativeEngine, NativeError, Node, NodeType,
    RaftConfig, RaftNode, RaftServer, RemoteDevice, RestartPolicy, TierPolicy, ROOT_INO,
};

const WAIT: Duration = Duration::from_secs(10);
/// How long to watch for something that must not happen.
const QUIET: Duration = Duration::from_millis(200);

fn eio() -> io::Error {
    io::Error::other("injected writeback error")
}

/// Once armed, tells the next sync of a writeback error. Every other call succeeds, as the
/// kernel's would.
#[derive(Debug, Clone, Default)]
struct FailOnce(Arc<AtomicBool>);

impl FailOnce {
    fn arm(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl DeviceSync for FailOnce {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        if self.0.swap(false, Ordering::SeqCst) {
            return Err(eio());
        }
        file.sync_data()
    }
}

/// A sync call that has started and waits to be told what it returns.
struct Held(Sender<bool>);

impl Held {
    fn succeed(self) {
        self.0.send(true).unwrap();
    }
    fn fail(self) {
        self.0.send(false).unwrap();
    }
}

/// Holds every sync call until the test decides its result. Holding the call that is to fail,
/// after the others have been told to succeed, is the window in which one thread has been given
/// the error by the kernel and nobody has recorded it yet.
#[derive(Debug, Clone)]
struct Gate(Sender<Held>);

impl Gate {
    fn new() -> (Self, Syncs) {
        let (tx, rx) = channel();
        (Self(tx), Syncs(rx))
    }

    fn hold(&self) -> io::Result<()> {
        let (tx, rx) = channel();
        self.0
            .send(Held(tx))
            .map_err(|_| io::Error::other("nobody is watching the syncs"))?;
        match rx.recv_timeout(WAIT * 2) {
            Ok(true) => Ok(()),
            Ok(false) => Err(eio()),
            Err(e @ (RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected)) => {
                Err(io::Error::other(e))
            }
        }
    }
}

impl DeviceSync for Gate {
    fn sync_data(&self, _file: &File) -> io::Result<()> {
        self.hold()
    }
}

/// The calls a gated device makes, in the order they start.
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

/// [`PreadIo`] whose `O_DSYNC` writes return what the gate says.
#[derive(Debug)]
struct GatedIo {
    io: PreadIo,
    gate: Gate,
}

impl AlignedIo for GatedIo {
    fn read_at(&self, offset: u64, buf: &mut AlignedBuf) -> io::Result<()> {
        self.io.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &AlignedBuf) -> io::Result<()> {
        self.io.write_at(offset, buf)?;
        self.gate.hold()
    }
    fn capacity(&self) -> Option<u64> {
        self.io.capacity()
    }
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn join<T>(h: ScopedJoinHandle<'_, T>) -> T {
    wait_for("a writer to be released", || h.is_finished());
    h.join().unwrap()
}

/// B's own sync succeeds while A's, which holds the error, has not returned: B must not be
/// acknowledged, then or afterwards.
fn error_held_by_another_writer(device: &dyn BlockStore, syncs: &Syncs) {
    thread::scope(|s| {
        let a = s.spawn(|| device.append(0, &[1; 4096]));
        let first = syncs.next();
        let b = s.spawn(|| device.append(0, &[2; 4096]));
        // Both calls are in progress at once: syncs are not serialised.
        let second = syncs.next();
        second.succeed();
        thread::sleep(QUIET);
        assert!(
            !b.is_finished(),
            "a writer was answered while another sync, which may hold its error, was running"
        );
        first.fail();
        assert!(join(a).is_err());
        let b = join(b);
        assert!(b.is_err(), "acknowledged after a sync failure: {b:?}");
    });
    let len = device.len().unwrap();
    assert!(device.append(0, &[3; 4096]).is_err());
    assert!(device.write_at(0, 0, &[3; 4096]).is_err());
    syncs.none();
    assert_eq!(
        device.len().unwrap(),
        len,
        "a failed device handed out space"
    );
}

#[test]
fn file_device_does_not_acknowledge_a_writer_whose_error_another_thread_holds() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let (gate, syncs) = Gate::new();
    let d = FileDevice::open_with_sync(&path, Box::new(gate)).unwrap();
    error_held_by_another_writer(&d, &syncs);
    // Reading goes on, and reopening the device lets it write again.
    assert_eq!(BlockStore::read_exact_at(&d, 0, 4).unwrap().len(), 4);
    drop(d);
    let d = FileDevice::open(&path).unwrap();
    let again = d.append(b"again").unwrap();
    if cfg!(feature = "prealloc-test") {
        // A failed device's file is not cut back, so its prepared tail stays in it.
        assert!(again >= 8192, "{again}");
    } else {
        assert_eq!(again, 8192);
    }
}

#[test]
fn aligned_device_does_not_acknowledge_a_writer_whose_error_another_thread_holds() {
    let td = tempfile::tempdir().unwrap();
    let (gate, syncs) = Gate::new();
    let io = GatedIo {
        io: PreadIo::open(&td.path().join("dev")).unwrap(),
        gate,
    };
    let d = RawDevice::open(io, td.path().join("dev.hwm"), 0).unwrap();
    error_held_by_another_writer(&d, &syncs);
}

#[test]
fn a_failed_sync_refuses_all_later_writes() {
    let td = tempfile::tempdir().unwrap();
    let sync = FailOnce::default();
    let d = FileDevice::open_with_sync(td.path().join("dev"), Box::new(sync.clone())).unwrap();
    d.append(b"durable").unwrap();
    sync.arm();
    assert!(d.append(b"lost").is_err());
    // The kernel would now report success; the device must not.
    let later = d.append(b"later");
    assert!(
        later.is_err(),
        "acknowledged after a failed sync: {later:?}"
    );
    assert!(d.write_at(0, b"DUR").is_err());
    assert_eq!(d.read_exact_at(0, 7).unwrap(), b"durable");
}

#[test]
fn a_writer_waits_only_for_syncs_that_began_before_its_own_returned() {
    let td = tempfile::tempdir().unwrap();
    let (gate, syncs) = Gate::new();
    let d = FileDevice::open_with_sync(td.path().join("dev"), Box::new(gate)).unwrap();
    let d = &d;
    thread::scope(|s| {
        let a = s.spawn(move || d.append(b"aaaa"));
        let first = syncs.next();
        let b = s.spawn(move || d.append(b"bbbb"));
        let second = syncs.next();
        second.succeed();
        thread::sleep(QUIET);
        assert!(!b.is_finished(), "did not wait for the earlier sync");
        // This one starts after B's returned, so B does not depend on it.
        let c = s.spawn(move || d.append(b"cccc"));
        let third = syncs.next();
        first.succeed();
        join(b).unwrap();
        thread::sleep(QUIET);
        assert!(!a.is_finished(), "C started before A's sync returned");
        assert!(!c.is_finished());
        third.succeed();
        join(a).unwrap();
        join(c).unwrap();
    });
}

fn record(index: u64) -> WalRecord<String> {
    WalRecord {
        term: 1,
        index,
        command: format!("c{index}"),
    }
}

fn indexes(wal: &Wal) -> Vec<u64> {
    wal.replay::<String>()
        .unwrap()
        .iter()
        .map(|r| r.index)
        .collect()
}

#[test]
fn a_failed_log_sync_is_not_forgotten() {
    let td = tempfile::tempdir().unwrap();
    let sync = FailOnce::default();
    let mut wal = Wal::open(td.path()).unwrap();
    wal.set_sync(Box::new(sync.clone()));
    wal.append(&record(1)).unwrap();
    sync.arm();
    assert!(wal.append(&record(2)).is_err());
    let retry = wal.sync();
    assert!(retry.is_err(), "a retried fsync counted as success");
    assert_eq!(wal.synced_index(), 1);
    assert!(wal.append_unsynced(&record(3)).is_err());
    assert!(wal.compact_through(1).is_err());
    assert_eq!(wal.synced_index(), 1);
    drop(wal);

    // What the page cache still holds is written again, to a new file, before it counts.
    let path = td.path().join("metadata.wal");
    let before = fs::metadata(&path).unwrap().ino();
    let wal = Wal::open(td.path()).unwrap();
    assert_eq!(indexes(&wal), [1, 2]);
    assert_eq!(wal.synced_index(), 2);
    assert_ne!(fs::metadata(&path).unwrap().ino(), before);
}

#[test]
fn a_failed_log_rewrite_fails_the_log() {
    let td = tempfile::tempdir().unwrap();
    let mut wal = Wal::open(td.path()).unwrap();
    wal.append(&record(1)).unwrap();
    wal.append(&record(2)).unwrap();
    let tmp = td.path().join("metadata.wal.tmp");
    fs::create_dir(&tmp).unwrap();
    assert!(wal.compact_through(1).is_err());
    fs::remove_dir(&tmp).unwrap();
    assert!(wal.append(&record(3)).is_err());
}

fn volume(id: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: id.into(),
        name: id.into(),
        size_bytes: 4096,
    }
}

fn single_voter(dir: &Path) -> RaftNode {
    let mut node = RaftNode::open(RaftConfig::new("a", vec![], dir)).unwrap();
    for _ in 0..100 {
        if node.is_leader() {
            return node;
        }
        node.tick().unwrap();
    }
    panic!("a single voter did not become leader");
}

#[test]
fn a_leader_does_not_count_an_entry_whose_sync_failed() {
    let td = tempfile::tempdir().unwrap();
    let sync = FailOnce::default();
    let mut node = single_voter(td.path());
    node.wal_mut().set_sync(Box::new(sync.clone()));
    node.propose(volume("v1")).unwrap();
    let committed = node.commit_index();
    sync.arm();
    assert!(node.propose(volume("v2")).is_err());
    let tick = node.tick();
    assert_eq!(
        node.commit_index(),
        committed,
        "committed an unsynced entry"
    );
    assert!(!node.catalog().volumes.contains_key("v2"));
    assert!(tick.is_err(), "kept running after a failed log sync");
    assert!(node.propose(volume("v3")).is_err());
    assert!(node.take_messages().is_err());
}

fn append_entries(term: u64, entries: Vec<WalRecord<MetaCommand>>) -> Envelope {
    Envelope {
        from: "a".into(),
        to: "b".into(),
        group: 0,
        msg: Message::AppendEntries {
            term,
            prev_log_index: 0,
            prev_log_term: 0,
            entries,
            leader_commit: 0,
            read_seq: 0,
        },
    }
}

#[test]
fn a_follower_never_acknowledges_an_entry_whose_sync_failed() {
    let td = tempfile::tempdir().unwrap();
    let sync = FailOnce::default();
    let cfg = RaftConfig::new("b", vec!["a".into(), "c".into()], td.path());
    let mut node = RaftNode::open(cfg).unwrap();
    node.wal_mut().set_sync(Box::new(sync.clone()));
    let entry = WalRecord {
        term: 1,
        index: 1,
        command: MetaCommand::Noop,
    };
    node.step(append_entries(1, vec![entry])).unwrap();
    sync.arm();
    assert!(node.take_messages().is_err());
    let again = node.take_messages();
    assert!(again.is_err(), "released after a failed sync: {again:?}");
    assert!(node.step(append_entries(1, vec![])).is_err());
}

#[test]
fn a_node_that_could_not_persist_its_term_stops_acting() {
    let td = tempfile::tempdir().unwrap();
    let mut node = single_voter(td.path());
    let state = td.path().join("raft_state.json");
    fs::remove_file(&state).unwrap();
    fs::create_dir(&state).unwrap();
    let term = node.term();
    assert!(node.step(append_entries(term + 1, vec![])).is_err());
    // It still believes it leads, now in a term it was never elected in.
    let proposed = node.propose(volume("v"));
    assert!(proposed.is_err(), "proposed in an unpersisted term");
    assert!(node.take_messages().is_err());
}

#[test]
fn a_raft_server_stops_after_a_failed_log_sync() {
    let td = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = RaftServer::start(
        RaftConfig::new("a", vec![], td.path()),
        listener,
        BTreeMap::<String, String>::new(),
        Duration::from_millis(5),
    )
    .unwrap();
    let timeout = Duration::from_secs(5);
    wait_for("a leader", || server.leader_ready(timeout).is_ok());
    server.propose(volume("v1"), timeout).unwrap();
    let sync = FailOnce::default();
    server.with_node(|n| n.wal_mut().set_sync(Box::new(sync.clone())));
    sync.arm();
    assert!(server.propose(volume("v2"), timeout).is_err());
    wait_for("the server to stop", || server.fatal_error().is_some());
    assert!(server.propose(volume("v3"), timeout).is_err());
}

/// A single voter whose log the test syncs itself, with one unsynced proposal `v`.
fn externally_synced(dir: &Path) -> RaftNode {
    let mut node = single_voter(dir);
    node.sync_leader_log_externally();
    node
}

#[test]
fn a_failed_external_log_sync_stops_the_leader_before_it_is_reported() {
    let td = tempfile::tempdir().unwrap();
    let sync = FailOnce::default();
    let mut node = externally_synced(td.path());
    node.wal_mut().set_sync(Box::new(sync.clone()));
    let committed = node.commit_index();
    node.propose(volume("v1")).unwrap();
    let pending = node.begin_log_sync().unwrap().unwrap();
    sync.arm();
    let failed = pending.run();
    // The kernel would now report success to every other caller, and the node has not heard yet.
    assert!(node.propose(volume("v2")).is_err());
    assert!(node.tick().is_err());
    assert!(node.take_messages().is_err());
    assert!(node.begin_log_sync().is_err());
    assert!(node.finish_log_sync(failed).is_err());
    assert_eq!(
        node.commit_index(),
        committed,
        "committed an unsynced entry"
    );
    assert!(!node.catalog().volumes.contains_key("v1"));
    assert!(node.propose(volume("v3")).is_err());
}

/// The background sync holds the writeback error; the sync the node makes itself while stepping
/// down is told of success, and must not count it.
#[test]
fn an_inline_sync_does_not_succeed_while_a_background_sync_holds_the_error() {
    let td = tempfile::tempdir().unwrap();
    let (gate, syncs) = Gate::new();
    let mut node = externally_synced(td.path());
    node.wal_mut().set_sync(Box::new(gate));
    node.propose(volume("v1")).unwrap();
    let pending = node.begin_log_sync().unwrap().unwrap();
    // A newer entry, so that stepping down has something of its own to sync.
    node.propose(volume("v2")).unwrap();
    let term = node.term();
    thread::scope(|s| {
        let background = s.spawn(move || pending.run());
        let first = syncs.next();
        let node = &mut node;
        let stepping = s.spawn(move || {
            let r = node.step(append_entries(term + 1, vec![]));
            (r, node)
        });
        syncs.next().succeed();
        thread::sleep(QUIET);
        assert!(
            !stepping.is_finished(),
            "answered while another sync, which may hold its error, was running"
        );
        first.fail();
        let failed = join(background);
        let (stepped, node) = join(stepping);
        assert!(stepped.is_err(), "synced after a sync failure");
        assert!(node.take_messages().is_err());
        assert!(node.finish_log_sync(failed).is_err());
    });
}

#[test]
fn the_log_is_not_rewritten_under_a_running_sync() {
    let td = tempfile::tempdir().unwrap();
    let (gate, syncs) = Gate::new();
    let mut wal = Wal::open(td.path()).unwrap();
    wal.append(&record(1)).unwrap();
    wal.set_sync(Box::new(gate));
    wal.append_unsynced(&record(2)).unwrap();
    let pending = wal.begin_sync().unwrap().unwrap();
    thread::scope(|s| {
        let background = s.spawn(move || pending.run());
        let held = syncs.next();
        let wal = &mut wal;
        let rewrite = s.spawn(move || (wal.compact_through(1), wal));
        thread::sleep(QUIET);
        assert!(!rewrite.is_finished(), "rewrote a file being synced");
        held.fail();
        let failed = join(background);
        let (rewritten, wal) = join(rewrite);
        assert!(rewritten.is_err(), "compacted after a failed sync");
        assert!(wal.finish_sync(failed).is_err());
        assert_eq!(wal.synced_index(), 1);
    });
}

/// A single voter syncing its log in parallel, once it leads.
fn parallel_server(dir: &Path) -> RaftServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = RaftConfig::new("a", vec![], dir);
    cfg.parallel_log_sync = true;
    let server = RaftServer::start(
        cfg,
        listener,
        BTreeMap::<String, String>::new(),
        Duration::from_millis(5),
    )
    .unwrap();
    let timeout = Duration::from_secs(5);
    wait_for("a leader", || server.leader_ready(timeout).is_ok());
    server
}

#[test]
fn a_server_syncing_in_parallel_holds_no_lock_while_it_waits_for_the_disk() {
    let td = tempfile::tempdir().unwrap();
    let server = parallel_server(td.path());
    let (gate, syncs) = Gate::new();
    server.with_node(|n| n.wal_mut().set_sync(Box::new(gate)));
    let applied = server.status().unwrap().applied_index;
    thread::scope(|s| {
        let propose = |name: &'static str| {
            let server = &server;
            s.spawn(move || server.propose(volume(name), WAIT))
        };
        let a = propose("v1");
        let first = syncs.next();
        let b = propose("v2");
        let c = propose("v3");
        // Both are appended while the first sync is still on its way to the disk.
        wait_for("the log to grow", || {
            server.with_node(|n| n.last_index()) == applied + 3
        });
        thread::sleep(QUIET);
        assert!(!a.is_finished(), "answered before the sync returned");
        assert_eq!(server.status().unwrap().applied_index, applied);
        syncs.none();
        first.succeed();
        assert_eq!(join(a).unwrap(), applied + 1);
        assert!(!b.is_finished() && !c.is_finished());
        // One more sync covers both.
        syncs.next().succeed();
        join(b).unwrap();
        join(c).unwrap();
        syncs.none();
    });
}

#[test]
fn a_server_syncing_in_parallel_stops_after_a_failed_log_sync() {
    let td = tempfile::tempdir().unwrap();
    let server = parallel_server(td.path());
    server.propose(volume("v1"), WAIT).unwrap();
    let sync = FailOnce::default();
    server.with_node(|n| n.wal_mut().set_sync(Box::new(sync.clone())));
    sync.arm();
    assert!(server.propose(volume("v2"), WAIT).is_err());
    wait_for("the server to stop", || server.fatal_error().is_some());
    assert!(server.propose(volume("v3"), WAIT).is_err());
}

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        failure_domain: FailureDomain {
            zone: "z".into(),
            rack: id.into(),
            host: id.into(),
        },
        free_bytes: 1 << 30,
        healthy: true,
    }
}

#[test]
fn a_local_engine_neither_shows_nor_follows_an_unsynced_command() {
    let td = tempfile::tempdir().unwrap();
    let nodes = || ["n0", "n1", "n2"].map(node).to_vec();
    let cfg = EngineConfig::new(td.path());
    let engine = NativeEngine::open(cfg.clone(), nodes()).unwrap();
    engine.create_volume_as("v1".into(), "v1", 1 << 23).unwrap();
    let sync = FailOnce::default();
    engine.set_log_sync(Box::new(sync.clone()));
    sync.arm();
    assert!(engine.create_volume_as("v2".into(), "v2", 4096).is_err());
    let ids: Vec<String> = engine
        .volumes()
        .unwrap()
        .into_iter()
        .map(|v| v.id)
        .collect();
    assert_eq!(ids, ["v1"], "an unacknowledged command is visible");
    let later = engine.create_volume_as("v3".into(), "v3", 4096);
    assert!(later.is_err(), "committed after a failed log sync");
    // Nothing is placed either: it could overwrite what the unsynced record refers to.
    let used = engine.device_len("n0").unwrap();
    assert!(engine.write("v1", 0, &[1; 4096]).is_err());
    assert_eq!(engine.device_len("n0").unwrap(), used);
    drop(engine);

    let engine = NativeEngine::open(cfg, nodes()).unwrap();
    engine.create_volume_as("v3".into(), "v3", 4096).unwrap();
    engine.write("v1", 0, &[1; 4096]).unwrap();
}

#[test]
fn a_local_engine_keeps_hiding_an_unsynced_command_after_later_ones() {
    let td = tempfile::tempdir().unwrap();
    let nodes = || ["n0", "n1", "n2"].map(node).to_vec();
    let engine = NativeEngine::open(EngineConfig::new(td.path()), nodes()).unwrap();
    engine.create_volume_as("v1".into(), "v1", 4096).unwrap();
    let sync = FailOnce::default();
    engine.set_log_sync(Box::new(sync.clone()));
    sync.arm();
    assert!(engine.create_volume_as("v2".into(), "v2", 4096).is_err());
    // Every later command is refused, and none of them may bring v2 back.
    for id in ["v3", "v4"] {
        assert!(engine.create_volume_as(id.into(), id, 4096).is_err());
        let ids: Vec<String> = engine
            .volumes()
            .unwrap()
            .into_iter()
            .map(|v| v.id)
            .collect();
        assert_eq!(ids, ["v1"], "an unacknowledged command is visible");
    }
}

#[test]
fn a_local_engine_keeps_the_object_an_unsynced_tiering_record_names() {
    const EXTENT: usize = 4096;
    let td = tempfile::tempdir().unwrap();
    let nodes = || ["n0", "n1", "n2"].map(node).to_vec();
    let mut cfg = EngineConfig::new(td.path().join("meta"));
    cfg.extent_bytes = EXTENT;
    cfg.objects = Some(Arc::new(
        DirObjectStore::new(td.path().join("objects")).unwrap(),
    ));
    let engine = NativeEngine::open(cfg.clone(), nodes()).unwrap();
    engine
        .create_volume_as("v".into(), "v", EXTENT as u64)
        .unwrap();
    engine.write("v", 0, &[7; EXTENT]).unwrap();
    let policy = TierPolicy {
        cold_after: Duration::ZERO,
        min_bytes: 0,
    };
    let cold = engine.cold_extents(&policy).unwrap();
    assert_eq!(cold.len(), 1);
    let sync = FailOnce::default();
    engine.set_log_sync(Box::new(sync.clone()));
    sync.arm();
    assert!(engine.tier_extent(&cold[0]).is_err());
    // A later maintenance pass. Whether the record reached the log is unknown, so the object
    // it names must stay: here it is still in the page cache, and a restart applies it.
    let swept = engine.sweep_objects();
    drop(engine);

    let engine = NativeEngine::open(cfg, nodes()).unwrap();
    let got = engine.read("v", 0, EXTENT);
    assert!(
        matches!(&got, Ok(b) if *b == [7; EXTENT]),
        "acknowledged data lost (sweep: {swept:?}): {:?}",
        got.map(|_| "other bytes")
    );
}

#[test]
fn replicas_move_off_a_data_node_whose_device_failed() {
    const EXTENT: usize = 4096;
    let td = tempfile::tempdir().unwrap();
    let mut servers = Vec::new();
    let mut syncs = Vec::new();
    let mut stores: Vec<(Node, Arc<dyn BlockStore>)> = Vec::new();
    for id in ["n0", "n1", "n2", "n3"] {
        let root = td.path().join(id);
        let sync = FailOnce::default();
        let device =
            FileDevice::open_with_sync(root.join("d.data"), Box::new(sync.clone())).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server =
            DataNodeServer::start_devices(id, &root, vec![Arc::new(device)], listener, None)
                .unwrap();
        let remote = RemoteDevice::new(server.local_addr(), WAIT);
        stores.push((node(id), Arc::new(remote) as Arc<dyn BlockStore>));
        servers.push(server);
        syncs.push(sync);
    }
    let mut cfg = EngineConfig::new(td.path().join("meta"));
    cfg.extent_bytes = EXTENT;
    // Retried at once, so that the failed device keeps being offered writes.
    cfg.node_retry_after = Duration::ZERO;
    let engine = NativeEngine::open_with(cfg, stores, MetaBackend::Local).unwrap();
    engine
        .create_volume_as("v".into(), "v", 64 * EXTENT as u64)
        .unwrap();
    engine.write("v", 0, &[1; EXTENT]).unwrap();
    let first = ["n0", "n1", "n2", "n3"]
        .iter()
        .position(|n| engine.device_len(n).unwrap() > 0)
        .unwrap();
    syncs[first].arm();
    for i in 1..16u8 {
        engine
            .write("v", u64::from(i) * EXTENT as u64, &[i + 1; EXTENT])
            .unwrap();
    }
    for i in 0..16u8 {
        let got = engine
            .read("v", u64::from(i) * EXTENT as u64, EXTENT)
            .unwrap();
        assert_eq!(got, vec![i + 1; EXTENT]);
    }
    let direct = RemoteDevice::new(servers[first].local_addr(), WAIT);
    let refused = direct.append(0, b"x");
    assert!(refused.is_err(), "the failed device accepted {refused:?}");
    let metrics = servers[first].render_metrics().unwrap();
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("atlas_native_data_device_failed{") && l.ends_with(" 1")),
        "{metrics}"
    );
    let status = engine.node_status();
    assert!(status[first].failures >= 1, "{status:?}");
}

const BLOCK: usize = 4096;

/// One HTTP request; the status and body.
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

/// Three nodes in one process, each a metadata voter and a data node with the same number of
/// devices, with three replicas.
struct Trio {
    td: tempfile::TempDir,
    devices: usize,
    data_addrs: Vec<SocketAddr>,
    raft_addrs: Vec<SocketAddr>,
    nodes: Vec<Option<NativeNode>>,
}

impl Trio {
    /// One device per node; `syncs[i]` replaces `fdatasync` on node `i`'s until the node is
    /// restarted.
    fn start(syncs: [Box<dyn DeviceSync>; 3]) -> Self {
        Self::start_devices(syncs.map(|sync| vec![sync]))
    }

    /// `syncs[i][d]` replaces `fdatasync` on device `d` of node `i`, each node having as many
    /// devices as the first.
    fn start_devices(syncs: [Vec<Box<dyn DeviceSync>>; 3]) -> Self {
        let td = tempfile::tempdir().unwrap();
        let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
        let data: Vec<TcpListener> = (0..3).map(|_| bind()).collect();
        let raft: Vec<TcpListener> = (0..3).map(|_| bind()).collect();
        let addrs = |l: &[TcpListener]| l.iter().map(|l| l.local_addr().unwrap()).collect();
        let mut trio = Self {
            devices: syncs[0].len(),
            data_addrs: addrs(&data),
            raft_addrs: addrs(&raft),
            td,
            nodes: Vec::new(),
        };
        for (i, ((d, r), syncs)) in data.into_iter().zip(raft).zip(syncs).enumerate() {
            let devices = syncs
                .into_iter()
                .enumerate()
                .map(|(device, sync)| {
                    let path = trio.device_path(i, device);
                    let device = FileDevice::open_with_sync(path, sync).unwrap();
                    Arc::new(device) as Arc<dyn BlockStore>
                })
                .collect();
            let node = NativeNode::start_with_devices(
                trio.config(i),
                Listeners {
                    http: Some(bind()),
                    data_node: Some(d),
                    metadata: Some(r),
                },
                devices,
            )
            .unwrap();
            trio.nodes.push(Some(node));
        }
        trio
    }

    /// Where node `i` keeps its device `device`: with one, where a node keeps it unconfigured.
    fn device_path(&self, i: usize, device: usize) -> std::path::PathBuf {
        let data = self.root(i).join("data");
        if self.devices == 1 {
            data.join("nvme0.data")
        } else {
            data.join(format!("d{device}"))
        }
    }

    fn root(&self, i: usize) -> std::path::PathBuf {
        self.td.path().join(format!("n{i}"))
    }

    fn config(&self, i: usize) -> NodeConfig {
        let data_nodes = self
            .data_addrs
            .iter()
            .enumerate()
            .map(|(i, a)| DataNodeSpec {
                id: format!("n{i}"),
                addr: a.to_string(),
                zone: None,
                rack: None,
                host: None,
                free_bytes: 1 << 30,
                devices: self.devices,
            })
            .collect();
        NodeConfig {
            node_id: format!("n{i}"),
            data_dir: self.root(i),
            http_listen: "127.0.0.1:0".parse().unwrap(),
            api_token_file: None,
            api_tokens_file: None,
            tls: None,
            http_tls: None,
            data_node: Some(DataNodeRole {
                listen: self.data_addrs[i],
                devices: (0..self.devices)
                    .filter(|_| self.devices > 1)
                    .map(|device| DeviceConfig {
                        path: self.device_path(i, device),
                        backend: DeviceBackend::File,
                    })
                    .collect(),
                preallocate: Default::default(),
            }),
            metadata: Some(MetadataRole {
                listen: self.raft_addrs[i],
                peers: self
                    .raft_addrs
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (format!("n{i}"), a.to_string()))
                    .collect(),
                bootstrap: None,
                data_nodes,
                replicas: 3,
                erasure: None,
                erasure_min_bytes: 64 << 10,
                compression: false,
                dedup: false,
                cross_group_dedup: false,
                concurrent_commits: None,
                parallel_log_sync: None,
                delta_writes: Default::default(),
                rebuild_delay_secs: 60,
                rebuild_bytes_per_sec: 0,
                scrub_bytes_per_sec: 0,
                tiering: None,
                extent_bytes: BLOCK,
                tick_ms: 10,
                proposal_timeout_ms: 3000,
                repair_interval_secs: 0,
                gc_interval_secs: 0,
                groups: 1,
                cache_inodes: crate::store::DEFAULT_CACHE_INODES,
                store_cache_bytes: crate::store::DEFAULT_STORE_CACHE_BYTES,
            }),
            max_request_bytes: 1 << 20,
            grant_key_file: None,
        }
    }

    /// What a liveness-probing supervisor does: restarts, from disk and with the real device,
    /// every node whose `/healthz` does not answer 200. Returns the nodes restarted.
    fn supervise(&mut self) -> Vec<usize> {
        let mut restarted = Vec::new();
        for i in 0..self.nodes.len() {
            let node = self.nodes[i].as_ref().unwrap();
            if http(node.http_addr(), "GET", "/healthz", b"").0 == 200 {
                continue;
            }
            self.restart(i);
            restarted.push(i);
        }
        restarted
    }

    /// Restarts node `i` from disk, with real devices.
    fn restart(&mut self, i: usize) {
        // The process exits first: its sockets close.
        self.nodes[i] = None;
        let node = NativeNode::start_with(
            self.config(i),
            Listeners {
                http: Some(TcpListener::bind("127.0.0.1:0").unwrap()),
                data_node: Some(TcpListener::bind(self.data_addrs[i]).unwrap()),
                metadata: Some(TcpListener::bind(self.raft_addrs[i]).unwrap()),
            },
        )
        .unwrap();
        self.nodes[i] = Some(node);
    }

    /// The leader's answer to a request (421 means ask another node).
    fn on_leader(&self, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let deadline = Instant::now() + WAIT;
        loop {
            for node in self.nodes.iter().flatten() {
                let (status, body) = http(node.http_addr(), method, path, body);
                if status != 421 {
                    return (status, body);
                }
            }
            assert!(Instant::now() < deadline, "no leader for {method} {path}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn write(&self, block: usize, byte: u8) -> u16 {
        let path = format!("/v1/volumes/v/data?offset={}", block * BLOCK);
        self.on_leader("PUT", &path, &[byte; BLOCK]).0
    }

    fn assert_block(&self, block: usize, byte: u8) {
        let path = format!("/v1/volumes/v/data?offset={}&len={BLOCK}", block * BLOCK);
        let (status, got) = self.on_leader("GET", &path, b"");
        assert_eq!(status, 200, "block {block}");
        assert!(got == [byte; BLOCK], "block {block} reads other bytes");
    }

    fn create_volume(&self) {
        let body = br#"{"id":"v","name":"v","size_bytes":1048576}"#;
        wait_for("the volume", || {
            self.on_leader("POST", "/v1/volumes", body).0 == 201
        });
    }
}

/// One failed sync on one of three nodes, with three replicas: no write can have its replicas
/// until that device is reopened, so a supervisor must be told to restart the node.
#[test]
fn one_failed_sync_does_not_stop_writes_for_good() {
    let sync = FailOnce::default();
    let mut trio = Trio::start([
        Box::new(DataSync),
        Box::new(sync.clone()),
        Box::new(DataSync),
    ]);
    trio.create_volume();
    wait_for("the first write", || trio.write(0, 1) == 204);

    sync.arm();
    assert_ne!(trio.write(1, 2), 204, "acknowledged without three replicas");

    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut failed, mut restarts) = (1, Vec::new());
    loop {
        restarts.extend(trio.supervise());
        if trio.write(1, 2) == 204 {
            break;
        }
        failed += 1;
        assert!(
            Instant::now() < deadline,
            "{failed} writes failed, nodes restarted: {restarts:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        restarts,
        [1],
        "only the node with the failed device restarts"
    );
    for i in 0..100 {
        assert_eq!(
            trio.write(2 + i, i as u8),
            204,
            "write {i} after the restart"
        );
    }
    trio.assert_block(0, 1);
    trio.assert_block(1, 2);
    for i in 0..100 {
        trio.assert_block(2 + i, i as u8);
    }
}

/// Data nodes `n0`.. of several devices each, behind real servers, and an engine over them with
/// three replicas.
struct Disks {
    td: tempfile::TempDir,
    syncs: Vec<Vec<FailOnce>>,
    servers: Vec<Option<DataNodeServer>>,
    engine: NativeEngine,
}

const GRANTS: &[u8] = b"disk-failure-test-secret";

impl Disks {
    fn start(nodes: usize, devices: usize, tweak: impl FnOnce(&mut EngineConfig)) -> Self {
        let td = tempfile::tempdir().unwrap();
        let key = GrantKey::new(GRANTS).unwrap();
        let (mut syncs, mut servers) = (Vec::new(), Vec::new());
        let mut stores = Vec::new();
        for i in 0..nodes {
            let id = format!("n{i}");
            let root = td.path().join(&id);
            let mut local: Vec<Arc<dyn BlockStore>> = Vec::new();
            let mut node_syncs = Vec::new();
            for d in 0..devices {
                let sync = FailOnce::default();
                let path = root.join(format!("d{d}"));
                let device = FileDevice::open_with_sync(path, Box::new(sync.clone())).unwrap();
                local.push(Arc::new(device));
                node_syncs.push(sync);
            }
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let server = DataNodeServer::start_devices_with(
                id.clone(),
                &root,
                local,
                listener,
                None,
                Some(key.clone()),
            )
            .unwrap();
            let remote: Vec<Arc<dyn BlockStore>> = (0..devices)
                .map(|d| {
                    let device = RemoteDevice::new(server.local_addr(), WAIT)
                        .with_grant_key(key.clone())
                        .on_device(d);
                    Arc::new(device) as Arc<dyn BlockStore>
                })
                .collect();
            stores.push((node(&id), remote));
            syncs.push(node_syncs);
            servers.push(Some(server));
        }
        let mut cfg = EngineConfig::new(td.path().join("meta"));
        cfg.extent_bytes = BLOCK;
        cfg.grant_key = Some(key);
        tweak(&mut cfg);
        let engine = NativeEngine::open_with_devices(cfg, stores, MetaBackend::Local).unwrap();
        engine
            .create_volume_as("v".into(), "v", 256 * BLOCK as u64)
            .unwrap();
        Self {
            td,
            syncs,
            servers,
            engine,
        }
    }

    fn write(&self, block: u64) -> Result<(), NativeError> {
        self.engine
            .write("v", block * BLOCK as u64, &[block as u8 + 1; BLOCK])
    }

    fn assert_reads(&self, blocks: std::ops::Range<u64>) {
        for block in blocks {
            let got = self.engine.read("v", block * BLOCK as u64, BLOCK).unwrap();
            assert!(got == [block as u8 + 1; BLOCK], "block {block}");
        }
    }

    /// Replicas on device `d` of node `n{i}`.
    fn on(&self, i: usize, d: usize) -> usize {
        let id = format!("n{i}");
        self.engine
            .extent_replicas()
            .iter()
            .flatten()
            .filter(|r| r.node_id == id && r.device_index == d)
            .count()
    }

    /// Every extent has `parts` replicas or shards, each on a different node.
    fn assert_spread(&self, parts: usize) {
        for replicas in self.engine.extent_replicas() {
            let mut nodes: Vec<&str> = replicas.iter().map(|r| r.node_id.as_str()).collect();
            nodes.sort_unstable();
            nodes.dedup();
            assert_eq!(
                (replicas.len(), nodes.len()),
                (parts, parts),
                "{replicas:?}"
            );
        }
    }

    /// Restarts node `i`'s server at the same address with sound devices over the same files.
    fn restart(&mut self, i: usize, devices: usize) {
        let addr = {
            let server = self.servers[i].take().unwrap();
            server.local_addr()
        };
        let id = format!("n{i}");
        let root = self.td.path().join(&id);
        let local: Vec<Arc<dyn BlockStore>> = (0..devices)
            .map(|d| {
                Arc::new(FileDevice::open(root.join(format!("d{d}"))).unwrap())
                    as Arc<dyn BlockStore>
            })
            .collect();
        let listener = TcpListener::bind(addr).unwrap();
        let key = GrantKey::new(GRANTS).unwrap();
        let server =
            DataNodeServer::start_devices_with(id, &root, local, listener, None, Some(key))
                .unwrap();
        self.servers[i] = Some(server);
    }
}

fn settle(r: &mut Rebuilder, engine: &NativeEngine) {
    let stop = AtomicBool::new(false);
    for _ in 0..100 {
        if !r.step(engine, &stop).unwrap() {
            return;
        }
    }
    panic!("the controller never went idle");
}

fn controller(delay: Duration) -> Rebuilder {
    Rebuilder::new(RebuildConfig {
        delay,
        rebuild_bytes_per_sec: 0,
        scrub_interval: Duration::ZERO,
        scrub_bytes_per_sec: 0,
    })
}

#[test]
fn writes_go_on_to_a_nodes_other_device_and_repair_moves_off_the_failed_one() {
    let c = Disks::start(3, 2, |_| {});
    for block in 0..8 {
        c.write(block).unwrap();
    }
    let before = c.on(1, 0);
    assert!(before > 0, "nothing on the device that is to fail");
    c.syncs[1][0].arm();
    // Every write succeeds, the one that meets the failure included, although the back-off is
    // the default five seconds: the node stays in use.
    for block in 8..24 {
        c.write(block).unwrap();
    }
    c.assert_spread(3);
    assert_eq!(c.on(1, 0), before, "placed on the failed device");
    assert_eq!(c.on(1, 1), 24 - before);
    let status = &c.engine.node_status()[1];
    assert!(status.up, "{status:?}");
    assert_eq!(status.failed_devices, [0]);
    let metrics = c.engine.render_metrics().unwrap();
    assert!(
        metrics.contains("atlas_native_node_failed_devices{node=\"n1\"} 1"),
        "{metrics}"
    );
    c.assert_reads(0..24);

    let failed = BTreeSet::from([("n1".to_string(), 0)]);
    assert_eq!(c.engine.failed_devices(Duration::ZERO), failed);
    let degraded = c
        .engine
        .degraded_extents_on(&BTreeSet::new(), &failed)
        .unwrap();
    assert_eq!(degraded.len(), before);
    assert!(degraded
        .iter()
        .all(|d| d.lost == 0 && d.failed == 1 && d.spare == 1 && d.rebuildable()));
    let st = c.engine.repair_once().unwrap();
    assert_eq!(
        (st.replicas_repaired, st.deferred, st.unrecoverable),
        (before as u64, 0, 0)
    );
    assert_eq!(c.on(1, 0), 0);
    c.assert_spread(3);
    c.assert_reads(0..24);
}

#[test]
fn a_write_still_needs_every_replica_and_repair_keeps_what_it_cannot_move() {
    let mut c = Disks::start(3, 2, |cfg| cfg.node_retry_after = Duration::ZERO);
    for block in 0..8 {
        c.write(block).unwrap();
    }
    c.syncs[1][0].arm();
    c.syncs[1][1].arm();
    for block in [8, 9] {
        let e = c.write(block).unwrap_err();
        assert!(
            matches!(
                e,
                NativeError::InsufficientReplicas {
                    needed: 3,
                    found: 2
                }
            ),
            "{e}"
        );
    }
    let on_n1 = c.on(1, 0) + c.on(1, 1);
    assert_eq!(on_n1, 8);
    // No node can take the replicas: they stay where they are, and still count.
    let st = c.engine.repair_once().unwrap();
    assert_eq!(
        (st.replicas_repaired, st.deferred, st.unrecoverable),
        (0, 8, 0)
    );
    c.assert_spread(3);
    assert_eq!(c.on(1, 0) + c.on(1, 1), on_n1);
    c.assert_reads(0..8);

    // Reopened devices say they accept writes again, and are used.
    c.restart(1, 2);
    c.write(8).unwrap();
    c.engine.probe_devices();
    assert!(c.engine.failed_devices(Duration::ZERO).is_empty());
    c.assert_spread(3);
    c.assert_reads(0..9);
}

#[test]
fn the_rebuild_controller_moves_off_a_device_failed_for_the_delay() {
    let c = Disks::start(3, 2, |_| {});
    for block in 0..8 {
        c.write(block).unwrap();
    }
    let before = c.on(1, 0);
    c.syncs[1][0].arm();
    c.write(8).unwrap();
    c.write(9).unwrap();

    let mut patient = controller(Duration::from_secs(60));
    settle(&mut patient, &c.engine);
    assert_eq!(patient.status().rebuilt.replicas_repaired, 0);
    assert_eq!(c.on(1, 0), before);

    let mut now = controller(Duration::ZERO);
    settle(&mut now, &c.engine);
    assert_eq!(now.status().rebuilt.replicas_repaired, before as u64);
    assert_eq!(c.on(1, 0), 0);
    c.assert_spread(3);
    c.assert_reads(0..10);
}

#[test]
fn the_rebuild_controller_does_not_read_what_has_nowhere_to_go() {
    // No back-off, so that the node (which answers) does not count as lost.
    let c = Disks::start(3, 1, |cfg| cfg.node_retry_after = Duration::ZERO);
    for block in 0..4 {
        c.write(block).unwrap();
    }
    c.syncs[1][0].arm();
    c.write(4).unwrap_err();
    let mut r = controller(Duration::ZERO);
    settle(&mut r, &c.engine);
    let st = r.status();
    assert_eq!(st.failed_devices, ["n1/0"]);
    assert_eq!((st.degraded_extents, st.unrecoverable_extents), (4, 0));
    assert_eq!(
        (st.rebuilt.bytes_read, st.rebuilt.replicas_repaired),
        (0, 0)
    );
    c.assert_spread(3);
    c.assert_reads(0..4);
}

#[test]
fn shards_move_off_a_failed_device() {
    let c = Disks::start(3, 2, |cfg| {
        cfg.erasure = Some("2+1".parse::<EcScheme>().unwrap());
        cfg.erasure_min_bytes = 1;
    });
    for block in 0..8 {
        c.write(block).unwrap();
    }
    let before = c.on(2, 1);
    assert!(before > 0);
    c.syncs[2][1].arm();
    for block in 8..16 {
        c.write(block).unwrap();
    }
    assert_eq!(c.on(2, 1), before);
    let st = c.engine.repair_once().unwrap();
    assert_eq!(
        (st.replicas_repaired, st.deferred, st.unrecoverable),
        (before as u64, 0, 0)
    );
    assert_eq!(c.on(2, 1), 0);
    c.assert_spread(3);
    c.assert_reads(0..16);
}

#[test]
fn direct_writes_set_aside_only_the_failed_device() {
    let c = Disks::start(3, 2, |_| {});
    c.engine.create_fs_as("f".into(), "fs").unwrap();
    let ino = c
        .engine
        .fs_mknode(
            "f",
            ROOT_INO,
            NewNode {
                name: "a".into(),
                op_id: "op-a".into(),
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
    c.syncs[1][0].arm();
    let data = vec![5u8; BLOCK];
    let mut failures = 0;
    for _ in 0..4 {
        let lease = c
            .engine
            .direct_write_lease("f", ino, 0, BLOCK as u64)
            .unwrap();
        let (mut replicas, mut failed) = (Vec::new(), Vec::new());
        for t in &lease.cells[0].targets {
            let device = RemoteDevice::new(&t.endpoint, WAIT)
                .on_device(t.device_index)
                .in_group(t.group);
            let written = match t.offset {
                Some(at) => device
                    .write_at_granted(Some(t.grant.clone()), lease.term, at, &data)
                    .map(|()| (at, None)),
                None => device.append_granted(Some(t.grant.clone()), lease.term, &data),
            };
            match written {
                Ok((offset, receipt)) => replicas.push(DirectReplicaDone {
                    node_id: t.node_id.clone(),
                    device_index: t.device_index,
                    offset,
                    receipt,
                }),
                Err(_) => failed.push(t.node_id.clone()),
            }
        }
        let commit = DirectCommit {
            lease: lease.lease.clone(),
            cells: vec![DirectCellDone {
                logical_offset: 0,
                checksum: crate::checksum::sha256(&data)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                replicas,
            }],
            failed,
        };
        match c.engine.direct_write_commit("f", ino, commit) {
            Ok(_) => break,
            Err(_) => failures += 1,
        }
    }
    // The first lease includes the failed device; the next one does not.
    assert_eq!(failures, 1);
    assert_eq!(c.engine.read_file("f", ino, 0, BLOCK).unwrap(), data);
    let status = &c.engine.node_status()[1];
    assert!(status.up, "the whole node was set aside: {status:?}");
    // Every later lease avoids the failed device.
    for _ in 0..4 {
        let lease = c
            .engine
            .direct_write_lease("f", ino, 0, BLOCK as u64)
            .unwrap();
        for t in &lease.cells[0].targets {
            assert!(t.node_id != "n1" || t.device_index == 1, "{t:?}");
        }
        c.engine.direct_write_abort(&lease.lease).unwrap();
    }
}

/// A disk that fails every sync, after as many restarts as one likes.
#[derive(Debug)]
struct BadDisk;

impl DeviceSync for BadDisk {
    fn sync_data(&self, _file: &File) -> io::Result<()> {
        Err(eio())
    }
}

/// A node with the data role only.
fn data_config(root: &Path, devices: Vec<DeviceConfig>) -> NodeConfig {
    NodeConfig {
        node_id: "d".into(),
        data_dir: root.to_path_buf(),
        http_listen: "127.0.0.1:0".parse().unwrap(),
        api_token_file: None,
        api_tokens_file: None,
        tls: None,
        http_tls: None,
        data_node: Some(DataNodeRole {
            listen: "127.0.0.1:0".parse().unwrap(),
            devices,
            preallocate: Default::default(),
        }),
        metadata: None,
        max_request_bytes: 1 << 20,
        grant_key_file: None,
    }
}

/// A data node over `d0`.. under `root`, device `i` syncing through `syncs[i]`.
fn data_node(root: &Path, syncs: Vec<Box<dyn DeviceSync>>) -> NativeNode {
    let devices = syncs
        .into_iter()
        .enumerate()
        .map(|(i, sync)| {
            let device = FileDevice::open_with_sync(root.join(format!("d{i}")), sync).unwrap();
            Arc::new(device) as Arc<dyn BlockStore>
        })
        .collect();
    NativeNode::start_with_devices(data_config(root, Vec::new()), Listeners::default(), devices)
        .unwrap()
}

fn client(node: &NativeNode, device: usize) -> RemoteDevice {
    RemoteDevice::new(node.data_node_addr().unwrap(), WAIT).on_device(device)
}

fn health(node: &NativeNode) -> (u16, u16) {
    let get = |path| http(node.http_addr(), "GET", path, b"").0;
    (get("/healthz"), get("/readyz"))
}

fn metrics(node: &NativeNode) -> String {
    String::from_utf8(http(node.http_addr(), "GET", "/metrics", b"").1).unwrap()
}

#[test]
fn a_disk_bad_for_good_gets_a_bounded_number_of_restarts() {
    let td = tempfile::tempdir().unwrap();
    let mut restarts = 0;
    let node = loop {
        let node = data_node(td.path(), vec![Box::new(BadDisk)]);
        assert_eq!(health(&node), (200, 200));
        assert!(client(&node, 0).append(0, b"x").is_err());
        if node.restart_reason().is_none() {
            break node;
        }
        assert_eq!(health(&node), (503, 503));
        let m = metrics(&node);
        assert!(
            m.contains("atlas_native_restart_required{node=\"d\"} 1"),
            "{m}"
        );
        restarts += 1;
        assert!(restarts <= 10, "restarts without end");
    };
    assert_eq!(restarts, 3);
    // Up, so that nothing restarts it again; not ready, so that someone notices.
    assert_eq!(health(&node), (200, 503));
    let m = metrics(&node);
    for line in [
        "atlas_native_restart_required{node=\"d\"} 0",
        "atlas_native_failed_without_restart{node=\"d\",component=\"device0\"} 1",
        "atlas_native_failure_restarts{node=\"d\",component=\"device0\"} 3",
        "atlas_native_data_device_failed{node=\"d\",device=\"0\"} 1",
    ] {
        assert!(m.contains(line), "{line} is not in {m}");
    }
}

#[test]
fn by_default_a_restart_waits_until_no_device_accepts_writes() {
    let td = tempfile::tempdir().unwrap();
    let node = data_node(td.path(), vec![Box::new(BadDisk), Box::new(BadDisk)]);
    assert!(client(&node, 0).append(0, b"x").is_err());
    assert_eq!(health(&node), (200, 200));
    assert!(node.restart_reason().is_none());
    assert!(client(&node, 1).append(0, b"x").is_err());
    assert_eq!(health(&node), (503, 503));
}

#[test]
fn any_asks_for_a_restart_on_the_first_failed_device() {
    let td = tempfile::tempdir().unwrap();
    let node = data_node(td.path(), vec![Box::new(BadDisk), Box::new(DataSync)]);
    node.set_restart_policy(RestartPolicy {
        devices: DeviceRestart::Any,
        ..RestartPolicy::default()
    });
    assert!(client(&node, 0).append(0, b"x").is_err());
    assert_eq!(health(&node), (503, 503));
    // The other device serves meanwhile.
    client(&node, 1).append(0, b"y").unwrap();
}

/// The same failed sync as in `one_failed_sync_does_not_stop_writes_for_good`, on nodes with a
/// second device: no write fails, nothing is restarted, the failed device is visible, and a
/// restart by hand brings it back.
#[test]
fn a_node_with_a_healthy_device_keeps_writing_and_is_not_restarted() {
    let sound = || Box::new(DataSync) as Box<dyn DeviceSync>;
    let sync = FailOnce::default();
    let mut trio = Trio::start_devices([
        vec![sound(), sound()],
        vec![Box::new(sync.clone()), sound()],
        vec![sound(), sound()],
    ]);
    trio.create_volume();
    wait_for("the first write", || trio.write(0, 1) == 204);

    sync.arm();
    for i in 0..100 {
        assert_eq!(trio.write(1 + i, i as u8), 204, "write {i}");
        assert!(trio.supervise().is_empty(), "restarted after write {i}");
    }
    trio.assert_block(0, 1);
    for i in 0..100 {
        trio.assert_block(1 + i, i as u8);
    }

    let node = trio.nodes[1].as_ref().unwrap();
    assert_eq!(health(node), (200, 200));
    assert!(node.restart_reason().is_none());
    let m = metrics(node);
    for line in [
        "atlas_native_data_device_failed{node=\"n1\",device=\"0\"} 1",
        "atlas_native_data_device_failed{node=\"n1\",device=\"1\"} 0",
        "atlas_native_failed_without_restart{node=\"n1\",component=\"device0\"} 1",
        "atlas_native_restart_required{node=\"n1\"} 0",
    ] {
        assert!(m.contains(line), "{line} is not in {m}");
    }
    let (_, status) = http(node.http_addr(), "GET", "/v1/status", b"");
    let status = String::from_utf8(status).unwrap();
    assert!(status.contains("\"restart_wanted\":false"), "{status}");
    // The metadata leader says which node has a failed device.
    let seen = "atlas_native_node_failed_devices{node=\"n1\"} 1";
    assert!(
        trio.nodes
            .iter()
            .flatten()
            .any(|n| metrics(n).contains(seen)),
        "no node reports {seen}"
    );

    trio.restart(1);
    let device = RemoteDevice::new(trio.data_addrs[1], WAIT);
    assert_eq!(device.probe().unwrap(), None);
    let before = device.len().unwrap();
    let block = Cell::new(0);
    wait_for("the leader to place data on the device again", || {
        block.set(1 + block.get() % 100);
        trio.write(block.get(), 7) == 204 && device.len().unwrap() > before
    });
}

#[test]
fn a_node_starts_with_a_device_it_cannot_open() {
    let td = tempfile::tempdir().unwrap();
    // A directory cannot be opened as a device.
    let bad = td.path().join("bad");
    fs::create_dir(&bad).unwrap();
    let devices = [bad, td.path().join("good")]
        .map(|path| DeviceConfig {
            path,
            backend: DeviceBackend::File,
        })
        .to_vec();
    let node = NativeNode::start(data_config(td.path(), devices)).unwrap();
    assert!(client(&node, 0).append(0, b"x").is_err());
    assert!(client(&node, 0).probe().unwrap().is_some());
    let at = client(&node, 1).append(0, b"y").unwrap();
    assert_eq!(client(&node, 1).read_exact_at(at, 1).unwrap(), b"y");
    assert_eq!(health(&node), (200, 200));
    let (_, status) = http(node.http_addr(), "GET", "/v1/status", b"");
    let status = String::from_utf8(status).unwrap();
    assert!(status.contains("could not be opened"), "{status}");
    let m = metrics(&node);
    assert!(
        m.contains("atlas_native_failed_without_restart{node=\"d\",component=\"device0\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("atlas_native_data_device_failed{node=\"d\",device=\"0\"} 1"),
        "{m}"
    );
}

#[test]
fn a_failed_metadata_log_fails_liveness_until_the_node_is_restarted() {
    let mut trio = Trio::start([Box::new(DataSync), Box::new(DataSync), Box::new(DataSync)]);
    trio.create_volume();
    wait_for("the first write", || trio.write(0, 1) == 204);
    let sync = FailOnce::default();
    trio.nodes[2]
        .as_ref()
        .unwrap()
        .raft()
        .with_node(|n| n.wal_mut().set_sync(Box::new(sync.clone())));
    sync.arm();
    let block = Cell::new(1);
    wait_for("the replica to stop", || {
        trio.write(block.get(), 1);
        block.set(1 + block.get() % 100);
        trio.nodes[2].as_ref().unwrap().fatal_error().is_some()
    });
    let node = trio.nodes[2].as_ref().unwrap();
    assert_eq!(health(node), (503, 503));
    let reason = node.restart_reason().unwrap();
    assert!(reason.starts_with("metadata:"), "{reason}");
    let m = metrics(node);
    assert!(
        m.contains("atlas_native_metadata_failed{node=\"n2\"} 1"),
        "{m}"
    );
    assert_eq!(trio.supervise(), [2]);
    wait_for("writes after the restart", || trio.write(0, 2) == 204);
    trio.assert_block(0, 2);
    assert_eq!(health(trio.nodes[2].as_ref().unwrap()).0, 200);
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

// State read back after a restart may be a write whose directory sync failed, held only by the
// cache: it is written again (a new file) before it is relied on.

#[test]
fn a_fence_read_back_is_written_again_before_a_write_is_accepted_under_it() {
    let td = tempfile::tempdir().unwrap();
    let start = || {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        DataNodeServer::start("d", td.path(), listener).unwrap()
    };
    let server = start();
    RemoteDevice::new(server.local_addr(), WAIT)
        .append(5, b"x")
        .unwrap();
    drop(server);
    let fence = td.path().join("fence");
    let before = inode(&fence);
    let server = start();
    let device = RemoteDevice::new(server.local_addr(), WAIT);
    device.append(5, b"y").unwrap();
    let after = inode(&fence);
    assert_ne!(after, before);
    device.append(5, b"z").unwrap();
    assert_eq!(inode(&fence), after, "written for every write");
    assert_eq!(fs::read_to_string(&fence).unwrap(), "5");
}

#[test]
fn a_high_water_mark_and_a_vote_read_back_are_written_again() {
    let td = tempfile::tempdir().unwrap();
    let mark = td.path().join("d.hwm");
    let open = || {
        let io = PreadIo::open(&td.path().join("d.data")).unwrap();
        RawDevice::open(io, &mark, 0).unwrap()
    };
    open().append(0, b"x").unwrap();
    let before = inode(&mark);
    let device = open();
    assert_ne!(inode(&mark), before);
    drop(device);

    let root = td.path().join("raft");
    drop(single_voter(&root));
    let state = root.join("raft_state.json");
    let before = inode(&state);
    let _node = RaftNode::open(RaftConfig::new("a", vec![], &root)).unwrap();
    assert_ne!(inode(&state), before);
}
