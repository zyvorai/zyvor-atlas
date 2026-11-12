// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Event sources. `live()` attaches the block-layer BPF programs (`bpf` feature);
//! LiveSource is the honest stand-in when that is unavailable; FakeSource is the CI
//! and demo path.

use atlas_api_types::{BioEvent, IoNative, IoNativeSlow, IoNativeStat, IoOp, IoSensorHealth};

use crate::{CARDINALITY_CAP, NATIVE_SLOW_US, PIN_DIR};

pub trait IoSource: Send + Sync {
    fn mode(&self) -> &'static str;
    fn poll_events(&mut self) -> Vec<BioEvent>;
    fn health(&self, seen: u64, dropped: u64, cardinality: u64) -> IoSensorHealth;
    /// Atlas Native I/O, or `None` when the native maps are not loaded. Device names are
    /// left empty for the collector to fill from its device map.
    fn native(&self) -> Option<IoNative> {
        None
    }

    /// Mirror a cgroup→volume binding into a kernel map, when this source has one.
    /// `volume_id` of `None` removes the entry. The default does nothing (fake and
    /// the unattached stand-in). The collector's userspace map is the attribution source.
    fn set_cgroup_volume(&mut self, _cgroup_id: u64, _volume_id: Option<&str>) {}
}

/// Deterministic in-process source used by tests and `ATLAS_IO_MODE=fake`.
pub struct FakeSource {
    pub scripted: Vec<BioEvent>,
    loaded: Vec<String>,
}

impl FakeSource {
    pub fn new(scripted: Vec<BioEvent>) -> Self {
        Self {
            scripted,
            loaded: vec![
                "atlas_bio".into(),
                "atlas_native".into(),
                "atlas_cgroup".into(),
                "atlas_nfs".into(),
                "atlas_zfs".into(),
                "atlas_uring".into(),
            ],
        }
    }

    pub fn demo() -> Self {
        Self::new(demo_events())
    }
}

impl IoSource for FakeSource {
    fn mode(&self) -> &'static str {
        "fake"
    }

    fn poll_events(&mut self) -> Vec<BioEvent> {
        std::mem::take(&mut self.scripted)
    }

    fn health(&self, seen: u64, dropped: u64, cardinality: u64) -> IoSensorHealth {
        IoSensorHealth {
            mode: self.mode().to_string(),
            programs_loaded: self.loaded.clone(),
            programs_missing: Vec::new(),
            pin_dir: PIN_DIR.to_string(),
            events_seen: seen,
            events_dropped: dropped,
            map_cardinality: cardinality,
            cardinality_cap: CARDINALITY_CAP,
        }
    }

    fn native(&self) -> Option<IoNative> {
        Some(demo_native())
    }
}

/// The attached block-layer BPF source, or why it could not be attached (built without
/// the `bpf` feature, not Linux, missing CAP_BPF/CAP_PERFMON, no tracefs, …).
/// `native_processes` names the Atlas Native binaries whose I/O is aggregated kernel-side;
/// the native maps are pinned under `pin_dir` when given.
pub fn live(
    native_processes: &[String],
    pin_dir: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn IoSource>> {
    #[cfg(all(feature = "bpf", target_os = "linux"))]
    {
        Ok(Box::new(crate::bpf::BpfSource::attach(
            native_processes,
            pin_dir,
        )?))
    }
    #[cfg(not(all(feature = "bpf", target_os = "linux")))]
    {
        let _ = (native_processes, pin_dir);
        anyhow::bail!("atlas-io was built without the `bpf` feature (Linux only)")
    }
}

/// Stand-in when the BPF programs could not be attached: never claims programs are
/// loaded. The agent still serves health/coverage so `atlasctl io coverage` is honest
/// on a host without BPF.
pub struct LiveSource;

impl IoSource for LiveSource {
    fn mode(&self) -> &'static str {
        "live"
    }

    fn poll_events(&mut self) -> Vec<BioEvent> {
        Vec::new()
    }

    fn health(&self, seen: u64, dropped: u64, cardinality: u64) -> IoSensorHealth {
        IoSensorHealth {
            mode: self.mode().to_string(),
            programs_loaded: Vec::new(),
            programs_missing: vec![
                "atlas_bio".into(),
                "atlas_native".into(),
                "atlas_cgroup".into(),
                "atlas_nfs".into(),
                "atlas_zfs".into(),
                "atlas_uring".into(),
            ],
            pin_dir: PIN_DIR.to_string(),
            events_seen: seen,
            events_dropped: dropped,
            map_cardinality: cardinality,
            cardinality_cap: CARDINALITY_CAP,
        }
    }
}

