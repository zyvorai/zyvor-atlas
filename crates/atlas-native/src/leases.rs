// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Client sessions and the byte-range locks they hold, replicated in the catalog so they survive
//! a metadata leader failover. A session is a lease: the client renews it within its TTL, and
//! the leader expires one it stopped renewing, releasing its locks. Every time arrives in the
//! command (the proposing leader's clock), so every replica reaches the same state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{metadata::MetaError, namespace::FsId};

pub const MIN_TTL_MS: u64 = 1_000;
pub const MAX_TTL_MS: u64 = 300_000;
pub const MAX_SESSIONS: usize = 65_536;
pub const MAX_LOCKS_PER_SESSION: usize = 4_096;
pub const MAX_SESSION_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub ttl_ms: u64,
    /// Leader wall-clock time (ms since the epoch) after which the session may be expired.
    pub expires_ms: u64,
    /// The client caches under cache leases, so a new leader must wait out its predecessor's.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cache: bool,
    /// Raft term the session was opened in: only a leader of that term or later granted it
    /// cache leases.
    #[serde(default)]
    pub term: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    Read,
    Write,
}

/// A held lock on `[start, end]` (inclusive; `end == u64::MAX` reaches end of file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileLock {
    pub ino: u64,
    pub session: String,
    /// The client's lock owner (POSIX: the process's file table; `flock`: the open file).
    pub owner: u64,
    pub kind: LockKind,
    pub start: u64,
    pub end: u64,
    /// Process id on the client that took it, reported to `F_GETLK`.
    #[serde(default)]
    pub pid: u32,
}

impl FileLock {
    fn same_owner(&self, session: &str, owner: u64) -> bool {
        self.session == session && self.owner == owner
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }

    fn conflicts(&self, session: &str, owner: u64, kind: LockKind, start: u64, end: u64) -> bool {
        !self.same_owner(session, owner)
            && self.overlaps(start, end)
            && (self.kind == LockKind::Write || kind == LockKind::Write)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Leases {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sessions: BTreeMap<String, Session>,
    /// Held locks per filesystem.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locks: BTreeMap<FsId, Vec<FileLock>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LeaseOp {
    /// Opens a session, or renews it with a new TTL if it is open.
    Open {
        session: String,
        ttl_ms: u64,
        now_ms: u64,
        #[serde(default)]
        cache: bool,
    },
    /// Fails with `no_session` if the session was expired or closed.
    Renew { session: String, now_ms: u64 },
    /// Releases every lock the session holds.
    Close { session: String },
    /// Drops every session whose lease ran out by `now_ms`, with its locks.
    Expire { now_ms: u64 },
    /// POSIX `F_SETLK` semantics: takes `kind` over `[start, end]` for the owner, replacing what
    /// the owner held there (`None` unlocks), or fails with `locked` on another owner's
    /// conflicting lock. Renews the session.
    Lock {
        fs: FsId,
        ino: u64,
        session: String,
        owner: u64,
        kind: Option<LockKind>,
        start: u64,
        end: u64,
        #[serde(default)]
        pid: u32,
        now_ms: u64,
    },
    /// Releases every lock the owner holds on one inode (close of a file).
    ReleaseOwner {
        fs: FsId,
        ino: u64,
        session: String,
        owner: u64,
    },
}

fn check_holder_name(session: &str) -> Result<(), MetaError> {
    if session.is_empty() || session.len() > MAX_SESSION_ID_BYTES {
        return Err(MetaError::Invalid(format!(
            "session ids are 1 to {MAX_SESSION_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Leases {
    pub fn lock_count(&self) -> usize {
        self.locks.values().map(Vec::len).sum()
    }

    /// Locks on one inode.
    pub fn locks_on(&self, fs: &str, ino: u64) -> impl Iterator<Item = &FileLock> {
        self.locks
            .get(fs)
            .into_iter()
            .flatten()
            .filter(move |l| l.ino == ino)
    }

    /// The first lock that would block `kind` over `[start, end]` for the owner (`F_GETLK`).
    #[allow(clippy::too_many_arguments)]
    pub fn conflict(
        &self,
        fs: &str,
        ino: u64,
        session: &str,
        owner: u64,
        kind: LockKind,
        start: u64,
        end: u64,
    ) -> Option<&FileLock> {
        self.locks_on(fs, ino)
            .find(|l| l.conflicts(session, owner, kind, start, end))
    }

    /// Sessions whose lease ran out by `now_ms`.
    pub fn expired(&self, now_ms: u64) -> impl Iterator<Item = &String> {
        self.sessions
            .iter()
            .filter(move |(_, s)| s.expires_ms <= now_ms)
            .map(|(id, _)| id)
    }

    /// The longest TTL any open session has.
    pub fn max_ttl_ms(&self) -> u64 {
        self.sessions.values().map(|s| s.ttl_ms).max().unwrap_or(0)
    }

    /// Drops every lock on a deleted filesystem.
    pub fn forget_fs(&mut self, fs: &str) {
        self.locks.remove(fs);
    }

    fn session_mut(&mut self, session: &str) -> Result<&mut Session, MetaError> {
        self.sessions.get_mut(session).ok_or(MetaError::NoSession)
    }

    fn drop_sessions(&mut self, gone: &[String]) {
        if gone.is_empty() {
            return;
        }
        for s in gone {
            self.sessions.remove(s);
        }
        for locks in self.locks.values_mut() {
            locks.retain(|l| !gone.contains(&l.session));
        }
        self.locks.retain(|_, l| !l.is_empty());
    }

    /// Whether a caching session was opened before `term`, so it may hold cache leases a
    /// leader of an earlier term granted.
    pub fn caching_since_before(&self, term: u64) -> bool {
        self.sessions.values().any(|s| s.cache && s.term < term)
    }

    /// Applies `op` (committed in `term`), checking every input before changing anything.
    /// `file` says whether an inode of the filesystem is a regular file (`Lock` refuses anything
    /// else).
    pub fn apply(
        &mut self,
        op: &LeaseOp,
        term: u64,
        file: impl Fn(&str, u64) -> Result<(), MetaError>,
    ) -> Result<(), MetaError> {
        match op {
            LeaseOp::Open {
                session,
                ttl_ms,
                now_ms,
                cache,
            } => {
                check_holder_name(session)?;
                if !(MIN_TTL_MS..=MAX_TTL_MS).contains(ttl_ms) {
                    return Err(MetaError::Invalid(format!(
                        "session ttl_ms must be between {MIN_TTL_MS} and {MAX_TTL_MS}"
                    )));
                }
                if !self.sessions.contains_key(session) && self.sessions.len() >= MAX_SESSIONS {
                    return Err(MetaError::TooBig(format!(
                        "more than {MAX_SESSIONS} sessions"
                    )));
                }
                // Reopening keeps the first term: leases from then may still be held.
                let term = self.sessions.get(session).map_or(term, |s| s.term);
                self.sessions.insert(
                    session.clone(),
                    Session {
                        ttl_ms: *ttl_ms,
                        expires_ms: now_ms.saturating_add(*ttl_ms),
                        cache: *cache,
                        term,
                    },
                );
            }
            LeaseOp::Renew { session, now_ms } => {
                let s = self.session_mut(session)?;
                s.expires_ms = s.expires_ms.max(now_ms.saturating_add(s.ttl_ms));
            }
            LeaseOp::Close { session } => {
                if self.sessions.contains_key(session) {
                    self.drop_sessions(std::slice::from_ref(session));
                }
            }
            LeaseOp::Expire { now_ms } => {
                let gone: Vec<String> = self.expired(*now_ms).cloned().collect();
                self.drop_sessions(&gone);
            }
            LeaseOp::Lock {
                fs,
                ino,
                session,
                owner,
                kind,
                start,
                end,
                pid,
                now_ms,
            } => {
                if start > end {
                    return Err(MetaError::Invalid(format!(
                        "lock range starts at {start} after its end {end}"
                    )));
                }
                let ttl = self.session_mut(session)?.ttl_ms;
                file(fs, *ino)?;
                if let Some(kind) = kind {
                    if let Some(l) = self.conflict(fs, *ino, session, *owner, *kind, *start, *end) {
                        return Err(MetaError::Locked(format!(
                            "bytes {}..={} of inode {ino} are {:?}-locked by another owner",
                            l.start, l.end, l.kind
                        )));
                    }
                }
                let held = self.locks.get(fs).map_or(&[][..], Vec::as_slice);
                let mut next = relock(held, *ino, session, *owner, *kind, *start, *end, *pid);
                let mine = next
                    .iter()
                    .filter(|l| l.session == *session)
                    .count()
                    .saturating_add(
                        self.locks
                            .iter()
                            .filter(|(f, _)| *f != fs)
                            .flat_map(|(_, l)| l)
                            .filter(|l| l.session == *session)
                            .count(),
                    );
                if mine > MAX_LOCKS_PER_SESSION {
                    return Err(MetaError::TooBig(format!(
                        "a session would hold more than {MAX_LOCKS_PER_SESSION} locks"
                    )));
                }
                let s = self.session_mut(session)?;
                s.expires_ms = s.expires_ms.max(now_ms.saturating_add(ttl));
                if next.is_empty() {
                    self.locks.remove(fs);
                } else {
                    next.shrink_to_fit();
                    self.locks.insert(fs.clone(), next);
                }
            }
            LeaseOp::ReleaseOwner {
                fs,
                ino,
                session,
                owner,
            } => {
                if let Some(locks) = self.locks.get_mut(fs) {
                    locks.retain(|l| !(l.ino == *ino && l.same_owner(session, *owner)));
                    if locks.is_empty() {
                        self.locks.remove(fs);
                    }
                }
            }
        }
        Ok(())
    }
}

/// `held` (one filesystem's locks) after the owner sets `kind` (or unlocks) over `[start, end]`
/// of `ino`: its overlapping locks are cut back, then the new one merged with its neighbours of
/// the same kind.
#[allow(clippy::too_many_arguments)]
fn relock(
    held: &[FileLock],
    ino: u64,
    session: &str,
    owner: u64,
    kind: Option<LockKind>,
    start: u64,
    end: u64,
    pid: u32,
) -> Vec<FileLock> {
    let mut out = Vec::with_capacity(held.len() + 2);
    let mut new = kind.map(|kind| FileLock {
        ino,
        session: session.into(),
        owner,
        kind,
        start,
        end,
        pid,
    });
    for l in held {
        if l.ino != ino || !l.same_owner(session, owner) {
            out.push(l.clone());
            continue;
        }
        let touches = l.overlaps(start, end)
            || l.end.checked_add(1) == Some(start)
            || end.checked_add(1) == Some(l.start);
        if !touches {
            out.push(l.clone());
            continue;
        }
        if let Some(n) = new.as_mut().filter(|n| n.kind == l.kind) {
            // Same kind, overlapping or adjacent: absorb it.
            n.start = n.start.min(l.start);
            n.end = n.end.max(l.end);
            continue;
        }
        if !l.overlaps(start, end) {
            out.push(l.clone());
            continue;
        }
        if l.start < start {
            out.push(FileLock {
                end: start - 1,
                ..l.clone()
            });
        }
        if l.end > end {
            out.push(FileLock {
                start: end + 1,
                ..l.clone()
            });
        }
    }
    out.extend(new);
    out.sort_by(|a, b| {
        (a.ino, &a.session, a.owner, a.start).cmp(&(b.ino, &b.session, b.owner, b.start))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any_file(_: &str, _: u64) -> Result<(), MetaError> {
        Ok(())
    }

    fn open(l: &mut Leases, s: &str, now: u64) {
        l.apply(
            &LeaseOp::Open {
                session: s.into(),
                ttl_ms: 10_000,
                now_ms: now,
                cache: false,
            },
            1,
            any_file,
        )
        .unwrap();
    }

    fn lock(
        l: &mut Leases,
        s: &str,
        owner: u64,
        kind: Option<LockKind>,
        start: u64,
        end: u64,
    ) -> Result<(), MetaError> {
        l.apply(
            &LeaseOp::Lock {
                fs: "fs".into(),
                ino: 7,
                session: s.into(),
                owner,
                kind,
                start,
                end,
                pid: 1,
                now_ms: 0,
            },
            1,
            any_file,
        )
    }

    fn ranges(l: &Leases) -> Vec<(String, u64, LockKind, u64, u64)> {
        l.locks_on("fs", 7)
            .map(|l| (l.session.clone(), l.owner, l.kind, l.start, l.end))
            .collect()
    }

    #[test]
    fn posix_ranges_split_merge_and_conflict() {
        let mut l = Leases::default();
        open(&mut l, "a", 0);
        open(&mut l, "b", 0);
        use LockKind::{Read, Write};

        lock(&mut l, "a", 1, Some(Write), 0, 99).unwrap();
        // Unlocking the middle splits the lock.
        lock(&mut l, "a", 1, None, 40, 59).unwrap();
        assert_eq!(
            ranges(&l),
            vec![
                ("a".into(), 1, Write, 0, 39),
                ("a".into(), 1, Write, 60, 99)
            ]
        );
        // Another owner may take the gap, not the held ranges.
        lock(&mut l, "b", 1, Some(Write), 40, 59).unwrap();
        assert!(matches!(
            lock(&mut l, "b", 1, Some(Read), 30, 45),
            Err(MetaError::Locked(_))
        ));
        // The same owner in another session is another owner.
        assert!(matches!(
            lock(&mut l, "b", 1, Some(Read), 0, 0),
            Err(MetaError::Locked(_))
        ));
        // Downgrading part of a range keeps the rest.
        lock(&mut l, "a", 1, Some(Read), 90, u64::MAX).unwrap();
        assert_eq!(
            ranges(&l),
            vec![
                ("a".into(), 1, Write, 0, 39),
                ("a".into(), 1, Write, 60, 89),
                ("a".into(), 1, Read, 90, u64::MAX),
                ("b".into(), 1, Write, 40, 59),
            ]
        );
        // Readers share; adjacent read locks of one owner merge.
        lock(&mut l, "b", 2, Some(Read), 95, 100).unwrap();
        lock(&mut l, "a", 1, Some(Write), 60, 89).unwrap();
        lock(&mut l, "a", 1, Some(Write), 40, 59).unwrap_err();
        lock(&mut l, "b", 1, None, 0, u64::MAX).unwrap();
        lock(&mut l, "a", 1, Some(Write), 40, 59).unwrap();
        assert_eq!(
            ranges(&l)[0],
            ("a".into(), 1, Write, 0, 89),
            "{:?}",
            ranges(&l)
        );
        assert!(l
            .conflict("fs", 7, "b", 9, Write, 100, 100)
            .is_some_and(|c| c.kind == Read));
        assert!(l.conflict("fs", 7, "b", 9, Read, 100, 100).is_none());
    }

    #[test]
    fn expiry_and_close_release_locks_and_unknown_sessions_fail() {
        let mut l = Leases::default();
        open(&mut l, "a", 0);
        open(&mut l, "b", 5_000);
        lock(&mut l, "a", 1, Some(LockKind::Write), 0, 9).unwrap();
        lock(&mut l, "b", 1, Some(LockKind::Write), 10, 19).unwrap();
        assert!(matches!(
            lock(&mut l, "zz", 1, Some(LockKind::Write), 30, 39),
            Err(MetaError::NoSession)
        ));
        assert_eq!(l.expired(10_000).collect::<Vec<_>>(), vec!["a"]);
        l.apply(&LeaseOp::Expire { now_ms: 10_000 }, 1, any_file)
            .unwrap();
        assert!(!l.sessions.contains_key("a"));
        assert_eq!(l.lock_count(), 1);
        assert!(matches!(
            l.apply(
                &LeaseOp::Renew {
                    session: "a".into(),
                    now_ms: 10_000
                },
                1,
                any_file
            ),
            Err(MetaError::NoSession)
        ));
        l.apply(
            &LeaseOp::Close {
                session: "b".into(),
            },
            1,
            any_file,
        )
        .unwrap();
        assert_eq!(l, Leases::default());
        assert!(matches!(
            l.apply(
                &LeaseOp::Open {
                    session: "c".into(),
                    ttl_ms: 1,
                    now_ms: 0,
                    cache: false,
                },
                1,
                any_file
            ),
            Err(MetaError::Invalid(_))
        ));
    }

    #[test]
    fn release_owner_drops_only_that_owners_locks_on_the_inode() {
        let mut l = Leases::default();
        open(&mut l, "a", 0);
        lock(&mut l, "a", 1, Some(LockKind::Read), 0, 9).unwrap();
        lock(&mut l, "a", 2, Some(LockKind::Read), 0, 9).unwrap();
        l.apply(
            &LeaseOp::ReleaseOwner {
                fs: "fs".into(),
                ino: 7,
                session: "a".into(),
                owner: 1,
            },
            1,
            any_file,
        )
        .unwrap();
        assert_eq!(ranges(&l), vec![("a".into(), 2, LockKind::Read, 0, 9)]);
    }
}
