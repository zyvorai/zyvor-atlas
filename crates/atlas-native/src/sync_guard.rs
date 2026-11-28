// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Fail-stop for a file that several threads write and sync at once.
//!
//! Linux reports a writeback error to one sync call on an open file description and leaves the
//! failed pages marked clean: the next call returns 0 although the data never reached the disk
//! (<https://docs.kernel.org/filesystems/vfs.html>, "Handling errors during writeback"). A write
//! through `O_DSYNC` ends in the same check. So when writers share one description, a writer's
//! own successful return proves nothing while another writer's call may have been the one told
//! about the writer's data.
//!
//! [`SyncGuard::run`] therefore acknowledges a writer only once every call that began before the
//! writer's own call returned has itself returned, and none has failed. A call that was told
//! about the writer's data did so before the writer's call checked for errors, so it began before
//! that call returned and is among those waited for. Waiting for it to *return* matters: looking
//! at a "failed" flag as soon as the own call returns would miss a thread that already holds the
//! error and has not recorded it yet. Calls run concurrently; only acknowledgements wait.
//!
//! The first failure is permanent: the file accepts no further writes until it is reopened.

use std::{
    collections::BTreeSet,
    fmt, io,
    panic::{self, AssertUnwindSafe},
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
};

#[derive(Debug, Default)]
struct State {
    /// The ticket the next call takes.
    next: u64,
    /// Tickets of the calls that have begun and not returned.
    running: BTreeSet<u64>,
    /// The first failure.
    failed: Option<(io::ErrorKind, String)>,
}

pub(crate) struct SyncGuard {
    /// What the file is called in logs and errors.
    name: String,
    state: Mutex<State>,
    /// Signalled when a call returns.
    returned: Condvar,
}

impl fmt::Debug for SyncGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncGuard")
            .field("name", &self.name)
            .field("failure", &self.failure())
            .finish()
    }
}

impl SyncGuard {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: Mutex::default(),
            returned: Condvar::new(),
        }
    }

    /// Nothing panics while holding the lock, and a failure must stay readable if something did.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Why the file refuses writes, if it does.
    pub fn failure(&self) -> Option<String> {
        self.lock().failed.as_ref().map(|(_, cause)| cause.clone())
    }

    /// Errors once any call has failed.
    pub fn check(&self) -> io::Result<()> {
        Self::refused(&self.name, &self.lock())
    }

    fn refused(name: &str, state: &State) -> io::Result<()> {
        match &state.failed {
            None => Ok(()),
            Some((kind, cause)) => Err(io::Error::new(
                *kind,
                format!("{name} failed, no writes until it is reopened: {cause}"),
            )),
        }
    }

    fn record(name: &str, state: &mut State, e: &io::Error) {
        if state.failed.is_none() {
            let cause = e.to_string();
            eprintln!("atlas-native: {name} failed, refusing writes until it is reopened: {cause}");
            state.failed = Some((e.kind(), cause));
        }
    }

    /// Records a failure found by a sync that [`Self::run`] did not make: one on another open
    /// file description of the same file. Writers waiting to be acknowledged are refused too.
    pub fn fail(&self, error: &io::Error) {
        Self::record(&self.name, &mut self.lock(), error);
        self.returned.notify_all();
    }

    /// Runs `sync`, a call that makes data written to the file durable (`fdatasync`, or a write
    /// through `O_DSYNC`), for data this thread wrote before it or writes within it. `Ok` means
    /// that data is durable.
    pub fn run<T>(&self, sync: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let ticket = {
            let mut state = self.lock();
            Self::refused(&self.name, &state)?;
            let ticket = state.next;
            state.next += 1;
            state.running.insert(ticket);
            ticket
        };
        // A call that panics must not leave the others waiting for it, nor count as a success.
        let result = panic::catch_unwind(AssertUnwindSafe(sync))
            .unwrap_or_else(|_| Err(io::Error::other("sync panicked")));

        let mut state = self.lock();
        state.running.remove(&ticket);
        self.returned.notify_all();
        let value = match result {
            Ok(value) => value,
            Err(e) => {
                Self::record(&self.name, &mut state, &e);
                return Err(e);
            }
        };
        // Every call that began before this one returned has a ticket below `horizon`.
        let horizon = state.next;
        while state.failed.is_none() && state.running.first().is_some_and(|&t| t < horizon) {
            state = self
                .returned
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Self::refused(&self.name, &state)?;
        Ok(value)
    }
}
