// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The leader log synced outside the node (`metadata.parallel_log_sync`): what the leader may
//! count towards a quorum, and a randomised run with power failures, in both modes. A power
//! failure cuts a node's log file back to what the node had counted as durable.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

use atlas_native::{
    wal::LogSync, Envelope, MetaCommand, RaftConfig, RaftError, RaftNode, RaftServer, Role,
};

struct Cluster {
    _td: tempfile::TempDir,
    /// Leaders sync their log through `begin_log_sync` / `finish_log_sync`.
    external: bool,
    cfgs: BTreeMap<String, RaftConfig>,
    nodes: BTreeMap<String, Option<RaftNode>>,
    /// Sent and not delivered.
    net: Vec<Envelope>,
    /// Syncs handed out and not run.
    held: BTreeMap<String, LogSync>,
    isolated: BTreeSet<String>,
}

impl Cluster {
    fn new(n: usize, external: bool, compact_after: u64) -> Self {
        let td = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (1..=n).map(|i| format!("m{i}")).collect();
        let mut cfgs = BTreeMap::new();
        for id in &ids {
            let peers = ids.iter().filter(|p| *p != id).cloned().collect();
            let mut cfg = RaftConfig::new(id.clone(), peers, td.path().join(id));
            cfg.compact_after = compact_after;
            cfgs.insert(id.clone(), cfg);
        }
        let mut c = Self {
            _td: td,
            external,
            cfgs,
            nodes: BTreeMap::new(),
            net: Vec::new(),
            held: BTreeMap::new(),
            isolated: BTreeSet::new(),
        };
        for id in ids {
            c.restart(&id);
        }
        c
    }

    fn ids(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    fn node(&self, id: &str) -> &RaftNode {
        self.nodes[id].as_ref().expect("node is down")
    }

    fn node_mut(&mut self, id: &str) -> &mut RaftNode {
        self.nodes
            .get_mut(id)
            .unwrap()
            .as_mut()
            .expect("node is down")
    }

    fn up(&self, id: &str) -> bool {
        self.nodes.get(id).is_some_and(Option::is_some)
    }

    fn restart(&mut self, id: &str) {
        if self.up(id) {
            return;
        }
        let mut n = RaftNode::open(self.cfgs[id].clone()).unwrap();
        if self.external {
            n.sync_leader_log_externally();
        }
        self.nodes.insert(id.to_string(), Some(n));
    }

    /// The machine loses power: whatever the node had not counted as durable is gone.
    fn power_loss(&mut self, id: &str) {
        let Some(node) = self.nodes.get_mut(id).and_then(Option::take) else {
            return;
        };
        let durable = node.log_synced_index();
        drop(node);
        self.held.remove(id);
        let path = self.cfgs[id].root.join("wal").join("metadata.wal");
        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        let kept: String = text
            .lines()
            .filter(|l| {
                let record: serde_json::Value = serde_json::from_str(l).unwrap();
                record["index"].as_u64().unwrap() <= durable
            })
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(&path, kept).unwrap();
    }

    /// Sends what `id` has to send.
    fn flush(&mut self, id: &str) {
        if self.up(id) {
            let msgs = self.node_mut(id).take_messages().unwrap();
            self.net.extend(msgs);
        }
    }

    fn deliver_at(&mut self, i: usize) {
        let env = self.net.swap_remove(i);
        if self.isolated.contains(&env.from) || self.isolated.contains(&env.to) {
            return;
        }
        if let Some(Some(n)) = self.nodes.get_mut(&env.to) {
            n.step(env).unwrap();
        }
    }

    /// Delivers everything until the cluster is quiet, running no log sync.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            for id in self.ids() {
                self.flush(&id);
            }
            if self.net.is_empty() {
                return;
            }
            while !self.net.is_empty() {
                self.deliver_at(0);
            }
        }
        panic!("message storm: cluster never went quiet");
    }

    fn begin_sync(&mut self, id: &str) {
        if self.up(id) && !self.held.contains_key(id) {
            if let Some(sync) = self.node_mut(id).begin_log_sync().unwrap() {
                self.held.insert(id.to_string(), sync);
            }
        }
    }

    fn finish_sync(&mut self, id: &str) {
        if let Some(sync) = self.held.remove(id) {
            let synced = sync.run();
            self.node_mut(id).finish_log_sync(synced).unwrap();
        }
    }

    fn sync(&mut self, id: &str) {
        self.begin_sync(id);
        self.finish_sync(id);
    }

    fn tick(&mut self) {
        for id in self.ids() {
            if self.up(&id) {
                self.node_mut(&id).tick().unwrap();
            }
        }
        self.settle();
        for id in self.ids() {
            self.sync(&id);
        }
        self.settle();
    }

    /// The leader of the highest term among live, connected nodes.
    fn leader(&self) -> Option<String> {
        self.nodes
            .values()
            .flatten()
            .filter(|n| n.is_leader() && !self.isolated.contains(n.id()))
            .max_by_key(|n| n.term())
            .map(|n| n.id().to_string())
    }

    fn elect(&mut self) -> String {
        for _ in 0..500 {
            if let Some(l) = self.leader() {
                if self.node(&l).commit_index() == self.node(&l).last_index() {
                    return l;
                }
            }
            self.tick();
        }
        panic!("no leader elected");
    }

    fn has(&self, id: &str, name: &str) -> bool {
        self.node(id).catalog().volumes.contains_key(name)
    }
}

