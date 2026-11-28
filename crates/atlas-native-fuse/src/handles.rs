// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The open handles of a mount, and which of them still have to be told that bytes a write
//! call accepted were never stored. The rules follow Linux's write-back error reporting
//! (`errseq_t`); `docs/NATIVE_FS.md`, "Durability and write errors", states them.
//!
//! - Every failure takes the next number of a mount-wide sequence; a file keeps its latest.
//! - A handle reports at fsync every failure after its cursor, then moves the cursor. Only
//!   its own fsync moves it, so no other handle, close or internal flush can use the report up.
//! - A handle opened after a failure that no fsync has reported yet starts before it, so
//!   reopening a file to fsync it still tells.
//! - Close reports, to writable handles, failures since they were opened that their own fsync
//!   has not reported. It moves nothing: closes of duplicated descriptors are often ignored.
//!
//! Memory is one entry per open handle, one per file with an open handle, and at most
//! [`MAX_CLOSED_FAILURES`] failures of files nobody has open.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard},
};

use crate::ops::Errno;

/// Unreported failures kept for files with no open handle; the oldest is forgotten beyond this
/// (as Linux forgets one when it evicts the inode from memory), with a warning.
pub const MAX_CLOSED_FAILURES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy)]
struct Failure {
    seq: u64,
    errno: Errno,
    /// Whether any fsync has reported it.
    synced: bool,
}

#[derive(Debug, Clone, Copy)]
struct Handle {
    ino: u64,
    writable: bool,
    /// The sequence when the handle was opened: later failures happened while it was open.
    opened: u64,
    /// The fsync cursor: failures up to here need no report from this handle.
    synced: u64,
    /// The latest failure a write on this handle was refused for.
    refused: u64,
}

#[derive(Debug, Default)]
struct File {
    open: u32,
    failure: Option<Failure>,
}

#[derive(Debug, Default)]
struct State {
    seq: u64,
    next: u64,
    handles: HashMap<u64, Handle>,
    files: HashMap<u64, File>,
    /// Unreported failures of files with no open handle: sequence to inode, oldest first.
    closed: BTreeMap<u64, u64>,
}

impl State {
    fn failure(&self, ino: u64) -> Option<Failure> {
        self.files.get(&ino)?.failure
    }

    /// Where the fsync cursor of a new handle on `ino` starts.
    fn start(&self, ino: u64) -> u64 {
        match self.failure(ino) {
            Some(f) if !f.synced => f.seq - 1,
            _ => self.seq,
        }
    }

