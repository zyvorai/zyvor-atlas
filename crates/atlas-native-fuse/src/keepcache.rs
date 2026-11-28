// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Which opens may keep the file data the kernel already caches (`--keep-cache`,
//! `FOPEN_KEEP_CACHE`); `docs/NATIVE_FS.md`, "Keeping the page cache", states the rules.
//!
//! Without the flag the kernel drops a file's cached pages at every open. An open may keep them
//! only if all of these hold:
//!
//! - The file has a record: the cluster's content tag, read by an earlier open of this mount that
//!   dropped the cache. Everything cached since was read afterwards, so it is that content or
//!   newer.
//! - The tag read now equals the record's. Tags never repeat after a change, so nothing newer
//!   exists, and so nothing newer is cached either.
//! - The record is confirmed: a later request on a handle whose open dropped the cache proves the
//!   kernel has finished dropping it. Until then another open drops it too.
//! - This mount has not changed the file, nor lost bytes of it, since the record was made.
//!
//! Anything else drops the cache, which is what happens without the flag.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use serde::Serialize;

/// Files opened that are remembered; beyond this everything is forgotten (and so dropped at the
/// next open).
pub const MAX_FILES: usize = 64 * 1024;

/// What an open learned about the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// The cluster's content tag, read after the open began.
    Tag(String),
    /// The cluster says not to keep this file (a replica, not a regular file, too large).
    Uncacheable,
    /// The tag could not be read.
    Unavailable,
    /// Bytes this mount accepted were lost and no fsync has reported it yet.
    LostWrites,
}

/// Why an open kept or dropped the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Kept,
    /// No record of the file yet.
    First,
    /// The content tag moved.
    Changed,
    /// The record's cache drop is not known to be done yet.
    Unconfirmed,
    /// This mount changed the file while the open was reading the tag.
    LocalChange,
    Uncacheable,
    Unavailable,
    LostWrites,
}

/// Opens by verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct KeepCacheStats {
    pub kept: u64,
    pub dropped_first: u64,
    pub dropped_changed: u64,
    pub dropped_unconfirmed: u64,
    pub dropped_local_change: u64,
    pub dropped_uncacheable: u64,
    pub dropped_unavailable: u64,
    pub dropped_lost_writes: u64,
}

impl KeepCacheStats {
    pub fn dropped(&self) -> u64 {
        self.dropped_first
            + self.dropped_changed
            + self.dropped_unconfirmed
            + self.dropped_local_change
            + self.dropped_uncacheable
            + self.dropped_unavailable
            + self.dropped_lost_writes
    }

    fn count(&mut self, v: Verdict) {
        *match v {
            Verdict::Kept => &mut self.kept,
            Verdict::First => &mut self.dropped_first,
            Verdict::Changed => &mut self.dropped_changed,
            Verdict::Unconfirmed => &mut self.dropped_unconfirmed,
            Verdict::LocalChange => &mut self.dropped_local_change,
            Verdict::Uncacheable => &mut self.dropped_uncacheable,
            Verdict::Unavailable => &mut self.dropped_unavailable,
            Verdict::LostWrites => &mut self.dropped_lost_writes,
        } += 1;
    }
}

#[derive(Debug)]
struct Record {
    tag: String,
    id: u64,
    confirmed: bool,
}

#[derive(Debug, Default)]
struct File {
    /// Sequence of this mount's latest change to the file.
    changed: u64,
    /// Sequence of the latest change or cache drop: data read before it must not be cached.
    filled: u64,
    record: Option<Record>,
}

#[derive(Debug, Default)]
struct State {
    seq: u64,
    /// Everything up to here counts as changed: what was known about it has been forgotten.
    floor: u64,
    files: HashMap<u64, File>,
    /// Handles whose open dropped the cache for a record: handle to `(inode, record id)`.
    pending: HashMap<u64, (u64, u64)>,
    stats: KeepCacheStats,
}

impl State {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn file(&mut self, ino: u64) -> &mut File {
        if self.files.len() >= MAX_FILES && !self.files.contains_key(&ino) {
            self.floor = self.next();
            self.files.clear();
            self.pending.clear();
        }
        self.files.entry(ino).or_default()
    }