fn create(name: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: name.into(),
        name: name.into(),
        size_bytes: 4096,
    }
}

#[test]
fn the_leader_sends_before_its_own_copy_is_durable() {
    let mut c = Cluster::new(3, true, 0);
    let l = c.elect();
    let index = c.node_mut(&l).propose(create("a")).unwrap();
    let sent = c.node_mut(&l).take_messages().unwrap();
    assert!(c.node(&l).log_synced_index() < index);
    assert_eq!(sent.len(), 2, "{sent:?}");
}

#[test]
fn the_leader_counts_itself_only_for_what_is_durable() {
    let mut c = Cluster::new(3, true, 0);
    let l = c.elect();
    let cut = c.ids().into_iter().find(|id| *id != l).unwrap();
    c.isolated.insert(cut);
    let index = c.node_mut(&l).propose(create("a")).unwrap();
    c.settle();
    // One follower holds it durably. The leader's copy is not durable yet: no quorum.
    assert!(c.node(&l).log_synced_index() < index);
    assert!(c.node(&l).commit_index() < index);
    assert!(!c.has(&l, "a"));
    // Nothing but the report of its own sync makes the second copy.
    for _ in 0..2 {
        c.node_mut(&l).tick().unwrap();
        c.settle();
    }
    c.begin_sync(&l);
    assert!(
        c.node(&l).commit_index() < index,
        "counted a sync not run yet"
    );
    c.finish_sync(&l);
    assert_eq!(c.node(&l).commit_index(), index);
    assert!(c.has(&l, "a"));
}

#[test]
fn a_sync_covers_only_what_the_log_held_when_it_began() {
    let mut c = Cluster::new(3, true, 0);
    let l = c.elect();
    let cut = c.ids().into_iter().find(|id| *id != l).unwrap();
    c.isolated.insert(cut);
    let a = c.node_mut(&l).propose(create("a")).unwrap();
    c.begin_sync(&l);
    let b = c.node_mut(&l).propose(create("b")).unwrap();
    c.settle();
    c.finish_sync(&l);
    c.settle();
    assert_eq!(c.node(&l).log_synced_index(), a);
    assert_eq!(c.node(&l).commit_index(), a);
    c.sync(&l);
    assert_eq!(c.node(&l).commit_index(), b);
}

#[test]
fn two_followers_are_a_quorum_without_the_leaders_own_copy() {
    let mut c = Cluster::new(3, true, 0);
    let l = c.elect();
    let index = c.node_mut(&l).propose(create("a")).unwrap();
    c.settle();
    assert_eq!(c.node(&l).commit_index(), index);
    assert!(c.node(&l).log_synced_index() < index);
    // The leader loses power before its own copy reached the disk: the two followers have it.
    c.power_loss(&l);
    let new = c.elect();
    assert_ne!(new, l);
    assert!(c.has(&new, "a"));
    c.restart(&l);
    for _ in 0..40 {
        c.tick();
    }
    assert!(c.has(&l, "a"));
}

#[test]
fn a_single_voter_commits_only_after_its_sync() {
    let mut c = Cluster::new(1, true, 0);
    let l = c.elect();
    let index = c.node_mut(&l).propose(create("a")).unwrap();
    c.settle();
    c.node_mut(&l).tick().unwrap();
    assert!(c.node(&l).commit_index() < index);
    c.begin_sync(&l);
    c.power_loss(&l);
    c.restart(&l);
    assert!(!c.has(&c.elect(), "a"), "never durable, never committed");
    let index = c.node_mut(&l).propose(create("b")).unwrap();
    c.sync(&l);
    assert_eq!(c.node(&l).commit_index(), index);
    c.power_loss(&l);
    c.restart(&l);
    assert!(c.has(&c.elect(), "b"));
}

