// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Reed-Solomon erasure coding of extents. An extent under a `k+m` scheme is split into `k`
//! data shards (the extent's bytes, zero-padded to `k * shard_len`) and `m` parity shards, each
//! on its own node. Any `k` of the `k + m` shards rebuild the extent, so a 4+2 extent survives two
//! lost nodes at 1.5x raw space where three replicas need 3x for the same two losses.
//!
//! Every shard carries its own SHA-256, so a read or a scrub knows exactly which shards are bad;
//! the extent's checksum still covers its bytes as a whole.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{checksum, metadata::MetaError};

/// Bounds on a scheme: enough for 16+4 wide stripes, small enough that a stripe's shards fit on
/// a cluster's nodes.
pub const MAX_DATA_SHARDS: usize = 32;
pub const MAX_PARITY_SHARDS: usize = 8;

/// `data + parity` shards per extent, written `"4+2"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcScheme {
    pub data: usize,
    pub parity: usize,
}

impl EcScheme {
    pub fn new(data: usize, parity: usize) -> Result<Self, String> {
        if !(1..=MAX_DATA_SHARDS).contains(&data) || !(1..=MAX_PARITY_SHARDS).contains(&parity) {
            return Err(format!(
                "erasure scheme {data}+{parity}: data shards must be 1 to {MAX_DATA_SHARDS}, \
                 parity shards 1 to {MAX_PARITY_SHARDS}"
            ));
        }
        Ok(Self { data, parity })
    }

    pub fn shards(&self) -> usize {
        self.data + self.parity
    }

    /// Bytes per shard for an extent of `len` bytes: the codec works on an even byte count.
    pub fn shard_len(&self, len: usize) -> usize {
        let n = len.div_ceil(self.data).max(2);
        n + n % 2
    }

    /// Splits `data` into the scheme's shards (data shards first, then parity).
    pub fn encode(&self, data: &[u8]) -> Result<EcShards, MetaError> {
        let shard_len = self.shard_len(data.len());
        let mut shards: Vec<Vec<u8>> = (0..self.data)
            .map(|i| {
                let start = (i * shard_len).min(data.len());
                let end = ((i + 1) * shard_len).min(data.len());
                let mut s = data[start..end].to_vec();
                s.resize(shard_len, 0);
                s
            })
            .collect();
        shards.extend(self.parity_of(&shards)?);
        let layout = EcLayout {
            data: self.data,
            parity: self.parity,
            shard_len,
            shard_checksums: shards.iter().map(|s| checksum::sha256(s)).collect(),
        };
        Ok(EcShards { layout, shards })
    }

    fn parity_of(&self, data: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, MetaError> {
        reed_solomon_simd::encode(self.data, self.parity, data).map_err(codec)
    }
}

fn codec(e: reed_solomon_simd::Error) -> MetaError {
    MetaError::Invalid(format!("erasure codec: {e}"))
}

impl fmt::Display for EcScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}", self.data, self.parity)
    }
}

impl FromStr for EcScheme {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("erasure scheme {s:?} is not <data>+<parity>, e.g. \"4+2\"");
        let (d, p) = s.split_once('+').ok_or_else(bad)?;
        let d = d.trim().parse().map_err(|_| bad())?;
        let p = p.trim().parse().map_err(|_| bad())?;
        Self::new(d, p)
    }
}

impl Serialize for EcScheme {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EcScheme {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// How an erasure-coded extent is laid out: `replicas[i]` of its [`ExtentRef`] holds shard `i`.
///
/// [`ExtentRef`]: crate::metadata::ExtentRef
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EcLayout {
    pub data: usize,
    pub parity: usize,
    pub shard_len: usize,
    pub shard_checksums: Vec<[u8; 32]>,
}

/// An extent's shards with their layout, ready to place.
#[derive(Debug, Clone)]
pub struct EcShards {
    pub layout: EcLayout,
    pub shards: Vec<Vec<u8>>,
}

impl EcLayout {
    pub fn scheme(&self) -> Result<EcScheme, MetaError> {
        EcScheme::new(self.data, self.parity).map_err(MetaError::Invalid)
    }

    pub fn shards(&self) -> usize {
        self.data + self.parity
    }

    /// Whether `shard` is shard `i` as written.
    pub fn verify(&self, i: usize, shard: &[u8]) -> bool {
        shard.len() == self.shard_len
            && self
                .shard_checksums
                .get(i)
                .is_some_and(|c| checksum::verify(shard, c))
    }

    /// Checks the layout against an extent of `len` bytes on `replicas` devices.
    pub fn check(&self, len: usize, replicas: usize) -> Result<(), MetaError> {
        let scheme = self.scheme()?;
        if replicas != scheme.shards() || self.shard_checksums.len() != scheme.shards() {
            return Err(MetaError::Invalid(format!(
                "a {scheme} extent needs {} shards and shard checksums",
                scheme.shards()
            )));
        }
        if self.shard_len != scheme.shard_len(len) {
            return Err(MetaError::Invalid(format!(
                "a {scheme} extent of {len} bytes has {}-byte shards, not {}",
                scheme.shard_len(len),
                self.shard_len
            )));
        }
        Ok(())
    }

