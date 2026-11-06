// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Free-space allocation shared by every placement in flight.
//!
//! A placement writes data into free ranges before the command that installs it commits, so
//! the applied free list alone can't stop two placements from picking the same range. Every
//! placement (locked writes, unlocked aligned writes, repair) therefore draws from one view:
//! the applied free list minus each range a placement holds. A placement holds its ranges until
//! its commit has applied or failed; a commit whose outcome is unknown (a timeout, a lost
//! leadership) may still apply, so its ranges stay out of the view for [`QUARANTINE`].

use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use super::{NativeEngine, NativeError};
use crate::{
    alloc::{FreeList, FreeRange},
    raft::RaftError,
};

/// How long a view may be reused before it is rebuilt from the applied free list (picking up
/// space freed meanwhile).
const REFRESH: Duration = Duration::from_secs(1);
/// How long the ranges of a commit with an unknown outcome stay unavailable.
const QUARANTINE: Duration = Duration::from_secs(300);

#[derive(Debug, Default)]
pub(super) struct Inflight {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    view: Option<(FreeList, Instant)>,
    held: BTreeMap<u64, Vec<FreeRange>>,
    quarantine: Vec<(Instant, FreeRange)>,
    next: u64,
}

/// One placement's claim on free space; dropping it releases what it holds.
pub(super) struct Lease<'a> {
    engine: &'a NativeEngine,
    token: u64,
}

impl NativeEngine {
    pub(super) fn lease(&self) -> Result<Lease<'_>, NativeError> {
        // Leases clients held and never committed come due here, so their space is reclaimed
        // by the next placement.
        self.reap_direct();
        let mut s = self
            .inflight
            .state
            .lock()
            .map_err(|_| NativeError::Poisoned("alloc"))?;
        s.next += 1;
        let token = s.next;
        s.held.insert(token, Vec::new());
        Ok(Lease {
            engine: self,
            token,
        })
    }
}

impl Lease<'_> {
    /// Finds and holds `len` free bytes on `node_id`'s device `device_index`.
    pub fn take(
        &self,
        node_id: &str,
        device_index: usize,
        len: u64,
    ) -> Result<Option<u64>, NativeError> {
        let mut s = self
            .engine
            .inflight
            .state
            .lock()
            .map_err(|_| NativeError::Poisoned("alloc"))?;
        let s = &mut *s;
        if s.view
            .as_ref()
            .is_none_or(|(_, at)| at.elapsed() >= REFRESH)
        {
            let mut view = self.engine.with_catalog(|c| c.free.clone())?;
            let now = Instant::now();
            s.quarantine.retain(|(until, _)| *until > now);
            for r in s
                .held
                .values()
                .flatten()
                .chain(s.quarantine.iter().map(|(_, r)| r))
            {
                // A range whose commit already applied is no longer free: nothing to do.
                view.reserve(&r.node_id, r.device_index, r.offset, r.len);
            }
            s.view = Some((view, now));
        }
        let Some((view, _)) = s.view.as_mut() else {
            return Ok(None);
        };
        let Some(offset) = view.find(node_id, device_index, len) else {
            return Ok(None);
        };
        view.reserve(node_id, device_index, offset, len);
        s.held.entry(self.token).or_default().push(FreeRange {
            node_id: node_id.to_string(),
            device_index,
            offset,
            len,
        });
        Ok(Some(offset))
    }

    /// Keeps this lease's ranges out of the view after `e`, if `e` leaves it unknown whether
    /// the commit using them will apply.
    pub fn settle(&self, e: &NativeError) {
        let unknown = matches!(
            e,
            NativeError::Raft(
                RaftError::Timeout { .. }
                    | RaftError::LeadershipLost { .. }
                    | RaftError::Shutdown
                    | RaftError::Io(_)
                    | RaftError::Wal(_)
            )
        );
        if unknown {
            self.quarantine(QUARANTINE);
        }
    }

    /// Keeps everything this lease holds out of the view for `grace`, then lets it go. For
    /// ranges a client may still be writing, or whose commit may still apply.
    pub fn quarantine(&self, grace: Duration) {
        if let Ok(mut s) = self.engine.inflight.state.lock() {
            let until = Instant::now() + grace;
            let held = s.held.insert(self.token, Vec::new()).unwrap_or_default();
            s.quarantine.extend(held.into_iter().map(|r| (until, r)));
        }
    }

    /// Keeps the ranges held after this value is gone, identified by the returned token, so a
    /// client can hold a lease across requests ([`NativeEngine::adopt`] takes it back).
    pub fn detach(self) -> u64 {
        let token = self.token;
        std::mem::forget(self);
        token
    }
}

impl NativeEngine {
    /// The lease a [`Lease::detach`] returned the token of.
    pub(super) fn adopt(&self, token: u64) -> Lease<'_> {
        Lease {
            engine: self,
            token,
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.engine.inflight.state.lock() {
            s.held.remove(&self.token);
            if s.held.is_empty() && s.quarantine.is_empty() {
                s.view = None;
            }
        }
    }
}
