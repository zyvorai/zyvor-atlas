// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Failures to make data durable, with the sync call replaced by one that reports errors as
//! Linux does: one caller is told, and other calls on the same open file return 0 although the
//! data never reached the disk. The same behaviour on a real kernel, through a device that
//! fails writes, is in `tests/writeback_errors.rs`.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    net::TcpListener,
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
    device::{DeviceSync, FileDevice},
    metadata::MetaCommand,
    raw::{AlignedBuf, AlignedIo, PreadIo, RawDevice},
    wal::{Wal, WalRecord},
    BlockStore, DataNodeServer, EngineConfig, Envelope, FailureDomain, Message, MetaBackend,
    NativeEngine, Node, RaftConfig, RaftNode, RaftServer, RemoteDevice,
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
    assert_eq!(d.append(b"again").unwrap(), 8192);
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
