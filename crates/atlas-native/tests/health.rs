// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    io::{Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    metadata::ReplicaRef, BlockStore, Catalog, DataNodeServer, EngineConfig, FailureDomain,
    MetaBackend, MetaCommand, NativeEngine, NativeError, Node, RemoteDevice,
};

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

fn cfg(root: &Path) -> EngineConfig {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = 4096;
    c
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

    /// Stops data node `i` (1-based).
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

fn status(e: &NativeEngine, id: &str) -> (bool, u64) {
    let s = e.node_status().into_iter().find(|s| s.id == id).unwrap();
    (s.up, s.failures)
}

#[test]
fn write_moves_a_replica_off_a_failed_node_and_retries_it_after_backoff() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 4);
    let mut ec = cfg(&td.path().join("meta"));
    ec.node_retry_after = Duration::from_millis(1500);
    let e = c.engine(ec);
    let v = e.create_volume("v", 3 * 4096).unwrap();

    // n1 sorts first in placement order; with it down the replica lands on the spare n4.
    c.stop(1);
    e.write(&v, 0, &[1u8; 4096]).unwrap();
    assert_eq!(status(&e, "n1"), (false, 1));
    assert_eq!(e.device_len("n4").unwrap(), 4096);

    // While backed off n1 is not even tried.
    e.write(&v, 4096, &[2u8; 4096]).unwrap();
    assert_eq!(status(&e, "n1"), (false, 1));
    let m = e.render_metrics().unwrap();
    assert!(m.contains("atlas_native_node_up{node=\"n1\"} 0"), "{m}");
    assert!(
        m.contains("atlas_native_replica_write_failures_total 1"),
        "{m}"
    );
    assert!(!m.contains("atlas_native_device_bytes{node=\"n1\"}"), "{m}");

    // After the back-off window a recovered node is used again.
    c.restart(1);
    thread::sleep(Duration::from_millis(1600));
    e.write(&v, 8192, &[3u8; 4096]).unwrap();
    assert_eq!(status(&e, "n1"), (true, 1));
    assert_eq!(e.device_len("n1").unwrap(), 4096);
    for (off, b) in [(0, 1u8), (4096, 2), (8192, 3)] {
        assert_eq!(e.read(&v, off, 4096).unwrap(), vec![b; 4096]);
    }
}

#[test]
fn write_fails_when_too_few_nodes_remain() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 3);
    let e = c.engine(cfg(&td.path().join("meta")));
    let v = e.create_volume("v", 4096).unwrap();
    c.stop(2);
    assert!(matches!(
        e.write(&v, 0, &[1u8; 4096]),
        Err(NativeError::InsufficientReplicas {
            needed: 3,
            found: 2
        })
    ));
    // The failed node is now backed off, so the next attempt fails before any I/O.
    assert!(matches!(
        e.write(&v, 0, &[1u8; 4096]),
        Err(NativeError::InsufficientReplicas {
            needed: 3,
            found: 2
        })
    ));
    assert_eq!(status(&e, "n2"), (false, 1));
}

