// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Several Raft groups sharing each node's transport listener.

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use atlas_native::{MetaCommand, RaftConfig, RaftError, RaftMux, RaftServer, Role};

const TICK: Duration = Duration::from_millis(10);
const WAIT: Duration = Duration::from_secs(15);
const GROUPS: u32 = 3;

struct Cluster {
    td: tempfile::TempDir,
    addrs: BTreeMap<String, SocketAddr>,
    muxes: BTreeMap<String, Arc<RaftMux>>,
    /// `(node, group)` to its server, None while stopped.
    servers: BTreeMap<(String, u32), Option<RaftServer>>,
}

impl Cluster {
    fn new(n: usize) -> Self {
        let td = tempfile::tempdir().unwrap();
        let mut c = Self {
            td,
            addrs: BTreeMap::new(),
            muxes: BTreeMap::new(),
            servers: BTreeMap::new(),
        };
        for i in 1..=n {
            let id = format!("m{i}");
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            c.addrs.insert(id.clone(), l.local_addr().unwrap());
            c.muxes.insert(id, RaftMux::start(l, None).unwrap());
        }
        for id in c.addrs.keys().cloned().collect::<Vec<_>>() {
            for g in 0..GROUPS {
                c.start(&id, g);
            }
        }
        c
    }

    fn start(&mut self, id: &str, group: u32) {
        let peers: BTreeMap<String, SocketAddr> = self
            .addrs
            .iter()
            .filter(|(p, _)| *p != id)
            .map(|(p, a)| (p.clone(), *a))
            .collect();
        let root = self.td.path().join(id).join(format!("g{group}"));
        let cfg = RaftConfig::new(id, peers.keys().cloned().collect(), root);
        let s = RaftServer::start_in(&self.muxes[id], group, cfg, peers, TICK, None).unwrap();
        self.servers.insert((id.to_string(), group), Some(s));
    }

    fn stop(&mut self, id: &str, group: u32) {
        if let Some(Some(mut s)) = self.servers.insert((id.to_string(), group), None) {
            s.shutdown();
        }
    }

    fn live(&self, group: u32) -> impl Iterator<Item = (&String, &RaftServer)> {
        self.servers
            .iter()
            .filter(move |((_, g), _)| *g == group)
            .filter_map(|((id, _), s)| s.as_ref().map(|s| (id, s)))
    }

    fn wait_leader(&self, group: u32) -> String {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let leaders: Vec<&String> = self
                .live(group)
                .filter(|(_, s)| s.status().unwrap().role == Role::Leader)
                .map(|(id, _)| id)
                .collect();
            if leaders.len() == 1 {
                return leaders[0].clone();
            }
            thread::sleep(TICK);
        }
        panic!("group {group}: no single leader within {WAIT:?}");
    }

    fn propose(&self, group: u32, name: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let l = self.wait_leader(group);
            let s = self.servers[&(l, group)].as_ref().unwrap();
            match s.propose(create(name), Duration::from_secs(5)) {
                Ok(_) => return,
                Err(RaftError::NotLeader { .. } | RaftError::LeadershipLost { .. })
                    if Instant::now() < deadline =>
                {
                    thread::sleep(TICK)
                }
                Err(e) => panic!("group {group}: propose failed: {e}"),
            }
        }
    }

    /// Waits until every live replica of `group` holds exactly the volumes `names`.
    fn wait_volumes(&self, group: u32, names: &[&str]) {
        let want: Vec<String> = names.iter().map(|n| format!("vol-{n}")).collect();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.live(group).all(|(_, s)| {
                s.with_catalog(|c| c.volumes.keys().cloned().collect::<Vec<_>>())
                    .unwrap()
                    == want
            }) {
                return;
            }
            thread::sleep(TICK);
        }
        panic!("group {group}: replicas did not converge on {names:?}");
    }
}

fn create(name: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: format!("vol-{name}"),
        name: name.into(),
        size_bytes: 4096,
    }
}

#[test]
fn groups_on_one_listener_replicate_independently() {
    let c = Cluster::new(3);
    for g in 0..GROUPS {
        c.propose(g, &format!("g{g}"));
    }
    for g in 0..GROUPS {
        c.wait_volumes(g, &[&format!("g{g}")]);
    }
    for (id, s) in c.live(1) {
        assert_eq!(s.group(), 1);
        assert_eq!(s.local_addr(), c.addrs[id]);
        let m = s.render_metrics().unwrap();
        assert!(
            m.contains(&format!(
                "atlas_native_raft_term{{node=\"{id}\",group=\"1\"}} "
            )),
            "{m}"
        );
    }
}

#[test]
fn a_stopped_group_fails_over_while_the_others_keep_their_leaders() {
    let mut c = Cluster::new(3);
    for g in 0..GROUPS {
        c.propose(g, &format!("a{g}"));
    }
    let l1 = c.wait_leader(1);
    let terms: Vec<u64> = [0, 2]
        .iter()
        .map(|g| {
            let l = c.wait_leader(*g);
            c.servers[&(l, *g)].as_ref().unwrap().status().unwrap().term
        })
        .collect();

    // Group 1 loses its leader; the node's listener and its other groups keep running.
    c.stop(&l1, 1);
    c.propose(1, "b1");
    assert_ne!(c.wait_leader(1), l1);
    for (i, g) in [0u32, 2].iter().enumerate() {
        c.propose(*g, &format!("b{g}"));
        let l = c.wait_leader(*g);
        let term = c.servers[&(l, *g)].as_ref().unwrap().status().unwrap().term;
        assert_eq!(
            term, terms[i],
            "group {g} re-elected when group 1 failed over"
        );
    }

    // Back on the same listener, the stopped replica catches up.
    c.start(&l1, 1);
    c.wait_volumes(1, &["a1", "b1"]);
    c.wait_volumes(0, &["a0", "b0"]);
    c.wait_volumes(2, &["a2", "b2"]);
}

#[test]
fn a_group_runs_once_per_listener() {
    let td = tempfile::tempdir().unwrap();
    let mux = RaftMux::start(TcpListener::bind("127.0.0.1:0").unwrap(), None).unwrap();
    let start = |g: u32, dir: &str| {
        RaftServer::start_in(
            &mux,
            g,
            RaftConfig::new("m1", vec![], td.path().join(dir)),
            BTreeMap::<String, String>::new(),
            TICK,
            None,
        )
    };
    let _a = start(4, "a").unwrap();
    assert!(matches!(start(4, "b"), Err(RaftError::Config(_))));
    let mut c = start(5, "c").unwrap();
    c.shutdown();
    // A stopped group's number is free again.
    let _d = start(5, "d").unwrap();
}
