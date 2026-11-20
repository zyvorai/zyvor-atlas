// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::durable::sync_dir;

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

#[derive(Debug)]
pub struct Wal {
    dir: PathBuf,
    path: PathBuf,
    file: File,
    last_index: u64,
    /// Highest index known to be on disk (written by an fsynced append or a rewrite).
    synced_index: u64,
    records: u64,
}

impl Wal {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, WalError> {
        let dir = root.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let path = dir.join("metadata.wal");
        let (lines, torn) = scan(&path)?;
        let file = open_append(&path)?;
        let mut wal = Self {
            dir,
            path,
            file,
            last_index: lines.last().map(|(i, _)| *i).unwrap_or(0),
            synced_index: lines.last().map(|(i, _)| *i).unwrap_or(0),
            records: lines.len() as u64,
        };
        if torn {
            // A torn final line was never fsynced as a whole record, so it was never acknowledged.
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
        if record.index <= self.last_index {
            return Err(WalError::IndexRegression {
                last: self.last_index,
                next: record.index,
            });
        }
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.last_index = record.index;
        self.records += 1;
        Ok(())
    }

    /// Flushes every unsynced append with one fsync; a no-op when there is none.
    pub fn sync(&mut self) -> Result<(), WalError> {
        if self.synced_index < self.last_index {
            self.file.sync_data()?;
            self.synced_index = self.last_index;
        }
        Ok(())
    }

    pub fn synced_index(&self) -> u64 {
        self.synced_index
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

    fn rewrite(&mut self, keep: impl Fn(u64) -> bool) -> Result<u64, WalError> {
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
        self.file = open_append(&self.path)?;
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
        wal.append_unsynced(&rec(2)).unwrap();
        wal.append_unsynced(&rec(3)).unwrap();
        assert_eq!((wal.last_index(), wal.synced_index()), (3, 1));
        wal.sync().unwrap();
        assert_eq!(wal.synced_index(), 3);

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
}
