// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Live block-layer source: attaches `bpf/atlas_bio.bpf.c` (compiled by build.rs) to the
//! `block_rq_insert` / `block_rq_issue` / `block_rq_complete` tracepoints, drains its ring
//! buffers, keeps the Atlas Native PID map current and pins the native maps.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atlas_api_types::{BioEvent, IoNative, IoNativeSlow, IoNativeStat, IoSensorHealth};
use aya::maps::{Array, HashMap, MapData, RingBuf};
use aya::programs::TracePoint;
use aya::Ebpf;

use crate::procscan;
use crate::source::{parse_op, IoSource};
use crate::{CARDINALITY_CAP, NATIVE_SLOW_US, PIN_DIR};

static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/atlas_bio.bpf.o"));

const PROGRAMS: [(&str, &str); 2] = [
    ("atlas_rq_issue", "block_rq_issue"),
    ("atlas_rq_complete", "block_rq_complete"),
];

/// Feeds queue time only; without it every event reports `queued_ns = 0`.
const QUEUE_PROGRAM: (&str, &str) = ("atlas_rq_insert", "block_rq_insert");

const NATIVE_MAPS: [&str; 3] = [
    "atlas_native_pids",
    "atlas_native_stats",
    "atlas_native_slow",
];

/// Events taken per poll, so a burst can't hold the collector lock for long; the rest wait in
/// the ring buffer for the next poll.
const MAX_DRAIN: usize = 65_536;

/// Recent native slow requests kept for `/io/native`.
const SLOW_KEEP: usize = 256;

const PID_RESCAN: Duration = Duration::from_secs(5);

/// `struct atlas_bio_event`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawEvent {
    issued_ns: u64,
    completed_ns: u64,
    queued_ns: u64,
    bytes: u32,
    op: u32,
    major: u32,
    minor: u32,
    pid: u32,
    pad: u32,
    cgroup_id: u64,
    comm: [u8; 16],
}

/// `struct atlas_native_io_key`.
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeKey {
    major: u32,
    minor: u32,
    op: u32,
    pad: u32,
    cgroup_id: u64,
}

/// `struct atlas_native_io_value`.
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeValue {
    ios: u64,
    bytes: u64,
    latency_ns: u64,
    max_latency_ns: u64,
    errors: u64,
}

// SAFETY: repr(C) integer-only structs with no padding; any bit pattern is valid.
unsafe impl aya::Pod for NativeKey {}
// SAFETY: as above.
unsafe impl aya::Pod for NativeValue {}

/// `struct atlas_native_extent_event`.
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeSlowEvent {
    ts_ns: u64,
    cgroup_id: u64,
    sector: u64,
    latency_ns: u64,
    bytes: u32,
    major: u32,
    minor: u32,
    op: u32,
    pid: u32,
    error: i32,
    comm: [u8; 16],
}

struct Attached {
    events: RingBuf<MapData>,
    dropped: Array<MapData, u64>,
    native_pids: HashMap<MapData, u32, u8>,
    native_stats: HashMap<MapData, NativeKey, NativeValue>,
    native_slow: RingBuf<MapData>,
    recent_slow: VecDeque<IoNativeSlow>,
    names: Vec<String>,
    tracked: Vec<u32>,
    last_scan: Instant,
    pinned: Option<PathBuf>,
    // Owns the programs and their tracepoint links; dropping it detaches them.
    _ebpf: Ebpf,
}

impl Attached {
    /// Sync the kernel PID set with the processes currently running under those names.
    fn rescan(&mut self) {
        self.last_scan = Instant::now();
        let now = procscan::find_pids(Path::new("/proc"), &self.names);
        if now == self.tracked {
            return;
        }
        for pid in self.tracked.iter().filter(|p| !now.contains(p)) {
            let _ = self.native_pids.remove(pid);
        }
        let mut kept = Vec::with_capacity(now.len());
        for pid in now {
            if self.tracked.contains(&pid) || self.native_pids.insert(&pid, &1u8, 0).is_ok() {
                kept.push(pid);
            }
        }
        tracing::info!(pids = ?kept, "atlas-native pid set updated");
        self.tracked = kept;
    }

    fn kernel_dropped(&self, slots: std::ops::Range<u32>) -> u64 {
        slots
            .filter_map(|slot| self.dropped.get(&slot, 0).ok())
            .sum()
    }
}

pub struct BpfSource {
    inner: Mutex<Attached>,
}

/// Pin the native maps under `dir` (a bpffs path), replacing pins left by an earlier run.
fn pin_native(ebpf: &Ebpf, dir: &Path) -> Result<PathBuf> {
    let dir = dir.to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    for name in NATIVE_MAPS {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove stale {}", path.display())),
        }
        ebpf.map(name)
            .with_context(|| format!("{name} missing from the BPF object"))?
            .pin(&path)
            .with_context(|| format!("pin {name} at {}", path.display()))?;
    }
    Ok(dir)
}

