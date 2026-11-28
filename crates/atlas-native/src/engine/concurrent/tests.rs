// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Concurrent-commit rules that need a log made to stall or fail; the rest is in
//! `tests/concurrent_commits.rs`.

use std::{
    collections::BTreeMap,
    fs::File,
    io,
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use super::*;
use crate::{
    device::{BlockStore, DeviceSync, FileDevice},
    engine::{EngineConfig, MetaBackend, NewNode},
    namespace::{NodeType, ROOT_INO},
    placement::{FailureDomain, Node},
    raft::RaftConfig,
    raft_server::RaftServer,
};

const GRID: usize = 4096;
const WAIT: Duration = Duration::from_secs(30);
/// How long to watch for something that must not happen.
const QUIET: Duration = Duration::from_millis(300);
/// Long enough that a stall of [`QUIET`] or so does not cost the leader its quorum.
const TICK: Duration = Duration::from_millis(200);

fn node(i: usize) -> Node {
    Node {
        id: format!("n{i}"),
        failure_domain: FailureDomain {
            zone: "z".into(),
            rack: format!("r{i}"),
            host: format!("h{i}"),
        },
        free_bytes: 1 << 30,
        healthy: true,
    }
}

fn config(root: &std::path::Path, concurrent: bool) -> EngineConfig {
    let mut cfg = EngineConfig::new(root);
    cfg.extent_bytes = GRID;
    cfg.delta_writes.enabled = false;
    cfg.concurrent_commits = concurrent;
    cfg
}

fn file(e: &NativeEngine, name: &str) -> u64 {
    let new = NewNode {
        name: name.into(),
        op_id: name.into(),
        kind: NodeType::File,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
    };
    e.fs_mknode("f", ROOT_INO, new).unwrap().ino
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Holds log syncs while stalled.
#[derive(Debug, Clone, Default)]
struct Stall(Arc<(Mutex<bool>, Condvar)>);

impl Stall {
    fn set(&self, stalled: bool) {
        *self.0 .0.lock().unwrap() = stalled;
        self.0 .1.notify_all();
    }
}

impl DeviceSync for Stall {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        let (lock, changed) = &*self.0;
        let stalled = lock.lock().unwrap();
        drop(
            changed
                .wait_timeout_while(stalled, WAIT, |stalled| *stalled)
                .unwrap(),
        );
        file.sync_data()
    }
}

/// Releases the stall however the test ends, so a failure cannot leave it hung.
struct Release(Stall);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// A leader and two followers in this process, over the same three devices.
struct Cluster {
    _td: tempfile::TempDir,
    members: Vec<(Arc<RaftServer>, NativeEngine)>,
}

impl Cluster {
    fn start(concurrent: bool) -> Self {
        let td = tempfile::tempdir().unwrap();
        let devices: Vec<Arc<dyn BlockStore>> = (1..=3)
            .map(|i| {
                let path = td.path().join(format!("d{i}"));
                Arc::new(FileDevice::open(path).unwrap()) as Arc<dyn BlockStore>
            })
            .collect();
        let listeners: BTreeMap<String, TcpListener> = (1..=3)
            .map(|i| (format!("m{i}"), TcpListener::bind("127.0.0.1:0").unwrap()))
            .collect();
        let addrs: BTreeMap<String, std::net::SocketAddr> = listeners
            .iter()
            .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
            .collect();
        let members = listeners
            .into_iter()
            .map(|(id, listener)| {
                let mut peers = addrs.clone();
                peers.remove(&id);
                let root = td.path().join(&id);
                let rcfg = RaftConfig::new(&id, peers.keys().cloned().collect(), root.join("raft"));
                let raft = Arc::new(RaftServer::start(rcfg, listener, peers, TICK).unwrap());
                let stores = (1..=3).map(node).zip(devices.iter().cloned()).collect();
                let engine = NativeEngine::open_with(
                    config(&root.join("engine"), concurrent),
                    stores,
                    MetaBackend::Raft {
                        server: raft.clone(),
                        timeout: WAIT,
                    },
                )
                .unwrap();
                (raft, engine)
            })
            .collect();
        Self { _td: td, members }
    }

    /// The leader, once it has applied everything.
    fn leader(&self) -> usize {
        let ready = || {
            self.members
                .iter()
                .position(|(_, e)| e.ensure_leader().is_ok())
        };
        wait_for("a leader", || ready().is_some());
        ready().unwrap()
    }

    /// Stalls the log of every replica but `leader`.
    fn stall_followers(&self, leader: usize) -> Release {
        let stall = Stall::default();
        for (i, (raft, _)) in self.members.iter().enumerate() {
            if i != leader {
                let stall = stall.clone();
                raft.with_node(|n| n.wal_mut().set_sync(Box::new(stall)));
            }
        }
        stall.set(true);
        Release(stall)
    }
}

/// While one write waits for its commit, writes elsewhere are proposed, and a partial overwrite
/// of the same cell waits for it and merges it.
#[test]
fn a_write_waiting_for_its_commit_blocks_only_what_reads_its_cell() {
    let c = Cluster::start(true);
    let leader = c.leader();
    let (raft, e) = &c.members[leader];
    e.create_fs_as("f".into(), "f").unwrap();
    let (a, b) = (file(e, "a"), file(e, "b"));
    e.write_file("f", a, 0, &[1; 100]).unwrap();
    let last = || raft.with_node(|n| n.last_index());
    let before = last();
    let release = c.stall_followers(leader);
    thread::scope(|s| {
        // Replaces all of the cell's 100 bytes.
        let whole = s.spawn(|| e.write_file("f", a, 0, &[2; 200]));
        wait_for("the first proposal", || last() == before + 1);
        let other = s.spawn(|| e.write_file("f", b, 0, &[3; 100]));
        wait_for("a second proposal beside it", || last() == before + 2);
        // Reads the cell, so it has to see the first write.
        let partial = s.spawn(|| e.write_file("f", a, 250, &[4; 50]));
        thread::sleep(QUIET);
        assert_eq!(last(), before + 2, "merged before the write below applied");
        assert!(!whole.is_finished() && !other.is_finished() && !partial.is_finished());
        release.0.set(false);
        for h in [whole, other, partial] {
            h.join().unwrap().unwrap();
        }
    });
    let mut want = vec![2; 200];
    want.resize(250, 0);
    want.resize(300, 4);
    assert_eq!(e.read_file("f", a, 0, GRID).unwrap(), want);
    assert_eq!(e.read_file("f", b, 0, GRID).unwrap(), vec![3; 100]);
    assert_eq!(e.telemetry.snapshot().concurrent_conflicts, 0);
}

/// Off, the same writes go one commit at a time.
#[test]
fn without_the_option_one_commit_waits_at_a_time() {
    let c = Cluster::start(false);
    let leader = c.leader();
    let (raft, e) = &c.members[leader];
    e.create_fs_as("f".into(), "f").unwrap();
    let (a, b) = (file(e, "a"), file(e, "b"));
    let last = || raft.with_node(|n| n.last_index());
    let before = last();
    let release = c.stall_followers(leader);
    thread::scope(|s| {
        let first = s.spawn(|| e.write_file("f", a, 0, &[2; 100]));
        wait_for("the first proposal", || last() == before + 1);
        let second = s.spawn(|| e.write_file("f", b, 0, &[3; 100]));
        thread::sleep(QUIET);
        assert_eq!(last(), before + 1);
        release.0.set(false);
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
    });
    assert_eq!(e.telemetry.snapshot().concurrent_commits, 0);
}

#[derive(Debug, Clone, Default)]
struct FailOnce(Arc<AtomicBool>);

impl DeviceSync for FailOnce {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        if self.0.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected writeback error"));
        }
        file.sync_data()
    }
}

