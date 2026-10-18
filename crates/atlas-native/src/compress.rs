// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Transparent LZ4 compression of extent payloads (`EngineConfig::compression`).
//!
//! An extent is compressed before it is replicated or erasure-coded, so every layer below the
//! engine's read path (devices, scrub, repair, tiering) handles the payload as opaque bytes and
//! `ExtentRef::checksum` covers the payload. Only the read path decompresses.

use crate::engine::NativeError;

/// Extents shorter than this are stored as they are: the saving would be smaller than a block.
pub const MIN_COMPRESS_BYTES: usize = 4096;

/// The LZ4 payload for `chunk`, or `None` when it is too small or does not shrink by at least
/// 12.5% (random, encrypted or already-compressed data), so incompressible data costs one failed
/// attempt and no extra reads.
pub fn compress(chunk: &[u8]) -> Option<Vec<u8>> {
    if chunk.len() < MIN_COMPRESS_BYTES {
        return None;
    }
    let out = lz4_flex::block::compress(chunk);
    (out.len() + chunk.len() / 8 <= chunk.len()).then_some(out)
}

/// Inverse of [`compress`]; the result must be exactly `raw_len` bytes.
pub fn decompress(payload: &[u8], raw_len: usize) -> Result<Vec<u8>, NativeError> {
    let out = lz4_flex::block::decompress(payload, raw_len)
        .map_err(|e| NativeError::Invalid(format!("corrupt compressed extent: {e}")))?;
    if out.len() != raw_len {
        return Err(NativeError::Invalid(format!(
            "compressed extent expanded to {} bytes, expected {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_data_round_trips_and_shrinks() {
        let data = b"timestamp=2026-10-18 level=info msg=ok\n".repeat(500);
        let c = compress(&data).expect("log lines compress");
        assert!(c.len() < data.len() / 2);
        assert_eq!(decompress(&c, data.len()).unwrap(), data);
    }

    #[test]
    fn random_and_tiny_data_stay_raw() {
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let random: Vec<u8> = (0..65536)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        assert!(compress(&random).is_none());
        assert!(compress(&[0u8; 100]).is_none());
    }

    #[test]
    fn corrupt_or_mislabelled_payloads_are_errors() {
        let data = vec![7u8; 8192];
        let c = compress(&data).unwrap();
        assert!(decompress(&c, data.len() + 1).is_err());
        assert!(decompress(&c[..c.len() / 2], data.len()).is_err());
    }
}