    /// Drops what no handle, present or future, needs to hear, and bounds the rest.
    fn tidy(&mut self, ino: u64) {
        let Some(file) = self.files.get(&ino) else {
            return;
        };
        if file.open > 0 {
            return;
        }
        match file.failure {
            Some(f) if !f.synced => {
                self.closed.insert(f.seq, ino);
            }
            Some(f) => {
                self.closed.remove(&f.seq);
                self.files.remove(&ino);
            }
            None => {
                self.files.remove(&ino);
            }
        }
        while self.closed.len() > MAX_CLOSED_FAILURES {
            if let Some((_, old)) = self.closed.pop_first() {
                self.files.remove(&old);
                tracing::warn!(
                    ino = old,
                    "too many unreported write-back failures; forgetting the oldest"
                );
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Handles(Mutex<State>);

impl Handles {
    fn lock(&self) -> Result<MutexGuard<'_, State>, Errno> {
        self.0.lock().map_err(|_| libc::EIO)
    }

    /// Registers an open file; returns its handle (never 0, never reused).
    pub fn open(&self, ino: u64, writable: bool) -> Result<u64, Errno> {
        let mut s = self.lock()?;
        s.next += 1;
        let id = s.next;
        let handle = Handle {
            ino,
            writable,
            opened: s.seq,
            synced: s.start(ino),
            refused: 0,
        };
        s.handles.insert(id, handle);
        if let Some(f) = s.failure(ino) {
            s.closed.remove(&f.seq);
        }
        s.files.entry(ino).or_default().open += 1;
        Ok(id)
    }

    /// The file `id` has open.
    pub fn ino(&self, id: u64) -> Result<u64, Errno> {
        self.lock()?
            .handles
            .get(&id)
            .map(|h| h.ino)
            .ok_or(libc::EBADF)
    }

    /// Forgets handle `id`; true if it was the last one of its file.
    pub fn release(&self, id: u64) -> Result<bool, Errno> {
        let mut s = self.lock()?;
        let h = s.handles.remove(&id).ok_or(libc::EBADF)?;
        let last = s.files.get_mut(&h.ino).is_some_and(|f| {
            f.open = f.open.saturating_sub(1);
            f.open == 0
        });
        s.tidy(h.ino);
        Ok(last)
    }

    pub fn is_open(&self, ino: u64) -> bool {
        self.lock()
            .is_ok_and(|s| s.files.get(&ino).is_some_and(|f| f.open > 0))
    }

    /// Accepted bytes of `ino` were not stored.
    pub fn fail(&self, ino: u64, errno: Errno) {
        // A poisoned lock must not hide a loss: every later report would fail with EIO anyway.
        let Ok(mut s) = self.lock() else { return };
        s.seq += 1;
        let failure = Failure {
            seq: s.seq,
            errno,
            synced: false,
        };
        let old = s.files.entry(ino).or_default().failure.replace(failure);
        if let Some(old) = old {
            s.closed.remove(&old.seq);
        }
        s.tidy(ino);
    }

    /// A point in the sequence, to ask [`Self::failed_since`] about.
    pub fn mark(&self) -> Result<u64, Errno> {
        Ok(self.lock()?.seq)
    }

    /// Whether `ino` failed after `mark`. Uses nothing up.
    pub fn failed_since(&self, ino: u64, mark: u64) -> Result<(), Errno> {
        match self.lock()?.failure(ino) {
            Some(f) if f.seq > mark => Err(f.errno),
            _ => Ok(()),
        }
    }

    /// The file and fsync cursor of handle `id`, read before an fsync starts waiting: calls on
    /// one handle that overlap then all report the same failure.
    pub fn sync_from(&self, id: u64) -> Result<(u64, u64), Errno> {
        let s = self.lock()?;
        let h = s.handles.get(&id).ok_or(libc::EBADF)?;
        Ok((h.ino, h.synced))
    }

    /// The same for an fsync by inode number, which counts as a handle opened just now.
    pub fn sync_start(&self, ino: u64) -> Result<u64, Errno> {
        Ok(self.lock()?.start(ino))
    }

    /// Reports to an fsync of `ino` that began at cursor `from` (of handle `id`, if it has one)
    /// the latest failure after it.
    pub fn report_sync(&self, ino: u64, from: u64, id: Option<u64>) -> Result<(), Errno> {
        let mut s = self.lock()?;
        let Some(f) = s.files.get_mut(&ino).and_then(|f| f.failure.as_mut()) else {
            return Ok(());
        };
        if f.seq <= from {
            return Ok(());
        }
        f.synced = true;
        let (seq, errno) = (f.seq, f.errno);
        if let Some(h) = id.and_then(|id| s.handles.get_mut(&id)) {
            h.synced = h.synced.max(seq);
        }
        s.tidy(ino);
        Err(errno)
    }

    /// What closing a descriptor of handle `id` reports.
    pub fn report_close(&self, id: u64) -> Result<(), Errno> {
        let s = self.lock()?;
        let h = s.handles.get(&id).ok_or(libc::EBADF)?;
        match s.failure(h.ino) {
            Some(f) if h.writable && f.seq > h.opened.max(h.synced) => Err(f.errno),
            _ => Ok(()),
        }
    }

    /// Fails one write on handle `id` for each failure while it was open that its fsync has
    /// not reported, so a writer stops early. The fsync and close reports stay due.
    pub fn refuse_write(&self, id: u64) -> Result<(), Errno> {
        let mut s = self.lock()?;
        let h = *s.handles.get(&id).ok_or(libc::EBADF)?;
        match s.failure(h.ino) {
            Some(f) if f.seq > h.opened.max(h.synced).max(h.refused) => {
                if let Some(h) = s.handles.get_mut(&id) {
                    h.refused = f.seq;
                }
                Err(f.errno)
            }
            _ => Ok(()),
        }
    }

    /// The failure of `ino` that no fsync has reported yet.
    pub fn unsynced(&self, ino: u64) -> Result<(), Errno> {
        match self.lock()?.failure(ino) {
            Some(f) if !f.synced => Err(f.errno),
            _ => Ok(()),
        }
    }

    /// Every `(inode, errno)` that no fsync has reported.
    pub fn unreported(&self) -> Vec<(u64, Errno)> {
        let Ok(s) = self.lock() else {
            return Vec::new();
        };
        let mut all: Vec<_> = s
            .files
            .iter()
            .filter_map(|(ino, f)| f.failure.filter(|f| !f.synced).map(|f| (*ino, f.errno)))
            .collect();
        all.sort_unstable();
        all
    }

    /// Entries held: `(handles, files, unreported failures of closed files)`.
    #[cfg(test)]
    fn sizes(&self) -> (usize, usize, usize) {
        let s = self.lock().expect("not poisoned");
        (s.handles.len(), s.files.len(), s.closed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fsync of handle `id`, with nothing to wait for.
    fn fsync(h: &Handles, id: u64) -> Result<(), Errno> {
        let (ino, from) = h.sync_from(id)?;
        h.report_sync(ino, from, Some(id))
    }

    #[test]
    fn every_handle_open_at_a_failure_reports_it_once_at_its_own_fsync() {
        let h = Handles::default();
        let (a, b) = (h.open(1, true).unwrap(), h.open(1, false).unwrap());
        let other = h.open(2, true).unwrap();
        fsync(&h, a).unwrap();
        h.fail(1, libc::ENOSPC);
        assert_eq!(fsync(&h, a), Err(libc::ENOSPC));
        assert_eq!(fsync(&h, a), Ok(()));
        assert_eq!(fsync(&h, b), Err(libc::ENOSPC));
        assert_eq!(fsync(&h, b), Ok(()));
        assert_eq!(fsync(&h, other), Ok(()));
        // A later failure is due again; two before one fsync make one report, of the latest.
        h.fail(1, libc::EIO);
        h.fail(1, libc::EDQUOT);
        assert_eq!(fsync(&h, a), Err(libc::EDQUOT));
        assert_eq!(fsync(&h, a), Ok(()));
    }

    #[test]
    fn fsyncs_of_one_handle_that_overlap_all_report() {
        let h = Handles::default();
        let a = h.open(1, true).unwrap();
        let (first, second) = (h.sync_from(a).unwrap(), h.sync_from(a).unwrap());
        h.fail(1, libc::EIO);
        assert_eq!(h.report_sync(first.0, first.1, Some(a)), Err(libc::EIO));
        assert_eq!(h.report_sync(second.0, second.1, Some(a)), Err(libc::EIO));
        assert_eq!(fsync(&h, a), Ok(()));
    }

    #[test]
    fn close_tells_writers_open_at_the_failure_and_uses_nothing_up() {
        let h = Handles::default();
        let w = h.open(1, true).unwrap();
        let r = h.open(1, false).unwrap();
        h.fail(1, libc::ENOSPC);
        // Any number of closes of duplicated descriptors, then still the fsync.
        assert_eq!(h.report_close(w), Err(libc::ENOSPC));
        assert_eq!(h.report_close(w), Err(libc::ENOSPC));
        // A reader lost nothing.
        assert_eq!(h.report_close(r), Ok(()));
        assert_eq!(fsync(&h, w), Err(libc::ENOSPC));
        assert_eq!(h.report_close(w), Ok(()));
        // Opened afterwards: nothing to say at close.
        let late = h.open(1, true).unwrap();
        assert_eq!(h.report_close(late), Ok(()));
    }

    #[test]
    fn a_failure_nobody_synced_is_told_to_a_later_opener_until_somebody_has() {
        let h = Handles::default();
        let w = h.open(1, true).unwrap();
        h.fail(1, libc::EIO);
        assert_eq!(h.report_close(w), Err(libc::EIO));
        assert!(h.release(w).unwrap());
        let reader = h.open(1, false).unwrap();
        assert_eq!(h.report_close(reader), Ok(()));
        let again = h.open(1, true).unwrap();
        // Close is about its own lifetime; fsync also about what nobody has synced.
        assert_eq!(h.report_close(again), Ok(()));
        assert_eq!(h.refuse_write(again), Ok(()));
        assert_eq!(fsync(&h, again), Err(libc::EIO));
        // Open before that fsync: still told. Opened after it: clean.
        assert_eq!(fsync(&h, reader), Err(libc::EIO));
        let late = h.open(1, true).unwrap();
        assert_eq!(fsync(&h, late), Ok(()));
        assert!(h.unreported().is_empty());
    }

    #[test]
    fn an_fsync_by_inode_tells_what_nobody_synced_and_takes_nothing_from_handles() {
        let h = Handles::default();
        let a = h.open(1, true).unwrap();
        h.fail(1, libc::EDQUOT);
        let by_ino = |ino| h.report_sync(ino, h.sync_start(ino).unwrap(), None);
        assert_eq!(h.unsynced(1), Err(libc::EDQUOT));
        assert_eq!(by_ino(1), Err(libc::EDQUOT));
        assert_eq!(by_ino(1), Ok(()));
        assert_eq!(h.unsynced(1), Ok(()));
        assert_eq!(fsync(&h, a), Err(libc::EDQUOT));
    }

    #[test]
    fn one_write_per_failure_is_refused_and_the_reports_stay_due() {
        let h = Handles::default();
        let (a, b) = (h.open(1, true).unwrap(), h.open(1, true).unwrap());
        h.fail(1, libc::ENOSPC);
        assert_eq!(h.refuse_write(a), Err(libc::ENOSPC));
        assert_eq!(h.refuse_write(a), Ok(()));
        assert_eq!(h.refuse_write(b), Err(libc::ENOSPC));
        assert_eq!(fsync(&h, a), Err(libc::ENOSPC));
        h.fail(1, libc::EIO);
        assert_eq!(h.refuse_write(a), Err(libc::EIO));
    }

    #[test]
    fn state_is_dropped_with_the_last_handle_once_synced() {
        let h = Handles::default();
        let (a, b) = (h.open(1, true).unwrap(), h.open(1, true).unwrap());
        assert!(!h.release(a).unwrap());
        assert!(h.release(b).unwrap());
        assert_eq!(h.sizes(), (0, 0, 0));
        assert_eq!(h.release(b), Err(libc::EBADF));
        let a = h.open(1, true).unwrap();
        h.fail(1, libc::EIO);
        h.fail(1, libc::EIO);
        h.release(a).unwrap();
        assert_eq!(h.sizes(), (0, 1, 1));
        let a = h.open(1, true).unwrap();
        assert_eq!(h.sizes(), (1, 1, 0));
        assert_eq!(fsync(&h, a), Err(libc::EIO));
        h.release(a).unwrap();
        assert_eq!(h.sizes(), (0, 0, 0));
    }

    #[test]
    fn failures_of_closed_files_are_bounded_and_open_ones_are_kept() {
        let h = Handles::default();
        let held = h.open(0, true).unwrap();
        h.fail(0, libc::EIO);
        for ino in 1..=MAX_CLOSED_FAILURES as u64 + 10 {
            h.fail(ino, libc::EIO);
            h.fail(ino, libc::EIO);
        }
        let bound = MAX_CLOSED_FAILURES;
        assert_eq!(h.sizes(), (1, bound + 1, bound));
        // The oldest went; the newest and the open one remain.
        assert_eq!(h.unsynced(10), Ok(()));
        assert_eq!(h.unsynced(11), Err(libc::EIO));
        assert_eq!(fsync(&h, held), Err(libc::EIO));
    }
}