    /// Compares `tag` with the record of `ino`. Unless the open as `handle` keeps the cache,
    /// makes a record if there is none for this tag, and lets the handle confirm it.
    fn compare(&mut self, ino: u64, handle: u64, tag: String) -> Verdict {
        let (verdict, id) = match &self.file(ino).record {
            Some(r) if r.tag == tag && r.confirmed => return Verdict::Kept,
            Some(r) if r.tag == tag => (Verdict::Unconfirmed, r.id),
            old => {
                let verdict = match old {
                    Some(_) => Verdict::Changed,
                    None => Verdict::First,
                };
                let id = self.next();
                self.file(ino).record = Some(Record {
                    tag,
                    id,
                    confirmed: false,
                });
                (verdict, id)
            }
        };
        self.pending.insert(handle, (ino, id));
        verdict
    }
}

#[derive(Debug, Default)]
pub struct KeepCache(Mutex<State>);

impl KeepCache {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update leaves the state consistent, and a lost one only drops caches.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// To call before reading anything [`Self::decide`] is told about `ino`: returns the mark
    /// to give it.
    pub fn begin(&self, ino: u64) -> u64 {
        let mut s = self.lock();
        // Remembered from here, so a change made meanwhile is noticed.
        s.file(ino);
        s.seq
    }

    /// Whether the open of `ino` as handle `handle`, which called [`Self::begin`] returning
    /// `mark`, keeps the kernel's cache ([`Verdict::Kept`]) or drops it (anything else).
    pub fn decide(&self, ino: u64, handle: u64, mark: u64, observed: Observed) -> Verdict {
        let mut s = self.lock();
        let floor = s.floor;
        let changed = s.file(ino).changed.max(floor) > mark;
        let verdict = match observed {
            _ if changed => Verdict::LocalChange,
            Observed::LostWrites => Verdict::LostWrites,
            Observed::Uncacheable => Verdict::Uncacheable,
            Observed::Unavailable => Verdict::Unavailable,
            Observed::Tag(tag) => s.compare(ino, handle, tag),
        };
        if verdict != Verdict::Kept {
            let now = s.next();
            let file = s.file(ino);
            file.filled = now;
            let recorded = matches!(
                verdict,
                Verdict::First | Verdict::Changed | Verdict::Unconfirmed
            );
            if !recorded {
                file.record = None;
            }
        }
        s.stats.count(verdict);
        verdict
    }

    /// A request arrived on `handle`, so its open has returned to the kernel: whatever it was
    /// to drop is gone.
    pub fn confirm(&self, handle: u64) {
        let mut s = self.lock();
        let Some((ino, id)) = s.pending.remove(&handle) else {
            return;
        };
        if let Some(r) = s.files.get_mut(&ino).and_then(|f| f.record.as_mut()) {
            if r.id == id {
                r.confirmed = true;
            }
        }
    }

    /// `handle` is gone; it can confirm nothing any more.
    pub fn forget_handle(&self, handle: u64) {
        self.lock().pending.remove(&handle);
    }

    /// This mount is changing `ino`, or lost bytes of it: the kernel may hold data the cluster
    /// does not.
    pub fn changed(&self, ino: u64) {
        let mut s = self.lock();
        let now = s.next();
        // A file never opened has no record to forget and no open under way.
        if let Some(file) = s.files.get_mut(&ino) {
            file.changed = now;
            file.filled = now;
            file.record = None;
        }
    }

    /// A value that moves when data of `ino` read earlier must no longer be cached.
    pub fn fill_epoch(&self, ino: u64) -> u64 {
        let s = self.lock();
        s.files.get(&ino).map_or(0, |f| f.filled).max(s.floor)
    }

