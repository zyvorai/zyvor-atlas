// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    device::{DataSync, DeviceSync},
    durable::sync_dir,
    metrics::{Histogram, HistogramSnapshot},
    sync_guard::SyncGuard,
};

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("wal io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wal decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("wal index regression: expected > {last}, got {next}")]
    IndexRegression { last: u64, next: u64 },
    #[error("wal corrupt at line {line}: {reason}")]
    Corrupt { line: usize, reason: String },
    #[error("wal failed earlier, refusing writes until it is reopened: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalRecord<T> {
    pub term: u64,
    pub index: u64,
    pub command: T,
}

#[derive(Deserialize)]
struct IndexOnly {
    index: u64,
}

/// The log. After any failed write or sync it refuses every later one until it is reopened: a
/// failed `fdatasync` is reported once and leaves the unwritten records in the page cache marked
/// clean, so syncing again would succeed without them being on disk.
///
/// A sync may also run on another thread ([`Wal::begin_sync`]) while the log is appended to and
/// synced here. Every sync of the file goes through one [`SyncGuard`], because the kernel reports
/// a writeback error to only one of the calls on an open file.
#[derive(Debug)]
pub struct Wal {
    dir: PathBuf,
    path: PathBuf,
    file: Arc<File>,
    last_index: u64,
    /// Highest index known to be on disk (written by an fsynced append or a rewrite).
    synced_index: u64,
    records: u64,
    /// The fsyncs of appended records (rewrites of the file are not among them). Behind a
    /// pointer, which also keeps the log, which the engine holds in an enum, small.
    syncs: Arc<Histogram>,
    /// What `file` is flushed with.
    sync: Arc<dyn DeviceSync>,
    /// Orders the answers of syncs that overlap, and keeps the first one's failure.
    guard: Arc<SyncGuard>,
    /// Raised whenever the file is replaced, which cuts or renumbers what a sync begun earlier
    /// covered.
    generation: u64,
    /// The first failed write or sync.
    failed: Option<String>,
}

/// A sync of every record the log held when [`Wal::begin_sync`] made this, to be run without the
/// log: records may be appended meanwhile.
#[derive(Debug)]
pub struct LogSync {
    file: Arc<File>,
    syncs: Arc<Histogram>,
    sync: Arc<dyn DeviceSync>,
    guard: Arc<SyncGuard>,
    through: u64,
    generation: u64,
}

impl LogSync {
    /// The last index this sync covers.
    pub fn through(&self) -> u64 {
        self.through
    }

    /// Waits for the disk. The result goes to [`Wal::finish_sync`]; until then the log does not
    /// count anything this covered.
    pub fn run(self) -> LogSynced {
        let result = self
            .guard
            .run(|| self.syncs.time(|| self.sync.sync_data(&self.file)));
        LogSynced {
            through: self.through,
            generation: self.generation,
            result,
        }
    }
}

/// What a [`LogSync`] did.
#[derive(Debug)]
pub struct LogSynced {
    through: u64,
    generation: u64,
    result: io::Result<()>,
}

impl Wal {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, WalError> {
        let dir = root.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let path = dir.join("metadata.wal");
        let (lines, torn) = scan(&path)?;
        let file = Arc::new(open_append(&path)?);
        let mut wal = Self {
            dir,
            path,
            file,
            last_index: lines.last().map(|(i, _)| *i).unwrap_or(0),
            synced_index: lines.last().map(|(i, _)| *i).unwrap_or(0),
            records: lines.len() as u64,
            syncs: Arc::default(),
            sync: Arc::new(DataSync),
            guard: Arc::new(SyncGuard::new("metadata log")),
            generation: 0,
            failed: None,
        };
        // What was just read may exist only in the page cache: a sync that failed before a
        // restart leaves its records there. Writing them to a new file makes them durable, as
        // `synced_index` claims they are. A torn final line was never fsynced as a whole record,
        // so it was never acknowledged, and is dropped.
        if torn || wal.records > 0 {
            wal.rewrite(|_| true)?;
        }
        Ok(wal)
    }

    pub fn append<T: Serialize>(&mut self, record: &WalRecord<T>) -> Result<(), WalError> {
        self.append_unsynced(record)?;
        self.sync()
    }

    /// Writes the record without waiting for the disk; it is durable once [`Self::sync`] returns
    /// (or `synced_index()` covers it). A crash before that may lose it, never tear it.
    pub fn append_unsynced<T: Serialize>(&mut self, record: &WalRecord<T>) -> Result<(), WalError> {
        self.check()?;
        if record.index <= self.last_index {
            return Err(WalError::IndexRegression {
                last: self.last_index,
                next: record.index,
            });
        }
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        // A partial line must not end up in front of a later record.
        (&*self.file)
            .write_all(&line)
            .map_err(|e| self.fail(e.into()))?;
        self.last_index = record.index;
        self.records += 1;
        Ok(())
    }

