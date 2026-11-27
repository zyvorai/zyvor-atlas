// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Group commit of delta writes: writers that have replicated their data queue up, and one of
//! them at a time proposes everything queued as one command.
//!
//! Every writer gets an [`Outcome`]: its delta applied, it certainly did not, or nobody can
//! tell (a timeout, lost leadership, a failed log write, a committer that panicked). The space of
//! an outcome nobody can tell stays reserved, because the command may still apply.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Condvar, Mutex, PoisonError},
};

use super::{delta::DeltaPoint, inflight::QUARANTINE, Meta, NativeEngine, NativeError};
use crate::{
    metadata::{Catalog, DeltaItem, DeltaTarget, MetaCommand},
    namespace::FsOp,
    raft::RaftError,
};

/// Most deltas one command carries.
const MAX_BATCH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    /// Certainly not applied: from another term, stale, on a blocked target, or rejected. The
    /// writer may try again.
    Rejected,
    /// May or may not apply.
    Unknown(String),
}

#[derive(Debug)]
pub(super) struct Pending {
    pub ticket: u64,
    /// The term the data was replicated under.
    pub fence: u64,
    pub item: DeltaItem,
    /// The detached lease holding the data's space.
    pub lease: u64,
}

#[derive(Debug, Default)]
struct State {
    next: u64,
    queue: VecDeque<Pending>,
    /// A writer is proposing a batch.
    committing: bool,
    done: HashMap<u64, Outcome>,
}

#[derive(Debug, Default)]
pub(super) struct Combiner {
    state: Mutex<State>,
    wake: Condvar,
}

/// The batch being committed. Dropping it hands every writer its outcome and settles its space;
/// if the committer never decided (it panicked), that outcome is unknown.
pub(super) struct Batch<'a> {
    pub engine: &'a NativeEngine,
    pub entries: Vec<Pending>,
    pub outcomes: Option<Vec<Outcome>>,
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        let decided = self.outcomes.take().unwrap_or_else(|| {
            vec![Outcome::Unknown("the delta committer failed".into()); self.entries.len()]
        });
        for (p, outcome) in self.entries.iter().zip(&decided) {
            let lease = self.engine.adopt(p.lease);
            if matches!(outcome, Outcome::Unknown(_)) {
                lease.quarantine(QUARANTINE);
            }
        }
        let combiner = &self.engine.delta.combiner;
        let mut s = combiner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (p, outcome) in self.entries.drain(..).zip(decided) {
            s.done.insert(p.ticket, outcome);
        }
        s.committing = false;
        combiner.wake.notify_all();
    }
}

fn kind(item: &DeltaItem) -> bool {
    matches!(item.target, DeltaTarget::Volume { .. })
}

/// Whether an error from a proposal leaves it unknown whether the command applies.
fn unknown(e: &NativeError) -> bool {
    !matches!(
        e,
        NativeError::Metadata(_)
            | NativeError::Raft(
                RaftError::Rejected(_)
                    | RaftError::NotLeader { .. }
                    | RaftError::TermChanged { .. }
                    | RaftError::MembershipBusy
                    | RaftError::Config(_)
            )
    )
}

impl NativeEngine {
    /// Queues a replicated delta for the next command and returns what became of it. The lease
    /// is settled before this returns.
    pub(super) fn submit_delta(
        &self,
        fence: u64,
        item: DeltaItem,
        lease: u64,
    ) -> Result<Outcome, NativeError> {
        let combiner = &self.delta.combiner;
        let poisoned = |_| NativeError::Poisoned("delta committer");
        let mut s = combiner.state.lock().map_err(poisoned)?;
        s.next += 1;
        let ticket = s.next;
        s.queue.push_back(Pending {
            ticket,
            fence,
            item,
            lease,
        });
        loop {
            if let Some(outcome) = s.done.remove(&ticket) {
                return Ok(outcome);
            }
            if s.committing {
                s = combiner
                    .wake
                    .wait(s)
                    .map_err(|_| NativeError::Poisoned("delta committer"))?;
                continue;
            }
            s.committing = true;
            // One command names only volumes or only files.
            let volumes = s.queue.front().map(|p| kind(&p.item));
            let mut entries = Vec::new();
            let mut rest = VecDeque::new();
            while let Some(p) = s.queue.pop_front() {
                if entries.len() < MAX_BATCH && Some(kind(&p.item)) == volumes {
                    entries.push(p);
                } else {
                    rest.push_back(p);
                }
            }
            s.queue = rest;
            drop(s);
            let mut batch = Batch {
                engine: self,
                entries,
                outcomes: None,
            };
            batch.outcomes = Some(self.commit_batch(&batch.entries));
            drop(batch);
            s = combiner.state.lock().map_err(poisoned)?;
        }
    }

    /// Proposes the entries still good as one command and waits for it to apply.
    fn commit_batch(&self, entries: &[Pending]) -> Vec<Outcome> {
        let mut out = vec![Outcome::Rejected; entries.len()];
        self.delta_point(DeltaPoint::BeforePropose);
        let mut taken: Vec<usize> = Vec::new();
        let proposal = {
            // Held only to propose: whoever holds it to replace a cell's base knows that no
            // delta is proposed meanwhile, and `leader_ready` then covers those already
            // proposed.
            let Ok(_write) = self.write_lock.lock() else {
                return out;
            };
            let fence = match &self.meta {
                Meta::Local { .. } => Some(0),
                Meta::Raft { server, .. } => server.leader_term().ok().flatten(),
            };
            let eligible: Vec<usize> = (0..entries.len())
                .filter(|&i| {
                    Some(entries[i].fence) == fence && !self.is_blocked(&entries[i].item.target)
                })
                .collect();
            let Some(fence) = fence.filter(|_| !eligible.is_empty()) else {
                return out;
            };
            // Runs under the lock the command is proposed under: a write whose file was
            // unlinked or cut meanwhile is left out instead of rejecting everyone else's.
            self.commit_with(fence, |c: &Catalog| {
                let items: Vec<DeltaItem> = eligible
                    .iter()
                    .filter(|&&i| c.check_delta(&entries[i].item).is_ok())
                    .map(|&i| {
                        taken.push(i);
                        entries[i].item.clone()
                    })
                    .collect();
                let volumes = items.first().map(kind)?;
                Some(if volumes {
                    MetaCommand::AppendDeltas { items }
                } else {
                    MetaCommand::Fs {
                        op: FsOp::AppendDeltas { items },
                    }
                })
            })
        };
        let applied = proposal.and_then(|proposed| {
            self.delta_point(DeltaPoint::Proposed);
            self.await_commit(proposed)
        });
        let outcome = match applied {
            Ok(()) => Outcome::Applied,
            Err(e) if unknown(&e) => Outcome::Unknown(e.to_string()),
            Err(_) => Outcome::Rejected,
        };
        for i in taken {
            out[i] = outcome.clone();
        }
        out
    }
}
