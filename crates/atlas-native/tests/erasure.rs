// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Erasure-coded extents: space, reads that survive lost and corrupt shards, repair onto spare
//! nodes, and coexistence with replicated extents.

use std::{
    io::{Seek, SeekFrom, Write},
    net::TcpListener,
    path::Path,
    sync::Arc,
    time::Duration,
};

use atlas_native::{
    ec::EcScheme, BlockStore, DataNodeServer, EngineConfig, FailureDomain, MetaBackend,
    NativeEngine, NativeError, Node, RemoteDevice,
};

const EXTENT: usize = 64 << 10;
/// 4+2 shards of a full extent.
const SHARD: u64 = (EXTENT / 4) as u64;

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
    c.erasure_min_bytes = 4096;
    c.node_retry_after = Duration::from_secs(60);
    c
}

fn local(root: &Path, nodes: usize, erasure: Option<&str>) -> NativeEngine {
    NativeEngine::open(cfg(root, erasure), (1..=nodes).map(node).collect()).unwrap()
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn used(e: &NativeEngine, nodes: usize) -> Vec<u64> {
    (1..=nodes)
        .map(|i| e.device_len(&format!("n{i}")).unwrap())
        .collect()
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
fn a_coded_extent_takes_one_and_a_half_times_its_size_on_six_nodes() {
    let td = tempfile::tempdir().unwrap();
    let e = local(td.path(), 7, Some("4+2"));
    let v = e.create_volume("v", 2 * EXTENT as u64).unwrap();
    let data = pattern(1, EXTENT);
    e.write(&v, 0, &data).unwrap();
    assert_eq!(used(&e, 7), [SHARD, SHARD, SHARD, SHARD, SHARD, SHARD, 0]);
    assert_eq!(e.read(&v, 0, EXTENT).unwrap(), data);
    assert_eq!(e.read(&v, 1000, 10).unwrap(), data[1000..1010]);

    // A partial overwrite reads, merges and re-codes the extent; GC returns the old shards.
    e.write(&v, 100, &[9u8; 50]).unwrap();
    let mut want = data.clone();
    want[100..150].fill(9);
    assert_eq!(e.read(&v, 0, EXTENT).unwrap(), want);
    let st = e.gc_once().unwrap();
    assert_eq!(st.reclaimed, 1);
    assert_eq!(st.freed_bytes, 6 * SHARD);
}

#[test]
fn small_extents_stay_replicated_and_old_extents_stay_readable() {
    let td = tempfile::tempdir().unwrap();
    let e = local(td.path(), 6, None);
    let v = e.create_volume("v", 2 * EXTENT as u64).unwrap();
    let old = pattern(2, EXTENT);
    e.write(&v, 0, &old).unwrap();
    drop(e);

    let e = local(td.path(), 6, Some("4+2"));
    assert_eq!(e.read(&v, 0, EXTENT).unwrap(), old);
    // Below erasure_min_bytes: three full copies.
    e.write(&v, EXTENT as u64, &[3u8; 1000]).unwrap();
    let copies = used(&e, 6)
        .iter()
        .filter(|b| **b == EXTENT as u64 + 1000)
        .count();
    assert_eq!(copies, 3);
    assert_eq!(e.read(&v, EXTENT as u64, 1000).unwrap(), vec![3u8; 1000]);
}

#[test]
fn a_corrupt_shard_is_read_around_and_repaired() {
    let td = tempfile::tempdir().unwrap();
    let e = local(td.path(), 6, Some("4+2"));
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    let data = pattern(4, EXTENT);
    e.write(&v, 0, &data).unwrap();
    // Two data shards bad: the read decodes from parity.
    corrupt(td.path(), "n1", 0);
    corrupt(td.path(), "n3", 0);
    assert_eq!(e.read(&v, 0, EXTENT).unwrap(), data);
    let t = e.telemetry.snapshot();
    assert_eq!(t.checksum_failures, 2);

    // Every node holds a shard, so each rebuilt shard goes to a fresh range on its own node.
    let st = e.repair_once().unwrap();
    assert_eq!((st.replicas_repaired, st.unrecoverable), (2, 0));
    assert_eq!(e.free_bytes().unwrap(), 2 * SHARD);
    assert_eq!(e.repair_once().unwrap().replicas_repaired, 0);
    drop(e);
    let e = local(td.path(), 6, Some("4+2"));
    assert_eq!(e.read(&v, 0, EXTENT).unwrap(), data);
    assert_eq!(e.telemetry.snapshot().checksum_failures, 0);
}

#[test]
fn three_bad_shards_of_four_plus_two_are_unrecoverable() {
    let td = tempfile::tempdir().unwrap();
    let e = local(td.path(), 6, Some("4+2"));
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    e.write(&v, 0, &pattern(5, EXTENT)).unwrap();
    for n in ["n2", "n4", "n6"] {
        corrupt(td.path(), n, 0);
    }
    assert!(matches!(
        e.read(&v, 0, EXTENT),
        Err(NativeError::Checksum(_))
    ));
    let st = e.repair_once().unwrap();
    assert_eq!((st.unrecoverable, st.replicas_repaired), (1, 0));
}

#[test]
fn a_write_needs_a_node_per_shard() {
    let td = tempfile::tempdir().unwrap();
    let e = local(td.path(), 5, Some("4+2"));
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    assert!(matches!(
        e.write(&v, 0, &pattern(6, EXTENT)),
        Err(NativeError::InsufficientReplicas {
            needed: 6,
            found: 5
        })
    ));
}

struct Cluster {
    servers: Vec<Option<DataNodeServer>>,
    engine: NativeEngine,
}

impl Cluster {
    fn start(root: &Path, n: usize, erasure: &str) -> Self {
        let servers: Vec<DataNodeServer> = (1..=n)
            .map(|i| {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                DataNodeServer::start(format!("n{i}"), root.join(format!("dn{i}")), l).unwrap()
            })
            .collect();
        let stores = servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    node(i + 1),
                    Arc::new(RemoteDevice::new(s.local_addr(), Duration::from_secs(2)))
                        as Arc<dyn BlockStore>,
                )
            })
            .collect();
        let engine = NativeEngine::open_with(
            cfg(&root.join("meta"), Some(erasure)),
            stores,
            MetaBackend::Local,
        )
        .unwrap();
        Self {
            servers: servers.into_iter().map(Some).collect(),
            engine,
        }
    }

    fn stop(&mut self, i: usize) {
        if let Some(mut s) = self.servers[i - 1].take() {
            s.shutdown();
        }
    }
}

#[test]
fn lost_data_nodes_are_read_around_then_rebuilt_onto_spares() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 8, "4+2");
    let e = &c.engine;
    let v = e.create_volume("v", 3 * EXTENT as u64).unwrap();
    let data = pattern(7, 3 * EXTENT);
    e.write(&v, 0, &data).unwrap();
    let snap = e.create_snapshot(&v, "s").unwrap();

    // Two nodes holding shards of every extent go away: reads decode from what is left.
    c.stop(1);
    c.stop(2);
    let e = &c.engine;
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    assert!(e.telemetry.snapshot().replica_fallbacks >= 1);

    // Repair rebuilds each lost shard on a node that holds no other shard of its extent.
    let st = e.repair_once().unwrap();
    assert_eq!((st.extents_checked, st.unrecoverable), (3, 0));
    assert_eq!(st.replicas_repaired, 6);
    assert_eq!(e.repair_once().unwrap().replicas_repaired, 0);

    // Two more losses are survivable again.
    c.stop(3);
    c.stop(4);
    let e = &c.engine;
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    assert_eq!(e.read_snapshot(&snap, 0, data.len()).unwrap(), data);
}
