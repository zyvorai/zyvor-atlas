// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Block-hash scan behind the dedup-savings estimator: feed it data, read back how much of it was
//! duplicate and how well the distinct blocks compress.
//!
//! All-zero blocks are counted apart and left out of both ratios: a sparse image already costs
//! nothing, so counting its holes would make every thin volume look wildly deduplicable.
//! Distinct blocks are identified by the first 16 bytes of their SHA-256, which makes an accidental
//! collision negligible at any size a pool reaches while halving the memory of a full digest.

use std::collections::HashSet;

use atlas_api_types::DedupEstimate;
use sha2::{Digest, Sha256};

/// Default ceiling on distinct blocks held in memory (about 32 bytes each in a `HashSet`).
pub const DEFAULT_MAX_UNIQUE: usize = 8 << 20;

/// A saving smaller than this fraction is not worth a compression header, so the block is counted
/// raw. Same threshold as atlas-native's compression (`docs/NATIVE_DATA_REDUCTION.md`).
const MIN_COMPRESSION_SAVING: f64 = 0.125;

#[derive(Debug, Clone, PartialEq)]
pub struct ScanResult {
    pub block_size: u64,
    /// Bytes in non-zero blocks.
    pub scanned_bytes: u64,
    pub zero_bytes: u64,
    /// Bytes of distinct non-zero blocks.
    pub unique_bytes: u64,
    /// Bytes the distinct blocks would occupy after compression.
    pub unique_compressed_bytes: u64,
    /// The distinct-block ceiling was hit and the scan stopped; the figures cover only what it saw.
    pub truncated: bool,
}

impl ScanResult {
    /// scanned / unique; 1.0 when nothing was scanned.
    pub fn dedup_ratio(&self) -> f64 {
        ratio(self.scanned_bytes, self.unique_bytes)
    }
    /// unique / compressed; 1.0 when nothing was scanned.
    pub fn compression_ratio(&self) -> f64 {
        ratio(self.unique_bytes, self.unique_compressed_bytes)
    }
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        1.0
    } else {
        num as f64 / den as f64
    }
}

pub struct DedupScanner {
    block_size: usize,
    max_unique: usize,
    seen: HashSet<[u8; 16]>,
    scanned_bytes: u64,
    zero_bytes: u64,
    unique_bytes: u64,
    unique_compressed_bytes: u64,
    truncated: bool,
}

impl DedupScanner {
    pub fn new(block_size: u64, max_unique: usize) -> Self {
        Self {
            block_size: block_size.clamp(4 << 10, 16 << 20) as usize,
            max_unique,
            seen: HashSet::new(),
            scanned_bytes: 0,
            zero_bytes: 0,
            unique_bytes: 0,
            unique_compressed_bytes: 0,
            truncated: false,
        }
    }

    /// Bytes examined so far, zero blocks included.
    pub fn examined_bytes(&self) -> u64 {
        self.scanned_bytes + self.zero_bytes
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Scan `data` as one contiguous extent, cut into blocks from its start; a short final block
    /// counts as a block of its own length. Returns false once the scan has stopped for good
    /// (distinct-block ceiling reached), so the caller can quit reading.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        for block in data.chunks(self.block_size) {
            if self.truncated {
                return false;
            }
            self.scan_block(block);
        }
        !self.truncated
    }

    /// Account for `len` bytes known to be zero without reading them (a hole in a sparse stream).
    pub fn feed_zeros(&mut self, len: u64) {
        self.zero_bytes += len;
    }

    fn scan_block(&mut self, block: &[u8]) {
        let len = block.len() as u64;
        if block.iter().all(|&b| b == 0) {
            self.zero_bytes += len;
            return;
        }
        let digest = Sha256::digest(block);
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        self.scanned_bytes += len;
        if self.seen.contains(&key) {
            return;
        }
        if self.seen.len() >= self.max_unique {
            // Not counting a block we cannot remember would inflate the dedup ratio, so stop and
            // take it back out of the scanned total: the figures then cover exactly what was seen.
            self.scanned_bytes -= len;
            self.truncated = true;
            return;
        }
        self.seen.insert(key);
        self.unique_bytes += len;
        let compressed = lz4_flex::block::compress(block).len() as u64;
        let saving = 1.0 - compressed as f64 / len as f64;
        self.unique_compressed_bytes += if saving >= MIN_COMPRESSION_SAVING {
            compressed
        } else {
            len
        };
    }

    pub fn finish(self) -> ScanResult {
        ScanResult {
            block_size: self.block_size as u64,
            scanned_bytes: self.scanned_bytes,
            zero_bytes: self.zero_bytes,
            unique_bytes: self.unique_bytes,
            unique_compressed_bytes: self.unique_compressed_bytes,
            truncated: self.truncated,
        }
    }
}

