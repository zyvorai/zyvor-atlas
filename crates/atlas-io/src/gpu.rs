// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Read-only GPU telemetry and a storage-starvation heuristic.
//!
//! Same fake-first pattern as the I/O sources: [`FakeGpuSource`] is the default and needs no
//! hardware; [`NvidiaSmiSource`] shells out to `nvidia-smi` (arg-arrays only) and returns a real
//! error when the tool or a driver is missing. Nothing is ever invented in real mode.
//!
//! [`correlate`] joins GPU processes with the per-pid storage latency the collector already
//! accounts for. It can only say a GPU looks *starved while its process's storage I/O is slow*;
//! it cannot see the data loader, so every verdict lists the numbers behind it and nothing acts
//! on it.

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use atlas_api_types::IoWorkload;
use serde::Serialize;

/// GPU utilisation (%) at or below which a GPU with work counts as underused.
const LOW_UTIL_PERCENT: u32 = 40;
/// Average storage latency (µs) per request of the GPU's processes above which storage is slow.
const SLOW_AVG_US: u64 = 2_000;
/// Fewer storage requests than this and the verdict is `insufficient_data`.
const MIN_OPS: u64 = 20;
/// How long a `nvidia-smi` snapshot is reused, so scrapes do not fork a process per request.
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GpuDevice {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub util_percent: u32,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub temp_c: u32,
    /// `None` when the driver reports the field as unsupported.
    pub power_w: Option<f64>,
    pub ecc_uncorrected: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GpuProcess {
    pub pid: u32,
    pub gpu_uuid: String,
    pub name: String,
    pub used_mib: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct GpuSnapshot {
    pub devices: Vec<GpuDevice>,
    pub processes: Vec<GpuProcess>,
}

pub trait GpuSource: Send + Sync {
    fn mode(&self) -> &'static str;
    fn snapshot(&self) -> Result<GpuSnapshot, String>;
}

/// Deterministic source for tests, `make run` and `ATLAS_GPU_MODE=fake`. Matches the pids of the
/// fake I/O source so the correlation has something to join.
pub struct FakeGpuSource;

impl GpuSource for FakeGpuSource {
    fn mode(&self) -> &'static str {
        "fake"
    }

    fn snapshot(&self) -> Result<GpuSnapshot, String> {
        let dev = |index, util, used, temp, power| GpuDevice {
            index,
            uuid: format!("GPU-fake-{index}"),
            name: "Fake H100 80GB".into(),
            util_percent: util,
            mem_used_mib: used,
            mem_total_mib: 81_559,
            temp_c: temp,
            power_w: Some(power),
            ecc_uncorrected: Some(0),
        };
        Ok(GpuSnapshot {
            devices: vec![
                dev(0, 8, 41_000, 52, 120.5),
                dev(1, 94, 70_200, 71, 610.0),
                dev(2, 0, 4, 34, 62.0),
            ],
            processes: vec![
                GpuProcess {
                    pid: 99,
                    gpu_uuid: "GPU-fake-0".into(),
                    name: "python3".into(),
                    used_mib: 41_000,
                },
                GpuProcess {
                    pid: 4242,
                    gpu_uuid: "GPU-fake-1".into(),
                    name: "python3".into(),
                    used_mib: 70_200,
                },
            ],
        })
    }
}

/// Real source: `nvidia-smi` CSV queries. A missing binary or driver is an error, not an empty list.
pub struct NvidiaSmiSource {
    bin: String,
    cache: Mutex<Option<(Instant, GpuSnapshot)>>,
}

impl NvidiaSmiSource {
    pub fn new() -> Self {
        Self::with_bin("nvidia-smi")
    }

    pub fn with_bin(bin: &str) -> Self {
        Self {
            bin: bin.to_string(),
            cache: Mutex::new(None),
        }
    }

    fn query(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.bin))?;
        if !out.status.success() {
            return Err(format!(
                "{} failed: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Default for NvidiaSmiSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuSource for NvidiaSmiSource {
    fn mode(&self) -> &'static str {
        "real"
    }

    fn snapshot(&self) -> Result<GpuSnapshot, String> {
        if let Some((at, snap)) = self.cache.lock().expect("gpu cache").as_ref() {
            if at.elapsed() < SNAPSHOT_TTL {
                return Ok(snap.clone());
            }
        }
        let gpus = self.query(&[
            "--query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,ecc.errors.uncorrected.aggregate.total",
            "--format=csv,noheader,nounits",
        ])?;
        let procs = self.query(&[
            "--query-compute-apps=pid,gpu_uuid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])?;
        let snap = GpuSnapshot {
            devices: parse_devices(&gpus)?,
            processes: parse_processes(&procs)?,
        };
        *self.cache.lock().expect("gpu cache") = Some((Instant::now(), snap.clone()));
        Ok(snap)
    }
}

fn fields(line: &str) -> Vec<&str> {
    line.split(',').map(str::trim).collect()
}

/// `nvidia-smi` prints `[N/A]` / `[Not Supported]` for fields a GPU does not expose.
fn opt<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.starts_with('[') {
        None
    } else {
        s.parse().ok()
    }
}

fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("unexpected nvidia-smi {what}: {s:?}"))
}

pub fn parse_devices(csv: &str) -> Result<Vec<GpuDevice>, String> {
    let mut out = Vec::new();
    for line in csv.lines().filter(|l| !l.trim().is_empty()) {
        let f = fields(line);
        if f.len() != 9 {
            return Err(format!("unexpected nvidia-smi gpu row: {line:?}"));
        }
        out.push(GpuDevice {
            index: num(f[0], "index")?,
            uuid: f[1].to_string(),
            name: f[2].to_string(),
            util_percent: num(f[3], "utilization")?,
            mem_used_mib: num(f[4], "memory.used")?,
            mem_total_mib: num(f[5], "memory.total")?,
            temp_c: num(f[6], "temperature")?,
            power_w: opt(f[7]),
            ecc_uncorrected: opt(f[8]),
        });
    }
    Ok(out)
}

pub fn parse_processes(csv: &str) -> Result<Vec<GpuProcess>, String> {
    let mut out = Vec::new();
    for line in csv.lines().filter(|l| !l.trim().is_empty()) {
        let f = fields(line);
        if f.len() != 4 {
            return Err(format!("unexpected nvidia-smi process row: {line:?}"));
        }
        out.push(GpuProcess {
            pid: num(f[0], "pid")?,
            gpu_uuid: f[1].to_string(),
            name: f[2].to_string(),
            used_mib: opt(f[3]).unwrap_or(0),
        });
    }
    Ok(out)
}

/// Owns the source and turns its errors into a report the API can serve.
pub struct GpuMonitor {
    source: Option<Box<dyn GpuSource>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuReport {
    /// `fake`, `real` or `off`.
    pub mode: &'static str,
    pub devices: Vec<GpuDevice>,
    pub processes: Vec<GpuProcess>,
    /// Set when the source failed; the lists are then empty rather than guessed.
    pub error: Option<String>,
}

impl GpuMonitor {
    pub fn new(source: Box<dyn GpuSource>) -> Self {
        Self {
            source: Some(source),
        }
    }

    pub fn off() -> Self {
        Self { source: None }
    }

    pub fn from_mode(mode: &str) -> Self {
        match mode {
            "real" => Self::new(Box::new(NvidiaSmiSource::new())),
            "fake" => Self::new(Box::new(FakeGpuSource)),
            _ => Self::off(),
        }
    }

    pub fn report(&self) -> GpuReport {
        let Some(src) = &self.source else {
            return GpuReport {
                mode: "off",
                devices: Vec::new(),
                processes: Vec::new(),
                error: None,
            };
        };
        match src.snapshot() {
            Ok(s) => GpuReport {
                mode: src.mode(),
                devices: s.devices,
                processes: s.processes,
                error: None,
            },
            Err(e) => GpuReport {
                mode: src.mode(),
                devices: Vec::new(),
                processes: Vec::new(),
                error: Some(e),
            },
        }
    }

    pub fn correlation(&self, workloads: &[IoWorkload]) -> Vec<GpuCorrelation> {
        let r = self.report();
        correlate(&r.devices, &r.processes, workloads)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GpuCorrelation {
    pub gpu_index: u32,
    pub gpu_uuid: String,
    pub util_percent: u32,
    pub pids: Vec<u32>,
    /// Storage requests accounted to those pids.
    pub storage_ops: u64,
    pub storage_bytes: u64,
    /// Mean latency per request; `None` without any accounted request.
    pub storage_avg_us: Option<u64>,
    /// `idle` (no GPU process), `insufficient_data`, `storage_starved_suspected`,
    /// `compute_bound_or_ok` or `underused_storage_ok`.
    pub verdict: &'static str,
    /// The numbers behind the verdict, so a reader can disagree with it.
    pub evidence: Vec<String>,
}

/// Join GPU processes with per-pid storage accounting. Pure; no I/O.
pub fn correlate(
    devices: &[GpuDevice],
    procs: &[GpuProcess],
    workloads: &[IoWorkload],
) -> Vec<GpuCorrelation> {
    devices
        .iter()
        .map(|d| {
            let mut pids: Vec<u32> = procs
                .iter()
                .filter(|p| p.gpu_uuid == d.uuid)
                .map(|p| p.pid)
                .collect();
            pids.sort_unstable();
            pids.dedup();
            let (mut ops, mut bytes, mut sum_us) = (0u64, 0u64, 0u64);
            for w in workloads.iter().filter(|w| pids.contains(&w.pid)) {
                ops += w.ops;
                bytes += w.bytes;
                sum_us += w.sum_us;
            }
            let avg = sum_us.checked_div(ops);
            let mut evidence = vec![
                format!("gpu utilisation {}%", d.util_percent),
                format!("{} GPU process(es): {:?}", pids.len(), pids),
                format!("{ops} storage requests, {bytes} bytes"),
            ];
            if let Some(a) = avg {
                evidence.push(format!("mean storage latency {a} µs"));
            }
            let verdict = if pids.is_empty() {
                "idle"
            } else if ops < MIN_OPS {
                "insufficient_data"
            } else if d.util_percent <= LOW_UTIL_PERCENT && avg.unwrap_or(0) >= SLOW_AVG_US {
                "storage_starved_suspected"
            } else if d.util_percent <= LOW_UTIL_PERCENT {
                "underused_storage_ok"
            } else {
                "compute_bound_or_ok"
            };
            GpuCorrelation {
                gpu_index: d.index,
                gpu_uuid: d.uuid.clone(),
                util_percent: d.util_percent,
                pids,
                storage_ops: ops,
                storage_bytes: bytes,
                storage_avg_us: avg,
                verdict,
                evidence,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct GdsReadiness {
    pub nvidia_fs_module_loaded: bool,
    pub gdscheck_found: bool,
    pub ready: bool,
    pub note: &'static str,
}

/// Read-only GPUDirect Storage readiness: the `nvidia_fs` kernel module and NVIDIA's `gdscheck`.
/// It does not prove GDS works with a given filesystem; run `gdscheck -p` for that.
pub fn gds_readiness() -> GdsReadiness {
    let module = std::fs::read_to_string("/proc/modules")
        .map(|m| m.lines().any(|l| l.starts_with("nvidia_fs ")))
        .unwrap_or(false);
    let gdscheck = ["/usr/local/cuda/gds/tools/gdscheck", "/usr/local/cuda/gds/tools/gdscheck.py"]
        .iter()
        .any(|p| std::path::Path::new(p).exists());
    GdsReadiness {
        nvidia_fs_module_loaded: module,
        gdscheck_found: gdscheck,
        ready: module,
        note: "module presence only; run gdscheck -p to check the filesystem and driver stack",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(pid: u32, ops: u64, sum_us: u64) -> IoWorkload {
        IoWorkload {
            cgroup_id: 1,
            pid,
            comm: "x".into(),
            device: "sda".into(),
            volume_id: None,
            ops,
            bytes: ops * 4096,
            sum_us,
        }
    }

    #[test]
    fn parses_gpu_and_process_csv() {
        let d = parse_devices(
            "0, GPU-a, NVIDIA A100, 37, 1024, 40960, 45, 88.12, 0\n1, GPU-b, T4, 0, 0, 15360, 30, [N/A], [N/A]\n",
        )
        .unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].power_w, Some(88.12));
        assert_eq!(d[1].power_w, None);
        assert_eq!(d[1].ecc_uncorrected, None);
        let p = parse_processes("4242, GPU-a, python3, 512\n").unwrap();
        assert_eq!(p[0].pid, 4242);
        assert!(parse_devices("garbage").is_err());
    }

    #[test]
    fn fake_demo_flags_the_starved_gpu_only() {
        let s = FakeGpuSource.snapshot().unwrap();
        let c = correlate(&s.devices, &s.processes, &[wl(99, 20, 360_000), wl(4242, 80, 9_600)]);
        assert_eq!(c[0].verdict, "storage_starved_suspected");
        assert_eq!(c[0].storage_avg_us, Some(18_000));
        assert_eq!(c[1].verdict, "compute_bound_or_ok");
        assert_eq!(c[2].verdict, "idle");
    }

    #[test]
    fn little_storage_data_is_not_a_verdict() {
        let s = FakeGpuSource.snapshot().unwrap();
        let c = correlate(&s.devices, &s.processes, &[wl(99, 3, 90_000)]);
        assert_eq!(c[0].verdict, "insufficient_data");
    }

    #[test]
    fn underused_but_fast_storage_is_not_blamed() {
        let s = FakeGpuSource.snapshot().unwrap();
        let c = correlate(&s.devices, &s.processes, &[wl(99, 100, 10_000)]);
        assert_eq!(c[0].verdict, "underused_storage_ok");
    }

    #[test]
    fn real_mode_without_nvidia_smi_reports_an_error_not_data() {
        let m = GpuMonitor::new(Box::new(NvidiaSmiSource::with_bin("/nonexistent/nvidia-smi")));
        let r = m.report();
        assert_eq!(r.mode, "real");
        assert!(r.devices.is_empty());
        assert!(r.error.is_some());
    }

    #[test]
    fn off_mode_is_empty_and_has_no_error() {
        let r = GpuMonitor::off().report();
        assert_eq!(r.mode, "off");
        assert!(r.error.is_none());
    }
}
