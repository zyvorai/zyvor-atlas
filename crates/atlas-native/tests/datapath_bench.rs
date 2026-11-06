// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Engine data-path throughput over real data-node connections on localhost (3 replicas).
//! `cargo test --release -p atlas-native --test datapath_bench -- --ignored --nocapture`
//!
//! Environment:
//! - `ATLAS_BENCH_DIR`: directory for the data nodes' files (default: the system temp dir). Point
//!   it at the filesystem under test; the test only creates and removes its own `atlas-bench-*`
//!   subdirectory.
//! - `ATLAS_BENCH_BACKEND`: `file`, `aligned`, `io_uring` or `all` (default `file`; `io_uring`
//!   needs the `io-uring` cargo feature).
//! - `BENCH_MIB` (default 256), `BENCH_SMALL_OPS` (default 200, 64 KiB random I/Os),
//!   `BENCH_WRITERS` (checkpoint test).
//!
//! `datapath_report` prints one `BENCH_JSON {...}` line per backend: bandwidth, per-I/O latency
//! percentiles (p50/p99/p99.9) and process CPU time per MiB from `getrusage` (the data nodes run
//! in-process, so this covers client and servers together). Files are `posix_fadvise(DONTNEED)`ed
//! between the write and read phases so reads are not served from the page cache; the system page
//! cache is never dropped.

use std::{
    net::TcpListener,
    sync::Arc,
    time::{Duration, Instant},
};

use atlas_native::{
    open_store, BlockStore, DataNodeServer, DeviceBackend, EngineConfig, FailureDomain,
    MetaBackend, NativeEngine, Node, RemoteDevice,
};

const MIB: usize = 1 << 20;

/// An engine over three data nodes on localhost, with 1 MiB extents.
fn engine(td: &std::path::Path) -> (Vec<DataNodeServer>, NativeEngine) {
    engine_with(td, DeviceBackend::File)
}

fn engine_with(
    td: &std::path::Path,
    backend: DeviceBackend,
) -> (Vec<DataNodeServer>, NativeEngine) {
    let servers: Vec<DataNodeServer> = (1..=3)
        .map(|i| {
            let root = td.join(format!("dn{i}"));
            std::fs::create_dir_all(&root).unwrap();
            let dev = open_store(&root.join("nvme0.data"), backend, &root).unwrap();
            DataNodeServer::start_devices(
                format!("n{i}"),
                &root,
                vec![dev],
                TcpListener::bind("127.0.0.1:0").unwrap(),
                None,
            )
            .unwrap()
        })
        .collect();
    let stores: Vec<(Node, Arc<dyn BlockStore>)> = servers
        .iter()
        .enumerate()
        .map(|(i, s)| {
            (
                Node {
                    id: format!("n{}", i + 1),
                    failure_domain: FailureDomain {
                        zone: "z1".into(),
                        rack: format!("r{i}"),
                        host: format!("h{i}"),
                    },
                    free_bytes: 1 << 40,
                    healthy: true,
                },
                Arc::new(RemoteDevice::new(s.local_addr(), Duration::from_secs(30)))
                    as Arc<dyn BlockStore>,
            )
        })
        .collect();
    let mut cfg = EngineConfig::new(td.join("meta"));
    cfg.extent_bytes = MIB;
    let e = NativeEngine::open_with(cfg, stores, MetaBackend::Local).unwrap();
    (servers, e)
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore]
fn datapath_throughput() {
    let total_mib = env("BENCH_MIB", 256);
    let io = 8 * MIB;
    let td = tempfile::tempdir().unwrap();
    let (_servers, e) = engine(td.path());
    let size = (total_mib * MIB) as u64;
    let v = e.create_volume("bench", size).unwrap();
    let buf: Vec<u8> = (0..io).map(|i| (i * 13 % 251) as u8).collect();

    let t = Instant::now();
    let mut off = 0u64;
    while off < size {
        e.write(&v, off, &buf).unwrap();
        off += io as u64;
    }
    let w = t.elapsed();

    let t = Instant::now();
    let mut off = 0u64;
    while off < size {
        let got = e.read(&v, off, io).unwrap();
        assert_eq!(got.len(), io);
        off += io as u64;
    }
    let r = t.elapsed();

    let mb = total_mib as f64;
    println!(
        "datapath: {total_mib} MiB, 8 MiB I/Os, 1 MiB extents, 3 replicas: \
         write {:.0} MiB/s, read {:.0} MiB/s",
        mb / w.as_secs_f64(),
        mb / r.as_secs_f64()
    );
}

