// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! File creates per second through the local-WAL engine and a 3-voter Raft group on localhost,
//! from one thread and from several, and whether the rate holds as the namespace grows.
//! `BENCH_WAL_COMPACT_AFTER` overrides the local engine's checkpoint interval (0: never) and
//! `BENCH_CACHE_INODES` its catalog cache.
//! `BENCH_FILES=20000 cargo test --release -p atlas-native --test metadata_bench -- --ignored --nocapture`

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use atlas_native::{
    engine::NewNode, BlockStore, EngineConfig, FailureDomain, FileDevice, MetaBackend,
    NativeEngine, Node, NodeType, RaftConfig, RaftServer, ROOT_INO,
};

fn files() -> usize {
    std::env::var("BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

fn new_file(i: usize) -> NewNode {
    NewNode {
        name: format!("file-{i}"),
        op_id: format!("op-{i}"),
        kind: NodeType::File,
        target: None,
        rdev: 0,
        mode: 0o644,
        create_mode: None,
        uid: 0,
        gid: 0,
    }
}

/// Creates files `ids` from `threads` threads and reports the rate.
fn run(label: &str, e: &NativeEngine, ids: Range<usize>, threads: usize) {
    let n = ids.len();
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            let ids = ids.clone();
            s.spawn(move || {
                for i in ids.skip(t).step_by(threads) {
                    e.fs_mknode("f", ROOT_INO, new_file(i)).unwrap();
                }
            });
        }
    });
    println!(
        "{label}: {n} creates from {threads} threads, {:.0}/s",
        n as f64 / start.elapsed().as_secs_f64()
    );
}

fn node(i: usize) -> Node {
    Node {
        id: format!("n{i}"),
        failure_domain: FailureDomain {
            zone: "z1".into(),
            rack: format!("r{i}"),
            host: format!("h{i}"),
        },
        free_bytes: 1 << 30,
        healthy: true,
    }
}

#[test]
#[ignore]
fn raft_file_creates_per_second() {
    let files = files();
    let td = tempfile::tempdir().unwrap();
    let ids: Vec<String> = (1..=3).map(|i| format!("m{i}")).collect();
    let listeners: BTreeMap<String, TcpListener> = ids
        .iter()
        .map(|id| (id.clone(), TcpListener::bind("127.0.0.1:0").unwrap()))
        .collect();
    let addrs: BTreeMap<String, SocketAddr> = listeners
        .iter()
        .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
        .collect();
    let mut engines = Vec::new();
    for (id, l) in listeners {
        let peers: BTreeMap<String, SocketAddr> = addrs
            .iter()
            .filter(|(p, _)| **p != id)
            .map(|(p, a)| (p.clone(), *a))
            .collect();
        let rcfg = RaftConfig::new(
            id.clone(),
            peers.keys().cloned().collect(),
            td.path().join(&id).join("raft"),
        );
        let server =
            Arc::new(RaftServer::start(rcfg, l, peers, Duration::from_millis(10)).unwrap());
        let stores = (1..=3)
            .map(|i| {
                let dev = FileDevice::open(td.path().join(&id).join(format!("n{i}.data"))).unwrap();
                (node(i), Arc::new(dev) as Arc<dyn BlockStore>)
            })
            .collect();
        let e = NativeEngine::open_with(
            EngineConfig::new(td.path().join(&id).join("engine")),
            stores,
            MetaBackend::Raft {
                server,
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        engines.push(e);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let leader = loop {
        if let Some(e) = engines
            .iter()
            .find(|e| e.create_fs_as("f".into(), "fs").is_ok())
        {
            break e;
        }
        assert!(Instant::now() < deadline, "no leader");
        std::thread::sleep(Duration::from_millis(20));
    };
    run("raft (3 voters)", leader, 0..files / 2, 1);
    run("raft (3 voters)", leader, files / 2..files, 8);
}

#[test]
#[ignore]
fn file_creates_per_second() {
    let files = files();
    let td = tempfile::tempdir().unwrap();
    let nodes = (1..=3).map(node).collect();
    let mut cfg = EngineConfig::new(td.path());
    if let Some(n) = std::env::var("BENCH_WAL_COMPACT_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        cfg.wal_compact_after = n;
    }
    if let Some(n) = std::env::var("BENCH_CACHE_INODES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        cfg.catalog_cache_inodes = n;
    }
    let e = NativeEngine::open(cfg, nodes).unwrap();
    e.create_fs_as("f".into(), "fs").unwrap();

    let window = (files / 10).max(1);
    let start = Instant::now();
    let mut lap = Instant::now();
    let mut rates = Vec::new();
    for i in 0..files {
        e.fs_mknode("f", ROOT_INO, new_file(i)).unwrap();
        if (i + 1) % window == 0 {
            rates.push(window as f64 / lap.elapsed().as_secs_f64());
            lap = Instant::now();
        }
    }
    let total = files as f64 / start.elapsed().as_secs_f64();
    println!(
        "local: {files} creates from 1 thread, {total:.0}/s; first tenth {:.0}/s, last tenth {:.0}/s",
        rates.first().copied().unwrap_or(total),
        rates.last().copied().unwrap_or(total),
    );
    run("local", &e, files..2 * files, 8);
    if let Some(peak) = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        })
    {
        println!(
            "local: peak RSS {}",
            peak.trim_start_matches("VmHWM:").trim()
        );
    }
}