#[test]
fn repair_re_replicates_after_losing_a_node() {
    let td = tempfile::tempdir().unwrap();
    let mut c = Cluster::start(td.path(), 4);
    let e = c.engine(cfg(&td.path().join("meta")));
    let v = e.create_volume("v", 2 * 4096).unwrap();
    e.write(&v, 0, &[4u8; 4096]).unwrap();
    e.write(&v, 4096, &[5u8; 4096]).unwrap();
    let snap = e.create_snapshot(&v, "s").unwrap();
    assert_eq!(e.device_len("n4").unwrap(), 0);

    c.stop(2);
    let st = e.repair_once().unwrap();
    assert_eq!(st.extents_checked, 2);
    assert_eq!(st.replicas_repaired, 2);
    assert_eq!((st.unrecoverable, st.deferred), (0, 0));
    assert_eq!(e.device_len("n4").unwrap(), 2 * 4096);
    assert!(e
        .render_metrics()
        .unwrap()
        .contains("atlas_native_replicas_repaired_total 2"));

    // Nothing left to do, and the extents survive losing a second original node.
    assert_eq!(e.repair_once().unwrap().replicas_repaired, 0);
    c.stop(1);
    assert_eq!(e.read(&v, 0, 4096).unwrap(), vec![4u8; 4096]);
    assert_eq!(e.read_snapshot(&snap, 4096, 4096).unwrap(), vec![5u8; 4096]);
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
fn repair_rewrites_a_corrupt_replica_and_frees_the_old_range() {
    let td = tempfile::tempdir().unwrap();
    let e = NativeEngine::open(cfg(td.path()), (1..=3).map(node).collect()).unwrap();
    let v = e.create_volume("v", 4096).unwrap();
    e.write(&v, 0, &[6u8; 4096]).unwrap();
    corrupt(td.path(), "n1", 0);

    let st = e.repair_once().unwrap();
    assert_eq!(st.replicas_repaired, 1);
    // Every other node already holds a replica, so the new copy goes to a fresh range on n1 and
    // the corrupt range returns to the free list.
    assert_eq!(e.device_len("n1").unwrap(), 2 * 4096);
    assert_eq!(e.free_bytes().unwrap(), 4096);
    assert_eq!(e.repair_once().unwrap().replicas_repaired, 0);

    drop(e);
    let e = NativeEngine::open(cfg(td.path()), (1..=3).map(node).collect()).unwrap();
    assert_eq!(e.read(&v, 0, 4096).unwrap(), vec![6u8; 4096]);
    assert_eq!(e.telemetry.snapshot().checksum_failures, 0);
    assert_eq!(e.free_bytes().unwrap(), 4096);
}

#[test]
fn repair_reports_extents_with_no_good_replica() {
    let td = tempfile::tempdir().unwrap();
    let e = NativeEngine::open(cfg(td.path()), (1..=3).map(node).collect()).unwrap();
    let v = e.create_volume("v", 4096).unwrap();
    e.write(&v, 0, &[7u8; 4096]).unwrap();
    for n in ["n1", "n2", "n3"] {
        corrupt(td.path(), n, 0);
    }
    let st = e.repair_once().unwrap();
    assert_eq!((st.unrecoverable, st.replicas_repaired), (1, 0));
    assert!(matches!(e.read(&v, 0, 4096), Err(NativeError::Checksum(_))));
}

#[test]
fn replace_replica_rejects_invalid_moves() {
    let mut c = Catalog::default();
    let r = |n: &str, off: u64| ReplicaRef {
        node_id: n.into(),
        device_index: 0,
        offset: off,
    };
    c.apply(
        1,
        1,
        &MetaCommand::CreateVolume {
            id: "v".into(),
            name: "v".into(),
            size_bytes: 4096,
        },
    )
    .unwrap();
    c.apply(
        1,
        2,
        &MetaCommand::InstallExtent {
            volume_id: "v".into(),
            logical_offset: 0,
            extent: atlas_native::metadata::ExtentRef {
                id: "e".into(),
                logical_offset: 0,
                payload_len: 4096,
                raw_len: 0,
                checksum: [0; 32],
                content: None,
                replicas: vec![r("a", 0), r("b", 0)],
                ec: None,
                created_ms: 0,
                object: None,
            },
        },
    )
    .unwrap();
    let replace = |old, new| MetaCommand::ReplaceReplica {
        extent_id: "e".into(),
        old,
        new,
    };
    for (i, bad) in [
        replace(r("a", 0), r("a", 0)),
        replace(r("a", 0), r("b", 4096)),
        replace(r("c", 0), r("d", 0)),
    ]
    .iter()
    .enumerate()
    {
        assert!(c.apply(1, 3 + i as u64, bad).is_err(), "case {i} accepted");
    }
    c.apply(1, 10, &replace(r("a", 0), r("c", 0))).unwrap();
    let replicas = &c.extents["e"].extent.replicas;
    assert_eq!(replicas, &vec![r("c", 0), r("b", 0)]);
    assert_eq!(c.free.total_bytes(), 4096);
}
