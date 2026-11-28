// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Preparing a [`FileDevice`]'s space ahead of its appends (`data_node.preallocate`, off by
//! default; `docs/NATIVE_NODE.md`, "Preallocation").
//!
//! An append to a growing file changes the file's size and allocates blocks, so the `fdatasync`
//! that acknowledges it has to commit filesystem metadata as well as the data. Overwriting
//! blocks that were written before needs neither. A background thread therefore writes zeros past
//! the append position, a step at a time, and syncs them. `fallocate` is not enough: it leaves
//! unwritten extents, and the first write to one is a metadata change again.
//!
//! # What the file means
//!
//! Nothing new is stored on disk. The one rule recovery relies on is unchanged: **no byte at or
//! beyond the file's length was ever handed out**, so opening a device appends from there,
//! whether or not preparation was on before, is on now, or was interrupted half way. While a
//! preparing device is open its length runs ahead of the append position, and [`Tail`] (not the
//! file length) says how much is in use.
//!
//! - A clean stop ([`Tail::seal`]: dropping the device, or the node binary on `SIGTERM`) cuts the
//!   file back to the append position, so nothing is lost.
//! - Any other stop leaves the zeroed tail inside the file. Where the appends ended in it cannot
//!   be told from the bytes (data may end in zeros), so it counts as used and nothing reuses it:
//!   up to `ahead_bytes + step_bytes` per device and unclean stop. Preparation starts only after
//!   the first append since opening, so a node that restarts without writes loses nothing more.
//!
//! # Not stalling writers, not hiding errors
//!
//! Zeros are only ever written past the append position. The preparer claims a range, and an
//! append that would reach into it waits until the zeros are *written* (copied to the page cache,
//! not synced). Appends never wait for the preparer's sync. When appends have caught up with the
//! prepared space, the preparer skips one step ahead of them rather than racing at the frontier.
//!
//! The preparer syncs through an open file description of its own, so it cannot consume a
//! writeback error that a writer's `fdatasync` has to see: Linux reports one to every description
//! that was open when it happened (`sync_guard.rs` is about the writers, which share one). Its
//! syncs are therefore not ones a writer's acknowledgement waits for. If one fails the device
//! fails exactly as after a failed foreground sync: no more writes until it is reopened.
//!
//! No write's durability depends on preparation: every acknowledged write is synced as before.

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::Deserialize;

use crate::{
    device::{BlockStore, DataSync, DeviceSync, FileDevice},
    engine::NativeError,
    metrics::PromText,
    raw::{open_store, DeviceBackend},
    sync_guard::SyncGuard,
};

/// Ranges are zeroed from and in multiples of this.
const PAGE: u64 = 4096;
const MAX_STEP_BYTES: u64 = 16 << 20;
const MAX_AHEAD_BYTES: u64 = 4 << 30;
/// How long the preparer pauses after it could not write (a full filesystem, say).
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// `data_node.preallocate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PreallocConfig {
    /// Off by default. `file` devices only.
    pub enabled: bool,
    /// Zeroed space to keep ahead of each device's append position.
    pub ahead_bytes: u64,
    /// How much is zeroed and synced at a time.
    pub step_bytes: u64,
}

impl Default for PreallocConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ahead_bytes: 64 << 20,
            step_bytes: 1 << 20,
        }
    }
}

