// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Failures to make data durable, with the sync call replaced by one that reports errors as
//! Linux does: one caller is told, and other calls on the same open file return 0 although the
//! data never reached the disk. The same behaviour on a real kernel, through a device that
//! fails writes, is in `tests/writeback_errors.rs`.

use std::{
    collections::BTreeMap,
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
    metadata::MetaCommand,
    node::{DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig},
    object::DirObjectStore,
    raw::{AlignedBuf, AlignedIo, PreadIo, RawDevice},
    wal::{Wal, WalRecord},
    BlockStore, DataNodeServer, EngineConfig, Envelope, FailureDomain, Message, MetaBackend,
    NativeEngine, Node, RaftConfig, RaftNode, RaftServer, RemoteDevice, TierPolicy,
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

/// Three nodes in one process, each a metadata voter and a data node with one device, with
/// three replicas.
struct Trio {
    td: tempfile::TempDir,
    data_addrs: Vec<SocketAddr>,
    raft_addrs: Vec<SocketAddr>,
    nodes: Vec<Option<NativeNode>>,
}

impl Trio {
    /// `syncs[i]` replaces `fdatasync` on node `i`'s device until the node is restarted.
    fn start(syncs: [Box<dyn DeviceSync>; 3]) -> Self {
        let td = tempfile::tempdir().unwrap();
        let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
        let data: Vec<TcpListener> = (0..3).map(|_| bind()).collect();
        let raft: Vec<TcpListener> = (0..3).map(|_| bind()).collect();
        let addrs = |l: &[TcpListener]| l.iter().map(|l| l.local_addr().unwrap()).collect();
        let mut trio = Self {
            data_addrs: addrs(&data),
            raft_addrs: addrs(&raft),
            td,
            nodes: Vec::new(),
        };
        for (i, ((d, r), sync)) in data.into_iter().zip(raft).zip(syncs).enumerate() {
            let path = trio.root(i).join("data").join("nvme0.data");
            let device = FileDevice::open_with_sync(path, sync).unwrap();
            let node = NativeNode::start_with_devices(
                trio.config(i),
                Listeners {
                    http: Some(bind()),
                    data_node: Some(d),
                    metadata: Some(r),
                },
                vec![Arc::new(device)],
            )
            .unwrap();
            trio.nodes.push(Some(node));
        }
        trio
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
                devices: 1,
            })
            .collect();
        NodeConfig {
            node_id: format!("n{i}"),
            data_dir: self.root(i),
            http_listen: "127.0.0.1:0".parse().unwrap(),
            api_token_file: None,
            tls: None,
            http_tls: None,
            data_node: Some(DataNodeRole {
                listen: self.data_addrs[i],
                devices: Vec::new(),
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
            restarted.push(i);
        }
        restarted
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
