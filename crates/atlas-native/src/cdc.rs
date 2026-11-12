// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Content-defined chunking scaffolding for atlas-native data reduction.
//!
//! The write path still stores fixed-size extents and deduplicates them inside one metadata
//! group. This module splits a byte string with a gear hash so a later cross-group index can
//! share the pieces. [`cross_group_fingerprints`] returns those hashes only when the flag is on.
//! Nothing here is consulted by [`crate::engine::NativeEngine`] writes.

use crate::checksum;

const fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut s = 0x9E3779B97F4A7C15u64;
    let mut i = 0;
    while i < 256 {
        s = s
            .wrapping_mul(0xBF58476D1CE4E5B9)
            .wrapping_add(0x94D049BB133111EBu64);
        s ^= s >> 30;
        t[i] = s | 1;
        i += 1;
    }
    t
}

const GEAR: [u64; 256] = gear_table();

/// Sizes for the scaffolding chunker. `avg_size` is rounded up to a power of two for the mask.
#[derive(Debug, Clone, Copy)]
pub struct Chunker {
    pub min_size: usize,
    pub avg_size: usize,
    pub max_size: usize,
}

impl Chunker {
    pub fn scaffolding() -> Self {
        Self {
            min_size: 64,
            avg_size: 256,
            max_size: 1024,
        }
    }

    /// Chunks of `data`, covering every byte. A short input is one chunk.
    ///
    /// The gear hash resets at each cut, so a boundary is a synchronization point: the bytes
    /// after it chunk the same way on their own.
    pub fn chunk<'a>(&self, data: &'a [u8]) -> Vec<&'a [u8]> {
        if data.is_empty() {
            return Vec::new();
        }
        let min = self.min_size.max(1);
        let max = self.max_size.max(min);
        let avg = self.avg_size.max(1).next_power_of_two();
        let mask = (avg as u64).wrapping_sub(1);
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut hash = 0u64;
        let mut i = 0usize;
        while i < data.len() {
            hash = hash.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            let len = i + 1 - start;
            let at_max = len >= max;
            let at_cut = len >= min && (hash & mask) == 0;
            if at_max || at_cut || i + 1 == data.len() {
                out.push(&data[start..i + 1]);
                start = i + 1;
                hash = 0;
            }
            i += 1;
        }
        out
    }
}

/// SHA-256 of each content-defined chunk when `enabled`. An empty list when the flag is off:
/// cross-group sharing is not in effect.
pub fn cross_group_fingerprints(enabled: bool, data: &[u8]) -> Vec<[u8; 32]> {
    if !enabled || data.is_empty() {
        return Vec::new();
    }
    Chunker::scaffolding()
        .chunk(data)
        .into_iter()
        .map(checksum::sha256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_defaults_off_and_fingerprints_need_it() {
        let cfg = crate::engine::EngineConfig::new(std::env::temp_dir());
        assert!(!cfg.cross_group_dedup);
        assert!(!cfg.dedup);
        let data = vec![7u8; 2000];
        assert!(cross_group_fingerprints(false, &data).is_empty());
        let on = cross_group_fingerprints(true, &data);
        assert!(!on.is_empty());
        assert_eq!(on, cross_group_fingerprints(true, &data));
    }

    #[test]
    fn a_prefix_chunk_still_shares_the_tail_after_a_boundary() {
        let pattern: Vec<u8> = (0u8..64).cycle().take(4000).collect();
        let chunker = Chunker::scaffolding();
        let a = chunker.chunk(&pattern);
        assert!(a.len() > 2, "synthetic input should cross a boundary");
        let head = a[0].len();
        let tail = chunker.chunk(&pattern[head..]);
        assert_eq!(tail, &a[1..]);
        // A different prefix that itself ends on a content-defined cut, then the same tail.
        // The tail chunks are unchanged: a boundary is a sync point.
        let noise_buf = vec![0xA5u8; 2000];
        let noise = chunker.chunk(&noise_buf);
        assert!(noise.len() > 1);
        let mut shifted = noise[0].to_vec();
        shifted.extend_from_slice(&pattern[head..]);
        let b = chunker.chunk(&shifted);
        assert_eq!(b[0], noise[0]);
        assert_eq!(&b[1..], tail.as_slice());
        let sum: usize = a.iter().map(|c| c.len()).sum();
        assert_eq!(sum, pattern.len());
        let fps = cross_group_fingerprints(true, &pattern);
        assert_eq!(fps.len(), a.len());
        let tail_fps = cross_group_fingerprints(true, &pattern[head..]);
        assert_eq!(tail_fps.len(), tail.len());
        assert!(tail_fps.iter().all(|f| fps.contains(f)));
    }

    #[test]
    fn empty_input_is_empty_and_a_short_input_is_one_chunk() {
        let chunker = Chunker::scaffolding();
        assert!(chunker.chunk(b"").is_empty());
        assert_eq!(chunker.chunk(b"hello").len(), 1);
    }
}
