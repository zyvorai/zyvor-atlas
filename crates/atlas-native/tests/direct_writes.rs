// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Direct client writes at the engine level: the engine leases targets and grants, a "client"
//! (plain `RemoteDevice`s that hold no key) writes the data nodes, and the engine commits. Data
//! nodes require write grants; the fault cases are a client that dies between its data writes and
//! the commit, a client from a stale term, and a data node that is down.

use std::{net::TcpListener, path::Path, sync::Arc, thread, time::Duration};

use atlas_native::{
    checksum,
    device::{BlockStore, FileDevice},
    engine::{DirectCellDone, DirectCommit, DirectLease, DirectReplicaDone, NewNode},
    DataNodeServer, EngineConfig, FailureDomain, GrantKey, MetaBackend, NativeEngine, NativeError,
    Node, NodeType, RemoteDevice, ROOT_INO,
};

const EXTENT: usize = 64 << 10;
const SECRET: &[u8] = b"direct-writes-test-secret";

fn key() -> GrantKey {
    GrantKey::new(SECRET).unwrap()
}

struct Cluster {
    _td: tempfile::TempDir,
    servers: Vec<Option<DataNodeServer>>,
    engine: NativeEngine,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Cluster {
    /// `n` data nodes that require write grants, an engine over them (three replicas).
    fn start(n: usize) -> Self {
        let td = tempfile::tempdir().unwrap();
        let mut servers = Vec::new();
        let mut stores: Vec<(Node, Arc<dyn BlockStore>)> = Vec::new();
        for i in 1..=n {
            let id = format!("n{i}");
            let root = td.path().join(&id);
            std::fs::create_dir_all(&root).unwrap();
            let dev: Arc<dyn BlockStore> = Arc::new(FileDevice::open(root.join("d.data")).unwrap());
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let srv = DataNodeServer::start_devices_with(
                id.clone(),
                &root,
                vec![dev],
                l,
                None,
                Some(key()),
            )
            .unwrap();
            let remote =
                RemoteDevice::new(srv.local_addr(), Duration::from_secs(5)).with_grant_key(key());
            stores.push((
                Node {
                    id: id.clone(),
                    failure_domain: FailureDomain {
                        zone: "z".into(),
                        rack: format!("r{i}"),
                        host: format!("h{i}"),
                    },
                    free_bytes: 1 << 30,
                    healthy: true,
                },
                Arc::new(remote),
            ));
            servers.push(Some(srv));
        }
        let mut cfg = EngineConfig::new(td.path().join("engine"));
        cfg.extent_bytes = EXTENT;
        cfg.grant_key = Some(key());
        // Failed nodes are retried quickly in these tests.
        cfg.node_retry_after = Duration::from_secs(60);
        let engine = NativeEngine::open_with(cfg, stores, MetaBackend::Local).unwrap();
        Self {
            _td: td,
            servers,
            engine,
        }
    }

    fn file(&self, name: &str) -> u64 {
        let _ = self.engine.create_fs_as("f".into(), "fs");
        self.engine
            .fs_mknode(
                "f",
                ROOT_INO,
                NewNode {
                    name: name.into(),
                    op_id: format!("op-{name}"),
                    kind: NodeType::File,
                    target: None,
                    rdev: 0,
                    mode: 0o644,
                    create_mode: None,
                    uid: 0,
                    gid: 0,
                },
            )
            .unwrap()
            .ino
    }
}

fn remote(t: &atlas_native::engine::DirectTarget) -> RemoteDevice {
    // What a client holds: an address and the grants, never the key.
    RemoteDevice::new(&t.endpoint, Duration::from_secs(5))
        .on_device(t.device_index)
        .in_group(t.group)
}

/// A client's write of `data` to every target of a lease; `skip` are node ids it can't reach.
fn client_write(lease: &DirectLease, data: &[u8], skip: &[&str]) -> DirectCommit {
    let mut cells = Vec::new();
    let mut failed = Vec::new();
    for (cell, bytes) in lease
        .cells
        .iter()
        .zip(data.chunks(lease.extent_bytes as usize))
    {
        let mut replicas = Vec::new();
        for t in &cell.targets {
            if replicas.len() == lease.needed {
                break;
            }
            let dev = remote(t);
            let res = match t.offset {
                Some(off) => dev
                    .write_at_granted(Some(t.grant.clone()), lease.term, off, bytes)
                    .map(|()| (off, None)),
                None => dev.append_granted(Some(t.grant.clone()), lease.term, bytes),
            };
            match res {
                Ok((offset, receipt)) if !skip.contains(&t.node_id.as_str()) => {
                    replicas.push(DirectReplicaDone {
                        node_id: t.node_id.clone(),
                        device_index: t.device_index,
                        offset,
                        receipt,
                    })
                }
                _ => failed.push(t.node_id.clone()),
            }
        }
        cells.push(DirectCellDone {
            logical_offset: cell.logical_offset,
            checksum: hex(&checksum::sha256(bytes)),
            replicas,
        });
    }
    DirectCommit {
        lease: lease.lease.clone(),
        cells,
        failed,
    }
}

/// `(node, offset)` of the first cell's targets, sorted by node.
fn placements(lease: &DirectLease) -> Vec<(String, Option<u64>)> {
    let mut p: Vec<_> = lease.cells[0]
        .targets
        .iter()
        .map(|t| (t.node_id.clone(), t.offset))
        .collect();
    p.sort();
    p
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

#[test]
fn a_client_writes_the_data_nodes_and_the_leader_commits() {
    let c = Cluster::start(3);
    let ino = c.file("a");
    let want = data(3 * EXTENT, 1);
    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, want.len() as u64)
        .unwrap();
    assert_eq!((lease.cells.len(), lease.needed), (3, 3));
    let commit = client_write(&lease, &want, &[]);
    let attr = c.engine.direct_write_commit("f", ino, commit).unwrap();
    assert_eq!(attr.size, want.len() as u64);
    assert_eq!(c.engine.read_file("f", ino, 0, want.len()).unwrap(), want);
    assert_eq!(c.engine.direct_write_leases(), 0);
    // The scrubber agrees the replicas match the checksums the client reported.
    let st = c.engine.repair_once().unwrap();
    assert_eq!((st.replicas_repaired, st.unrecoverable), (0, 0));

    // Overwrite one extent in place the same way, after the first lease is consumed.
    let new = data(EXTENT, 9);
    let lease = c
        .engine
        .direct_write_lease("f", ino, EXTENT as u64, EXTENT as u64)
        .unwrap();
    c.engine
        .direct_write_commit("f", ino, client_write(&lease, &new, &[]))
        .unwrap();
    let mut want = want;
    want[EXTENT..2 * EXTENT].copy_from_slice(&new);
    assert_eq!(c.engine.read_file("f", ino, 0, want.len()).unwrap(), want);
}

#[test]
fn data_nodes_refuse_writes_without_a_matching_grant() {
    let c = Cluster::start(3);
    let ino = c.file("a");
    let addr = c.servers[0].as_ref().unwrap().local_addr();
    // The read credential (an address) is not a write credential.
    let anon = RemoteDevice::new(addr, Duration::from_secs(5));
    let err = anon.write_at(1, 0, b"x").unwrap_err();
    assert!(err.to_string().contains("grant required"), "{err}");
    assert!(anon.append(1, b"x").is_err());

    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    let t = &lease.cells[0].targets[0];
    let dev = remote(t);
    let bytes = data(EXTENT, 3);
    // A grant covers exactly its operation, range, length, fence and node.
    let write = |grant, fence, offset: Option<u64>, payload: &[u8]| match offset {
        Some(o) => dev.write_at_granted(Some(grant), fence, o, payload),
        None => dev.append_granted(Some(grant), fence, payload).map(|_| ()),
    };
    let other_offset = match t.offset {
        Some(o) => Some(o + 1),
        None => Some(0),
    };
    for res in [
        write(t.grant.clone(), lease.term, other_offset, &bytes),
        write(t.grant.clone(), lease.term, t.offset, &bytes[..EXTENT - 1]),
        write(t.grant.clone(), lease.term + 1, t.offset, &bytes),
    ] {
        let err = res.unwrap_err();
        assert!(
            err.to_string().contains("grant"),
            "a write outside its grant went through: {err}"
        );
    }
    // The grant of one node does not work on another.
    let t2 = &lease.cells[0].targets[1];
    let res = match t2.offset {
        Some(o) => remote(t2).write_at_granted(Some(t.grant.clone()), lease.term, o, &bytes),
        None => remote(t2)
            .append_granted(Some(t.grant.clone()), lease.term, &bytes)
            .map(|_| ()),
    };
    assert!(res.is_err());
    // An expired grant is refused.
    let expired = key().mint(
        "g",
        &t.node_id,
        lease.term,
        t.group,
        t.device_index,
        None,
        1,
        1,
    );
    let err = remote(t)
        .append_granted(Some(expired), lease.term, b"x")
        .unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");
    // And the engine's own writes still go through (it holds the key).
    c.engine.write_file("f", ino, 0, &data(EXTENT, 5)).unwrap();
}

/// Reuse of a freed range: files written, overwritten and collected leave free ranges on every
/// node for the allocator to hand out.
fn with_free_ranges(c: &Cluster) -> u64 {
    let ino = c.file("churn");
    c.engine
        .write_file("f", ino, 0, &data(6 * EXTENT, 1))
        .unwrap();
    c.engine
        .write_file("f", ino, 0, &data(6 * EXTENT, 2))
        .unwrap();
    assert_eq!(c.engine.gc_once().unwrap().reclaimed, 6);
    assert!(c.engine.free_bytes().unwrap() >= 18 * EXTENT as u64);
    // The allocator's view of the free list is reused for up to a second.
    thread::sleep(Duration::from_millis(1100));
    ino
}

#[test]
fn a_client_that_dies_before_committing_loses_its_lease_and_the_space_comes_back() {
    let c = Cluster::start(3);
    with_free_ranges(&c);
    let ino = c.file("victim");
    c.engine
        .set_direct_write_timing(Duration::from_millis(100), Duration::from_secs(2));

    let lease_a = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    let a_offsets = placements(&lease_a);
    assert!(
        a_offsets.iter().all(|(_, o)| o.is_some()),
        "free ranges expected: {a_offsets:?}"
    );
    // The client writes all its replicas, then dies: no commit.
    let doomed = data(EXTENT, 7);
    let _ = client_write(&lease_a, &doomed, &[]);

    // While the lease is live nobody else is given its ranges.
    let lease_b = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    let b_offsets = placements(&lease_b);
    for (a, b) in a_offsets.iter().zip(&b_offsets) {
        assert_eq!(a.0, b.0);
        assert_ne!(a.1, b.1, "a live lease's range was handed out twice");
    }
    c.engine.direct_write_abort(&lease_b.lease).unwrap();

    // Nothing was committed: the file is empty, the doomed bytes are unreferenced.
    assert_eq!(c.engine.fs_getattr("f", ino).unwrap().size, 0);

    // Expiry: a late commit is refused, and the lease is gone.
    thread::sleep(Duration::from_millis(250));
    let late = client_write(&lease_a, &doomed, &[]);
    let err = c.engine.direct_write_commit("f", ino, late).unwrap_err();
    assert!(matches!(err, NativeError::Invalid(_)), "{err}");
    assert_eq!(c.engine.fs_getattr("f", ino).unwrap().size, 0);
    assert_eq!(c.engine.direct_write_leases(), 0);

    // The ranges stay quarantined for the grace period (the client could still be mid-write)...
    thread::sleep(Duration::from_millis(1200));
    let lease_c = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    let c_offsets = placements(&lease_c);
    for (a, cc) in a_offsets.iter().zip(&c_offsets) {
        assert_eq!(a.0, cc.0);
        assert_ne!(a.1, cc.1, "a range was reused inside the quarantine");
    }
    c.engine.direct_write_abort(&lease_c.lease).unwrap();

    // ...and then the free list offers them again.
    thread::sleep(Duration::from_millis(3300));
    let lease_d = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    assert_eq!(
        placements(&lease_d),
        a_offsets,
        "expired space was not reclaimed"
    );
    // And a fresh client writes and commits there normally.
    let good = data(EXTENT, 11);
    c.engine
        .direct_write_commit("f", ino, client_write(&lease_d, &good, &[]))
        .unwrap();
    assert_eq!(c.engine.read_file("f", ino, 0, EXTENT).unwrap(), good);
}

#[test]
fn a_client_from_a_stale_term_is_fenced() {
    let c = Cluster::start(3);
    let ino = c.file("a");
    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    // A newer leader (term 9) has written to the data nodes since the lease was granted.
    for s in c.servers.iter().flatten() {
        let g = key().mint("newer", "", 9, 0, 0, None, 1, u64::MAX);
        RemoteDevice::new(s.local_addr(), Duration::from_secs(5))
            .append_granted(Some(g), 9, b"x")
            .unwrap();
        assert_eq!(s.fence(), 9);
    }
    let t = &lease.cells[0].targets[0];
    let bytes = data(EXTENT, 1);
    let err = match t.offset {
        Some(o) => remote(t).write_at_granted(Some(t.grant.clone()), lease.term, o, &bytes),
        None => remote(t)
            .append_granted(Some(t.grant.clone()), lease.term, &bytes)
            .map(|_| ()),
    }
    .unwrap_err();
    assert!(matches!(err, NativeError::Fenced { current: 9 }), "{err}");
    // Nothing from it can be committed.
    let commit = client_write(&lease, &bytes, &[]);
    assert!(commit.cells[0].replicas.is_empty());
    assert!(c.engine.direct_write_commit("f", ino, commit).is_err());
    assert_eq!(c.engine.fs_getattr("f", ino).unwrap().size, 0);
}

#[test]
fn a_replica_that_is_down_is_replaced_by_the_spare() {
    let mut c = Cluster::start(4);
    let ino = c.file("a");
    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, 2 * EXTENT as u64)
        .unwrap();
    assert!(
        lease.cells.iter().all(|cell| cell.targets.len() == 4),
        "needed + one spare"
    );
    let dead = lease.cells[0].targets[0].node_id.clone();
    let idx: usize = dead[1..].parse::<usize>().unwrap() - 1;
    c.servers[idx].take().unwrap().shutdown();