/// A write that had placed its data when the local log failed commits nothing, and neither does
/// it bring the unacknowledged record back.
#[test]
fn a_write_in_flight_when_the_local_log_fails_changes_nothing() {
    let td = tempfile::tempdir().unwrap();
    let nodes = (1..=3).map(node).collect();
    let e = NativeEngine::open(config(td.path(), true), nodes).unwrap();
    e.create_fs_as("f".into(), "f").unwrap();
    let a = file(&e, "a");
    let (reached, waiting) = mpsc::channel();
    let (go, held) = mpsc::channel::<()>();
    let held = Mutex::new(held);
    e.set_commit_hook(Some(Arc::new(move |point| {
        if point == CommitPoint::Placed {
            reached.send(()).unwrap();
            held.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
    })));
    thread::scope(|s| {
        let write = s.spawn(|| e.write_file("f", a, 0, &[2; 100]));
        waiting.recv_timeout(WAIT).unwrap();
        let sync = FailOnce::default();
        e.set_log_sync(Box::new(sync.clone()));
        sync.0.store(true, Ordering::SeqCst);
        assert!(e.create_volume_as("v".into(), "v", 4096).is_err());
        go.send(()).unwrap();
        assert!(write.join().unwrap().is_err());
    });
    assert!(
        e.volumes().unwrap().is_empty(),
        "an unacknowledged command is visible"
    );
    assert_eq!(e.fs_getattr("f", a).unwrap().size, 0);
}
