// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Per-inode write-back buffering. The kernel hands FUSE writes of at most 128 KiB; coalescing
//! a sequential stream into one contiguous run per inode turns those into a few large PUTs.
//! A full run is cut at a multiple of the run size, so a long sequential stream is sent in
//! aligned runs (whole extents when the run size is a multiple of the extent size, which the
//! cluster writes without serializing them) even if it started mid-extent.

use std::collections::HashMap;

/// Buffered bytes not yet sent to the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirty {
    pub ino: u64,
    pub offset: u64,
    pub data: Vec<u8>,
}

impl Dirty {
    pub fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }
}

pub struct WriteBack {
    limit: usize,
    runs: HashMap<u64, Dirty>,
}

impl WriteBack {
    /// Runs are flushed once they reach `limit` bytes; 0 disables buffering.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            runs: HashMap::new(),
        }
    }

    /// Buffers a write. Returns the runs that must be sent now, in order: a run the write could
    /// not extend (it is not contiguous with or inside it), then the full part of the new run
    /// (up to the last multiple of the run size; the rest stays buffered).
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Vec<Dirty> {
        let mut out = Vec::new();
        let end = offset + data.len() as u64;
        match self.runs.get_mut(&ino) {
            Some(run)
                if offset >= run.offset
                    && offset <= run.end()
                    && (end.max(run.end()) - run.offset) as usize <= self.limit.max(data.len()) =>
            {
                let at = (offset - run.offset) as usize;
                if run.data.len() < at + data.len() {
                    run.data.resize(at + data.len(), 0);
                }
                run.data[at..at + data.len()].copy_from_slice(data);
            }
            _ => {
                if let Some(old) = self.runs.remove(&ino) {
                    out.push(old);
                }
                self.runs.insert(
                    ino,
                    Dirty {
                        ino,
                        offset,
                        data: data.to_vec(),
                    },
                );
            }
        }
        if self.runs[&ino].data.len() >= self.limit {
            let mut run = self.runs.remove(&ino).expect("just inserted");
            let limit = self.limit.max(1) as u64;
            let cut = run.end() - run.end() % limit;
            if cut > run.offset && cut < run.end() {
                let tail = run.data.split_off((cut - run.offset) as usize);
                self.runs.insert(
                    ino,
                    Dirty {
                        ino,
                        offset: cut,
                        data: tail,
                    },
                );
            }
            out.push(run);
        }
        out
    }

    pub fn take(&mut self, ino: u64) -> Option<Dirty> {
        self.runs.remove(&ino)
    }

    pub fn take_all(&mut self) -> Vec<Dirty> {
        self.runs.drain().map(|(_, d)| d).collect()
    }

    /// End of the buffered run, which the file size must account for until it is flushed.
    pub fn end(&self, ino: u64) -> Option<u64> {
        self.runs.get(&ino).map(Dirty::end)
    }

    /// Forgets buffered bytes at or past `size` (the file was truncated).
    pub fn truncate(&mut self, ino: u64, size: u64) {
        if let Some(run) = self.runs.get_mut(&ino) {
            if size <= run.offset {
                self.runs.remove(&ino);
            } else if size < run.end() {
                run.data.truncate((size - run.offset) as usize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_sequential_and_overlapping_writes() {
        let mut wb = WriteBack::new(8);
        assert!(wb.write(1, 0, b"abc").is_empty());
        assert!(wb.write(1, 3, b"de").is_empty());
        assert!(wb.write(1, 1, b"XY").is_empty());
        assert_eq!(wb.end(1), Some(5));
        // Reaching the limit sends the run.
        let out = wb.write(1, 5, b"fgh");
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].offset, out[0].data.as_slice()),
            (0, &b"aXYdefgh"[..])
        );
        assert_eq!(wb.end(1), None);
    }

    #[test]
    fn a_gap_or_backwards_write_flushes_the_old_run_first() {
        let mut wb = WriteBack::new(64);
        wb.write(1, 10, b"abc");
        let out = wb.write(1, 0, b"z");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].offset, 10);
        let out = wb.write(1, 5, b"q");
        assert_eq!(out[0].data, b"z");
        // Other inodes have their own runs.
        assert!(wb.write(2, 0, b"x").is_empty());
        assert_eq!(wb.take_all().len(), 2);
    }

    #[test]
    fn full_runs_are_cut_at_multiples_of_the_run_size() {
        let mut wb = WriteBack::new(8);
        // Starts mid-run: the first run ends at 8, the rest stays buffered.
        let out = wb.write(1, 3, b"abcdefgh");
        assert_eq!((out[0].offset, out[0].data.as_slice()), (3, &b"abcde"[..]));
        assert_eq!(wb.end(1), Some(11));
        let out = wb.write(1, 11, b"ijklm");
        assert_eq!(
            (out[0].offset, out[0].data.as_slice()),
            (8, &b"fghijklm"[..])
        );
        assert_eq!(wb.end(1), None);
    }

    #[test]
    fn disabled_buffering_and_truncate() {
        let mut wb = WriteBack::new(0);
        assert_eq!(wb.write(1, 0, b"abc").len(), 1);
        let mut wb = WriteBack::new(64);
        wb.write(1, 4, b"abcdef");
        wb.truncate(1, 6);
        assert_eq!(wb.end(1), Some(6));
        wb.truncate(1, 2);
        assert_eq!(wb.take(1), None);
    }
}
