// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The rebuild controller: lost nodes found by probing, degraded extents rebuilt most at risk
//! first and at a capped rate, no rebuild for a node back within the delay, and an incremental
//! scrub that resumes where it stopped.

use std::{
    collections::BTreeSet,
    io::{Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    ec::EcScheme,
    rebuild::{RebuildConfig, Rebuilder},
    BlockStore, DataNodeServer, EngineConfig, FailureDomain, MetaBackend, NativeEngine, Node,
    RemoteDevice,
};

const EXTENT: usize = 64 << 10;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

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

fn cfg(root: &Path, erasure: Option<&str>) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = EXTENT;
    c.erasure = erasure.map(|s| s.parse::<EcScheme>().unwrap());
    c.erasure_min_bytes = 32 << 10;
    c.node_retry_after = Duration::from_millis(200);
    c
}

fn controller(delay: Duration, rebuild_bytes_per_sec: u64, scrub: Duration) -> Rebuilder {
    Rebuilder::new(RebuildConfig {
        delay,
        rebuild_bytes_per_sec,
        scrub_interval: scrub,
        scrub_bytes_per_sec: 0,
    })
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

struct Cluster {
    root: PathBuf,
    servers: Vec<Option<DataNodeServer>>,
    addrs: Vec<SocketAddr>,
}

impl Cluster {
    fn start(root: &Path, n: usize) -> Self {
        let servers: Vec<DataNodeServer> = (1..=n)
            .map(|i| {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                DataNodeServer::start(format!("n{i}"), root.join(format!("dn{i}")), l).unwrap()
            })
            .collect();
        let addrs = servers.iter().map(DataNodeServer::local_addr).collect();
        Self {
            root: root.to_path_buf(),
            servers: servers.into_iter().map(Some).collect(),
            addrs,
        }
    }

    fn engine(&self, c: EngineConfig) -> NativeEngine {
        let stores = self
            .addrs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                (
                    node(i + 1),
                    Arc::new(RemoteDevice::new(*a, IO_TIMEOUT)) as Arc<dyn BlockStore>,
                )
            })
            .collect();
        NativeEngine::open_with(c, stores, MetaBackend::Local).unwrap()
    }

    fn stop(&mut self, i: usize) {
        if let Some(mut s) = self.servers[i - 1].take() {
            s.shutdown();
        }
    }

    fn restart(&mut self, i: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let l = loop {
            match TcpListener::bind(self.addrs[i - 1]) {
                Ok(l) => break l,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("rebind n{i}: {e}"),
            }
        };
        let s =
            DataNodeServer::start(format!("n{i}"), self.root.join(format!("dn{i}")), l).unwrap();
        self.servers[i - 1] = Some(s);
    }
}

/// Steps until the controller reports no more waiting work.
fn settle(r: &mut Rebuilder, e: &NativeEngine) {
    let stop = AtomicBool::new(false);
    for _ in 0..100 {
        if !r.step(e, &stop).unwrap() {
            return;
        }
    }
    panic!("the controller never settled");
}

#[test]
fn nodes_that_die_idle_are_found_and_their_shards_rebuilt_at_the_capped_rate() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 8);
    let e = c.engine(cfg(&td.path().join("meta"), Some("4+2")));
    let v = e.create_volume("v", 4 * EXTENT as u64).unwrap();
    let data = pattern(3, 4 * EXTENT);
    e.write(&v, 0, &data).unwrap();

    // Two nodes die with no client I/O in flight; only the probe can notice.
    c.stop(1);
    c.stop(2);
    let mut r = controller(Duration::ZERO, 1 << 20, Duration::ZERO);
    let t = Instant::now();
    settle(&mut r, &e);
    let took = t.elapsed();
    let st = r.status().clone();
    assert_eq!(
        st.lost_nodes,
        ["n1", "n2"],
        "the probe finds both dead nodes"
    );
    // Every extent had a shard on each of the first six nodes.
    assert_eq!(st.rebuilt.replicas_repaired, 8, "{st:?}");
    assert_eq!(st.rebuilt.bytes_written, 8 * (EXTENT as u64 / 4));
    let moved = st.rebuilt.bytes_read + st.rebuilt.bytes_written;
    assert!(
        took >= Duration::from_secs_f64(moved as f64 / (1 << 20) as f64 * 0.9),
        "{moved} bytes in {took:?} exceeds 1 MiB/s"
    );
    assert!(e
        .degraded_extents(&e.lost_nodes(Duration::ZERO))
        .unwrap()
        .is_empty());

    // Rebuilt onto n7 and n8, the extents again survive losing two more nodes.
    c.stop(3);
    c.stop(4);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
}