    /// Every shard, the missing (`None`) ones rebuilt from any `data` of the others. Each
    /// present shard must already have passed [`Self::verify`].
    pub fn rebuild(&self, shards: Vec<Option<Vec<u8>>>) -> Result<Vec<Vec<u8>>, MetaError> {
        let scheme = self.scheme()?;
        let parity: Vec<Option<Vec<u8>>> = shards.get(self.data..).unwrap_or_default().to_vec();
        let data = self.rebuild_data(shards)?;
        let parity = if parity.iter().any(Option::is_none) {
            scheme.parity_of(&data)?
        } else {
            parity.into_iter().flatten().collect()
        };
        Ok(data.into_iter().chain(parity).collect())
    }

    /// The data shards, the missing ones rebuilt as in [`Self::rebuild`] (parity is not).
    pub fn rebuild_data(
        &self,
        mut shards: Vec<Option<Vec<u8>>>,
    ) -> Result<Vec<Vec<u8>>, MetaError> {
        let scheme = self.scheme()?;
        if shards.len() != scheme.shards() {
            return Err(MetaError::Invalid("wrong shard count".into()));
        }
        let have = shards.iter().filter(|s| s.is_some()).count();
        if have < self.data {
            return Err(MetaError::Invalid(format!(
                "{have} of {} shards left, {} needed",
                scheme.shards(),
                self.data
            )));
        }
        if shards[..self.data].iter().any(Option::is_none) {
            let originals = shards[..self.data]
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
            let recovery = shards[self.data..]
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
            let restored = reed_solomon_simd::decode(self.data, self.parity, originals, recovery)
                .map_err(codec)?;
            for (i, s) in restored {
                shards[i] = Some(s);
            }
        }
        shards.truncate(self.data);
        shards
            .into_iter()
            .map(|s| s.ok_or_else(|| MetaError::Invalid("shard not rebuilt".into())))
            .collect()
    }

    /// The extent's `len` bytes from its data shards.
    pub fn join(&self, data_shards: &[Vec<u8>], len: usize) -> Vec<u8> {
        let mut out: Vec<u8> = data_shards[..self.data].concat();
        out.truncate(len);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 7) as u8).collect()
    }

    #[test]
    fn schemes_parse_and_round_trip() {
        let s: EcScheme = "4+2".parse().unwrap();
        assert_eq!((s.data, s.parity, s.shards()), (4, 2, 6));
        assert_eq!(s.to_string(), "4+2");
        assert_eq!(serde_json::to_string(&s).unwrap(), r#""4+2""#);
        assert_eq!(
            serde_json::from_str::<EcScheme>(r#""8+3""#).unwrap().data,
            8
        );
        for bad in ["4", "0+2", "4+0", "33+1", "4+9", "a+b", "4+2+1"] {
            assert!(bad.parse::<EcScheme>().is_err(), "{bad}");
        }
    }

    #[test]
    fn any_k_shards_rebuild_the_extent() {
        let s = EcScheme::new(4, 2).unwrap();
        for len in [1, 2, 7, 4096, 1 << 20, (1 << 20) + 3] {
            let data = bytes(len);
            let enc = s.encode(&data).unwrap();
            enc.layout.check(len, 6).unwrap();
            assert_eq!(enc.shards.len(), 6);
            assert!(enc.shards.iter().all(|x| x.len() == enc.layout.shard_len));
            for (i, sh) in enc.shards.iter().enumerate() {
                assert!(enc.layout.verify(i, sh));
            }
            assert_eq!(enc.layout.join(&enc.shards, len), data);
            // Every pair of lost shards.
            for a in 0..6 {
                for b in a + 1..6 {
                    let mut have: Vec<Option<Vec<u8>>> =
                        enc.shards.iter().cloned().map(Some).collect();
                    have[a] = None;
                    have[b] = None;
                    let all = enc.layout.rebuild(have).unwrap();
                    assert_eq!(all, enc.shards, "lost {a} and {b} of {len} bytes");
                }
            }
        }
    }

    /// Encode and two-shard rebuild throughput of 4 MiB extents (`--ignored`, release build).
    #[test]
    #[ignore]
    fn codec_throughput() {
        let data = bytes(4 << 20);
        for scheme in ["4+2", "8+3"] {
            let s: EcScheme = scheme.parse().unwrap();
            let rounds = 64;
            let t = std::time::Instant::now();
            let mut enc = None;
            for _ in 0..rounds {
                enc = Some(s.encode(&data).unwrap());
            }
            let encode = t.elapsed();
            let enc = enc.unwrap();
            let t = std::time::Instant::now();
            for _ in 0..rounds {
                let mut have: Vec<Option<Vec<u8>>> = enc.shards.iter().cloned().map(Some).collect();
                have[0] = None;
                have[1] = None;
                enc.layout.rebuild_data(have).unwrap();
            }
            let rebuild = t.elapsed();
            let mib = (rounds * 4) as f64;
            println!(
                "{scheme}: encode {:.0} MiB/s, rebuild two data shards {:.0} MiB/s",
                mib / encode.as_secs_f64(),
                mib / rebuild.as_secs_f64()
            );
        }
    }

    #[test]
    fn too_few_shards_or_a_wrong_layout_is_refused() {
        let s = EcScheme::new(8, 3).unwrap();
        let enc = s.encode(&bytes(10_000)).unwrap();
        let mut have: Vec<Option<Vec<u8>>> = enc.shards.iter().cloned().map(Some).collect();
        for x in have.iter_mut().take(4) {
            *x = None;
        }
        assert!(enc.layout.rebuild(have).is_err());
        assert!(enc.layout.check(10_000, 10).is_err());
        assert!(enc.layout.check(20_000, 11).is_err());
        assert!(!enc.layout.verify(0, &enc.shards[1]));
        assert!(!enc.layout.verify(11, &enc.shards[0]));
    }
}