    /// Flushes every unsynced append with one fsync; a no-op when there is none.
    pub fn sync(&mut self) -> Result<(), WalError> {
        self.check()?;
        if self.synced_index < self.last_index {
            self.guard
                .run(|| self.syncs.time(|| self.sync.sync_data(&self.file)))
                .map_err(|e| self.fail(e.into()))?;
            self.synced_index = self.last_index;
        }
        Ok(())
    }

    /// A sync of the unsynced appends for another thread to run, or `None` when there are none.
    pub fn begin_sync(&self) -> Result<Option<LogSync>, WalError> {
        self.check()?;
        Ok((self.synced_index < self.last_index).then(|| LogSync {
            file: self.file.clone(),
            syncs: self.syncs.clone(),
            sync: self.sync.clone(),
            guard: self.guard.clone(),
            through: self.last_index,
            generation: self.generation,
        }))
    }

    /// Counts what a [`LogSync`] of this log made durable. A failed one fails the log. One whose
    /// file has been replaced since counts for nothing: the records at those indexes may be
    /// others now.
    pub fn finish_sync(&mut self, synced: LogSynced) -> Result<(), WalError> {
        synced.result.map_err(|e| self.fail(e.into()))?;
        self.check()?;
        if synced.generation == self.generation {
            self.synced_index = self.synced_index.max(synced.through.min(self.last_index));
        }
        Ok(())
    }

    /// Errors once a write or sync of the log has failed, including one that ran elsewhere and
    /// has not been reported to [`Self::finish_sync`] yet.
    pub fn check(&self) -> Result<(), WalError> {
        match self.failed.clone().or_else(|| self.guard.failure()) {
            None => Ok(()),
            Some(cause) => Err(WalError::Failed(cause)),
        }
    }

    fn fail(&mut self, error: WalError) -> WalError {
        self.failed.get_or_insert_with(|| error.to_string());
        error
    }

    pub fn synced_index(&self) -> u64 {
        self.synced_index
    }