fn unpin_native(dir: &Path) {
    for name in NATIVE_MAPS {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

impl BpfSource {
    /// Load and attach the programs. Needs CAP_BPF + CAP_PERFMON (or root) and tracefs.
    /// `native_processes` are the binary names whose I/O lands in the native maps, which are
    /// pinned under `pin_dir` when given.
    pub fn attach(native_processes: &[String], pin_dir: Option<&Path>) -> Result<Self> {
        let mut ebpf = Ebpf::load(OBJECT).context("load the atlas_bio BPF object")?;
        for (name, tracepoint) in PROGRAMS {
            let prog: &mut TracePoint = ebpf
                .program_mut(name)
                .with_context(|| format!("{name} missing from the BPF object"))?
                .try_into()?;
            prog.load().with_context(|| format!("load {name}"))?;
            prog.attach("block", tracepoint)
                .with_context(|| format!("attach {name} to block/{tracepoint}"))?;
        }
        let (name, tracepoint) = QUEUE_PROGRAM;
        let queue: Result<()> = (|| {
            let prog: &mut TracePoint = ebpf
                .program_mut(name)
                .with_context(|| format!("{name} missing from the BPF object"))?
                .try_into()?;
            prog.load().with_context(|| format!("load {name}"))?;
            prog.attach("block", tracepoint)
                .with_context(|| format!("attach {name} to block/{tracepoint}"))?;
            Ok(())
        })();
        if let Err(e) = queue {
            tracing::warn!(error = %format!("{e:#}"), "queue-time program not attached; queued_ns stays 0");
        }
        let pinned = pin_dir.and_then(|dir| match pin_native(&ebpf, dir) {
            Ok(dir) => Some(dir),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "native maps not pinned");
                None
            }
        });
        let events = RingBuf::try_from(ebpf.take_map("atlas_events").context("atlas_events map")?)?;
        let dropped = Array::try_from(
            ebpf.take_map("atlas_dropped")
                .context("atlas_dropped map")?,
        )?;
        let native_pids = HashMap::try_from(
            ebpf.take_map("atlas_native_pids")
                .context("atlas_native_pids map")?,
        )?;
        let native_stats = HashMap::try_from(
            ebpf.take_map("atlas_native_stats")
                .context("atlas_native_stats map")?,
        )?;
        let native_slow = RingBuf::try_from(
            ebpf.take_map("atlas_native_slow")
                .context("atlas_native_slow map")?,
        )?;
        let mut attached = Attached {
            events,
            dropped,
            native_pids,
            native_stats,
            native_slow,
            recent_slow: VecDeque::with_capacity(SLOW_KEEP),
            names: native_processes.to_vec(),
            tracked: Vec::new(),
            last_scan: Instant::now(),
            pinned,
            _ebpf: ebpf,
        };
        attached.rescan();
        Ok(Self {
            inner: Mutex::new(attached),
        })
    }
}

impl Drop for BpfSource {
    fn drop(&mut self) {
        if let Ok(g) = self.inner.get_mut() {
            if let Some(dir) = &g.pinned {
                unpin_native(dir);
            }
        }
    }
}

impl IoSource for BpfSource {
    fn mode(&self) -> &'static str {
        "live"
    }

    fn poll_events(&mut self) -> Vec<BioEvent> {
        let g = self.inner.get_mut().expect("bpf source lock");
        if g.last_scan.elapsed() >= PID_RESCAN {
            g.rescan();
        }
        let mut out = Vec::new();
        while out.len() < MAX_DRAIN {
            let Some(item) = g.events.next() else { break };
            out.extend(decode(&item));
        }
        let mut slow = Vec::new();
        while slow.len() < SLOW_KEEP {
            let Some(item) = g.native_slow.next() else {
                break;
            };
            slow.extend(decode_slow(&item));
        }
        for ev in slow {
            if g.recent_slow.len() == SLOW_KEEP {
                g.recent_slow.pop_back();
            }
            g.recent_slow.push_front(ev);
        }
        out
    }

    fn health(&self, seen: u64, dropped: u64, cardinality: u64) -> IoSensorHealth {
        let kernel_dropped = self
            .inner
            .lock()
            .expect("bpf source lock")
            .kernel_dropped(0..2);
        IoSensorHealth {
            mode: self.mode().to_string(),
            programs_loaded: vec!["atlas_bio".into(), "atlas_native".into()],
            programs_missing: vec![
                "atlas_cgroup".into(),
                "atlas_nfs".into(),
                "atlas_zfs".into(),
                "atlas_uring".into(),
            ],
            pin_dir: PIN_DIR.to_string(),
            events_seen: seen,
            events_dropped: dropped + kernel_dropped,
            map_cardinality: cardinality,
            cardinality_cap: CARDINALITY_CAP,
        }
    }

    fn native(&self) -> Option<IoNative> {
        let g = self.inner.lock().expect("bpf source lock");
        let mut stats: Vec<IoNativeStat> = g
            .native_stats
            .iter()
            .filter_map(|r| r.ok())
            .take(CARDINALITY_CAP as usize)
            .map(|(k, v)| IoNativeStat {
                device: String::new(),
                major: k.major,
                minor: k.minor,
                op: parse_op(k.op),
                cgroup_id: k.cgroup_id,
                ios: v.ios,
                bytes: v.bytes,
                avg_us: v.latency_ns.checked_div(v.ios).unwrap_or(0) / 1_000,
                max_us: v.max_latency_ns / 1_000,
                errors: v.errors,
            })
            .collect();
        stats.sort_by_key(|s| std::cmp::Reverse(s.bytes));
        Some(IoNative {
            process_names: g.names.clone(),
            tracked_pids: g.tracked.clone(),
            pinned: g.pinned.as_ref().map(|p| p.display().to_string()),
            slow_threshold_us: NATIVE_SLOW_US,
            stats,
            slow: g.recent_slow.iter().cloned().collect(),
            dropped: g.kernel_dropped(2..4),
        })
    }
}

