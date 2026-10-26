// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Dedup-savings estimate for a zpool from `zdb -S <pool>`, which builds the dedup table ZFS
//! *would* have if `dedup=on` had been set from the start and prints the resulting ratios. ZFS
//! does this itself, so unlike the Ceph estimate nothing here hashes data.
//!
//! `zdb -S` reads every block pointer in the pool, so it is slow and I/O-heavy on a big pool:
//! callers bound it with a timeout and the gateway makes it an explicit, admin-only request.

use atlas_api_types::DedupEstimate;
use atlas_driver_core::DriverError;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZdbSimulation {
    pub dedup: f64,
    pub compress: f64,
    pub copies: f64,
    pub combined: f64,
}

/// Parse the closing line of `zdb -S`:
/// `dedup = 1.50, compress = 1.20, copies = 1.00, dedup * compress / copies = 1.80`.
pub fn parse_zdb_simulation(output: &str) -> Result<ZdbSimulation, DriverError> {
    let line = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("dedup = "))
        .ok_or_else(|| DriverError::Parse("zdb -S printed no `dedup = ...` summary line".into()))?;
    let mut dedup = None;
    let mut compress = None;
    let mut copies = None;
    let mut combined = None;
    for part in line.split(", ") {
        let (key, value) = part
            .rsplit_once(" = ")
            .ok_or_else(|| DriverError::Parse(format!("zdb -S summary field `{part}`")))?;
        let v: f64 = value
            .trim()
            .parse()
            .map_err(|_| DriverError::Parse(format!("zdb -S ratio `{value}` in `{line}`")))?;
        match key.trim() {
            "dedup" => dedup = Some(v),
            "compress" => compress = Some(v),
            "copies" => copies = Some(v),
            "dedup * compress / copies" => combined = Some(v),
            _ => {}
        }
    }
    match (dedup, compress, copies, combined) {
        (Some(dedup), Some(compress), Some(copies), Some(combined))
            if dedup >= 1.0 && combined > 0.0 =>
        {
            Ok(ZdbSimulation {
                dedup,
                compress,
                copies,
                combined,
            })
        }
        _ => Err(DriverError::Parse(format!(
            "incomplete zdb -S summary: `{line}`"
        ))),
    }
}

/// `alloc_bytes` is what the pool occupies now. Dedup removes `1 - 1/dedup` of it; compression is
/// reported but not counted, because pool blocks are already stored compressed if `compression`
/// is on, and the simulation cannot tell us whether it is.
pub fn estimate(
    backend_id: &str,
    pool: &str,
    sim: ZdbSimulation,
    alloc_bytes: Option<u64>,
) -> DedupEstimate {
    DedupEstimate {
        backend_id: backend_id.to_string(),
        pool: pool.to_string(),
        method: "zdb_simulated_ddt".to_string(),
        block_size: None,
        scanned_bytes: None,
        zero_bytes: None,
        unique_bytes: None,
        dedup_ratio: sim.dedup,
        compression_ratio: Some(sim.compress),
        combined_ratio: Some(sim.combined),
        estimated_saved_bytes: alloc_bytes.map(|a| (a as f64 * (1.0 - 1.0 / sim.dedup)) as u64),
        coverage: 1.0,
        lower_bound: false,
        truncated: false,
        note: "ZFS simulated its own dedup table over every block in the pool (`zdb -S`). Savings are \
               dedup only, taken off the space the pool uses now; the compression figure is the \
               simulation's and is not subtracted again."
            .to_string(),
    }
}

/// Recorded `zdb -S` output shape (OpenZFS `zdb(8)`), used by the fake driver and the tests.
pub const SAMPLE_ZDB_OUTPUT: &str = "\
Simulated DDT histogram:

bucket              allocated                       referenced
______   ______________________________   ______________________________
refcnt   blocks   LSIZE   PSIZE   DSIZE   blocks   LSIZE   PSIZE   DSIZE
------   ------   -----   -----   -----   ------   -----   -----   -----
     1    2.00K    256M    256M    256M    2.00K    256M    256M    256M
     2    1.00K    128M    128M    128M    2.00K    256M    256M    256M
 Total    3.00K    384M    384M    384M    4.00K    512M    512M    512M

dedup = 1.33, compress = 1.00, copies = 1.00, dedup * compress / copies = 1.33
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_summary_line() {
        let s = parse_zdb_simulation(SAMPLE_ZDB_OUTPUT).unwrap();
        assert_eq!(
            s,
            ZdbSimulation {
                dedup: 1.33,
                compress: 1.0,
                copies: 1.0,
                combined: 1.33
            }
        );
    }

    #[test]
    fn takes_the_last_summary_when_the_tool_prints_more_than_one() {
        let out = "dedup = 9.00, compress = 1.00, copies = 1.00, dedup * compress / copies = 9.00\nnoise\n\
                   dedup = 2.00, compress = 1.50, copies = 1.00, dedup * compress / copies = 3.00\n";
        assert_eq!(parse_zdb_simulation(out).unwrap().combined, 3.0);
    }

    #[test]
    fn rejects_output_without_a_summary_or_with_garbage_ratios() {
        assert!(parse_zdb_simulation("zdb: cannot open 'tank': no such pool").is_err());
        assert!(parse_zdb_simulation(
            "dedup = lots, compress = 1.00, copies = 1.00, dedup * compress / copies = 1.00"
        )
        .is_err());
        assert!(parse_zdb_simulation("dedup = 1.20, compress = 1.00").is_err());
        // A dedup ratio below 1 cannot happen; treat it as a misparse, not a result.
        assert!(parse_zdb_simulation(
            "dedup = 0.50, compress = 1.00, copies = 1.00, dedup * compress / copies = 0.50"
        )
        .is_err());
    }

    #[test]
    fn saved_bytes_are_dedup_only_taken_off_current_use() {
        let sim = parse_zdb_simulation(SAMPLE_ZDB_OUTPUT).unwrap();
        let e = estimate("bkd_zfs", "tank", sim, Some(1_000_000));
        // 1 - 1/1.33 = 0.2481...
        assert_eq!(e.estimated_saved_bytes, Some(248_120));
        assert_eq!(e.method, "zdb_simulated_ddt");
        assert!(!e.lower_bound);
        assert_eq!(estimate("b", "t", sim, None).estimated_saved_bytes, None);
    }
}