/// Turn a finished scan into the public estimate. `total_bytes` is how much data the pool holds
/// (zero blocks excluded as far as the backend can tell); `None` means unknown, which reads as
/// zero coverage. Savings are only claimed for the bytes actually scanned, so a partial scan is a
/// lower bound rather than an extrapolation.
pub fn estimate_from_scan(
    backend_id: &str,
    pool: &str,
    scan: &ScanResult,
    total_bytes: Option<u64>,
) -> DedupEstimate {
    let examined = scan.scanned_bytes + scan.zero_bytes;
    let coverage = match total_bytes {
        Some(t) if t > 0 => (examined as f64 / t as f64).min(1.0),
        Some(_) => 1.0,
        None => 0.0,
    };
    let lower_bound = coverage < 1.0 || scan.truncated;
    let combined = scan.dedup_ratio() * scan.compression_ratio();
    let note = if lower_bound {
        "Lower bound: blocks that repeat between scanned and unscanned data are invisible to this scan, \
         so the real saving is at least this. Raise max_bytes to see more."
    } else {
        "The whole pool was scanned. Ratios are for this block size; a different record size or a \
         content-defined chunker would find different duplicates."
    };
    DedupEstimate {
        backend_id: backend_id.to_string(),
        pool: pool.to_string(),
        method: "block_hash_scan".to_string(),
        block_size: Some(scan.block_size),
        scanned_bytes: Some(scan.scanned_bytes),
        zero_bytes: Some(scan.zero_bytes),
        unique_bytes: Some(scan.unique_bytes),
        dedup_ratio: scan.dedup_ratio(),
        compression_ratio: Some(scan.compression_ratio()),
        combined_ratio: Some(combined),
        estimated_saved_bytes: Some(
            scan.scanned_bytes
                .saturating_sub(scan.unique_compressed_bytes),
        ),
        coverage,
        lower_bound,
        truncated: scan.truncated,
        note: note.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: u64 = 4096;

    /// Deterministic incompressible bytes (xorshift), distinct per `seed`.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
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
    fn identical_blocks_dedup_to_one() {
        let mut s = DedupScanner::new(BS, 1000);
        let b = noise(1, BS as usize);
        for _ in 0..10 {
            s.feed(&b);
        }
        let r = s.finish();
        assert_eq!(r.scanned_bytes, 10 * BS);
        assert_eq!(r.unique_bytes, BS);
        assert_eq!(r.dedup_ratio(), 10.0);
    }

    #[test]
    fn distinct_blocks_do_not_dedup() {
        let mut s = DedupScanner::new(BS, 1000);
        for seed in 1..=8 {
            s.feed(&noise(seed, BS as usize));
        }
        let r = s.finish();
        assert_eq!(r.dedup_ratio(), 1.0);
        assert_eq!(r.unique_bytes, 8 * BS);
    }

    #[test]
    fn zero_blocks_are_left_out_of_both_ratios() {
        let mut s = DedupScanner::new(BS, 1000);
        s.feed(&vec![0u8; 100 * BS as usize]);
        s.feed_zeros(50 * BS);
        s.feed(&noise(7, BS as usize));
        let r = s.finish();
        assert_eq!(r.zero_bytes, 150 * BS);
        assert_eq!(r.scanned_bytes, BS);
        assert_eq!(r.dedup_ratio(), 1.0);
    }

    #[test]
    fn compressible_blocks_report_a_compression_ratio_and_random_ones_do_not() {
        let mut s = DedupScanner::new(BS, 1000);
        let text: Vec<u8> = b"2026-10-26 INFO request served in 3ms\n"
            .iter()
            .cycle()
            .take(BS as usize)
            .copied()
            .collect();
        s.feed(&text);
        assert!(s.finish().compression_ratio() > 4.0);

        let mut s = DedupScanner::new(BS, 1000);
        s.feed(&noise(3, BS as usize));
        assert_eq!(s.finish().compression_ratio(), 1.0);
    }

    #[test]
    fn short_final_block_counts_at_its_own_length() {
        let mut s = DedupScanner::new(BS, 1000);
        let mut data = noise(1, BS as usize);
        data.extend_from_slice(&noise(2, 100));
        s.feed(&data);
        let r = s.finish();
        assert_eq!(r.scanned_bytes, BS + 100);
        assert_eq!(r.unique_bytes, BS + 100);
    }

    #[test]
    fn block_size_is_clamped() {
        assert_eq!(DedupScanner::new(1, 10).finish().block_size, 4096);
        assert_eq!(
            DedupScanner::new(u64::MAX, 10).finish().block_size,
            16 << 20
        );
    }

    #[test]
    fn hitting_the_memory_ceiling_stops_the_scan_and_keeps_the_figures_honest() {
        let mut s = DedupScanner::new(BS, 3);
        let mut more = true;
        for seed in 1..=10 {
            more = s.feed(&noise(seed, BS as usize));
            if !more {
                break;
            }
        }
        assert!(!more);
        let r = s.finish();
        assert!(r.truncated);
        // Three distinct blocks were remembered; the one that did not fit is not in the totals.
        assert_eq!(r.unique_bytes, 3 * BS);
        assert_eq!(r.scanned_bytes, 3 * BS);
        assert_eq!(r.dedup_ratio(), 1.0);
    }

    #[test]
    fn empty_scan_is_a_ratio_of_one() {
        let r = DedupScanner::new(BS, 10).finish();
        assert_eq!(r.dedup_ratio(), 1.0);
        assert_eq!(r.compression_ratio(), 1.0);
    }

    #[test]
    fn estimate_is_a_lower_bound_until_the_whole_pool_was_seen() {
        let mut s = DedupScanner::new(BS, 1000);
        let b = noise(1, BS as usize);
        for _ in 0..4 {
            s.feed(&b);
        }
        let scan = s.finish();
        let partial = estimate_from_scan("bkd", "p", &scan, Some(16 * BS));
        assert!(partial.lower_bound);
        assert_eq!(partial.coverage, 0.25);
        assert_eq!(partial.dedup_ratio, 4.0);
        assert_eq!(partial.estimated_saved_bytes, Some(3 * BS));
        let full = estimate_from_scan("bkd", "p", &scan, Some(4 * BS));
        assert!(!full.lower_bound);
        assert_eq!(full.coverage, 1.0);
        let unknown = estimate_from_scan("bkd", "p", &scan, None);
        assert!(unknown.lower_bound);
        assert_eq!(unknown.coverage, 0.0);
    }
}