    /// Count and latency of the fsyncs of appended records since the log was opened.
    pub fn sync_latency(&self) -> HistogramSnapshot {
        self.syncs.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn set_sync(&mut self, sync: Box<dyn DeviceSync>) {
        self.sync = Arc::from(sync);
    }

    pub fn replay<T: DeserializeOwned>(&self) -> Result<Vec<WalRecord<T>>, WalError> {
        let (lines, _) = scan(&self.path)?;
        lines
            .iter()
            .map(|(_, l)| serde_json::from_str(l).map_err(WalError::from))
            .collect()
    }

    pub fn last_index(&self) -> u64 {
        self.last_index
    }

    /// Number of records currently retained in the log file.
    pub fn len(&self) -> u64 {
        self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// Keeps `last_index` at or above a checkpointed index, so indexes stay monotonic even when
    /// compaction has removed every record from the file.
    pub fn raise_floor(&mut self, index: u64) {
        self.last_index = self.last_index.max(index);
        self.synced_index = self.synced_index.max(index);
    }

    /// Drops every record with `index <= through`. Only safe once state covering `through` has
    /// been durably checkpointed.
    pub fn compact_through(&mut self, through: u64) -> Result<u64, WalError> {
        self.rewrite(|i| i > through)
    }

    /// Drops every record with `index > after` (Raft log-conflict resolution).
    pub fn truncate_after(&mut self, after: u64) -> Result<u64, WalError> {
        let removed = self.rewrite(|i| i <= after)?;
        self.last_index = after;
        self.synced_index = after;
        Ok(removed)
    }

    /// Empties the log and restarts indexing just above `floor` (snapshot installation).
    pub fn reset(&mut self, floor: u64) -> Result<(), WalError> {
        self.rewrite(|_| false)?;
        self.last_index = floor;
        self.synced_index = floor;
        Ok(())
    }

    /// Replaces the file with one holding the records `keep` selects. A failure part-way (the
    /// new file may be in place while `file` still names the old one) fails the log.
    fn rewrite(&mut self, keep: impl Fn(u64) -> bool) -> Result<u64, WalError> {
        // A sync running on the old file may be about to report that its data was lost.
        self.guard.drain().map_err(|e| self.fail(e.into()))?;
        self.check()?;
        self.replace_file(keep).map_err(|e| self.fail(e))
    }

    fn replace_file(&mut self, keep: impl Fn(u64) -> bool) -> Result<u64, WalError> {
        let (lines, _) = scan(&self.path)?;
        let tmp = self.dir.join("metadata.wal.tmp");
        let mut kept = 0u64;
        {
            let mut f = File::create(&tmp)?;
            for (i, l) in &lines {
                if keep(*i) {
                    f.write_all(l.as_bytes())?;
                    f.write_all(b"\n")?;
                    kept += 1;
                }
            }
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        sync_dir(&self.dir)?;
        self.file = Arc::new(open_append(&self.path)?);
        self.generation += 1;
        self.synced_index = self.last_index;
        let removed = self.records.saturating_sub(kept);
        self.records = kept;
        Ok(removed)
    }
}

fn open_append(path: &Path) -> Result<File, WalError> {
    Ok(OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?)
}

/// Returns every complete `(index, raw line)` and whether the final line was torn.
fn scan(path: &Path) -> Result<(Vec<(u64, String)>, bool), WalError> {
    if !path.exists() {
        return Ok((Vec::new(), false));
    }
    let raw: Vec<String> = BufReader::new(File::open(path)?)
        .lines()
        .collect::<Result<_, _>>()?;
    let last_non_empty = raw.iter().rposition(|l| !l.trim().is_empty());
    let mut out = Vec::with_capacity(raw.len());
    let mut torn = false;
    for (n, line) in raw.into_iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<IndexOnly>(&line) {
            Ok(r) => {
                if let Some((prev, _)) = out.last() {
                    if r.index <= *prev {
                        return Err(WalError::Corrupt {
                            line: n + 1,
                            reason: format!("index {} after {prev}", r.index),
                        });
                    }
                }
                out.push((r.index, line));
            }
            Err(_) if Some(n) == last_non_empty => torn = true,
            Err(e) => {
                return Err(WalError::Corrupt {
                    line: n + 1,
                    reason: e.to_string(),
                })
            }
        }
    }
    Ok((out, torn))
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn rec(index: u64) -> WalRecord<String> {
        WalRecord {
            term: 1,
            index,
            command: format!("c{index}"),
        }
    }

    #[test]
    fn unsynced_appends_are_covered_by_one_sync() {
        let td = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(td.path()).unwrap();
        wal.append(&rec(1)).unwrap();
        assert_eq!(wal.synced_index(), 1);
        assert_eq!(wal.sync_latency().count(), 1);
        wal.append_unsynced(&rec(2)).unwrap();
        wal.append_unsynced(&rec(3)).unwrap();
        assert_eq!((wal.last_index(), wal.synced_index()), (3, 1));
        wal.sync().unwrap();
        assert_eq!(wal.synced_index(), 3);
        // A sync with nothing to flush is not counted.
        wal.sync().unwrap();
        assert_eq!(wal.sync_latency().count(), 2);

        wal.append_unsynced(&rec(4)).unwrap();
        wal.truncate_after(2).unwrap();
        assert_eq!((wal.last_index(), wal.synced_index()), (2, 2));
        drop(wal);
        let wal = Wal::open(td.path()).unwrap();
        let replayed: Vec<u64> = wal
            .replay::<String>()
            .unwrap()
            .iter()
            .map(|r| r.index)
            .collect();
        assert_eq!(replayed, [1, 2]);
        assert_eq!(wal.synced_index(), 2);
    }

    #[test]
    fn a_sync_run_elsewhere_counts_only_what_it_began_with() {
        let td = tempfile::tempdir().unwrap();
        let mut wal = Wal::open(td.path()).unwrap();
        assert!(wal.begin_sync().unwrap().is_none());
        wal.append_unsynced(&rec(1)).unwrap();
        wal.append_unsynced(&rec(2)).unwrap();
        let sync = wal.begin_sync().unwrap().unwrap();
        wal.append_unsynced(&rec(3)).unwrap();
        assert_eq!(sync.through(), 2);
        let synced = thread::spawn(move || sync.run()).join().unwrap();
        assert_eq!(wal.synced_index(), 0, "counted before it was reported");
        wal.finish_sync(synced).unwrap();
        assert_eq!(wal.synced_index(), 2);
        assert_eq!(wal.sync_latency().count(), 1);

        // The file is replaced and index 3 written again before the sync is reported: it says
        // nothing about the new record.
        let sync = wal.begin_sync().unwrap().unwrap();
        wal.truncate_after(2).unwrap();
        wal.append_unsynced(&rec(3)).unwrap();
        wal.finish_sync(sync.run()).unwrap();
        assert_eq!((wal.last_index(), wal.synced_index()), (3, 2));
        wal.sync().unwrap();
        assert_eq!(wal.synced_index(), 3);
    }
}