#[test]
fn a_sync_reported_after_the_log_was_cut_counts_for_nothing() {
    let mut c = Cluster::new(3, true, 0);
    let old = c.elect();
    c.isolated.insert(old.clone());
    for name in ["lost1", "lost2", "lost3"] {
        c.node_mut(&old).propose(create(name)).unwrap();
    }
    c.begin_sync(&old);
    let new = c.elect();
    c.node_mut(&new).propose(create("kept")).unwrap();
    // The new leader's entries replace the old tail; `old` has appended them but sent nothing,
    // so it has not synced them.
    c.isolated.clear();
    c.net.clear();
    let mut stepped = false;
    for _ in 0..20 {
        c.node_mut(&new).tick().unwrap();
        c.flush(&new);
        while !c.net.is_empty() {
            let i = 0;
            let to = c.net[i].to.clone();
            c.deliver_at(i);
            if to == old {
                // The rejection tells the leader where to continue.
                if c.node(&old).last_index() == c.node(&new).last_index() {
                    stepped = true;
                    break;
                }
                c.flush(&old);
            }
        }
        if stepped {
            break;
        }
    }
    assert!(stepped, "the old leader never caught up");
    let durable = c.node(&old).log_synced_index();
    assert!(durable < c.node(&old).last_index());
    c.finish_sync(&old);
    assert_eq!(c.node(&old).log_synced_index(), durable);
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

/// A proposal a client is waiting for.
struct Waiting {
    node: String,
    term: u64,
    index: u64,
    name: String,
}

/// Random ticks, message loss and reordering, syncs finished late, partitions, restarts and power
/// failures. A proposal counts as acknowledged exactly when `RaftServer` would answer its caller:
/// the proposing node still leads that term and has applied the index. No acknowledged command
/// may ever be missing again, whoever leads. Returns how many were acknowledged, and how many of
/// them while the leader's own copy was not durable.
fn random_run(seed: u64, voters: usize, external: bool, steps: usize) -> (usize, usize) {
    let compact = if seed.is_multiple_of(2) { 0 } else { 4 };
    let mut c = Cluster::new(voters, external, compact);
    let mut rng = Rng(seed);
    let mut waiting: Vec<Waiting> = Vec::new();
    let mut acked: BTreeMap<u64, String> = BTreeMap::new();
    let (mut proposed, mut ahead) = (0, 0);
    for step in 0..steps {
        let ids = c.ids();
        let id = ids[rng.below(ids.len())].clone();
        match rng.below(100) {
            0..=9 => {
                if c.up(&id) {
                    c.node_mut(&id).tick().unwrap();
                }
            }
            10..=29 => c.flush(&id),
            30..=59 => {
                for _ in 0..=rng.below(3) {
                    if !c.net.is_empty() {
                        let i = rng.below(c.net.len());
                        if rng.below(20) == 0 {
                            c.net.swap_remove(i);
                        } else {
                            c.deliver_at(i);
                        }
                    }
                }
            }
            60..=65 => c.begin_sync(&id),
            66..=71 => c.finish_sync(&id),
            72..=83 => {
                let leaders: Vec<String> = ids
                    .iter()
                    .filter(|i| c.up(i) && c.node(i).is_leader())
                    .cloned()
                    .collect();
                if !leaders.is_empty() {
                    let node = leaders[rng.below(leaders.len())].clone();
                    proposed += 1;
                    let name = format!("v{proposed}");
                    let n = c.node_mut(&node);
                    let term = n.term();
                    match n.propose(create(&name)) {
                        Ok(index) => waiting.push(Waiting {
                            node,
                            term,
                            index,
                            name,
                        }),
                        Err(e) => panic!("step {step}: {e}"),
                    }
                }
            }
            // Heartbeats, so that elections finish and followers catch up.
            84..=91 => {
                for _ in 0..3 {
                    for i in &ids {
                        if c.up(i) {
                            c.node_mut(i).tick().unwrap();
                        }
                        c.flush(i);
                    }
                    while !c.net.is_empty() {
                        c.deliver_at(0);
                    }
                }
            }
            92 => {
                if c.isolated.len() < voters / 2 {
                    c.isolated.insert(id);
                }
            }
            93 => c.isolated.clear(),
            94 | 95 => c.power_loss(&id),
            96 => {
                // The leader loses power, often just after acknowledging.
                if let Some(l) = c.leader() {
                    c.power_loss(&l);
                }
            }
            97 => {
                if rng.below(4) == 0 {
                    for i in &ids {
                        c.power_loss(i);
                    }
                    c.net.clear();
                }
            }
            _ => {
                for i in &ids {
                    if !c.up(i) {
                        c.restart(i);
                        break;
                    }
                }
            }
        }
        waiting.retain(|w| {
            let Some(Some(n)) = c.nodes.get(&w.node) else {
                return false;
            };
            if !n.is_leader() || n.term() != w.term {
                return false;
            }
            if n.applied_index() < w.index {
                return true;
            }
            assert!(
                n.catalog().volumes.contains_key(&w.name),
                "step {step}: {} acknowledged a command it did not apply",
                w.node
            );
            if n.log_synced_index() < w.index {
                ahead += 1;
            }
            acked.insert(w.index, w.name.clone());
            false
        });
        let mut terms = BTreeSet::new();
        for n in c.nodes.values().flatten() {
            if n.role() == Role::Leader {
                assert!(terms.insert(n.term()), "step {step}: two leaders in a term");
            }
            // Whatever a node has applied agrees with what was acknowledged.
            for (index, name) in acked.range(..=n.applied_index()) {
                assert!(
                    n.catalog().volumes.contains_key(name),
                    "step {step}: {} applied {index} without {name}",
                    n.id()
                );
            }
        }
    }
    // Every machine loses power, then all come back.
    for id in c.ids() {
        c.power_loss(&id);
    }
    c.net.clear();
    c.isolated.clear();
    for id in c.ids() {
        c.restart(&id);
    }
    let leader = c.elect();
    c.node_mut(&leader).propose(create("last")).unwrap();
    for _ in 0..100 {
        c.tick();
    }
    for id in c.ids() {
        assert!(c.has(&id, "last"), "{id} never caught up");
        for name in acked.values() {
            assert!(c.has(&id, name), "seed {seed}: {id} lost {name}");
        }
    }
    (acked.len(), ahead)
}

fn random_runs(voters: usize, external: bool) {
    let (mut acked, mut ahead) = (0, 0);
    for seed in 1..=12u64 {
        let (a, h) = random_run(seed * 0x9e37_79b9 + seed, voters, external, 3000);
        acked += a;
        ahead += h;
    }
    assert!(
        acked > 100,
        "only {acked} acknowledged: the runs barely commit"
    );
    if external && voters > 1 {
        assert!(
            ahead > 0,
            "no acknowledgement ran ahead of the leader's disk"
        );
    } else {
        assert_eq!(ahead, 0, "acknowledged before the leader's own sync");
    }
}

#[test]
fn random_runs_with_one_voter_and_the_inline_sync() {
    random_runs(1, false);
}

#[test]
fn random_runs_with_one_voter_and_the_external_sync() {
    random_runs(1, true);
}

#[test]
fn random_runs_with_three_voters_and_the_inline_sync() {
    random_runs(3, false);
}

#[test]
fn random_runs_with_three_voters_and_the_external_sync() {
    random_runs(3, true);
}

#[test]
fn random_runs_with_five_voters_and_the_external_sync() {
    random_runs(5, true);
}

#[test]
fn a_server_with_the_parallel_sync_counts_its_syncs() {
    use std::{net::TcpListener, time::Duration};
    let td = tempfile::tempdir().unwrap();
    let mut cfg = RaftConfig::new("a", vec![], td.path());
    cfg.parallel_log_sync = true;
    let server = RaftServer::start(
        cfg,
        TcpListener::bind("127.0.0.1:0").unwrap(),
        BTreeMap::<String, String>::new(),
        Duration::from_millis(5),
    )
    .unwrap();
    let wait = Duration::from_secs(10);
    let deadline = std::time::Instant::now() + wait;
    while let Err(e) = server.leader_ready(wait) {
        assert!(
            matches!(e, RaftError::NotLeader { .. }) && std::time::Instant::now() < deadline,
            "{e}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::scope(|s| {
        for w in 0..4 {
            let server = &server;
            s.spawn(move || {
                for i in 0..25 {
                    server.propose(create(&format!("v{w}-{i}")), wait).unwrap();
                }
            });
        }
    });
    assert_eq!(server.catalog().unwrap().volumes.len(), 100);
    let metrics = server.render_metrics().unwrap();
    let count = metrics
        .lines()
        .find_map(|l| l.strip_prefix("atlas_native_raft_parallel_log_syncs_total{"))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap();
    assert!(count > 0, "{metrics}");
}
