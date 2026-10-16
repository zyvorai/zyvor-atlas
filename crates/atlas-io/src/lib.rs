// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Observe-first storage I/O sensor for Atlas.
//!
//! The control plane stays intent-based. This crate is an optional node agent that
//! joins block-layer issue/complete events, attributes them to a volume via a
//! device map, and feeds histograms + RCA. Live eBPF attach is opt-in
//! (`ATLAS_IO_MODE=live` on a build with the `bpf` feature); CI and `make run` use
//! the in-process fake source so nothing here requires `CAP_BPF`.
//!
//! Enforcement (`write_freeze` leases) is time-limited and fails open.

#[cfg(all(feature = "bpf", target_os = "linux"))]
pub mod bpf;
pub mod collector;
pub mod devmap;
pub mod hist;
pub mod http;
pub mod lease;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod procscan;
pub mod rca;
pub mod risk;
pub mod source;

pub use collector::Collector;
pub use risk::{score, CapacitySignal, RiskLevel};
pub use source::{FakeSource, IoSource};

/// Pin directory advertised in health; the live source pins the native maps under
/// [`NATIVE_PIN_DIR`].
pub const PIN_DIR: &str = "/sys/fs/bpf/atlas";
/// Default bpffs directory for `atlas_native_{pids,stats,slow}` (best effort; the agent's
/// `--native-pin-dir` overrides it, an empty value disables pinning).
pub const NATIVE_PIN_DIR: &str = "/sys/fs/bpf/atlas/native";
/// Must match `ATLAS_NATIVE_SLOW_IO_NS` in `bpf/atlas_native_io.bpf.c`.
pub const NATIVE_SLOW_US: u64 = 5_000;
/// Atlas Native binaries tracked by default (`--native-process` overrides).
pub const DEFAULT_NATIVE_PROCESSES: &str = "atlas-native-node,atlas-native-mount,atlas-native-nfs,atlas-native-smb,atlas-native-s3,atlas-native-replicate";

/// Hard cap on (dev, op) histogram keys and workload keys.
pub const CARDINALITY_CAP: u64 = 4096;

/// Log2-microsecond bucket count (1µs … ~35s).
pub const HIST_BUCKETS: usize = 26;