impl PreallocConfig {
    /// What [`FileDevice::open`] uses: off, except under the `prealloc-test` feature, which runs
    /// the existing tests with small steps so that appends and preparation keep meeting.
    pub(crate) fn implicit() -> Self {
        if cfg!(feature = "prealloc-test") {
            Self {
                enabled: true,
                ahead_bytes: 256 << 10,
                step_bytes: 64 << 10,
            }
        } else {
            Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), NativeError> {
        let invalid = |m: String| Err(NativeError::Invalid(m));
        if self.step_bytes < PAGE
            || self.step_bytes > MAX_STEP_BYTES
            || !self.step_bytes.is_multiple_of(PAGE)
        {
            return invalid(format!(
                "preallocate.step_bytes must be a multiple of {PAGE} up to {MAX_STEP_BYTES}"
            ));
        }
        if self.ahead_bytes < 2 * self.step_bytes || self.ahead_bytes > MAX_AHEAD_BYTES {
            return invalid(format!(
                "preallocate.ahead_bytes must be at least twice step_bytes and at most \
                 {MAX_AHEAD_BYTES}"
            ));
        }
        Ok(())
    }
}

/// A preparing device's counters since it was opened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PreallocStats {
    /// Zeroed bytes past the append position now.
    pub ahead_bytes: u64,
    /// Zero bytes written and synced.
    pub prepared_bytes: u64,
    /// Appends that landed wholly in zeroed, synced space.
    pub appends_prepared: u64,
    /// Appends that did not.
    pub appends_unprepared: u64,
    /// Appends that waited for a range to be zeroed.
    pub append_waits: u64,
    /// Writes and syncs of the preparer that failed. A failed sync also fails the device.
    pub failures: u64,
}

#[derive(Debug, Default)]
struct State {
    /// The next append offset: everything below has been handed out.
    end: u64,
    /// Zeroed bytes to keep past `end`; 0 while nothing prepares this file.
    ahead: u64,
    /// Start of the range being zeroed right now. No append may reach into it.
    claim: Option<u64>,
    /// Appends waiting for `claim`; no new range is claimed while there are any.
    waiting: usize,
    /// End of the last range zeroed (0: none yet).
    zeroed: u64,
    /// The latest run of ranges that are zeroed and synced.
    ready: (u64, u64),
    /// The furthest this file may have been extended by preparation.
    furthest: u64,
    /// An append was made since the file was opened.
    appended: bool,
    /// Sealed: no more appends, no more preparation.
    sealed: bool,
    stats: PreallocStats,
}

/// A file device's append position, shared with its preparer.
#[derive(Debug)]
pub(crate) struct Tail {
    state: Mutex<State>,
    /// The preparer waits here for work.
    work: Condvar,
    /// Appends (and [`Tail::seal`]) wait here for the claimed range.
    released: Condvar,
}

