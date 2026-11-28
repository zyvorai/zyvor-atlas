// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The conditional rename through Raft: its capability gate in a cluster with an older metadata
//! node, after the cluster has used it once, and one winner among concurrent callers.

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    engine::NewNode, metadata::MetaError, raft::CAP_DELTA, raft::CAP_RENAME_IF, BlockStore,
    ConditionalRename, EngineConfig, FailureDomain, FileDevice, MetaBackend, NativeEngine,
    NativeError, Node, NodeType, RaftConfig, RaftError, RaftServer, RenameExpect, ROOT_INO,
};

const TICK: Duration = Duration::from_millis(10);
/// The node's default Raft tick: with much shorter ones, an fsync on a busy disk outlasts an
/// election timeout and no leader holds.
const RAFT_TICK: Duration = Duration::from_millis(50);
const WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Meta {
    engine: Arc<NativeEngine>,
    raft: Arc<RaftServer>,
}

/// Three metadata replicas, no file data.
struct Cluster {
    td: tempfile::TempDir,
    addrs: BTreeMap<String, SocketAddr>,
    metas: BTreeMap<String, Meta>,
}

fn stores(root: &Path) -> Vec<(Node, Arc<dyn BlockStore>)> {
    (1..=3)
        .map(|i| {
            let node = Node {
                id: format!("n{i}"),
                failure_domain: FailureDomain {
                    zone: "z1".into(),
                    rack: format!("r{i}"),
                    host: format!("h{i}"),
                },
                free_bytes: 1 << 30,
                healthy: true,
            };
            let device = FileDevice::open(root.join(format!("n{i}.data"))).unwrap();
            (node, Arc::new(device) as Arc<dyn BlockStore>)
        })
        .collect()
}

impl Cluster {
    fn start(capabilities: impl Fn(&str) -> u64) -> Self {
        let listeners: BTreeMap<String, TcpListener> = (1..=3)
            .map(|i| (format!("m{i}"), TcpListener::bind("127.0.0.1:0").unwrap()))
            .collect();
        let mut c = Self {
            td: tempfile::tempdir().unwrap(),
            addrs: listeners
                .iter()
                .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
                .collect(),
            metas: BTreeMap::new(),
        };
        for (id, l) in listeners {
            c.open(&id, l, capabilities(&id));
        }
        c
    }

