// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Dedup-savings estimate for an RBD pool. Ceph has no dedup to simulate, so this reads the data:
//! each image's allocated extents come from `rbd export-diff pool/image -` (a sparse stream, so
//! holes are never read) and go through [`DedupScanner`].
//!
//! The scan is bounded by a byte budget. When it stops short of the pool's used space the result
//! is flagged as a lower bound (see `atlas_driver_core::dedup::estimate_from_scan`).
//!
//! Diff stream format (`rbd export-diff`, version 1, little-endian), from the Ceph RBD docs:
//! `"rbd diff v1\n"`, then records: `f`/`t` u32 length + snapshot name, `s` u64 image size,
//! `w` u64 offset + u64 length + data, `z` u64 offset + u64 length (zeroed range), `e` end.

use atlas_driver_core::dedup::DedupScanner;
use atlas_driver_core::DriverError;
use tokio::io::{AsyncRead, AsyncReadExt};

const HEADER: &[u8] = b"rbd diff v1\n";
/// Largest single read while scanning a data record.
const READ_CHUNK: u64 = 8 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum ScanEnd {
    /// The whole stream was read.
    Complete,
    /// The byte budget ran out first.
    Budget,
    /// The scanner hit its distinct-block ceiling.
    Truncated,
}

fn io(e: std::io::Error) -> DriverError {
    DriverError::Parse(format!("rbd export-diff stream: {e}"))
}

/// Feed one export-diff stream into `scanner`, stopping once `budget` bytes have been examined.
pub async fn scan_export_diff<R: AsyncRead + Unpin>(
    r: &mut R,
    scanner: &mut DedupScanner,
    budget: u64,
    block_size: u64,
) -> Result<ScanEnd, DriverError> {
    let mut header = [0u8; HEADER.len()];
    r.read_exact(&mut header).await.map_err(io)?;
    if header != *HEADER {
        return Err(DriverError::Parse("not an `rbd diff v1` stream".into()));
    }
    // Read in whole blocks so block boundaries line up across chunks.
    let chunk = (READ_CHUNK / block_size.max(1)).max(1) * block_size.max(1);
    let start = scanner.examined_bytes();
    loop {
        let tag = r.read_u8().await.map_err(io)?;
        match tag {
            b'e' => return Ok(ScanEnd::Complete),
            b'f' | b't' => {
                let n = r.read_u32_le().await.map_err(io)?;
                if n > 4096 {
                    return Err(DriverError::Parse(format!(
                        "rbd diff: snapshot name of {n} bytes"
                    )));
                }
                let mut name = vec![0u8; n as usize];
                r.read_exact(&mut name).await.map_err(io)?;
            }
            b's' => {
                r.read_u64_le().await.map_err(io)?;
            }
            b'z' => {
                let _offset = r.read_u64_le().await.map_err(io)?;
                let len = r.read_u64_le().await.map_err(io)?;
                scanner.feed_zeros(len);
            }
            b'w' => {
                let _offset = r.read_u64_le().await.map_err(io)?;
                let mut left = r.read_u64_le().await.map_err(io)?;
                while left > 0 {
                    let spent = scanner.examined_bytes() - start;
                    if spent >= budget {
                        return Ok(ScanEnd::Budget);
                    }
                    let want = left.min(chunk).min(budget - spent).max(1);
                    let mut buf = vec![0u8; want as usize];
                    r.read_exact(&mut buf).await.map_err(io)?;
                    left -= want;
                    if !scanner.feed(&buf) {
                        return Ok(ScanEnd::Truncated);
                    }
                }
            }
            other => {
                return Err(DriverError::Parse(format!(
                    "rbd diff: unknown record tag 0x{other:02x}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: u64 = 4096;

    fn stream(records: &[Vec<u8>]) -> Vec<u8> {
        let mut v = HEADER.to_vec();
        for r in records {
            v.extend_from_slice(r);
        }
        v.push(b'e');
        v
    }
    fn size(n: u64) -> Vec<u8> {
        let mut v = vec![b's'];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
    fn write(offset: u64, data: &[u8]) -> Vec<u8> {
        let mut v = vec![b'w'];
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&(data.len() as u64).to_le_bytes());
        v.extend_from_slice(data);
        v
    }
    fn zero(offset: u64, len: u64) -> Vec<u8> {
        let mut v = vec![b'z'];
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v
    }
    fn snap(tag: u8, name: &str) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(name.as_bytes());
        v
    }
    fn block(seed: u8) -> Vec<u8> {
        (0..BS as usize)
            .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed ^ 0x5a))
            .collect()
    }

    #[tokio::test]
    async fn scans_writes_and_counts_zero_ranges() {
        let mut data = block(3);
        data.extend_from_slice(&block(3)); // a duplicate within the extent
        data.extend_from_slice(&block(5));
        let s = stream(&[
            snap(b'f', "a"),
            snap(b't', "b"),
            size(1 << 30),
            write(0, &data),
            zero(1 << 20, 8 * BS),
        ]);
        let mut sc = DedupScanner::new(BS, 100);
        let end = scan_export_diff(&mut s.as_slice(), &mut sc, u64::MAX, BS)
            .await
            .unwrap();
        assert_eq!(end, ScanEnd::Complete);
        let r = sc.finish();
        assert_eq!(r.scanned_bytes, 3 * BS);
        assert_eq!(r.unique_bytes, 2 * BS);
        assert_eq!(r.zero_bytes, 8 * BS);
    }

    #[tokio::test]
    async fn stops_at_the_byte_budget_mid_record() {
        let data: Vec<u8> = (1..=10u8).flat_map(block).collect();
        let s = stream(&[write(0, &data)]);
        let mut sc = DedupScanner::new(BS, 100);
        let end = scan_export_diff(&mut s.as_slice(), &mut sc, 4 * BS, BS)
            .await
            .unwrap();
        assert_eq!(end, ScanEnd::Budget);
        assert_eq!(sc.finish().scanned_bytes, 4 * BS);
    }

    #[tokio::test]
    async fn reports_truncation_from_the_scanner() {
        let data: Vec<u8> = (1..=10u8).flat_map(block).collect();
        let s = stream(&[write(0, &data)]);
        let mut sc = DedupScanner::new(BS, 2);
        let end = scan_export_diff(&mut s.as_slice(), &mut sc, u64::MAX, BS)
            .await
            .unwrap();
        assert_eq!(end, ScanEnd::Truncated);
    }

    #[tokio::test]
    async fn rejects_a_bad_header_unknown_tags_and_short_streams() {
        let mut sc = DedupScanner::new(BS, 10);
        assert!(scan_export_diff(&mut &b"rbd diff v2\n"[..], &mut sc, 1, BS)
            .await
            .is_err());
        let mut bad = HEADER.to_vec();
        bad.push(b'?');
        assert!(scan_export_diff(&mut bad.as_slice(), &mut sc, 1, BS)
            .await
            .is_err());
        let mut short = HEADER.to_vec();
        short.extend_from_slice(&write(0, &block(1))[..20]); // cut mid-record
        assert!(
            scan_export_diff(&mut short.as_slice(), &mut sc, u64::MAX, BS)
                .await
                .is_err()
        );
        assert!(scan_export_diff(&mut &b"rbd"[..], &mut sc, 1, BS)
            .await
            .is_err());
    }
}