/// A checkpoint: `BENCH_WRITERS` ranks each stream their own shard in aligned 8 MiB writes.
/// Aligned whole-extent writes place their data concurrently, so the total should scale with
/// writers until the devices or the network saturate.
/// `BENCH_WRITERS=8 cargo test --release -p atlas-native --test datapath_bench checkpoint -- --ignored --nocapture`
#[test]
#[ignore]
fn checkpoint_throughput() {
    let shard_mib = env("BENCH_MIB", 128);
    let io = 8 * MIB;
    let td = tempfile::tempdir().unwrap();
    let (_servers, e) = engine(td.path());
    let buf: Vec<u8> = (0..io).map(|i| (i * 13 % 251) as u8).collect();
    for writers in [1, env("BENCH_WRITERS", 8)] {
        let vols: Vec<String> = (0..writers)
            .map(|w| {
                e.create_volume(format!("ckpt-{writers}-{w}"), (shard_mib * MIB) as u64)
                    .unwrap()
            })
            .collect();
        let t = Instant::now();
        std::thread::scope(|s| {
            for v in &vols {
                let (e, buf) = (&e, &buf);
                s.spawn(move || {
                    let mut off = 0u64;
                    while off < (shard_mib * MIB) as u64 {
                        e.write(v, off, buf).unwrap();
                        off += io as u64;
                    }
                });
            }
        });
        let secs = t.elapsed().as_secs_f64();
        println!(
            "checkpoint: {writers} writer(s) x {shard_mib} MiB, 8 MiB I/Os, 1 MiB extents, \
             3 replicas: {:.0} MiB/s",
            (writers * shard_mib) as f64 / secs
        );
    }
}

fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Evicts the page-cache pages of every file under `dir` (per file; the system cache is not dropped).
#[cfg(target_os = "linux")]
fn evict(dir: &std::path::Path) {
    use std::os::fd::AsRawFd;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            evict(&p);
        } else if let Ok(f) = std::fs::File::open(&p) {
            let _ = f.sync_all();
            unsafe {
                libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn evict(_dir: &std::path::Path) {}

fn pct(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let i = ((p / 100.0 * sorted_ms.len() as f64).ceil() as usize).clamp(1, sorted_ms.len()) - 1;
    sorted_ms[i]
}

type Op<'a> = Box<dyn FnMut() + 'a>;

/// Runs each op once, timing it; returns seconds, CPU seconds and latency percentiles in ms.
fn phase(ops: Vec<Op<'_>>, mib: f64) -> serde_json::Value {
    let (c0, t0) = (cpu_secs(), Instant::now());
    let mut lat = Vec::with_capacity(ops.len());
    for mut op in ops {
        let t = Instant::now();
        op();
        lat.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let (secs, cpu) = (t0.elapsed().as_secs_f64(), cpu_secs() - c0);
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    serde_json::json!({
        "ops": lat.len(), "mib": mib, "seconds": secs, "cpu_seconds": cpu,
        "mib_per_s": mib / secs, "cpu_ms_per_mib": cpu * 1e3 / mib,
        "p50_ms": pct(&lat, 50.0), "p99_ms": pct(&lat, 99.0), "p99_9_ms": pct(&lat, 99.9),
        "max_ms": lat.last().copied().unwrap_or(0.0),
    })
}

fn run_backend(base: &std::path::Path, name: &str, backend: DeviceBackend) -> serde_json::Value {
    let total_mib = env("BENCH_MIB", 256);
    let small_ops = env("BENCH_SMALL_OPS", 200);
    let (io, small) = (8 * MIB, 64 * 1024);
    let td = tempfile::Builder::new()
        .prefix("atlas-bench-")
        .tempdir_in(base)
        .unwrap();
    let (_servers, e) = engine_with(td.path(), backend);
    let size = (total_mib * MIB) as u64;
    let v = e.create_volume("bench", size).unwrap();
    let buf: Vec<u8> = (0..io).map(|i| (i * 13 % 251) as u8).collect();
    let offs: Vec<u64> = (0..size / io as u64).map(|i| i * io as u64).collect();
    let (e, v, buf) = (&e, &v, &buf);

    let mib = total_mib as f64;
    let seq_w = phase(
        offs.iter()
            .map(|&o| Box::new(move || e.write(v, o, buf).unwrap()) as Op)
            .collect(),
        mib,
    );
    evict(td.path());
    let seq_r = phase(
        offs.iter()
            .map(|&o| {
                Box::new(move || assert_eq!(e.read(v, o, io).unwrap().len(), io)) as Op
            })
            .collect(),
        mib,
    );
    // Deterministic pseudo-random 64 KiB-aligned offsets (xorshift).
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let slots = size / small as u64;
    let roffs: Vec<u64> = (0..small_ops)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % slots) * small as u64
        })
        .collect();
    let sbuf = vec![0xA5u8; small];
    let sbuf = &sbuf;
    let sm = small_ops as f64 * small as f64 / MIB as f64;
    let rnd_w = phase(
        roffs
            .iter()
            .map(|&o| Box::new(move || e.write(v, o, sbuf).unwrap()) as Op)
            .collect(),
        sm,
    );
    evict(td.path());
    let rnd_r = phase(
        roffs
            .iter()
            .map(|&o| {
                Box::new(move || assert_eq!(e.read(v, o, small).unwrap().len(), small)) as Op
            })
            .collect(),
        sm,
    );
    serde_json::json!({
        "backend": name, "total_mib": total_mib, "io_bytes": io, "small_io_bytes": small,
        "extent_bytes": MIB, "replicas": 3,
        "seq_write": seq_w, "seq_read": seq_r,
        "rand_write_64k": rnd_w, "rand_read_64k": rnd_r,
    })
}

#[test]
#[ignore]
fn datapath_report() {
    let base = std::env::var("ATLAS_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    std::fs::create_dir_all(&base).unwrap();
    let want = std::env::var("ATLAS_BENCH_BACKEND").unwrap_or_else(|_| "file".into());
    let mut backends = vec![
        ("file", DeviceBackend::File),
        ("aligned", DeviceBackend::Aligned),
    ];
    if cfg!(all(target_os = "linux", feature = "io-uring")) {
        backends.push(("io_uring", DeviceBackend::IoUring));
    }
    let mut ran = 0;
    for (name, b) in backends {
        if want == "all" || want == name {
            // A backend the kernel/sandbox refuses (io_uring is blocked by many container
            // seccomp profiles) is reported as skipped instead of failing the run.
            let probe = tempfile::Builder::new()
                .prefix("atlas-bench-probe-")
                .tempdir_in(&base)
                .unwrap();
            if let Err(err) = open_store(&probe.path().join("probe.data"), b, probe.path()) {
                println!(
                    "BENCH_JSON {}",
                    serde_json::json!({"backend": name, "skipped": err.to_string()})
                );
                continue;
            }
            drop(probe);
            println!("BENCH_JSON {}", run_backend(&base, name, b));
            ran += 1;
        }
    }
    assert!(
        ran > 0,
        "ATLAS_BENCH_BACKEND={want} matched no available backend"
    );
}