    fn open(&mut self, id: &str, listener: TcpListener, capabilities: u64) {
        let peers: BTreeMap<String, SocketAddr> = self
            .addrs
            .iter()
            .filter(|(p, _)| *p != id)
            .map(|(p, a)| (p.clone(), *a))
            .collect();
        let root = self.td.path().join(id);
        std::fs::create_dir_all(root.join("engine")).unwrap();
        let mut rcfg = RaftConfig::new(id, peers.keys().cloned().collect(), root.join("raft"));
        rcfg.capabilities = capabilities;
        let raft = Arc::new(RaftServer::start(rcfg, listener, peers, RAFT_TICK).unwrap());
        let engine = NativeEngine::open_with(
            EngineConfig::new(root.join("engine")),
            stores(&root),
            MetaBackend::Raft {
                server: raft.clone(),
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        self.metas.insert(
            id.into(),
            Meta {
                engine: Arc::new(engine),
                raft,
            },
        );
    }

    /// Stops a replica as a crash would: it says nothing more.
    fn kill(&mut self, id: &str) {
        if let Some(m) = self.metas.remove(id) {
            m.raft.halt();
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

    /// Runs `op` on each live replica until the leader answers, with success or a metadata
    /// refusal.
    fn on_leader<T>(
        &self,
        mut op: impl FnMut(&NativeEngine) -> Result<T, NativeError>,
    ) -> (String, Result<T, MetaError>) {
        let deadline = Instant::now() + WAIT;
        let mut last = String::new();
        loop {
            for (id, m) in &self.metas {
                match op(&m.engine) {
                    Ok(v) => return (id.clone(), Ok(v)),
                    Err(NativeError::Metadata(e))
                    | Err(NativeError::Raft(RaftError::Rejected(e))) => {
                        return (id.clone(), Err(e))
                    }
                    Err(e) => last = e.to_string(),
                }
            }
            assert!(Instant::now() < deadline, "no leader answered: {last}");
            thread::sleep(TICK);
        }
    }

    fn file(&self, name: &str) -> u64 {
        let node = NewNode {
            name: name.into(),
            op_id: format!("op-{name}"),
            kind: NodeType::File,
            target: None,
            rdev: 0,
            mode: 0o644,
            create_mode: None,
            uid: 0,
            gid: 0,
        };
        self.on_leader(|e| e.fs_mknode("f", ROOT_INO, node.clone()))
            .1
            .unwrap()
            .ino
    }

    fn publish(&self, from: &str, ino: u64, to: &str) -> Result<(), MetaError> {
        self.on_leader(|e| e.fs_rename_if(publish(from, ino, to))).1
    }

    /// Publishes, waiting out the time until the leader has heard every replica.
    fn publish_once_supported(&self, from: &str, ino: u64, to: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.publish(from, ino, to) {
                Ok(()) => return,
                Err(MetaError::Unsupported(_)) => assert!(Instant::now() < deadline),
                Err(e) => panic!("{e}"),
            }
            thread::sleep(TICK);
        }
    }

    /// What `name` names, as the leader sees it.
    fn lookup(&self, name: &str) -> Option<u64> {
        match self.on_leader(|e| e.fs_lookup("f", ROOT_INO, name)).1 {
            Ok(a) => Some(a.ino),
            Err(MetaError::NotFound(_)) => None,
            Err(e) => panic!("{e}"),
        }
    }
}

fn publish(from: &str, ino: u64, to: &str) -> ConditionalRename {
    ConditionalRename {
        fs: "f".into(),
        parent: ROOT_INO,
        name: from.into(),
        ino,
        new_parent: ROOT_INO,
        new_name: to.into(),
        expect: Some(RenameExpect::Absent),
        retain_xattr: None,
        now_ns: 0,
    }
}

#[test]
fn an_older_metadata_node_closes_the_gate_until_the_cluster_has_used_it() {
    let mut c = Cluster::start(|id| {
        if id == "m3" {
            CAP_DELTA
        } else {
            CAP_DELTA | CAP_RENAME_IF
        }
    });
    c.on_leader(|e| e.create_fs_as("f".into(), "f")).1.unwrap();
    let a = c.file("a");
    // Time enough for every follower to have answered. Whoever leads (m3 stands in for an old
    // build by what it advertises, so if it leads its own bit is missing), nothing is renamed.
    thread::sleep(Duration::from_millis(500));
    let err = c.publish("a", a, "k").unwrap_err();
    assert!(matches!(err, MetaError::Unsupported(_)), "{err}");
    assert_eq!((c.lookup("a"), c.lookup("k")), (Some(a), None));

    // Upgraded: allowed once the leader has heard from it.
    c.restart("m3", CAP_DELTA | CAP_RENAME_IF);
    c.publish_once_supported("a", a, "k");
    assert_eq!(c.lookup("k"), Some(a));

    // Having used it, a new leader needs no word from a replica that is down.
    let (leader, _) = c.on_leader(|e| e.ensure_leader());
    c.kill(&leader);
    let b = c.file("b");
    c.publish("b", b, "j").unwrap();
    assert_eq!(c.lookup("j"), Some(b));
    // A retry of a publish that took effect succeeds without doing anything.
    c.publish("b", b, "j").unwrap();
}

#[test]
fn one_of_many_concurrent_creators_wins() {
    let c = Cluster::start(|_| CAP_DELTA | CAP_RENAME_IF);
    c.on_leader(|e| e.create_fs_as("f".into(), "f")).1.unwrap();
    let warm = c.file("warm");
    c.publish_once_supported("warm", warm, "warmed");
    for round in 0..5 {
        let files: Vec<(String, u64)> = (0..8)
            .map(|i| {
                let name = format!("t{round}-{i}");
                let ino = c.file(&name);
                (name, ino)
            })
            .collect();
        let to = format!("k{round}");
        let won: Vec<u64> = thread::scope(|s| {
            let handles: Vec<_> = files
                .iter()
                .map(|(name, ino)| {
                    let (c, to) = (&c, &to);
                    s.spawn(move || match c.publish(name, *ino, to) {
                        Ok(()) => Some(*ino),
                        Err(MetaError::Precondition(_)) => None,
                        Err(e) => panic!("{e}"),
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect()
        });
        assert_eq!(won.len(), 1, "round {round}");
        assert_eq!(c.lookup(&to), Some(won[0]));
    }
}
