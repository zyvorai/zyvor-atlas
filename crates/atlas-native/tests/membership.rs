// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Joint-consensus membership changes on in-memory Raft nodes.

use std::collections::{BTreeMap, BTreeSet};

use atlas_native::{Membership, MetaCommand, RaftConfig, RaftError, RaftNode, Role};

struct Sim {
    td: tempfile::TempDir,
    cfgs: BTreeMap<String, RaftConfig>,
    nodes: BTreeMap<String, Option<RaftNode>>,
    isolated: BTreeSet<String>,
    compact_after: u64,
}

fn ids(n: std::ops::RangeInclusive<usize>) -> Vec<String> {
    n.map(|i| format!("m{i}")).collect()
}

fn set(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn addrs(v: &[&str]) -> BTreeMap<String, String> {
    v.iter()
        .map(|s| (s.to_string(), format!("{s}:7482")))
        .collect()
}

fn create(name: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: format!("vol-{name}"),
        name: name.into(),
        size_bytes: 4096,
    }
}

impl Sim {
    /// `voters` bootstrap the group; every node knows every other.
    fn new(voters: usize, compact_after: u64) -> Self {
        let mut s = Self {
            td: tempfile::tempdir().unwrap(),
            cfgs: BTreeMap::new(),
            nodes: BTreeMap::new(),
            isolated: BTreeSet::new(),
            compact_after,
        };
        let all = ids(1..=voters);
        for id in &all {
            s.add_node(id, &all, None);
        }
        s
    }

    /// Opens `id` knowing `known`, with an explicit bootstrap voter set when given.
    fn add_node(&mut self, id: &str, known: &[String], bootstrap: Option<Vec<String>>) {
        let peers = known.iter().filter(|p| *p != id).cloned().collect();
        let mut cfg = RaftConfig::new(id, peers, self.td.path().join(id));
        cfg.compact_after = self.compact_after;
        cfg.bootstrap = bootstrap;
        self.nodes
            .insert(id.into(), Some(RaftNode::open(cfg.clone()).unwrap()));
        self.cfgs.insert(id.into(), cfg);
    }

    /// A node that is not a bootstrap voter: it waits to be added.
    fn add_joiner(&mut self, id: &str, voters: &[String]) {
        let mut known = voters.to_vec();
        known.push(id.into());
        self.add_node(id, &known, Some(voters.to_vec()));
    }

    fn node(&self, id: &str) -> &RaftNode {
        self.nodes[id].as_ref().unwrap()
    }

    fn node_mut(&mut self, id: &str) -> &mut RaftNode {
        self.nodes.get_mut(id).unwrap().as_mut().unwrap()
    }

    fn restart(&mut self, id: &str) {
        self.nodes.insert(id.into(), None);
        let n = RaftNode::open(self.cfgs[id].clone()).unwrap();
        self.nodes.insert(id.into(), Some(n));
    }

    fn deliver(&mut self) {
        for _ in 0..10_000 {
            let mut batch = Vec::new();
            for n in self.nodes.values_mut().flatten() {
                batch.extend(n.take_messages().unwrap());
            }
            if batch.is_empty() {
                return;
            }
            for env in batch {
                if self.isolated.contains(&env.from) || self.isolated.contains(&env.to) {
                    continue;
                }
                if let Some(Some(n)) = self.nodes.get_mut(&env.to) {
                    n.step(env).unwrap();
                }
            }
        }
        panic!("message storm");
    }

    fn tick(&mut self) {
        for n in self.nodes.values_mut().flatten() {
            n.tick().unwrap();
        }
        self.deliver();
    }

    fn run_until(&mut self, max: usize, pred: impl Fn(&Self) -> bool) -> bool {
        for _ in 0..max {
            if pred(self) {
                return true;
            }
            self.tick();
        }
        pred(self)
    }

    fn leader(&self) -> Option<String> {
        let leaders: Vec<&RaftNode> = self
            .nodes
            .values()
            .flatten()
            .filter(|n| n.is_leader() && !self.isolated.contains(n.id()))
            .collect();
        let terms: BTreeSet<u64> = leaders.iter().map(|n| n.term()).collect();
        assert_eq!(terms.len(), leaders.len(), "two leaders in one term");
        leaders
            .iter()
            .max_by_key(|n| n.term())
            .map(|n| n.id().to_string())
    }

    fn elect(&mut self) -> String {
        assert!(self.run_until(1000, |s| s.leader().is_some()), "no leader");
        self.leader().unwrap()
    }

    /// Proposes on the current leader once it has committed an entry in its term.
    fn add_voter(&mut self, id: &str) -> u64 {
        for _ in 0..1000 {
            let l = self.elect();
            match self
                .node_mut(&l)
                .add_voter(id.to_string(), format!("{id}:7482"))
            {
                Ok(i) => {
                    self.deliver();
                    return i;
                }
                Err(RaftError::MembershipBusy) => self.tick(),
                Err(e) => panic!("add_voter: {e}"),
            }
        }
        panic!("add_voter never accepted");
    }

