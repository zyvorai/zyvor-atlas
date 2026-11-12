// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Minimal Raft for the Atlas Native metadata log: leader election, log replication, quorum
//! commit, log compaction and snapshot installation. It replicates `MetaCommand` records and
//! applies them through the same `Catalog` state machine the single-node engine uses.
//!
//! The node is sans-IO: the caller drives [`RaftNode::tick`], feeds inbound messages to
//! [`RaftNode::step`] and delivers whatever [`RaftNode::take_messages`] returns. Transport is the
//! caller's concern. Durability is not: the vote is fsynced before any reply, log entries are
//! fsynced before they are acknowledged or counted toward the leader's own quorum vote, and
//! the catalog is checkpointed to its store before the log is compacted past it.
//!
//! Pre-vote keeps a partitioned node from inflating its term: it only starts a real election after
//! a majority confirms it could win. Check-quorum makes a leader that has not heard from a
//! majority within an election timeout step down, and lets nodes with a live leader ignore
//! disruptive vote requests.
//!
//! Membership changes use joint consensus ([`RaftNode::change_membership`]): a `Joint` config
//! entry needs majorities of both the old and new voter sets; once it commits the leader appends
//! the `Stable` new config, and steps down if it is not part of it. Until that stable entry
//! commits, replication still includes the voters it replaces, so a removed voter receives the
//! config that drops it. A config entry takes effect on every node as soon as it is in its log
//! (and reverts if that entry is truncated). A node outside the current voter set never
//! campaigns, so a new node can start with an empty log and wait to be added.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    durable,
    membership::Membership,
    metadata::{Catalog, MetaCommand, MetaError},
    raft_snapshot::{remove_staging, Incoming, Outgoing, SnapshotChunk},
    store::{
        load_checkpoint, remove_legacy_catalog, CatalogStore, CATALOG_STORE, DEFAULT_CACHE_INODES,
        DEFAULT_STORE_CACHE_BYTES,
    },
    wal::{Wal, WalError, WalRecord},
};

pub type NodeId = String;
pub type Entry = WalRecord<MetaCommand>;