    pub fn stats(&self) -> KeepCacheStats {
        self.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(t: &str) -> Observed {
        Observed::Tag(t.into())
    }

    fn open(k: &KeepCache, ino: u64, handle: u64, o: Observed) -> Verdict {
        let mark = k.begin(ino);
        k.decide(ino, handle, mark, o)
    }

    #[test]
    fn a_cache_is_kept_only_for_an_unchanged_tag_once_the_drop_is_confirmed() {
        let k = KeepCache::default();
        assert_eq!(open(&k, 1, 10, tag("a")), Verdict::First);
        // The first open may not have dropped the old pages yet.
        assert_eq!(open(&k, 1, 11, tag("a")), Verdict::Unconfirmed);
        k.confirm(11);
        assert_eq!(open(&k, 1, 12, tag("a")), Verdict::Kept);
        assert_eq!(open(&k, 1, 13, tag("a")), Verdict::Kept);
        assert_eq!(open(&k, 1, 14, tag("b")), Verdict::Changed);
        // A late request on a handle of the old record confirms nothing.
        k.confirm(10);
        assert_eq!(open(&k, 1, 15, tag("b")), Verdict::Unconfirmed);
        k.confirm(14);
        assert_eq!(open(&k, 1, 16, tag("b")), Verdict::Kept);
        // Each file for itself.
        assert_eq!(open(&k, 2, 17, tag("b")), Verdict::First);
        let s = k.stats();
        assert_eq!((s.kept, s.dropped()), (3, 5));
    }

    #[test]
    fn a_kept_open_confirms_nothing_and_a_request_on_it_is_harmless() {
        let k = KeepCache::default();
        open(&k, 1, 10, tag("a"));
        k.confirm(10);
        k.confirm(10);
        assert_eq!(open(&k, 1, 11, tag("a")), Verdict::Kept);
        k.confirm(11);
        k.confirm(99);
        assert_eq!(open(&k, 1, 12, tag("a")), Verdict::Kept);
    }

    #[test]
    fn a_local_change_forgets_the_record() {
        let k = KeepCache::default();
        open(&k, 1, 10, tag("a"));
        k.confirm(10);
        k.changed(1);
        // Even with the same tag: the kernel may hold bytes the cluster does not have.
        assert_eq!(open(&k, 1, 11, tag("a")), Verdict::First);
        // A change while the tag was being read: the open records nothing.
        k.confirm(11);
        let mark = k.begin(1);
        k.changed(1);
        assert_eq!(k.decide(1, 12, mark, tag("a")), Verdict::LocalChange);
        k.confirm(12);
        assert_eq!(open(&k, 1, 13, tag("a")), Verdict::First);
        // A change to another file does not matter.
        k.confirm(13);
        let mark = k.begin(1);
        k.changed(2);
        assert_eq!(k.decide(1, 14, mark, tag("a")), Verdict::Kept);
    }

    #[test]
    fn a_released_handle_confirms_nothing() {
        let k = KeepCache::default();
        open(&k, 1, 10, tag("a"));
        k.forget_handle(10);
        k.confirm(10);
        assert_eq!(open(&k, 1, 11, tag("a")), Verdict::Unconfirmed);
        assert_eq!(k.lock().pending.len(), 1);
    }

    #[test]
    fn no_confirmation_survives_a_change() {
        let k = KeepCache::default();
        open(&k, 1, 10, tag("a"));
        k.changed(1);
        k.confirm(10);
        assert_eq!(open(&k, 1, 11, tag("a")), Verdict::First);
    }

    #[test]
    fn anything_but_a_tag_forgets_the_record() {
        for (observed, verdict) in [
            (Observed::Uncacheable, Verdict::Uncacheable),
            (Observed::Unavailable, Verdict::Unavailable),
            (Observed::LostWrites, Verdict::LostWrites),
        ] {
            let k = KeepCache::default();
            open(&k, 1, 10, tag("a"));
            k.confirm(10);
            assert_eq!(open(&k, 1, 11, observed), verdict);
            k.confirm(11);
            assert_eq!(open(&k, 1, 12, tag("a")), Verdict::First);
        }
    }

    #[test]
    fn the_fill_epoch_moves_with_every_drop_and_change_but_not_when_kept() {
        let k = KeepCache::default();
        let mut last = k.fill_epoch(1);
        let mut moved = |k: &KeepCache| {
            let now = k.fill_epoch(1);
            let moved = now != last;
            last = now;
            moved
        };
        open(&k, 1, 10, tag("a"));
        assert!(moved(&k));
        open(&k, 1, 11, tag("a"));
        assert!(moved(&k));
        k.confirm(11);
        open(&k, 1, 12, tag("a"));
        assert!(!moved(&k));
        k.changed(1);
        assert!(moved(&k));
        open(&k, 1, 13, Observed::Unavailable);
        assert!(moved(&k));
    }

    #[test]
    fn forgetting_everything_counts_as_a_change_to_everything() {
        let k = KeepCache::default();
        open(&k, 1, 10, tag("a"));
        k.confirm(10);
        let epoch = k.fill_epoch(1);
        let mark = k.begin(1);
        // Writes to files never opened take no room.
        for ino in 2..=2 * MAX_FILES as u64 {
            k.changed(ino);
        }
        assert_eq!(k.lock().files.len(), 1);
        for ino in 2..=MAX_FILES as u64 + 1 {
            k.begin(ino);
        }
        assert!(k.fill_epoch(1) > epoch);
        // A change that has been forgotten may have been to this file.
        assert_eq!(k.decide(1, 11, mark, tag("a")), Verdict::LocalChange);
        assert_eq!(open(&k, 1, 12, tag("a")), Verdict::First);
        assert!(k.lock().files.len() <= MAX_FILES);
    }
}