    fn remove_voter(&mut self, id: &str) -> u64 {
        for _ in 0..1000 {
            let l = self.elect();
            match self.node_mut(&l).remove_voter(&id.to_string()) {
                Ok(i) => {
                    self.deliver();
                    return i;
                }
                Err(RaftError::MembershipBusy) => self.tick(),
                Err(e) => panic!("remove_voter: {e}"),
            }
        }
        panic!("remove_voter never accepted");
    }

    fn change(&mut self, voters: BTreeSet<String>, a: BTreeMap<String, String>) -> u64 {
        for _ in 0..1000 {
            let l = self.elect();
            match self
                .node_mut(&l)
                .change_membership(voters.clone(), a.clone())
            {
                Ok(i) => {
                    self.deliver();
                    return i;
                }
                Err(RaftError::MembershipBusy) => self.tick(),
                Err(e) => panic!("change_membership: {e}"),
            }
        }
        panic!("leader never accepted the change");
    }

    fn propose(&mut self, cmd: MetaCommand) {
        let l = self.elect();
        self.node_mut(&l).propose(cmd).unwrap();
        self.deliver();
    }

    fn stable_everywhere(&self, ids: &[&str], voters: &BTreeSet<String>) -> bool {
        ids.iter().all(|id| {
            let n = self.node(id);
            matches!(n.membership(), Membership::Stable { voters: v } if v == voters)
                && n.applied_index() >= n.latest_config_index()
        })
    }

    fn has_volume(&self, id: &str, name: &str) -> bool {
        self.node(id)
            .catalog()
            .volumes
            .contains_key(&format!("vol-{name}"))
    }
}

#[test]
fn joiner_waits_then_becomes_a_voter() {
    let mut s = Sim::new(3, 0);
    s.elect();
    s.propose(create("a"));
    s.add_joiner("m4", &ids(1..=3));

    // Not a voter yet: it never campaigns and the leader does not replicate to it.
    for _ in 0..300 {
        s.tick();
    }
    let m4 = s.node("m4");
    assert!(!m4.is_voter());
    assert_eq!(m4.role(), Role::Follower);
    assert_eq!(m4.term(), 0, "a non-voter must not campaign");
    assert!(!s.has_volume("m4", "a"));

    let four = set(&["m1", "m2", "m3", "m4"]);
    s.change(four.clone(), addrs(&["m4"]));
    assert!(
        s.run_until(500, |s| s
            .stable_everywhere(&["m1", "m2", "m3", "m4"], &four)),
        "membership did not settle on 4 voters"
    );
    assert!(s.node("m4").is_voter());
    assert!(s.has_volume("m4", "a"));
    assert_eq!(s.node("m1").catalog().raft_addrs["m4"], "m4:7482");

    s.propose(create("b"));
    assert!(s.run_until(200, |s| (1..=4)
        .all(|i| s.has_volume(&format!("m{i}"), "b"))));
}

#[test]
fn joint_entry_needs_a_majority_of_the_new_set_too() {
    let mut s = Sim::new(3, 0);
    s.elect();
    // Make sure m1 leads so it stays in both configurations.
    while s.leader().as_deref() != Some("m1") {
        let l = s.leader().unwrap();
        s.isolated.insert(l.clone());
        s.run_until(1000, |s| s.leader().is_some_and(|x| x != l));
        s.isolated.clear();
        s.run_until(1000, |s| s.leader().is_some());
    }
    s.add_joiner("m4", &ids(1..=3));
    s.add_joiner("m5", &ids(1..=3));
    s.isolated.extend(["m4".to_string(), "m5".to_string()]);

    let target = set(&["m1", "m4", "m5"]);
    let joint = s.change(target.clone(), addrs(&["m4", "m5"]));
    assert!(matches!(
        s.node_mut("m1")
            .change_membership(set(&["m1"]), BTreeMap::new()),
        Err(RaftError::MembershipBusy)
    ));
    for _ in 0..200 {
        s.tick();
    }
    // m2 and m3 (the old majority with m1) have the entry, but the new set has only m1.
    let m1 = s.node("m1");
    assert!(
        m1.commit_index() < joint,
        "joint entry committed without the new majority"
    );
    assert!(matches!(m1.membership(), Membership::Joint { .. }));

    s.isolated.clear();
    assert!(
        s.run_until(1000, |s| s.stable_everywhere(&["m1", "m4", "m5"], &target)),
        "change did not complete once the new voters were reachable"
    );
    s.propose(create("after"));
    assert!(s.run_until(200, |s| ["m1", "m4", "m5"]
        .iter()
        .all(|i| s.has_volume(i, "after"))));

    // Removed followers never see the final entry (it is replicated to the new voters only),
    // but they cannot disrupt the new group: a pre-vote needs a majority of the new set too.
    let leader = s.elect();
    let term = s.node(&leader).term();
    for _ in 0..300 {
        s.tick();
    }
    assert_eq!(
        s.leader().as_deref(),
        Some(leader.as_str()),
        "the new group was disrupted"
    );
    assert_eq!(s.node(&leader).term(), term, "the new group was disrupted");
    for i in ["m2", "m3"] {
        assert_ne!(s.node(i).role(), Role::Leader);
    }
}