impl Tail {
    pub fn new(end: u64) -> Self {
        Self {
            state: Mutex::new(State {
                end,
                ..State::default()
            }),
            work: Condvar::new(),
            released: Condvar::new(),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, NativeError> {
        self.state
            .lock()
            .map_err(|_| NativeError::Poisoned("device"))
    }

    /// For the preparer and the clean stop: nothing panics while holding the lock.
    fn lock_anyway(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn end(&self) -> Result<u64, NativeError> {
        Ok(self.lock()?.end)
    }

    /// Hands out `len` bytes at the append position.
    pub fn reserve(&self, len: u64) -> Result<u64, NativeError> {
        let mut s = self.lock()?;
        if s.claim.is_some_and(|from| s.end + len > from) {
            s.waiting += 1;
            s.stats.append_waits += 1;
            while s.claim.is_some_and(|from| s.end + len > from) {
                s = self
                    .released
                    .wait(s)
                    .map_err(|_| NativeError::Poisoned("device"))?;
            }
            s.waiting -= 1;
        }
        if s.sealed {
            return Err(NativeError::Invalid("device is closing".into()));
        }
        let off = s.end;
        s.end += len;
        if s.ahead > 0 {
            if off >= s.ready.0 && off + len <= s.ready.1 {
                s.stats.appends_prepared += 1;
            } else {
                s.stats.appends_unprepared += 1;
            }
            s.appended = true;
            if s.waiting == 0 && s.zeroed < s.end + s.ahead {
                self.work.notify_one();
            }
        }
        Ok(off)
    }

    /// Gives back the range of an append that failed, unless a later one took the space after it.
    pub fn unreserve(&self, off: u64, len: u64) {
        if let Ok(mut s) = self.state.lock() {
            if s.end == off + len {
                s.end = off;
            }
        }
    }

    /// Stops appends and preparation, and cuts `file` back to the append position unless
    /// `keep_length` (after a failed sync the file is left alone). Appends already handed their
    /// space are below that position and unaffected. Does nothing on a file nothing prepares.
    pub fn seal(&self, file: &File, keep_length: bool) -> io::Result<()> {
        let mut s = self.lock_anyway();
        if s.ahead == 0 {
            return Ok(());
        }
        s.sealed = true;
        self.work.notify_all();
        while s.claim.is_some() {
            s = self
                .released
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if keep_length {
            return Ok(());
        }
        let len = file.metadata()?.len();
        // A longer file than preparation can account for is not ours to cut.
        if len <= s.end || len > s.furthest {
            return Ok(());
        }
        // Not synced: a cut that is lost only leaves the file longer, which costs the space
        // and nothing else. A sync here, on the writers' description, could also be the one
        // told about their data's writeback error.
        file.set_len(s.end)
    }

    fn stats(&self) -> PreallocStats {
        let s = self.lock_anyway();
        PreallocStats {
            ahead_bytes: s.zeroed.saturating_sub(s.end),
            ..s.stats
        }
    }
}

/// A claimed range, released when dropped (also if the write panics).
struct Claim<'a> {
    tail: &'a Tail,
    zeroed: Option<u64>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut s = self.tail.lock_anyway();
        s.claim = None;
        if let Some(to) = self.zeroed {
            s.zeroed = to;
        }
        self.tail.released.notify_all();
    }
}

/// The background thread preparing one file.
#[derive(Debug)]
pub(crate) struct Preparer {
    tail: Arc<Tail>,
    thread: Option<JoinHandle<()>>,
}

struct Work {
    tail: Arc<Tail>,
    /// An open file description of its own (see the module docs).
    file: File,
    sync: Arc<dyn DeviceSync>,
    guard: Arc<SyncGuard>,
    name: String,
    step: u64,
}

impl Preparer {
    /// Opens `path` a second time and starts preparing it ahead of `tail`.
    pub fn start(
        path: &Path,
        tail: Arc<Tail>,
        sync: Arc<dyn DeviceSync>,
        guard: Arc<SyncGuard>,
        cfg: PreallocConfig,
    ) -> Result<Self, NativeError> {
        cfg.validate()?;
        let file = OpenOptions::new().write(true).open(path)?;
        if !file.metadata()?.is_file() {
            return Err(NativeError::Invalid(format!(
                "preallocation needs a regular file, {} is not one",
                path.display()
            )));
        }
        let work = Work {
            tail: tail.clone(),
            file,
            sync,
            guard,
            name: path.display().to_string(),
            step: cfg.step_bytes,
        };
        tail.lock()?.ahead = cfg.ahead_bytes;
        let thread = thread::Builder::new()
            .name("atlas-prealloc".into())
            .spawn(move || work.run())?;
        Ok(Self {
            tail,
            thread: Some(thread),
        })
    }

    pub fn stats(&self) -> PreallocStats {
        self.tail.stats()
    }
}

impl Drop for Preparer {
    fn drop(&mut self) {
        self.tail.lock_anyway().sealed = true;
        self.tail.work.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Work {
    /// The next range to zero, claimed; `None` once there is nothing left to do for good.
    fn claim(&self) -> Option<u64> {
        let mut s = self.tail.lock_anyway();
        loop {
            if s.sealed || self.guard.failure().is_some() {
                return None;
            }
            if s.appended && s.waiting == 0 && s.zeroed < s.end + s.ahead {
                break;
            }
            s = self
                .tail
                .work
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
        // Carry on from the last range while it is a step ahead of the appends; otherwise skip
        // ahead of them, so that an append rarely meets the range being zeroed.
        let from = if s.zeroed >= s.end + self.step {
            s.zeroed
        } else {
            (s.end + self.step).next_multiple_of(PAGE)
        };
        s.claim = Some(from);
        s.furthest = s.furthest.max(from + self.step);
        Some(from)
    }

    fn run(self) {
        let zeros = vec![0u8; self.step as usize];
        let mut failing = false;
        while let Some(from) = self.claim() {
            let to = from + self.step;
            let mut claim = Claim {
                tail: &self.tail,
                zeroed: None,
            };
            let written = self.file.write_all_at(&zeros, from);
            claim.zeroed = written.is_ok().then_some(to);
            drop(claim);
            if let Err(e) = written {
                // Nothing was lost: appends go on without prepared space.
                if !failing {
                    eprintln!(
                        "atlas-native: cannot preallocate {} at {from}, retrying: {e}",
                        self.name
                    );
                    failing = true;
                }
                let mut s = self.tail.lock_anyway();
                s.stats.failures += 1;
                drop(
                    self.tail
                        .work
                        .wait_timeout_while(s, RETRY_AFTER, |s| !s.sealed)
                        .unwrap_or_else(PoisonError::into_inner),
                );
                continue;
            }
            failing = false;
            let synced = panic::catch_unwind(AssertUnwindSafe(|| self.sync.sync_data(&self.file)))
                .unwrap_or_else(|_| Err(io::Error::other("sync panicked")));
            if let Err(e) = synced {
                // The error may be about writers' data as much as about the zeros.
                self.tail.lock_anyway().stats.failures += 1;
                self.guard.fail(&e);
                return;
            }
            let mut s = self.tail.lock_anyway();
            s.stats.prepared_bytes += self.step;
            s.ready = if from == s.ready.1 {
                (s.ready.0, to)
            } else {
                (from, to)
            };
        }
    }
}

/// Opens one of a data node's devices, preparing it ahead if `cfg` says so.
pub(crate) fn open_device(
    path: &Path,
    backend: DeviceBackend,
    hwm_dir: &Path,
    cfg: &PreallocConfig,
) -> Result<Arc<dyn BlockStore>, NativeError> {
    if !cfg.enabled {
        return open_store(path, backend, hwm_dir);
    }
    if backend != DeviceBackend::File {
        return Err(NativeError::Invalid(
            "preallocation applies to the file backend only".into(),
        ));
    }
    Ok(Arc::new(FileDevice::open_with_config(
        path,
        Box::new(DataSync),
        *cfg,
    )?))
}

/// Adds a data node's preallocation metrics; nothing if no device prepares ahead.
pub(crate) fn render_metrics(p: &mut PromText, node: &str, devices: &[Arc<dyn BlockStore>]) {
    let stats: Vec<(String, PreallocStats)> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| Some((i.to_string(), d.prealloc_stats()?)))
        .collect();
    if stats.is_empty() {
        return;
    }
    type Get = fn(&PreallocStats) -> u64;
    let families: [(&str, &str, &str, Get); 4] = [
        (
            "atlas_native_data_prealloc_ahead_bytes",
            "gauge",
            "Zeroed bytes past each device's append position.",
            |s| s.ahead_bytes,
        ),
        (
            "atlas_native_data_prealloc_written_bytes_total",
            "counter",
            "Zero bytes written and synced ahead of appends.",
            |s| s.prepared_bytes,
        ),
        (
            "atlas_native_data_prealloc_append_waits_total",
            "counter",
            "Appends that waited for a range to be zeroed.",
            |s| s.append_waits,
        ),
        (
            "atlas_native_data_prealloc_failures_total",
            "counter",
            "Failed writes and syncs of the preparer.",
            |s| s.failures,
        ),
    ];
    for (name, kind, help, get) in families {
        p.family(name, kind, help);
        for (device, s) in &stats {
            p.sample(name, &[("node", node), ("device", device)], get(s));
        }
    }
    let name = "atlas_native_data_prealloc_appends_total";
    p.family(
        name,
        "counter",
        "Appends, by whether they landed wholly in prepared space.",
    );
    for (device, s) in &stats {
        for (prepared, n) in [
            ("true", s.appends_prepared),
            ("false", s.appends_unprepared),
        ] {
            let labels = [("node", node), ("device", device), ("prepared", prepared)];
            p.sample(name, &labels, n);
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

/// Catches `SIGTERM` and `SIGINT` so that [`stop_requested`] reports them. The node binary does
/// this when preallocation is on, to cut its device files back before it exits.
pub fn install_stop_signals() {
    // SAFETY: the handler only stores to an atomic, which is safe in a signal handler.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            libc::signal(signal, on_stop as *const () as libc::sighandler_t);
        }
    }
}

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Whether a signal caught by [`install_stop_signals`] has arrived.
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn preparing(end: u64) -> Tail {
        let t = Tail::new(end);
        t.lock_anyway().ahead = 1 << 20;
        t
    }

    #[test]
    fn an_append_waits_for_the_range_being_zeroed_and_no_other() {
        let t = preparing(0);
        t.lock_anyway().claim = Some(8192);
        // Ends where the claimed range begins.
        assert_eq!(t.reserve(8192).unwrap(), 0);
        thread::scope(|s| {
            let waiter = s.spawn(|| t.reserve(1).unwrap());
            wait_for("the append to wait", || t.lock_anyway().waiting == 1);
            thread::sleep(Duration::from_millis(50));
            assert!(
                !waiter.is_finished(),
                "an append went into a claimed range"
            );
            drop(Claim {
                tail: &t,
                zeroed: Some(12288),
            });
            assert_eq!(waiter.join().unwrap(), 8192);
        });
        let s = t.stats();
        assert_eq!((s.append_waits, s.ahead_bytes), (1, 12288 - 8193));
    }

    #[test]
    fn a_sealed_tail_waits_for_the_claim_then_cuts_the_file_and_refuses_appends() {
        let td = tempfile::tempdir().unwrap();
        let file = File::create(td.path().join("f")).unwrap();
        file.set_len(5000).unwrap();
        let t = preparing(100);
        {
            let mut s = t.lock_anyway();
            s.claim = Some(4096);
            s.furthest = 8192;
        }
        thread::scope(|s| {
            let sealing = s.spawn(|| t.seal(&file, false).unwrap());
            thread::sleep(Duration::from_millis(50));
            assert!(!sealing.is_finished(), "cut under a write in progress");
            drop(Claim {
                tail: &t,
                zeroed: None,
            });
            sealing.join().unwrap();
        });
        assert_eq!(file.metadata().unwrap().len(), 100);
        assert!(t.reserve(1).is_err());
    }

    #[test]
    fn sealing_leaves_a_file_it_cannot_account_for() {
        let td = tempfile::tempdir().unwrap();
        let file = File::create(td.path().join("f")).unwrap();
        for (keep, furthest) in [(true, 8192), (false, 4096)] {
            file.set_len(5000).unwrap();
            let t = preparing(100);
            t.lock_anyway().furthest = furthest;
            t.seal(&file, keep).unwrap();
            assert_eq!(file.metadata().unwrap().len(), 5000);
        }
        // Nothing prepares this one: appends go on.
        let t = Tail::new(100);
        t.seal(&file, false).unwrap();
        assert_eq!(t.reserve(1).unwrap(), 100);
    }

    #[test]
    fn a_failed_preparer_write_is_counted_and_does_not_fail_the_device() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("f");
        File::create(&path).unwrap();
        let tail = Arc::new(preparing(0));
        let guard = Arc::new(SyncGuard::new("test"));
        let work = Work {
            tail: tail.clone(),
            // Read-only: every write fails.
            file: File::open(&path).unwrap(),
            sync: Arc::new(DataSync),
            guard: guard.clone(),
            name: "test".into(),
            step: PAGE,
        };
        thread::scope(|s| {
            let running = s.spawn(move || work.run());
            assert_eq!(tail.reserve(10).unwrap(), 0);
            wait_for("the failure", || tail.stats().failures == 1);
            // The claim was released: appends of any size go on during the pause.
            assert_eq!(tail.reserve(1 << 20).unwrap(), 10);
            assert!(guard.failure().is_none());
            let file = File::open(&path).unwrap();
            // Ends the pause and the thread.
            tail.seal(&file, true).unwrap();
            running.join().unwrap();
        });
    }

    #[test]
    fn settings_are_checked() {
        let ok = PreallocConfig::default();
        assert!(!ok.enabled);
        ok.validate().unwrap();
        for (ahead_bytes, step_bytes, valid) in [
            (8192, 4096, true),
            (4096, 4096, false),
            (1 << 20, 0, false),
            (1 << 20, 4097, false),
            (64 << 20, 32 << 20, false),
            (8 << 30, 1 << 20, false),
        ] {
            let c = PreallocConfig {
                enabled: true,
                ahead_bytes,
                step_bytes,
            };
            assert_eq!(c.validate().is_ok(), valid, "{c:?}");
        }
    }
}