pub fn parse_op(cmd: u32) -> IoOp {
    // Linux REQ_OP_* low bits.
    match cmd & 0xff {
        0 => IoOp::Read,
        1 => IoOp::Write,
        2 => IoOp::Flush,
        3 => IoOp::Discard,
        _ => IoOp::Other,
    }
}

pub fn demo_events() -> Vec<BioEvent> {
    let mut evs = Vec::new();
    // Fast reads on rbd0 (8:16) from qemu.
    for i in 0..80 {
        evs.push(BioEvent {
            issued_ns: 1_000_000 + i * 10_000,
            completed_ns: 1_000_000 + i * 10_000 + 120_000, // 120µs
            queued_ns: 20_000,
            bytes: 4096,
            op: IoOp::Read,
            major: 8,
            minor: 16,
            pid: 4242,
            cgroup_id: 1001,
            comm: "qemu-system-x86".into(),
        });
    }
    // Slow writes on the same device from a clone job.
    for i in 0..20 {
        evs.push(BioEvent {
            issued_ns: 2_000_000 + i * 50_000,
            completed_ns: 2_000_000 + i * 50_000 + 18_000_000, // 18ms
            queued_ns: 2_000_000,
            bytes: 128 * 1024,
            op: IoOp::Write,
            major: 8,
            minor: 16,
            pid: 99,
            cgroup_id: 2002,
            comm: "rbd-nbd".into(),
        });
    }
    // Quiet ZFS zio on 8:32.
    evs.push(BioEvent {
        issued_ns: 3_000_000,
        completed_ns: 3_250_000,
        queued_ns: 8_000,
        bytes: 8192,
        op: IoOp::Write,
        major: 8,
        minor: 32,
        pid: 7,
        cgroup_id: 7,
        comm: "z_wr_iss".into(),
    });
    evs
}

/// Scripted native view: an atlas-native-node data server writing chunks to its data disk.
pub fn demo_native() -> IoNative {
    IoNative {
        process_names: vec!["atlas-native-node".into()],
        tracked_pids: vec![3100],
        pinned: None,
        slow_threshold_us: NATIVE_SLOW_US,
        stats: vec![
            IoNativeStat {
                device: String::new(),
                major: 8,
                minor: 48,
                op: IoOp::Write,
                cgroup_id: 4004,
                ios: 1200,
                bytes: 1200 * 1024 * 1024,
                avg_us: 900,
                max_us: 7_400,
                errors: 0,
            },
            IoNativeStat {
                device: String::new(),
                major: 8,
                minor: 48,
                op: IoOp::Flush,
                cgroup_id: 4004,
                ios: 300,
                bytes: 0,
                avg_us: 2_100,
                max_us: 6_100,
                errors: 0,
            },
        ],
        slow: vec![IoNativeSlow {
            device: String::new(),
            major: 8,
            minor: 48,
            op: IoOp::Write,
            pid: 3100,
            comm: "atlas-native-no".into(),
            cgroup_id: 4004,
            sector: 2_097_152,
            bytes: 1024 * 1024,
            latency_us: 7_400,
            error: 0,
        }],
        dropped: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_req_op() {
        assert_eq!(parse_op(0), IoOp::Read);
        assert_eq!(parse_op(1), IoOp::Write);
        assert_eq!(parse_op(0x100), IoOp::Read);
    }

    #[test]
    fn fake_drains_once() {
        let mut src = FakeSource::demo();
        let first = src.poll_events();
        assert_eq!(first.len(), 101);
        assert!(src.poll_events().is_empty());
    }
}
