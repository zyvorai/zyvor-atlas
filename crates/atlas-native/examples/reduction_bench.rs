// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Single-core cost of the atlas-native data-reduction primitives on 4 MiB extents.
//!
//! Run with `cargo run --release -p atlas-native --example reduction_bench`. It measures only the
//! per-extent work the write and read paths add (LZ4 compress and decompress, and the SHA-256 that
//! deduplication computes over the raw extent), one thread, in memory. It is not an end-to-end
//! throughput number: replication, erasure coding, devices and the network are not involved.

use std::hint::black_box;
use std::time::{Duration, Instant};

use atlas_native::checksum::sha256;
use atlas_native::compress::{compress, decompress};

const EXTENT: usize = 4 * 1024 * 1024;
const BUDGET: Duration = Duration::from_millis(1500);

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn zeros() -> Vec<u8> {
    vec![0; EXTENT]
}

/// Repetitive text with a changing counter, like application logs.
fn log_lines() -> Vec<u8> {
    let mut out = Vec::with_capacity(EXTENT + 128);
    let mut n = 0u64;
    while out.len() < EXTENT {
        let line = format!(
            "2026-10-26T05:{:02}:{:02}Z level=info svc=orders req={:08x} status=200 ms={}\n",
            (n / 60) % 60,
            n % 60,
            n.wrapping_mul(2_654_435_761) & 0xffff_ffff,
            10 + n % 90
        );
        out.extend_from_slice(line.as_bytes());
        n += 1;
    }
    out.truncate(EXTENT);
    out
}

/// Fixed-width records: a counter, small enums and a little noise. Compresses moderately.
fn records() -> Vec<u8> {
    let mut out = Vec::with_capacity(EXTENT);
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let mut id = 0u64;
    while out.len() < EXTENT {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&((id % 7) as u32).to_le_bytes());
        out.extend_from_slice(&((xorshift(&mut rng) % 1000) as u32).to_le_bytes());
        out.extend_from_slice(&xorshift(&mut rng).to_le_bytes());
        id += 1;
    }
    out.truncate(EXTENT);
    out
}

/// Encrypted or already-compressed data: LZ4 gains nothing.
fn random() -> Vec<u8> {
    let mut rng = 0xdead_beef_cafe_f00d_u64;
    (0..EXTENT).map(|_| (xorshift(&mut rng) >> 24) as u8).collect()
}

/// MiB/s over as many iterations as fit in the time budget (at least three).
fn rate(mut f: impl FnMut()) -> f64 {
    f(); // warm up
    let start = Instant::now();
    let mut n = 0u32;
    while n < 3 || start.elapsed() < BUDGET {
        f();
        n += 1;
    }
    (f64::from(n) * EXTENT as f64 / (1024.0 * 1024.0)) / start.elapsed().as_secs_f64()
}

fn main() {
    println!("atlas-native data reduction, one core, {} MiB extents\n", EXTENT >> 20);
    println!(
        "{:<12} {:>9} {:>14} {:>16} {:>13}",
        "data", "stored", "compress MiB/s", "decompress MiB/s", "sha256 MiB/s"
    );
    let sets: [(&str, Vec<u8>); 4] = [
        ("zeros", zeros()),
        ("log lines", log_lines()),
        ("records", records()),
        ("random", random()),
    ];
    for (name, data) in &sets {
        let packed = compress(data);
        let stored = packed.as_ref().map_or(data.len(), Vec::len);
        let c = rate(|| {
            black_box(compress(black_box(data)));
        });
        let d = packed.as_ref().map(|p| {
            rate(|| {
                black_box(decompress(black_box(p), data.len()).expect("round trip"));
            })
        });
        let h = rate(|| {
            black_box(sha256(black_box(data)));
        });
        let note = if packed.is_none() { " (kept raw)" } else { "" };
        println!(
            "{:<12} {:>8.1}% {:>14.0} {:>16} {:>13.0}{}",
            name,
            100.0 * stored as f64 / data.len() as f64,
            c,
            d.map_or("-".to_string(), |v| format!("{v:.0}")),
            h,
            note
        );
        if let Some(p) = &packed {
            assert_eq!(&decompress(p, data.len()).unwrap(), data);
        }
    }
}
