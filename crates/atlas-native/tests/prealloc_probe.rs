// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What preallocation does to the data writes of three replicas on one disk, through
//! [`FileDevice`] itself. Ignored: a measurement, run by
//! `deploy/bench-vs-ceph/prealloc-probe.sh`, which says what the output means.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Barrier, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    device::{DataSync, FileDevice},
    BlockStore, PreallocConfig,
};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| panic!("bad {name}: {v}")),
        Err(_) => default,
    }
}

/// `writers` logical writers, each appending the same bytes to three files at once and going on
/// when all three are durable. Thread names carry the setting, so that a tracer can count what
/// each one causes.
fn run(dir: &std::path::Path, enabled: bool, writers: usize, ramp: Duration, measure: Duration) {
    let cfg = PreallocConfig {
        enabled,
        ahead_bytes: env("PROBE_AHEAD_BYTES", PreallocConfig::default().ahead_bytes),
        step_bytes: env("PROBE_STEP_BYTES", PreallocConfig::default().step_bytes),
    };
    let size: usize = env("PROBE_BYTES", 4096);
    let devices: Vec<FileDevice> = (0..3)
        .map(|i| {
            let path = dir.join(format!("dev{i}"));
            let _ = std::fs::remove_file(&path);
            FileDevice::open_with_config(path, Box::new(DataSync), cfg).unwrap()
        })
        .collect();
    let data = vec![0x5a; size];
    let name = if enabled { "pw-on" } else { "pw-off" };
    // One append per device first: preparation starts with the first one.
    for d in &devices {
        d.append(&data).unwrap();
    }
    thread::sleep(Duration::from_secs(2));

    let measuring = AtomicU64::new(0);
    let appends = AtomicU64::new(0);
    let latencies = Mutex::new(Vec::new());
    let start = Instant::now();
    thread::scope(|s| {
        for _ in 0..writers {
            // The replicas of one logical write: they start together, and the writer goes on
            // when the slowest has finished.
            let gate = std::sync::Arc::new((Barrier::new(3), AtomicU64::new(0)));
            for (replica, d) in devices.iter().enumerate() {
                let gate = gate.clone();
                let (data, measuring, appends, latencies) =
                    (&data, &measuring, &appends, &latencies);
                thread::Builder::new()
                    .name(name.into())
                    .spawn_scoped(s, move || {
                        let mut mine = Vec::new();
                        loop {
                            if gate.0.wait().is_leader() {
                                gate.1.store(
                                    u64::from(start.elapsed() >= ramp + measure),
                                    Ordering::SeqCst,
                                );
                            }
                            // Every replica sees the same decision.
                            gate.0.wait();
                            if gate.1.load(Ordering::SeqCst) == 1 {
                                break;
                            }
                            let t = Instant::now();
                            d.append(data).unwrap();
                            appends.fetch_add(1, Ordering::Relaxed);
                            gate.0.wait();
                            if replica == 0
                                && start.elapsed() >= ramp
                                && start.elapsed() < ramp + measure
                            {
                                measuring.fetch_add(1, Ordering::Relaxed);
                                mine.push(t.elapsed());
                            }
                        }
                        latencies.lock().unwrap().extend(mine);
                    })
                    .unwrap();
            }
        }
    });
    let mut latencies = latencies.into_inner().unwrap();
    latencies.sort_unstable();
    let at = |p: usize| {
        latencies
            .get((latencies.len() * p / 100).min(latencies.len().saturating_sub(1)))
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    };
    let stats = devices
        .iter()
        .filter_map(|d| d.prealloc_stats())
        .fold((0, 0, 0, 0), |a, s| {
            (
                a.0 + s.appends_prepared,
                a.1 + s.appends_unprepared,
                a.2 + s.append_waits,
                a.3 + s.failures,
            )
        });
    println!(
        "PROBE prealloc={} writers={writers} bytes={size} logical_per_s={:.1} p50_ms={:.1} p99_ms={:.1} \
         appends={} prepared={} unprepared={} waits={} failures={}",
        if enabled { "on" } else { "off" },
        measuring.load(Ordering::Relaxed) as f64 / measure.as_secs_f64(),
        at(50),
        at(99),
        appends.load(Ordering::Relaxed),
        stats.0,
        stats.1,
        stats.2,
        stats.3,
    );
}

#[test]
#[ignore = "a measurement: see deploy/bench-vs-ceph/prealloc-probe.sh"]
fn prealloc_rate_probe() {
    let dir = PathBuf::from(std::env::var("PROBE_DIR").expect("set PROBE_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let writers: String = env("PROBE_WRITERS", "1 8 32 128".to_string());
    let rounds: usize = env("PROBE_ROUNDS", 2);
    let ramp = Duration::from_secs(env("PROBE_RAMP", 5));
    let measure = Duration::from_secs(env("PROBE_RUNTIME", 25));
    for round in 1..=rounds {
        println!("PROBE round={round}");
        for n in writers.split_whitespace() {
            let n: usize = n.parse().expect("PROBE_WRITERS");
            // The order alternates, so that neither setting always follows the other.
            let order = if round % 2 == 1 {
                [false, true]
            } else {
                [true, false]
            };
            for enabled in order {
                run(&dir, enabled, n, ramp, measure);
            }
        }
    }
}