#[test]
fn leader_that_removes_itself_steps_down() {
    let mut s = Sim::new(3, 0);
    let l = s.elect();
    let rest: BTreeSet<String> = ids(1..=3).into_iter().filter(|i| *i != l).collect();
    s.change(rest.clone(), BTreeMap::new());
    let rest_ids: Vec<&str> = rest.iter().map(String::as_str).collect();
    assert!(s.run_until(500, |s| s.stable_everywhere(&rest_ids, &rest)));
    assert!(s.run_until(100, |s| !s.node(&l).is_leader()));
    assert!(!s.node(&l).is_voter());

    let new = s.elect();
    assert!(rest.contains(&new), "removed node {l} leads again");
    s.propose(create("x"));
    assert!(s.run_until(200, |s| rest_ids.iter().all(|i| s.has_volume(i, "x"))));
}

#[test]
fn membership_is_only_changed_through_the_protocol() {
    let mut s = Sim::new(3, 0);
    let l = s.elect();
    let direct = MetaCommand::ChangeMembership {
        membership: Membership::stable(["m1".to_string()]),
        addrs: BTreeMap::new(),
    };
    assert!(matches!(
        s.node_mut(&l).propose(direct),
        Err(RaftError::Config(_))
    ));
    assert!(
        matches!(
            s.node_mut(&l)
                .change_membership(set(&["m1", "m9"]), BTreeMap::new()),
            Err(RaftError::Config(_))
        ),
        "a new voter without an address must be refused"
    );
    let f = ids(1..=3).into_iter().find(|i| *i != l).unwrap();
    assert!(matches!(
        s.node_mut(&f)
            .change_membership(set(&["m1"]), BTreeMap::new()),
        Err(RaftError::NotLeader { .. })
    ));
}

#[test]
fn membership_survives_restart_and_compaction() {
    let mut s = Sim::new(3, 4);
    s.elect();
    for i in 0..10 {
        s.propose(create(&format!("v{i}")));
    }
    // m4 joins after the log was compacted: it is brought up to date by a snapshot.
    s.add_joiner("m4", &ids(1..=3));
    let four = set(&["m1", "m2", "m3", "m4"]);
    s.change(four.clone(), addrs(&["m4"]));
    let all = ["m1", "m2", "m3", "m4"];
    assert!(s.run_until(500, |s| s.stable_everywhere(&all, &four)));
    for i in 10..20 {
        s.propose(create(&format!("v{i}")));
    }
    assert!(s.run_until(200, |s| s.has_volume("m4", "v19")
        && s.has_volume("m4", "v0")));

    for id in all {
        s.restart(id);
    }
    for id in all {
        assert!(
            matches!(s.node(id).membership(), Membership::Stable { voters } if *voters == four),
            "{id}: membership lost on restart"
        );
    }
    assert!(s.node("m4").is_voter());
    s.elect();
    // Entries above the last compaction re-apply once the new leader advertises its commit.
    assert!(s.run_until(200, |s| s.stable_everywhere(&all, &four)));
    s.propose(create("post-restart"));
    assert!(s.run_until(200, |s| all.iter().all(|i| s.has_volume(i, "post-restart"))));
}

#[test]
fn add_and_remove_voter_go_through_joint_consensus() {
    let mut s = Sim::new(3, 0);
    s.elect();
    s.propose(create("before"));
    s.add_joiner("m4", &ids(1..=3));
    s.add_voter("m4");
    let four = set(&["m1", "m2", "m3", "m4"]);
    let all = ["m1", "m2", "m3", "m4"];
    assert!(
        s.run_until(500, |s| s.stable_everywhere(&all, &four)),
        "add_voter did not settle on the joint-then-stable configuration"
    );
    assert!(s.node("m4").is_voter());
    assert!(s.has_volume("m4", "before"));

    s.remove_voter("m4");
    let three = set(&["m1", "m2", "m3"]);
    assert!(
        s.run_until(500, |s| s.stable_everywhere(&["m1", "m2", "m3"], &three)),
        "remove_voter did not settle"
    );
    assert!(!s.node("m4").is_voter());
    s.propose(create("after"));
    assert!(s.run_until(200, |s| s.has_volume("m1", "after")));
    assert!(
        !s.has_volume("m4", "after"),
        "a removed voter is not in the replication quorum"
    );
}