#[test]
fn a_node_back_within_the_delay_costs_no_rebuild() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 4);
    let e = c.engine(cfg(&td.path().join("meta"), None));
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    e.write(&v, 0, &pattern(1, 4096)).unwrap();

    c.stop(1);
    let mut r = controller(Duration::from_secs(60), 0, Duration::ZERO);
    settle(&mut r, &e);
    assert!(r.status().lost_nodes.is_empty());
    assert_eq!(r.status().rebuilt.replicas_repaired, 0);
    // Failing, but not yet for the delay: nothing counts as lost.
    assert!(e.lost_nodes(Duration::from_secs(60)).is_empty());
    assert_eq!(
        e.lost_nodes(Duration::ZERO),
        BTreeSet::from(["n1".to_string()])
    );

    // Back before the delay: the next probe (once the back-off ends) clears it.
    c.restart(1);
    thread::sleep(Duration::from_millis(250));
    e.probe_nodes();
    assert!(e.lost_nodes(Duration::ZERO).is_empty());
}

#[test]
fn degraded_extents_come_least_spare_first_and_unrebuildable_ones_are_left() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 6);
    let e = c.engine(cfg(&td.path().join("meta"), Some("4+2")));
    let v = e.create_volume("v", 8 * EXTENT as u64).unwrap();
    // Coded extents on all six nodes, replicated ones (below the size threshold) on three.
    for i in 0..4u64 {
        e.write(&v, 2 * i * EXTENT as u64, &pattern(i as u8, EXTENT))
            .unwrap();
        e.write(&v, (2 * i + 1) * EXTENT as u64, &pattern(9, 4096))
            .unwrap();
    }
    for n in [1, 2, 3] {
        c.stop(n);
    }
    let lost = BTreeSet::from(["n1", "n2", "n3"].map(String::from));
    let d = e.degraded_extents(&lost).unwrap();
    assert_eq!(d.len(), 8, "{d:?}");
    assert!(d.windows(2).all(|w| w[0].spare <= w[1].spare), "{d:?}");
    // Three lost parts is more than 4+2 or three replicas tolerate.
    assert!(d.iter().filter(|x| x.lost == 3).all(|x| x.spare < 0));

    let mut r = controller(Duration::ZERO, 0, Duration::ZERO);
    settle(&mut r, &e);
    let st = r.status();
    assert_eq!(st.degraded_extents, st.unrecoverable_extents, "{st:?}");
    assert_eq!(
        st.rebuilt.bytes_read, 0,
        "nothing that can't be rebuilt is read"
    );
}

fn corrupt(root: &Path, node: &str, offset: u64) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("nodes").join(node).join("nvme0.data"))
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&[0xFF; 16]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn the_scrub_resumes_across_batches_and_repairs_bit_rot() {
    let td = tempfile::tempdir().unwrap();
    let mut ec = cfg(td.path(), None);
    ec.extent_bytes = 4096;
    let e = NativeEngine::open(ec, (1..=4).map(node).collect()).unwrap();
    let v = e.create_volume("v", 100 * 4096).unwrap();
    let data = pattern(7, 100 * 4096);
    e.write(&v, 0, &data).unwrap();
    corrupt(td.path(), "n1", 4096 * 10);

    let mut r = controller(Duration::from_secs(60), 0, Duration::from_millis(1));
    thread::sleep(Duration::from_millis(5));
    let stop = AtomicBool::new(false);
    // 64 extents, then the other 36, then the end of the pass.
    assert!(r.step(&e, &stop).unwrap());
    assert_eq!(r.status().scrub.current.extents_checked, 64);
    assert!(r.step(&e, &stop).unwrap());
    assert!(!r.step(&e, &stop).unwrap());
    let st = r.status();
    assert_eq!(st.scrub.passes, 1);
    let pass = st.scrub.last.unwrap();
    assert_eq!(pass.extents_checked, 100);
    assert_eq!(pass.replicas_repaired, 1);
    assert_eq!(pass.unrecoverable, 0);
    assert_eq!(st.scrub.current.extents_checked, 0);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
}