    let want = data(2 * EXTENT, 4);
    let commit = client_write(&lease, &want, &[]);
    assert!(commit.failed.contains(&dead));
    c.engine.direct_write_commit("f", ino, commit).unwrap();
    assert_eq!(c.engine.read_file("f", ino, 0, want.len()).unwrap(), want);
    // Three live replicas each, none on the dead node, and the leader stopped using it.
    let layout = c.engine.file_layout("f", ino, 0, want.len()).unwrap();
    for ext in &layout.extents {
        assert_eq!(ext.replicas.len(), 3);
        assert!(ext.replicas.iter().all(|r| r.node_id != dead));
    }
    let again = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    assert!(again.cells[0].targets.iter().all(|t| t.node_id != dead));

    // With only three nodes up there is no spare left; losing another leaves the client short,
    // so nothing commits (the FUSE client then writes through the leader).
    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    assert_eq!(lease.cells[0].targets.len(), 3);
    let second = lease.cells[0].targets[1].node_id.clone();
    let commit = client_write(&lease, &data(EXTENT, 8), &[second.as_str()]);
    let err = c.engine.direct_write_commit("f", ino, commit).unwrap_err();
    assert!(
        matches!(err, NativeError::InsufficientReplicas { .. }),
        "{err}"
    );
    assert_eq!(c.engine.read_file("f", ino, 0, want.len()).unwrap(), want);
}