#[derive(Debug, thiserror::Error)]
pub enum RaftError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wal error: {0}")]
    Wal(#[from] WalError),
    #[error("not the leader (leader hint: {leader:?})")]
    NotLeader { leader: Option<NodeId> },
    #[error("rejected by the metadata state machine: {0}")]
    Rejected(#[from] MetaError),
    #[error("invalid raft config: {0}")]
    Config(String),
    #[error("raft log inconsistency: {0}")]
    Inconsistent(String),
    #[error("leadership lost before index {index} applied; it may or may not commit")]
    LeadershipLost { index: u64 },
    #[error("timed out waiting for index {index} to apply")]
    Timeout { index: u64 },
    #[error("raft server is shut down")]
    Shutdown,
    #[error("term changed from {expected} to {current} before the proposal")]
    TermChanged { expected: u64, current: u64 },
    #[error("a membership change is already in progress, or this leader has not committed an entry in its term yet")]
    MembershipBusy,
}

#[derive(Debug, Clone)]
pub struct RaftConfig {
    pub id: NodeId,
    /// The other nodes this one knows of (excluding `id`). Unless `bootstrap` says otherwise,
    /// they and `id` are the initial voters.
    pub peers: Vec<NodeId>,
    /// Initial voter set, used until the log carries a membership change. A node not listed
    /// starts as a non-voter that waits to be added. `None`: `peers` plus `id`.
    pub bootstrap: Option<Vec<NodeId>>,
    pub root: PathBuf,
    /// Election timeout is drawn uniformly from `[min, max)` ticks.
    pub election_ticks: (u64, u64),
    pub heartbeat_ticks: u64,
    pub max_batch: usize,
    /// Compact the log once twice this many applied entries sit above the last compaction point,
    /// keeping the newest `compact_after` so a follower that is a little behind catches up through
    /// AppendEntries rather than a full snapshot (0 disables compaction).
    pub compact_after: u64,
    /// Unchanged inodes the catalog keeps in memory; the rest stay in the catalog store.
    pub catalog_cache_inodes: usize,
    /// The catalog store's page cache.
    pub catalog_cache_bytes: usize,
}

impl RaftConfig {
    pub fn new(id: impl Into<NodeId>, peers: Vec<NodeId>, root: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            peers,
            bootstrap: None,
            root: root.into(),
            election_ticks: (10, 20),
            heartbeat_ticks: 3,
            max_batch: 64,
            compact_after: 1024,
            catalog_cache_inodes: DEFAULT_CACHE_INODES,
            catalog_cache_bytes: DEFAULT_STORE_CACHE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    /// Collecting pre-votes; has not incremented its term.
    PreCandidate,
    Candidate,
    Leader,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// `term` is the term the sender would campaign in (its current term + 1). Never changes the
    /// receiver's term.
    PreVote {
        term: u64,
        last_log_index: u64,
        last_log_term: u64,
    },
    /// When granted, `term` echoes the proposed term; otherwise it is the responder's term.
    PreVoteResponse {
        term: u64,
        granted: bool,
    },
    RequestVote {
        term: u64,
        last_log_index: u64,
        last_log_term: u64,
    },
    RequestVoteResponse {
        term: u64,
        granted: bool,
    },
    AppendEntries {
        term: u64,
        prev_log_index: u64,
        prev_log_term: u64,
        entries: Vec<Entry>,
        leader_commit: u64,
        /// The leader's read round when it sent this; echoed back, it confirms the leadership
        /// that read barriers registered up to that round depend on.
        #[serde(default, skip_serializing_if = "is_zero")]
        read_seq: u64,
    },
    /// On success `match_index` is the last index known to match the leader; on failure it is a
    /// hint for where the leader should retry.
    AppendEntriesResponse {
        term: u64,
        success: bool,
        match_index: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        read_seq: u64,
    },
    /// Chunk `seq` of the leader's applied catalog as of `index` (see [`crate::raft_snapshot`]);
    /// the header's `applied_index`/`current_term` are the snapshot's last included index/term.
    InstallSnapshot {
        term: u64,
        index: u64,
        seq: u64,
        #[serde(with = "embedded_json")]
        chunk: Box<SnapshotChunk>,
    },
    /// `match_index` is the follower's commit index: committed entries are the only ones
    /// guaranteed to match the leader (a kept log suffix may still hold stale entries). `ack` is
    /// the chunk staged; `None` asks the leader to start the transfer over.
    InstallSnapshotResponse {
        term: u64,
        match_index: u64,
        #[serde(default)]
        ack: Option<u64>,
    },
    /// A follower asks the leader for a read barrier: the leader's commit index, once a quorum
    /// has confirmed it is still the leader.
    ReadIndex {
        term: u64,
        id: u64,
    },
    /// `index` is None when the responder could not serve the barrier (not the leader, or not
    /// yet committed an entry in its term); the follower asks again.
    ReadIndexResponse {
        term: u64,
        id: u64,
        index: Option<u64>,
    },
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl Message {
    /// Whether this answers a request (votes, appends, snapshots) rather than asking.
    pub fn is_response(&self) -> bool {
        matches!(
            self,
            Message::PreVoteResponse { .. }
                | Message::RequestVoteResponse { .. }
                | Message::AppendEntriesResponse { .. }
                | Message::InstallSnapshotResponse { .. }
                | Message::ReadIndexResponse { .. }
        )
    }

    pub fn term(&self) -> u64 {
        match self {
            Message::PreVote { term, .. }
            | Message::PreVoteResponse { term, .. }
            | Message::RequestVote { term, .. }
            | Message::RequestVoteResponse { term, .. }
            | Message::AppendEntries { term, .. }
            | Message::AppendEntriesResponse { term, .. }
            | Message::InstallSnapshot { term, .. }
            | Message::InstallSnapshotResponse { term, .. }
            | Message::ReadIndex { term, .. }
            | Message::ReadIndexResponse { term, .. } => *term,
        }
    }
}

/// `Message` is internally tagged, so serde buffers each variant before decoding it, and buffered
/// integer map keys (`VolumeMeta::extents`) do not decode. Snapshot chunks therefore travel as an
/// embedded JSON string.
mod embedded_json {
    use serde::{
        de::{DeserializeOwned, Error as _},
        ser::Error as _,
        Deserialize, Deserializer, Serialize, Serializer,
    };

    pub fn serialize<T: Serialize, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&serde_json::to_string(v).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, T: DeserializeOwned, D: Deserializer<'de>>(
        d: D,
    ) -> Result<T, D::Error> {
        serde_json::from_str(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub from: NodeId,
    pub to: NodeId,
    /// The Raft group the message belongs to; several share one transport listener.
    #[serde(default, skip_serializing_if = "is_group_zero")]
    pub group: u32,
    pub msg: Message,
}

fn is_group_zero(g: &u32) -> bool {
    *g == 0
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct HardState {
    term: u64,
    voted_for: Option<NodeId>,
}

/// Monotonic event counters, exported as Prometheus counters by `RaftServer`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RaftCounters {
    /// Real elections started (after a successful pre-vote round).
    pub elections: u64,
    /// Times this node became leader.
    pub leader_terms: u64,
    /// `AppendEntries` rejections received while leader (log-consistency backoffs).
    pub append_rejections: u64,
}

/// A read barrier the leader is confirming: `index` is its commit index when the barrier was
/// registered in read round `seq`.
#[derive(Debug)]
struct PendingRead {
    seq: u64,
    index: u64,
    /// The follower that asked, or None for this node.
    from: Option<NodeId>,
    id: u64,
    acks: BTreeSet<NodeId>,
}

#[derive(Debug)]
pub struct RaftNode {
    cfg: RaftConfig,
    /// The latest config in the log, else the applied catalog's, else `initial`.
    membership: Membership,
    initial: Membership,
    counters: RaftCounters,
    role: Role,
    hard: HardState,
    leader: Option<NodeId>,
    /// Contiguous entries with `index > snapshot_index`.
    log: Vec<Entry>,
    snapshot_index: u64,
    snapshot_term: u64,
    commit_index: u64,
    /// Applied state machine; `catalog.applied_index` is the applied index.
    catalog: Catalog,
    /// Leader only: `catalog` plus every logged entry, kept up to date as entries are appended,
    /// so a proposal is validated without copying the catalog. Built on first use in a term.
    spec: Option<Catalog>,
    wal: Wal,
    store: CatalogStore,
    votes: BTreeSet<NodeId>,
    next_index: BTreeMap<NodeId, u64>,
    match_index: BTreeMap<NodeId, u64>,
    /// Peers that answered the leader since the last check-quorum round.
    recent_active: BTreeSet<NodeId>,
    check_quorum_elapsed: u64,
    election_elapsed: u64,
    election_timeout: u64,
    heartbeat_elapsed: u64,
    /// Ticks since start; times snapshot sends.
    ticks: u64,
    /// Leader only: snapshot transfers in progress. A chunk in flight is re-sent only after an
    /// election timeout without an answer.
    outgoing: BTreeMap<NodeId, Outgoing>,
    /// Follower only: a snapshot transfer being staged.
    incoming: Option<Incoming>,
    rng: u64,
    outbox: Vec<Envelope>,
    /// Leader only: the read round, raised whenever a barrier is registered.
    read_seq: u64,
    pending_reads: Vec<PendingRead>,
    next_read_id: u64,
    /// Barriers this node asked for: id to the index to wait for (None: ask again).
    read_results: BTreeMap<u64, Option<u64>>,
}

impl RaftNode {
    pub fn open(cfg: RaftConfig) -> Result<Self, RaftError> {
        let peers: BTreeSet<_> = cfg.peers.iter().collect();
        if peers.len() != cfg.peers.len() || peers.contains(&cfg.id) {
            return Err(RaftError::Config(
                "peers must be unique and must not include the node itself".into(),
            ));
        }
        let (lo, hi) = cfg.election_ticks;
        if lo == 0 || hi <= lo || cfg.heartbeat_ticks == 0 || cfg.heartbeat_ticks >= lo {
            return Err(RaftError::Config(
                "need 0 < heartbeat_ticks < election_ticks.0 < election_ticks.1".into(),
            ));
        }
        fs::create_dir_all(&cfg.root)?;

        let hard_path = cfg.root.join("raft_state.json");
        let mut hard: HardState = if hard_path.exists() {
            serde_json::from_slice(&fs::read(&hard_path)?)?
        } else {
            HardState::default()
        };
        let store =
            CatalogStore::open_with_cache(cfg.root.join(CATALOG_STORE), cfg.catalog_cache_bytes)?
                .with_cache_inodes(cfg.catalog_cache_inodes);
        let mut catalog = load_checkpoint(&cfg.root, &store)?;
        if !catalog.in_store {
            store.checkpoint(&mut catalog)?;
        }
        remove_legacy_catalog(&cfg.root)?;
        remove_staging(&cfg.root)?;
        let applied = catalog.applied_index;

        let mut wal = Wal::open(cfg.root.join("wal"))?;
        let log: Vec<Entry> = wal
            .replay::<MetaCommand>()?
            .into_iter()
            .filter(|e| e.index > applied)
            .collect();
        for (i, e) in log.iter().enumerate() {
            if e.index != applied + 1 + i as u64 {
                return Err(RaftError::Inconsistent(format!(
                    "log gap: expected index {}, found {}",
                    applied + 1 + i as u64,
                    e.index
                )));
            }
        }
        wal.compact_through(applied)?;
        wal.raise_floor(applied);
        hard.term = hard.term.max(catalog.current_term);

        let mut rng = 0xcbf2_9ce4_8422_2325u64;
        for b in cfg.id.bytes() {
            rng = (rng ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        let initial = match &cfg.bootstrap {
            Some(b) => {
                if b.is_empty() || b.iter().collect::<BTreeSet<_>>().len() != b.len() {
                    return Err(RaftError::Config(
                        "bootstrap voters must be non-empty and unique".into(),
                    ));
                }
                Membership::stable(b.iter().cloned())
            }
            None => Membership::stable(cfg.peers.iter().cloned().chain([cfg.id.clone()])),
        };
        let mut node = Self {
            membership: initial.clone(),
            initial,
            counters: RaftCounters::default(),
            role: Role::Follower,
            hard,
            leader: None,
            log,
            snapshot_index: applied,
            snapshot_term: catalog.current_term,
            commit_index: applied,
            catalog,
            spec: None,
            wal,
            store,
            votes: BTreeSet::new(),
            next_index: BTreeMap::new(),
            match_index: BTreeMap::new(),
            recent_active: BTreeSet::new(),
            check_quorum_elapsed: 0,
            election_elapsed: 0,
            election_timeout: 0,
            heartbeat_elapsed: 0,
            ticks: 0,
            outgoing: BTreeMap::new(),
            incoming: None,
            rng: rng | 1,
            outbox: Vec::new(),
            read_seq: 0,
            pending_reads: Vec::new(),
            next_read_id: 1,
            read_results: BTreeMap::new(),
            cfg,
        };
        node.refresh_membership();
        node.reset_election_timer();
        Ok(node)
    }

    pub fn id(&self) -> &str {
        &self.cfg.id
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    pub fn leader(&self) -> Option<&str> {
        self.leader.as_deref()
    }
    pub fn term(&self) -> u64 {
        self.hard.term
    }
    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }
    pub fn applied_index(&self) -> u64 {
        self.catalog.applied_index
    }
    pub fn snapshot_index(&self) -> u64 {
        self.snapshot_index
    }
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
    pub fn last_index(&self) -> u64 {
        self.log
            .last()
            .map(|e| e.index)
            .unwrap_or(self.snapshot_index)
    }
    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    /// Whether this node is in the current voter set.
    pub fn is_voter(&self) -> bool {
        self.membership.voters().contains(&self.cfg.id)
    }

    /// Transport addresses learned from membership changes (applied, then any in the log).
    pub fn peer_addrs(&self) -> BTreeMap<NodeId, String> {
        let mut addrs = self.catalog.raft_addrs.clone();
        for e in &self.log {
            if let MetaCommand::ChangeMembership { addrs: a, .. } = &e.command {
                addrs.extend(a.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }
        addrs
    }

    /// Index of the newest membership entry still in the log, 0 if there is none.
    pub fn latest_config_index(&self) -> u64 {
        self.config_index().unwrap_or(0)
    }

    /// Index of the newest membership entry still in the log (None if only the catalog's).
    fn config_index(&self) -> Option<u64> {
        self.log
            .iter()
            .rev()
            .find(|e| matches!(e.command, MetaCommand::ChangeMembership { .. }))
            .map(|e| e.index)
    }

    /// Every voter but this node: who to replicate to and ask for votes.
    ///
    /// While the newest config entry is still uncommitted, also includes voters of the config
    /// it replaces. A removed voter is not part of the new quorum, but it must receive the
    /// stable entry so it stops campaigning.
    fn peers(&self) -> Vec<NodeId> {
        let mut ids = self.membership.voters();
        if self.config_index().is_some_and(|i| i > self.commit_index) {
            ids.extend(self.previous_config_voters());
        }
        ids.into_iter().filter(|p| p != &self.cfg.id).collect()
    }

    /// Voters of the membership entry before the newest one in the log, else the applied catalog.
    fn previous_config_voters(&self) -> BTreeSet<NodeId> {
        let mut skipped_newest = false;
        for e in self.log.iter().rev() {
            if let MetaCommand::ChangeMembership { membership, .. } = &e.command {
                if !skipped_newest {
                    skipped_newest = true;
                    continue;
                }
                return membership.voters();
            }
        }
        self.catalog
            .membership
            .clone()
            .unwrap_or_else(|| self.initial.clone())
            .voters()
    }

    fn refresh_membership(&mut self) {
        let latest = self.log.iter().rev().find_map(|e| match &e.command {
            MetaCommand::ChangeMembership { membership, .. } => Some(membership.clone()),
            _ => None,
        });
        self.membership = latest
            .or_else(|| self.catalog.membership.clone())
            .unwrap_or_else(|| self.initial.clone());
        if self.role == Role::Leader {
            let peers = self.peers();
            let next = self.last_index() + 1;
            self.next_index.retain(|p, _| peers.contains(p));
            self.match_index.retain(|p, _| peers.contains(p));
            for p in peers {
                self.next_index.entry(p.clone()).or_insert(next);
                self.match_index.entry(p).or_insert(0);
            }
        }
    }

    /// Starts moving the voter set to `voters` (joint consensus); `addrs` supplies transport
    /// addresses for nodes not yet known. Returns the index of the `Joint` entry; the change is
    /// complete once the follow-up `Stable` entry applies (see [`Self::membership`]). Refused
    /// while another change is in flight or before this leader has committed an entry in its term.
    pub fn change_membership(
        &mut self,
        voters: BTreeSet<NodeId>,
        addrs: BTreeMap<NodeId, String>,
    ) -> Result<u64, RaftError> {
        if self.role != Role::Leader {
            return Err(RaftError::NotLeader {
                leader: self.leader.clone(),
            });
        }
        if voters.is_empty() {
            return Err(RaftError::Config("the new voter set is empty".into()));
        }
        let Membership::Stable { voters: old } = self.membership.clone() else {
            return Err(RaftError::MembershipBusy);
        };
        if self.config_index().is_some_and(|i| i > self.commit_index)
            || self.term_at(self.commit_index) != Some(self.hard.term)
        {
            return Err(RaftError::MembershipBusy);
        }
        if old == voters {
            return Ok(self.commit_index);
        }
        let known = self.peer_addrs();
        if let Some(missing) = voters.iter().find(|v| {
            **v != self.cfg.id
                && !addrs.contains_key(*v)
                && !known.contains_key(*v)
                && !self.cfg.peers.contains(*v)
        }) {
            return Err(RaftError::Config(format!(
                "no address for new voter {missing}"
            )));
        }
        let index = self.append_validated(MetaCommand::ChangeMembership {
            membership: Membership::Joint { old, new: voters },
            addrs,
        })?;
        self.broadcast_append();
        self.advance_commit()?;
        Ok(index)
    }

    /// Adds `id` through joint consensus ([`Membership::add_voter_steps`]). `addr` is the Raft
    /// `host:port` of a voter that is not this node. No-op when `id` is already a voter.
    pub fn add_voter(&mut self, id: NodeId, addr: impl Into<String>) -> Result<u64, RaftError> {
        let Membership::Stable { voters } = self.membership.clone() else {
            return Err(RaftError::MembershipBusy);
        };
        let Some([joint, _]) = Membership::add_voter_steps(&voters, &id) else {
            return Ok(self.commit_index);
        };
        let addr = addr.into();
        if id != self.cfg.id && addr.is_empty() {
            return Err(RaftError::Config(format!("no address for new voter {id}")));
        }
        let mut addrs = BTreeMap::new();
        if id != self.cfg.id {
            addrs.insert(id, addr);
        }
        self.change_membership(joint.target().clone(), addrs)
    }

    /// Removes `id` through joint consensus ([`Membership::remove_voter_steps`]). No-op when
    /// `id` is not a voter. Refuses to remove the last voter.
    pub fn remove_voter(&mut self, id: &NodeId) -> Result<u64, RaftError> {
        let Membership::Stable { voters } = self.membership.clone() else {
            return Err(RaftError::MembershipBusy);
        };
        let steps = Membership::remove_voter_steps(&voters, id)
            .map_err(|m| RaftError::Config(m.into()))?;
        let Some([joint, _]) = steps else {
            return Ok(self.commit_index);
        };
        self.change_membership(joint.target().clone(), BTreeMap::new())
    }

    /// Leader only: once a `Joint` config commits, append the `Stable` one; once a `Stable`
    /// config that excludes this leader commits, step down.
    fn finish_membership_change(&mut self) -> Result<(), RaftError> {
        if self.role != Role::Leader || self.config_index().is_some_and(|i| i > self.commit_index) {
            return Ok(());
        }
        match self.membership.clone() {
            Membership::Joint { new, .. } => {
                self.append_validated(MetaCommand::ChangeMembership {
                    membership: Membership::Stable { voters: new },
                    addrs: BTreeMap::new(),
                })?;
                self.broadcast_append();
            }
            Membership::Stable { voters } if !voters.contains(&self.cfg.id) => {
                self.become_follower(self.hard.term, None)?;
            }
            Membership::Stable { .. } => {}
        }
        Ok(())
    }
    /// Starts a read barrier and returns its id. Once [`Self::read_result`] yields an index,
    /// every entry committed before this call is at or below it, so a catalog applied through
    /// that index reflects every write acknowledged before the call (a linearizable read).
    /// The leader confirms its leadership with a quorum first; a follower asks the leader.
    pub fn read_index(&mut self) -> Result<u64, RaftError> {
        let id = self.next_read_id;
        self.next_read_id += 1;
        if self.role == Role::Leader {
            if !self.register_read(None, id) {
                return Err(RaftError::NotLeader {
                    leader: self.leader.clone(),
                });
            }
        } else {
            let Some(leader) = self.leader.clone() else {
                return Err(RaftError::NotLeader { leader: None });
            };
            let term = self.hard.term;
            self.send(leader, Message::ReadIndex { term, id });
        }
        Ok(id)
    }

    /// The outcome of barrier `id`: None while pending, `Some(None)` if the leader turned it
    /// down (ask again), `Some(Some(index))` once confirmed. Taking a result forgets it.
    pub fn read_result(&mut self, id: u64) -> Option<Option<u64>> {
        self.read_results.remove(&id)
    }

    /// Forgets barrier `id` (the caller gave up on it).
    pub fn forget_read(&mut self, id: u64) {
        self.read_results.remove(&id);
        self.pending_reads
            .retain(|r| !(r.from.is_none() && r.id == id));
    }

    /// Leader only: registers a barrier at the commit index, or returns false if this leader
    /// has not committed an entry in its term yet (its commit index may still trail a
    /// predecessor's).
    fn register_read(&mut self, from: Option<NodeId>, id: u64) -> bool {
        if self.term_at(self.commit_index) != Some(self.hard.term) {
            return false;
        }
        self.read_seq += 1;
        self.pending_reads.push(PendingRead {
            seq: self.read_seq,
            index: self.commit_index,
            from,
            id,
            acks: BTreeSet::from([self.cfg.id.clone()]),
        });
        // A single-voter group confirms at once; otherwise the next round of appends does.
        self.confirm_reads(None, 0);
        if !self.pending_reads.is_empty() {
            self.broadcast_append();
        }
        true
    }

    /// Counts `from`'s answer to read round `seq` toward every barrier registered by then and
    /// completes those a quorum has confirmed.
    fn confirm_reads(&mut self, from: Option<&NodeId>, seq: u64) {
        let mut done = Vec::new();
        self.pending_reads.retain_mut(|r| {
            if let Some(f) = from.filter(|_| seq >= r.seq) {
                r.acks.insert(f.clone());
            }
            if self.membership.has_quorum(&r.acks) {
                done.push((r.from.take(), r.id, r.index));
                false
            } else {
                true
            }
        });
        for (from, id, index) in done {
            self.answer_read(from, id, Some(index));
        }
    }

    fn answer_read(&mut self, from: Option<NodeId>, id: u64, index: Option<u64>) {
        match from {
            None => {
                self.read_results.insert(id, index);
            }
            Some(f) => {
                let term = self.hard.term;
                self.send(f, Message::ReadIndexResponse { term, id, index });
            }
        }
    }

    pub fn counters(&self) -> RaftCounters {
        self.counters
    }
    /// Highest log index known to be replicated on each peer; empty unless leader.
    pub fn peer_match_index(&self) -> BTreeMap<NodeId, u64> {
        if self.role == Role::Leader {
            self.match_index.clone()
        } else {
            BTreeMap::new()
        }
    }
    fn last_term(&self) -> u64 {
        self.log
            .last()
            .map(|e| e.term)
            .unwrap_or(self.snapshot_term)
    }

    /// The messages to send. A non-leader's log is made durable first: its outbox may hold
    /// AppendEntries acknowledgements for entries that were only written.
    pub fn take_messages(&mut self) -> Result<Vec<Envelope>, RaftError> {
        if self.role != Role::Leader {
            self.wal.sync()?;
        }
        Ok(std::mem::take(&mut self.outbox))
    }

    pub fn tick(&mut self) -> Result<(), RaftError> {
        self.ticks += 1;
        if self.role == Role::Leader {
            if self.wal.synced_index() < self.last_index() {
                self.wal.sync()?;
                self.advance_commit()?;
            }
            self.check_quorum_elapsed += 1;
            if self.check_quorum_elapsed >= self.cfg.election_ticks.0 {
                self.check_quorum_elapsed = 0;
                let mut active = std::mem::take(&mut self.recent_active);
                active.insert(self.cfg.id.clone());
                if !self.membership.has_quorum(&active) {
                    return self.become_follower(self.hard.term, None);
                }
            }
            self.heartbeat_elapsed += 1;
            if self.heartbeat_elapsed >= self.cfg.heartbeat_ticks {
                self.heartbeat_elapsed = 0;
                self.broadcast_append();
            }
        } else {
            self.election_elapsed += 1;
            if self.election_elapsed >= self.election_timeout {
                self.pre_campaign()?;
            }
        }
        Ok(())
    }

    /// Appends `command` to the replicated log. Returns its index; the command takes effect once
    /// `applied_index() >= index`. Validated against the leader's applied state plus every
    /// uncommitted entry, so a command that cannot apply is rejected here instead of being logged.
    pub fn propose(&mut self, command: MetaCommand) -> Result<u64, RaftError> {
        if self.role != Role::Leader {
            return Err(RaftError::NotLeader {
                leader: self.leader.clone(),
            });
        }
        if matches!(command, MetaCommand::ChangeMembership { .. }) {
            return Err(RaftError::Config(
                "membership changes go through change_membership".into(),
            ));
        }
        let index = self.append_validated(command)?;
        self.broadcast_append();
        self.advance_commit()?;
        Ok(index)
    }

    /// Appends `command` after checking it applies on top of the applied state plus every
    /// uncommitted entry.
    fn append_validated(&mut self, command: MetaCommand) -> Result<u64, RaftError> {
        let (term, index) = (self.hard.term, self.last_index() + 1);
        self.spec_mut().apply(term, index, &command)?;
        self.append_local(command).inspect_err(|_| self.spec = None)
    }

    fn spec_mut(&mut self) -> &mut Catalog {
        if self.spec.is_none() {
            let mut spec = self.catalog.clone();
            let applied = spec.applied_index;
            for e in self.log.iter().filter(|e| e.index > applied) {
                let _ = spec.apply_committed(e.term, e.index, &e.command);
            }
            self.spec = Some(spec);
        }
        self.spec.as_mut().expect("built above")
    }

    pub fn step(&mut self, env: Envelope) -> Result<(), RaftError> {
        let from = env.from;
        let term = env.msg.term();
        match env.msg {
            Message::PreVote { .. } | Message::PreVoteResponse { granted: true, .. } => {}
            Message::RequestVote { .. } if term > self.hard.term && self.leader_active() => {
                // Check-quorum lease: a live leader exists, so this election is disruptive.
                return Ok(());
            }
            _ if term > self.hard.term => {
                let hint = matches!(
                    env.msg,
                    Message::AppendEntries { .. } | Message::InstallSnapshot { .. }
                )
                .then(|| from.clone());
                self.become_follower(term, hint)?;
            }
            _ => {}
        }
        match env.msg {
            Message::PreVote {
                term,
                last_log_index,
                last_log_term,
            } => {
                let granted = term > self.hard.term
                    && self.log_up_to_date(last_log_index, last_log_term)
                    && !self.leader_active();
                self.send(
                    from,
                    Message::PreVoteResponse {
                        term: if granted { term } else { self.hard.term },
                        granted,
                    },
                );
            }
            Message::PreVoteResponse { term, granted } => {
                if self.role == Role::PreCandidate && granted && term == self.hard.term + 1 {
                    self.votes.insert(from);
                    if self.membership.has_quorum(&self.votes) {
                        self.campaign()?;
                    }
                }
            }
            Message::RequestVote {
                term,
                last_log_index,
                last_log_term,
            } => {
                let up_to_date = self.log_up_to_date(last_log_index, last_log_term);
                let can_vote = self.hard.voted_for.as_ref().is_none_or(|v| *v == from);
                let granted = term == self.hard.term && can_vote && up_to_date;
                if granted {
                    self.hard.voted_for = Some(from.clone());
                    self.persist_hard()?;
                    self.reset_election_timer();
                }
                self.send(
                    from,
                    Message::RequestVoteResponse {
                        term: self.hard.term,
                        granted,
                    },
                );
            }
            Message::RequestVoteResponse { term, granted } => {
                if self.role == Role::Candidate && term == self.hard.term && granted {
                    self.votes.insert(from);
                    if self.membership.has_quorum(&self.votes) {
                        self.become_leader()?;
                    }
                }
            }
            Message::AppendEntries {
                term,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                read_seq,
            } => {
                if term < self.hard.term {
                    self.send(
                        from,
                        Message::AppendEntriesResponse {
                            term: self.hard.term,
                            success: false,
                            match_index: 0,
                            read_seq,
                        },
                    );
                    return Ok(());
                }
                if self.role != Role::Follower {
                    self.become_follower(term, Some(from.clone()))?;
                }
                self.leader = Some(from.clone());
                self.reset_election_timer();
                let (success, match_index) =
                    self.handle_append(prev_log_index, prev_log_term, entries, leader_commit)?;
                self.send(
                    from,
                    Message::AppendEntriesResponse {
                        term: self.hard.term,
                        success,
                        match_index,
                        read_seq,
                    },
                );
            }
            Message::AppendEntriesResponse {
                term,
                success,
                match_index,
                read_seq,
            } => {
                if self.role != Role::Leader
                    || term != self.hard.term
                    || !self.next_index.contains_key(&from)
                {
                    return Ok(());
                }
                self.recent_active.insert(from.clone());
                if read_seq > 0 {
                    self.confirm_reads(Some(&from), read_seq);
                }
                if success {
                    let m = self.match_index.entry(from.clone()).or_insert(0);
                    *m = (*m).max(match_index);
                    // next_index already moved past entries in flight; an ack never moves it back.
                    let next = (*m + 1).max(self.next_index.get(&from).copied().unwrap_or(0));
                    self.next_index.insert(from.clone(), next);
                    // Covers every proposal appended since the last ack with one fsync.
                    self.wal.sync()?;
                    self.advance_commit()?;
                    if next <= self.last_index() {
                        self.send_append(&from);
                    }
                } else {
                    self.counters.append_rejections += 1;
                    let cur = self.next_index.get(&from).copied().unwrap_or(1);
                    let next = cur.saturating_sub(1).min(match_index + 1).max(1);
                    self.next_index.insert(from.clone(), next);
                    self.send_append(&from);
                }
            }
            Message::InstallSnapshot {
                term,
                index,
                seq,
                chunk,
            } => {
                let mut ack = None;
                if term >= self.hard.term {
                    if self.role != Role::Follower {
                        self.become_follower(term, Some(from.clone()))?;
                    }
                    self.leader = Some(from.clone());
                    self.reset_election_timer();
                    ack = self.handle_snapshot_chunk(index, seq, *chunk)?;
                }
                self.send(
                    from,
                    Message::InstallSnapshotResponse {
                        term: self.hard.term,
                        match_index: self.commit_index,
                        ack,
                    },
                );
            }
            Message::InstallSnapshotResponse {
                term,
                match_index,
                ack,
            } => {
                if self.role != Role::Leader
                    || term != self.hard.term
                    || !self.next_index.contains_key(&from)
                {
                    return Ok(());
                }
                self.recent_active.insert(from.clone());
                if let Some(o) = self.outgoing.get_mut(&from) {
                    if match_index < o.index {
                        match ack {
                            Some(seq) if seq == o.seq && !o.is_done() => {
                                o.advance(self.ticks).map_err(std::io::Error::other)?;
                                let msg = snapshot_message(self.hard.term, o);
                                self.send(from, msg);
                            }
                            Some(_) => {}
                            None => {
                                let o = Outgoing::start(self.catalog.clone(), self.ticks);
                                self.send(from.clone(), snapshot_message(self.hard.term, &o));
                                self.outgoing.insert(from, o);
                            }
                        }
                        return Ok(());
                    }
                }
                self.outgoing.remove(&from);
                let last = self.last_index();
                let m = self.match_index.entry(from.clone()).or_insert(0);
                *m = (*m).max(match_index.min(last));
                let next = *m + 1;
                self.next_index.insert(from.clone(), next);
                self.advance_commit()?;
                if next <= self.last_index() {
                    self.send_append(&from);
                }
            }
            Message::ReadIndex { term, id } => {
                if term != self.hard.term
                    || self.role != Role::Leader
                    || !self.register_read(Some(from.clone()), id)
                {
                    let term = self.hard.term;
                    self.send(
                        from,
                        Message::ReadIndexResponse {
                            term,
                            id,
                            index: None,
                        },
                    );
                }
            }
            Message::ReadIndexResponse { id, index, .. } => {
                if self.role != Role::Leader {
                    self.read_results.insert(id, index);
                }
            }
        }
        Ok(())
    }

    fn handle_append(
        &mut self,
        mut prev_index: u64,
        mut prev_term: u64,
        mut entries: Vec<Entry>,
        leader_commit: u64,
    ) -> Result<(bool, u64), RaftError> {
        if prev_index < self.snapshot_index {
            // Everything up to snapshot_index is committed and therefore identical to the leader.
            let skip = (self.snapshot_index - prev_index) as usize;
            if skip >= entries.len() {
                return Ok((true, self.snapshot_index));
            }
            entries.drain(..skip);
            prev_index = self.snapshot_index;
            prev_term = self.snapshot_term;
        }
        match self.term_at(prev_index) {
            None => return Ok((false, self.last_index())),
            Some(t) if t != prev_term => {
                let mut hint = prev_index - 1;
                while hint > self.snapshot_index && self.term_at(hint) == Some(t) {
                    hint -= 1;
                }
                return Ok((false, hint));
            }
            Some(_) => {}
        }
        let match_index = prev_index + entries.len() as u64;
        for e in entries {
            match self.term_at(e.index) {
                Some(t) if t == e.term => continue,
                Some(_) => {
                    if e.index <= self.commit_index {
                        return Err(RaftError::Inconsistent(format!(
                            "leader overwrote committed index {}",
                            e.index
                        )));
                    }
                    self.wal.truncate_after(e.index - 1)?;
                    self.log
                        .truncate((e.index - self.snapshot_index - 1) as usize);
                }
                None => {}
            }
            // Synced by `take_messages` before the acknowledgement leaves this node, so a burst of
            // AppendEntries stepped together shares one fsync.
            self.wal.append_unsynced(&e)?;
            self.log.push(e);
        }
        self.refresh_membership();
        if leader_commit > self.commit_index {
            self.commit_index = leader_commit.min(match_index).max(self.commit_index);
            self.apply_committed()?;
        }
        Ok((true, match_index))
    }

    /// Stages chunk `seq` of the snapshot as of `index`; returns the chunk to acknowledge, or
    /// `None` to have the leader start over.
    fn handle_snapshot_chunk(
        &mut self,
        index: u64,
        seq: u64,
        chunk: SnapshotChunk,
    ) -> Result<Option<u64>, RaftError> {
        if index <= self.commit_index {
            // Nothing to install; the response's match_index ends the transfer.
            self.incoming = None;
            return Ok(Some(seq));
        }
        if let SnapshotChunk::Header { catalog } = chunk {
            self.incoming = None;
            if seq != 0 {
                return Ok(None);
            }
            self.incoming = Some(Incoming::start(&self.cfg.root, index, *catalog)?);
            return Ok(Some(0));
        }
        let Some(inc) = self.incoming.as_mut().filter(|i| i.index == index) else {
            return Ok(None);
        };
        if seq == inc.seq {
            return Ok(Some(seq));
        }
        if seq != inc.seq + 1 {
            return Ok(None);
        }
        if matches!(chunk, SnapshotChunk::Done) {
            let inc = self.incoming.take().expect("matched above");
            self.install_staged(inc)?;
        } else {
            inc.stage(chunk)?;
            inc.seq = seq;
        }
        Ok(Some(seq))
    }

    /// Swaps a fully staged snapshot in for the catalog store and adopts it.
    fn install_staged(&mut self, inc: Incoming) -> Result<(), RaftError> {
        let staged = inc.finish();
        // Paged tables read the store being replaced; nothing may outlive the swap.
        self.spec = None;
        self.catalog = Catalog::default();
        let path = self.cfg.root.join(CATALOG_STORE);
        fs::rename(&staged, &path)?;
        durable::sync_dir(&self.cfg.root)?;
        self.store = CatalogStore::open_with_cache(&path, self.cfg.catalog_cache_bytes)?
            .with_cache_inodes(self.cfg.catalog_cache_inodes);
        let snap = load_checkpoint(&self.cfg.root, &self.store)?;
        self.adopt_snapshot(snap)
    }

    /// Takes over a snapshot the store already holds durably.
    fn adopt_snapshot(&mut self, snap: Catalog) -> Result<(), RaftError> {
        let si = snap.applied_index;
        let st = snap.current_term;
        // The snapshot is durable before the log entries it covers go.
        self.catalog = snap;
        if self.term_at(si) == Some(st) {
            self.log.drain(..(si - self.snapshot_index) as usize);
            self.wal.compact_through(si)?;
        } else {
            self.log.clear();
            self.wal.reset(si)?;
        }
        self.wal.raise_floor(si);
        self.snapshot_index = si;
        self.snapshot_term = st;
        self.commit_index = si;
        self.refresh_membership();
        Ok(())
    }

    fn log_up_to_date(&self, last_log_index: u64, last_log_term: u64) -> bool {
        last_log_term > self.last_term()
            || (last_log_term == self.last_term() && last_log_index >= self.last_index())
    }

    /// True while this node is the leader or has heard from one within the minimum election
    /// timeout.
    fn leader_active(&self) -> bool {
        self.role == Role::Leader
            || (self.leader.is_some() && self.election_elapsed < self.cfg.election_ticks.0)
    }

    fn pre_campaign(&mut self) -> Result<(), RaftError> {
        if !self.is_voter() {
            self.reset_election_timer();
            return Ok(());
        }
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes = BTreeSet::from([self.cfg.id.clone()]);
        self.reset_election_timer();
        if self.membership.has_quorum(&self.votes) {
            return self.campaign();
        }
        let msg = Message::PreVote {
            term: self.hard.term + 1,
            last_log_index: self.last_index(),
            last_log_term: self.last_term(),
        };
        for p in self.peers() {
            self.send(p, msg.clone());
        }
        Ok(())
    }

    fn campaign(&mut self) -> Result<(), RaftError> {
        self.counters.elections += 1;
        self.role = Role::Candidate;
        self.hard.term += 1;
        self.hard.voted_for = Some(self.cfg.id.clone());
        self.persist_hard()?;
        self.leader = None;
        self.votes = BTreeSet::from([self.cfg.id.clone()]);
        self.reset_election_timer();
        if self.membership.has_quorum(&self.votes) {
            return self.become_leader();
        }
        let msg = Message::RequestVote {
            term: self.hard.term,
            last_log_index: self.last_index(),
            last_log_term: self.last_term(),
        };
        for p in self.peers() {
            self.send(p, msg.clone());
        }
        Ok(())
    }

    fn become_follower(&mut self, term: u64, leader: Option<NodeId>) -> Result<(), RaftError> {
        // Only a leader has unsynced entries; a follower or candidate advertises a durable log.
        self.wal.sync()?;
        if term > self.hard.term {
            self.hard.term = term;
            self.hard.voted_for = None;
            self.persist_hard()?;
        }
        // Barriers this node was confirming as leader can no longer be; their askers retry.
        for r in std::mem::take(&mut self.pending_reads) {
            self.answer_read(r.from, r.id, None);
        }
        self.role = Role::Follower;
        self.spec = None;
        self.outgoing.clear();
        self.leader = leader;
        self.votes.clear();
        self.reset_election_timer();
        Ok(())
    }

    fn become_leader(&mut self) -> Result<(), RaftError> {
        self.counters.leader_terms += 1;
        self.role = Role::Leader;
        self.spec = None;
        self.outgoing.clear();
        self.incoming = None;
        self.leader = Some(self.cfg.id.clone());
        self.heartbeat_elapsed = 0;
        self.check_quorum_elapsed = 0;
        self.recent_active.clear();
        let next = self.last_index() + 1;
        self.next_index = self.peers().into_iter().map(|p| (p, next)).collect();
        self.match_index = self.peers().into_iter().map(|p| (p, 0)).collect();
        // Entries from earlier terms only commit once an entry of the current term does.
        self.append_local(MetaCommand::Noop)?;
        self.broadcast_append();
        self.advance_commit()
    }

    fn append_local(&mut self, command: MetaCommand) -> Result<u64, RaftError> {
        let index = self.last_index() + 1;
        let config = matches!(command, MetaCommand::ChangeMembership { .. });
        let e = Entry {
            term: self.hard.term,
            index,
            command,
        };
        // The leader writes in parallel with replication and counts itself towards a quorum only
        // up to `wal.synced_index()` (Raft thesis §10.2.1), so concurrent proposals share fsyncs.
        self.wal.append_unsynced(&e)?;
        if let Some(spec) = &mut self.spec {
            // A no-op for proposals, which append_validated already applied.
            let _ = spec.apply_committed(e.term, e.index, &e.command);
        }
        self.log.push(e);
        if config {
            self.refresh_membership();
        }
        Ok(index)
    }

    fn broadcast_append(&mut self) {
        for p in self.peers() {
            self.send_append(&p);
        }
    }

    fn send_append(&mut self, peer: &NodeId) {
        let next = self
            .next_index
            .get(peer)
            .copied()
            .unwrap_or(self.last_index() + 1)
            .min(self.last_index() + 1);
        let prev = next - 1;
        if prev < self.snapshot_index {
            // A chunk per heartbeat or proposal would pile up in the peer's queue.
            let term = self.hard.term;
            let msg = match self.outgoing.get_mut(peer) {
                Some(o) if self.ticks < o.sent_at + self.cfg.election_ticks.1 => return,
                Some(o) => {
                    o.sent_at = self.ticks;
                    snapshot_message(term, o)
                }
                None => {
                    let o = Outgoing::start(self.catalog.clone(), self.ticks);
                    let msg = snapshot_message(term, &o);
                    self.outgoing.insert(peer.clone(), o);
                    msg
                }
            };
            self.send(peer.clone(), msg);
            return;
        }
        let prev_term = self.term_at(prev).unwrap_or(0);
        let start = (next - self.snapshot_index - 1) as usize;
        let end = (start + self.cfg.max_batch).min(self.log.len());
        let entries = self.log[start..end].to_vec();
        if let Some(last) = entries.last() {
            // Pipelining: the next send carries only newer entries. A rejection or a lost message
            // (caught by the next heartbeat's consistency check) rewinds it.
            self.next_index.insert(peer.clone(), last.index + 1);
        }
        self.send(
            peer.clone(),
            Message::AppendEntries {
                term: self.hard.term,
                prev_log_index: prev,
                prev_log_term: prev_term,
                entries,
                leader_commit: self.commit_index,
                read_seq: self.read_seq,
            },
        );
    }

    fn advance_commit(&mut self) -> Result<(), RaftError> {
        // Committing a config entry can append the next one, which may commit at once (e.g. a
        // single-voter group), hence the loop.
        while self.role == Role::Leader {
            if self
                .membership
                .has_quorum(&BTreeSet::from([self.cfg.id.clone()]))
            {
                self.wal.sync()?;
            }
            let synced = self.wal.synced_index();
            let mut new_commit = self.commit_index;
            for n in (self.commit_index + 1..=self.last_index()).rev() {
                if self.term_at(n) != Some(self.hard.term) {
                    break;
                }
                let acks: BTreeSet<NodeId> = self
                    .match_index
                    .iter()
                    .filter(|(_, m)| **m >= n)
                    .map(|(p, _)| p.clone())
                    .chain((synced >= n).then(|| self.cfg.id.clone()))
                    .collect();
                if self.membership.has_quorum(&acks) {
                    new_commit = n;
                    break;
                }
            }
            if new_commit <= self.commit_index {
                break;
            }
            self.commit_index = new_commit;
            self.apply_committed()?;
            self.broadcast_append();
            let last = self.last_index();
            self.finish_membership_change()?;
            if self.last_index() == last {
                break;
            }
        }
        Ok(())
    }

    fn apply_committed(&mut self) -> Result<(), RaftError> {
        let start = self.catalog.applied_index;
        while self.catalog.applied_index < self.commit_index {
            let idx = self.catalog.applied_index + 1;
            let e = self
                .entry(idx)
                .ok_or_else(|| RaftError::Inconsistent(format!("missing entry {idx}")))?
                .clone();
            // A rejected command is a committed no-op on every replica; see apply_committed.
            let _ = self.catalog.apply_committed(e.term, e.index, &e.command);
        }
        if self.catalog.applied_index > start {
            self.maybe_compact()?;
        }
        Ok(())
    }

    fn maybe_compact(&mut self) -> Result<(), RaftError> {
        let applied = self.catalog.applied_index;
        let keep = self.cfg.compact_after;
        if keep == 0 || applied - self.snapshot_index < 2 * keep {
            return Ok(());
        }
        let through = applied - keep;
        let term = self
            .term_at(through)
            .ok_or_else(|| RaftError::Inconsistent(format!("no term for {through}")))?;
        // The catalog is only written here: until then the WAL holds every applied entry and a
        // restart re-applies them, so rewriting the whole catalog on every apply buys nothing.
        // It covers everything through `applied`; a restart starts its log above that.
        self.persist_catalog()?;
        self.wal.compact_through(through)?;
        self.log.drain(..(through - self.snapshot_index) as usize);
        self.snapshot_index = through;
        self.snapshot_term = term;
        Ok(())
    }

    fn term_at(&self, index: u64) -> Option<u64> {
        if index == self.snapshot_index {
            return Some(self.snapshot_term);
        }
        self.entry(index).map(|e| e.term)
    }

    fn entry(&self, index: u64) -> Option<&Entry> {
        if index <= self.snapshot_index {
            return None;
        }
        self.log.get((index - self.snapshot_index - 1) as usize)
    }

    fn reset_election_timer(&mut self) {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let (lo, hi) = self.cfg.election_ticks;
        self.election_elapsed = 0;
        self.election_timeout = lo + self.rng % (hi - lo);
    }

    fn send(&mut self, to: NodeId, msg: Message) {
        self.outbox.push(Envelope {
            from: self.cfg.id.clone(),
            to,
            group: 0,
            msg,
        });
    }

    fn persist_hard(&self) -> Result<(), RaftError> {
        let bytes = serde_json::to_vec(&self.hard)?;
        durable::write_atomic(&self.cfg.root.join("raft_state.json"), &bytes)?;
        Ok(())
    }

    fn persist_catalog(&mut self) -> Result<(), RaftError> {
        self.store.checkpoint(&mut self.catalog)?;
        Ok(())
    }
}

fn snapshot_message(term: u64, o: &Outgoing) -> Message {
    Message::InstallSnapshot {
        term,
        index: o.index,
        seq: o.seq,
        chunk: Box::new(o.chunk.clone()),
    }
}
