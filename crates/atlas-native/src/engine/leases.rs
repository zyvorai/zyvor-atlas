// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Client sessions and file locks ([`crate::leases`]) through the engine.

use serde::Serialize;

use super::{NativeEngine, NativeError};
use crate::{
    leases::{FileLock, LeaseOp, LockKind, Session},
    metadata::{MetaCommand, MetaError},
};

/// Wall-clock milliseconds since the epoch, stamped into lease commands by the proposer.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A lock request on one inode (`kind: None` unlocks).
#[derive(Debug, Clone)]
pub struct LockRequest {
    pub session: String,
    pub owner: u64,
    pub kind: Option<LockKind>,
    pub start: u64,
    pub end: u64,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockTable {
    pub sessions: usize,
    pub locks: Vec<FileLock>,
}

impl NativeEngine {
    fn lease_commit(&self, op: LeaseOp) -> Result<(), NativeError> {
        self.commit(MetaCommand::Lease { op }, None)
    }

    fn live_fs(&self, fs: &str) -> Result<(), NativeError> {
        self.with_catalog(|c| c.filesystem(fs).map(|_| ()))?
            .map_err(NativeError::from)
    }

    /// Opens (or renews, with a new TTL) a session for clients of `fs`; `cache` if it will hold
    /// cache leases.
    pub fn open_session(
        &self,
        fs: &str,
        session: &str,
        ttl_ms: u64,
        cache: bool,
    ) -> Result<Session, NativeError> {
        self.live_fs(fs)?;
        self.lease_commit(LeaseOp::Open {
            session: session.into(),
            ttl_ms,
            now_ms: now_ms(),
            cache,
        })?;
        self.session(session)
    }

    pub fn session(&self, session: &str) -> Result<Session, NativeError> {
        self.with_catalog(|c| c.leases.sessions.get(session).copied())?
            .ok_or_else(|| MetaError::NoSession.into())
    }

    pub fn renew_session(&self, session: &str) -> Result<Session, NativeError> {
        self.lease_commit(LeaseOp::Renew {
            session: session.into(),
            now_ms: now_ms(),
        })?;
        self.session(session)
    }

    pub fn close_session(&self, session: &str) -> Result<(), NativeError> {
        self.lease_commit(LeaseOp::Close {
            session: session.into(),
        })
    }

    pub fn set_lock(&self, fs: &str, ino: u64, req: LockRequest) -> Result<(), NativeError> {
        self.lease_commit(LeaseOp::Lock {
            fs: fs.into(),
            ino,
            session: req.session,
            owner: req.owner,
            kind: req.kind,
            start: req.start,
            end: req.end,
            pid: req.pid,
            now_ms: now_ms(),
        })
    }

    /// The first lock that would block `kind` over `[start, end]` for the owner (`F_GETLK`).
    #[allow(clippy::too_many_arguments)]
    pub fn test_lock(
        &self,
        fs: &str,
        ino: u64,
        session: &str,
        owner: u64,
        kind: LockKind,
        start: u64,
        end: u64,
    ) -> Result<Option<FileLock>, NativeError> {
        self.with_catalog(|c| {
            c.leases
                .conflict(fs, ino, session, owner, kind, start, end)
                .cloned()
        })
    }

    pub fn release_lock_owner(
        &self,
        fs: &str,
        ino: u64,
        session: &str,
        owner: u64,
    ) -> Result<(), NativeError> {
        let held = self.with_catalog(|c| {
            c.leases
                .locks_on(fs, ino)
                .any(|l| l.session == session && l.owner == owner)
        })?;
        if !held {
            return Ok(());
        }
        self.lease_commit(LeaseOp::ReleaseOwner {
            fs: fs.into(),
            ino,
            session: session.into(),
            owner,
        })
    }

    /// Every lock held on `fs`, and how many sessions are open in its group.
    pub fn fs_locks(&self, fs: &str) -> Result<LockTable, NativeError> {
        self.live_fs(fs)?;
        self.with_catalog(|c| LockTable {
            sessions: c.leases.sessions.len(),
            locks: c.leases.locks.get(fs).cloned().unwrap_or_default(),
        })
    }

    /// Whether a caching session was opened before `term` (see [`crate::leases::Leases`]).
    pub fn caching_since_before(&self, term: u64) -> Result<bool, NativeError> {
        self.with_catalog(|c| c.leases.caching_since_before(term))
    }

    /// The longest TTL of any open session (0 with none).
    pub fn max_session_ttl_ms(&self) -> Result<u64, NativeError> {
        self.with_catalog(|c| c.leases.max_ttl_ms())
    }

    /// Expires the sessions whose lease ran out by now (leader only); returns how many.
    pub fn expire_sessions(&self) -> Result<usize, NativeError> {
        let now = now_ms();
        let n = self.with_catalog(|c| c.leases.expired(now).count())?;
        if n > 0 {
            self.lease_commit(LeaseOp::Expire { now_ms: now })?;
        }
        Ok(n)
    }
}