fn comm_str(comm: &[u8; 16]) -> String {
    let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    String::from_utf8_lossy(&comm[..len]).into_owned()
}

fn decode_slow(buf: &[u8]) -> Option<IoNativeSlow> {
    if buf.len() < std::mem::size_of::<NativeSlowEvent>() {
        return None;
    }
    // SAFETY: the length is checked above and NativeSlowEvent is plain old data.
    let raw: NativeSlowEvent = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Some(IoNativeSlow {
        device: String::new(),
        major: raw.major,
        minor: raw.minor,
        op: parse_op(raw.op),
        pid: raw.pid,
        comm: comm_str(&raw.comm),
        cgroup_id: raw.cgroup_id,
        sector: raw.sector,
        bytes: raw.bytes,
        latency_us: raw.latency_ns / 1_000,
        error: raw.error,
    })
}

fn decode(buf: &[u8]) -> Option<BioEvent> {
    if buf.len() < std::mem::size_of::<RawEvent>() {
        return None;
    }
    // SAFETY: the length is checked above and RawEvent is plain old data, valid for any bytes.
    let raw: RawEvent = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Some(BioEvent {
        issued_ns: raw.issued_ns,
        completed_ns: raw.completed_ns,
        queued_ns: raw.queued_ns,
        bytes: raw.bytes,
        op: parse_op(raw.op),
        major: raw.major,
        minor: raw.minor,
        pid: raw.pid,
        cgroup_id: raw.cgroup_id,
        comm: comm_str(&raw.comm),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_api_types::IoOp;

    #[test]
    fn decodes_the_ring_buffer_record() {
        let mut comm = [0u8; 16];
        comm[..3].copy_from_slice(b"fio");
        let raw = RawEvent {
            issued_ns: 1_000,
            completed_ns: 251_000,
            queued_ns: 0,
            bytes: 4096,
            op: 1,
            major: 259,
            minor: 0,
            pid: 42,
            pad: 0,
            cgroup_id: 7,
            comm,
        };
        // SAFETY: RawEvent is repr(C) plain old data.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw as *const RawEvent).cast::<u8>(),
                std::mem::size_of::<RawEvent>(),
            )
        };
        let ev = decode(bytes).expect("decodes");
        assert_eq!(ev.latency_us(), 250);
        assert_eq!(ev.op, IoOp::Write);
        assert_eq!((ev.major, ev.minor, ev.pid, ev.cgroup_id), (259, 0, 42, 7));
        assert_eq!(ev.comm, "fio");
        assert!(decode(&bytes[..10]).is_none());
    }

    #[test]
    fn record_matches_the_c_layout() {
        assert_eq!(std::mem::size_of::<RawEvent>(), 72);
        assert_eq!(std::mem::size_of::<NativeKey>(), 24);
        assert_eq!(std::mem::size_of::<NativeValue>(), 40);
        assert_eq!(std::mem::size_of::<NativeSlowEvent>(), 72);
    }

    #[test]
    fn decodes_a_native_slow_event() {
        let mut comm = [0u8; 16];
        comm[..15].copy_from_slice(b"atlas-native-no");
        let raw = NativeSlowEvent {
            ts_ns: 9,
            cgroup_id: 4004,
            sector: 2048,
            latency_ns: 7_400_000,
            bytes: 1 << 20,
            major: 8,
            minor: 32,
            op: 1,
            pid: 3100,
            error: -5,
            comm,
        };
        // SAFETY: NativeSlowEvent is repr(C) plain old data.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw as *const NativeSlowEvent).cast::<u8>(),
                std::mem::size_of::<NativeSlowEvent>(),
            )
        };
        let ev = decode_slow(bytes).expect("decodes");
        assert_eq!((ev.latency_us, ev.pid, ev.error), (7_400, 3100, -5));
        assert_eq!(ev.op, IoOp::Write);
        assert_eq!(ev.comm, "atlas-native-no");
        assert!(decode_slow(&bytes[..8]).is_none());
    }
}