#[test]
fn a_commit_is_checked_against_its_lease() {
    let c = Cluster::start(3);
    let ino = c.file("a");
    let other = c.file("b");
    let bytes = data(EXTENT, 2);

    // A lease belongs to the file it was granted for, and is consumed by any commit.
    let lease = c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64)
        .unwrap();
    let commit = client_write(&lease, &bytes, &[]);
    assert!(c
        .engine
        .direct_write_commit("f", other, commit.clone())
        .is_err());
    assert!(
        c.engine.direct_write_commit("f", ino, commit).is_err(),
        "lease reused"
    );

    // A replica that was never leased, a wrong offset and a missing append receipt are refused.
    let forge = |edit: &dyn Fn(&mut DirectCommit)| {
        let lease = c
            .engine
            .direct_write_lease("f", ino, 0, EXTENT as u64)
            .unwrap();
        let mut commit = client_write(&lease, &bytes, &[]);
        edit(&mut commit);
        c.engine.direct_write_commit("f", ino, commit).unwrap_err()
    };
    let e = forge(&|cm| cm.cells[0].replicas[0].node_id = "intruder".into());
    assert!(e.to_string().contains("not leased"), "{e}");
    let e = forge(&|cm| {
        let r = &mut cm.cells[0].replicas[0];
        r.offset += 1;
        r.receipt = None;
    });
    assert!(matches!(e, NativeError::Invalid(_)), "{e}");
    let e = forge(&|cm| cm.cells[0].checksum = "zz".into());
    assert!(e.to_string().contains("checksum"), "{e}");
    let e = forge(&|cm| cm.cells.clear());
    assert!(matches!(e, NativeError::Invalid(_)), "{e}");
    assert_eq!(c.engine.fs_getattr("f", ino).unwrap().size, 0);

    // Misaligned ranges and too-large leases are refused up front.
    assert!(c
        .engine
        .direct_write_lease("f", ino, 1, EXTENT as u64)
        .is_err());
    assert!(c
        .engine
        .direct_write_lease("f", ino, 0, EXTENT as u64 + 1)
        .is_err());
    assert!(c
        .engine
        .direct_write_lease("f", ino, 0, 65 * EXTENT as u64)
        .is_err());
}

