// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Per-filesystem activity windows for ransomware assessment: files modified, renamed and
//! deleted, bytes written, and how much of the written data looks encrypted (high Shannon
//! entropy). Counted on the node that served the request and kept in memory only: a restart
//! starts a fresh history, and each node sees only the operations it served.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;

use serde::Serialize;

/// Length of one window.
pub const INTERVAL_SECS: u64 = 60;
/// Completed windows kept per filesystem (4 hours).
const KEEP: usize = 240;
/// Distinct inodes tracked per window before the count saturates.
const MAX_INODES: usize = 100_000;
/// Blocks sampled for entropy, per write, and their size.
const ENTROPY_BLOCK: usize = 4096;
const MAX_SAMPLED_BLOCKS: usize = 16;
/// Bits per byte above which a block counts as encrypted or compressed.
const HIGH_ENTROPY: f64 = 7.5;

/// One window, shaped like the gateway's `POST /ai/ransomware/assess` samples.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ActivitySample {
    pub start_unix: u64,
    pub bytes_written: f64,
    pub files_modified: f64,
    pub files_renamed: f64,
    pub files_deleted: f64,
    pub high_entropy_ratio: f64,
}

#[derive(Debug, Default)]
struct Window {
    start_unix: u64,
    bytes: u64,
    modified: HashSet<u64>,
    modified_overflow: u64,
    renamed: u64,
    deleted: u64,
    blocks: u64,
    high_blocks: u64,
}

impl Window {
    fn sample(&self) -> ActivitySample {
        ActivitySample {
            start_unix: self.start_unix,
            bytes_written: self.bytes as f64,
            files_modified: (self.modified.len() as u64 + self.modified_overflow) as f64,
            files_renamed: self.renamed as f64,
            files_deleted: self.deleted as f64,
            high_entropy_ratio: if self.blocks == 0 {
                0.0
            } else {
                self.high_blocks as f64 / self.blocks as f64
            },
        }
    }
}

#[derive(Debug, Default)]
struct Fs {
    current: Window,
    done: VecDeque<ActivitySample>,
}

#[derive(Debug, Default)]
pub struct FsActivity {
    inner: Mutex<BTreeMap<String, Fs>>,
}

/// Bits of entropy per byte (0..=8).
pub fn shannon_entropy(block: &[u8]) -> f64 {
    if block.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for b in block {
        counts[*b as usize] += 1;
    }
    let n = block.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

impl FsActivity {
    fn with<R>(&self, fs: &str, now: u64, f: impl FnOnce(&mut Window) -> R) -> R {
        let Ok(mut map) = self.inner.lock() else {
            return f(&mut Window::default());
        };
        let e = map.entry(fs.to_string()).or_default();
        let start = now - now % INTERVAL_SECS;
        if e.current.start_unix != start {
            if e.current.start_unix != 0 {
                let finished = e.current.sample();
                e.done.push_back(finished);
                while e.done.len() > KEEP {
                    e.done.pop_front();
                }
            }
            e.current = Window {
                start_unix: start,
                ..Window::default()
            };
        }
        f(&mut e.current)
    }

    pub fn record_write(&self, fs: &str, ino: u64, data: &[u8]) {
        self.record_write_at(fs, ino, data, unix_now());
    }

    pub(crate) fn record_write_at(&self, fs: &str, ino: u64, data: &[u8], now: u64) {
        // Evenly spaced full blocks, so a large write costs a bounded amount of hashing.
        let full = data.len() / ENTROPY_BLOCK;
        let stride = (full / MAX_SAMPLED_BLOCKS).max(1);
        let (mut blocks, mut high) = (0u64, 0u64);
        for i in (0..full).step_by(stride).take(MAX_SAMPLED_BLOCKS) {
            blocks += 1;
            if shannon_entropy(&data[i * ENTROPY_BLOCK..(i + 1) * ENTROPY_BLOCK]) > HIGH_ENTROPY {
                high += 1;
            }
        }
        self.with(fs, now, |w| {
            w.bytes += data.len() as u64;
            if w.modified.len() < MAX_INODES {
                w.modified.insert(ino);
            } else if !w.modified.contains(&ino) {
                w.modified_overflow += 1;
            }
            w.blocks += blocks;
            w.high_blocks += high;
        });
    }

    pub fn record_rename(&self, fs: &str) {
        self.with(fs, unix_now(), |w| w.renamed += 1);
    }

    pub fn record_delete(&self, fs: &str) {
        self.with(fs, unix_now(), |w| w.deleted += 1);
    }

    /// Completed windows oldest first, then the one in progress, as assessor input.
    pub fn samples(&self, fs: &str) -> Vec<ActivitySample> {
        self.samples_at(fs, unix_now())
    }

    pub(crate) fn samples_at(&self, fs: &str, now: u64) -> Vec<ActivitySample> {
        // Rolls the window forward if it is stale, so an idle filesystem reports zeros.
        self.with(fs, now, |_| ());
        let Ok(map) = self.inner.lock() else {
            return vec![];
        };
        map.get(fs).map_or_else(Vec::new, |e| {
            let mut v: Vec<_> = e.done.iter().cloned().collect();
            v.push(e.current.sample());
            v
        })
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_block(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn entropy_separates_text_zeros_and_random() {
        assert!(shannon_entropy(&[0u8; 4096]) < 0.01);
        let text = b"the quick brown fox jumps over the lazy dog ".repeat(100);
        assert!(shannon_entropy(&text) < 5.0);
        assert!(shannon_entropy(&random_block(7, 4096)) > HIGH_ENTROPY);
    }

    #[test]
    fn windows_roll_and_count_distinct_files() {
        let a = FsActivity::default();
        let t = 6000; // a multiple of INTERVAL_SECS
        a.record_write_at("f", 5, &random_block(1, 8192), t);
        a.record_write_at("f", 5, &random_block(2, 8192), t + 1);
        a.record_write_at("f", 6, &[0u8; 4096], t + 2);
        let s = a.samples_at("f", t + 3);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].files_modified, 2.0);
        assert_eq!(s[0].bytes_written, 20480.0);
        // 4 random blocks + 1 zero block.
        assert!((s[0].high_entropy_ratio - 0.8).abs() < 1e-9);

        let s = a.samples_at("f", t + INTERVAL_SECS + 1);
        assert_eq!(s.len(), 2, "the old window is complete, a new one is open");
        assert_eq!(s[0].files_modified, 2.0);
        assert_eq!(s[1].files_modified, 0.0);
    }

    #[test]
    fn large_writes_sample_a_bounded_number_of_blocks() {
        let a = FsActivity::default();
        a.record_write_at("f", 1, &random_block(3, ENTROPY_BLOCK * 400), 60);
        let w = a.inner.lock().unwrap();
        assert!(w["f"].current.blocks <= MAX_SAMPLED_BLOCKS as u64);
        assert_eq!(w["f"].current.bytes, (ENTROPY_BLOCK * 400) as u64);
    }
}