#[test]
fn direct_writes_need_a_grant_key_and_plain_replicas() {
    let td = tempfile::tempdir().unwrap();
    let node = |i: usize| Node {
        id: format!("n{i}"),
        failure_domain: FailureDomain {
            zone: "z".into(),
            rack: format!("r{i}"),
            host: format!("h{i}"),
        },
        free_bytes: 1 << 30,
        healthy: true,
    };
    let local = Cluster::local_engine(td.path(), |c| c.grant_key = None, node);
    local.create_fs_as("f".into(), "fs").unwrap();
    let err = local
        .direct_write_lease("f", ROOT_INO, 0, EXTENT as u64)
        .unwrap_err();
    assert!(err.to_string().contains("grant key"), "{err}");

    let td2 = tempfile::tempdir().unwrap();
    let compressed = Cluster::local_engine(td2.path(), |c| c.compression = true, node);
    compressed.create_fs_as("f".into(), "fs").unwrap();
    assert!(compressed
        .direct_write_lease("f", ROOT_INO, 0, EXTENT as u64)
        .is_err());
}

impl Cluster {
    /// An engine over local devices (no network endpoints), for the refusals that come first.
    fn local_engine(
        root: &Path,
        edit: impl FnOnce(&mut EngineConfig),
        node: impl Fn(usize) -> Node,
    ) -> NativeEngine {
        let mut cfg = EngineConfig::new(root);
        cfg.extent_bytes = EXTENT;
        cfg.grant_key = Some(key());
        edit(&mut cfg);
        NativeEngine::open(cfg, (1..=3).map(node).collect()).unwrap()
    }
}
