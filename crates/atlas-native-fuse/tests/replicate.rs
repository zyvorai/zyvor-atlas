// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Replication between two live in-process clusters (one metadata and one data node each):
//! a full round, increments that copy only changed extents, failover by promotion and failback
//! by demotion.

use std::{collections::BTreeMap, net::TcpListener, path::Path, time::Duration};

use atlas_native::{
    node::{DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig},
    NodeType, SetAttr, ROOT_INO,
};
use atlas_native_fuse::{
    client::{Body, Client, ClientConfig, Retry},
    ops::{Ops, OpsConfig},
    replicate::{ReplicateConfig, ReplicateError, Replicator},
};
use reqwest::Method;
use serde_json::{json, Value};

const TOKEN: &str = "replicate-test-token";
const GRID: u64 = 64 << 10;

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

struct Cluster {
    _td: tempfile::TempDir,
    _nodes: Vec<NativeNode>,
    endpoint: String,
}

impl Cluster {
    fn start() -> Self {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), TOKEN).unwrap();
        let (data_l, meta_l) = (bind(), bind());
        let base = |id: &str, root: &Path| NodeConfig {
            node_id: id.into(),
            data_dir: root.join(id),
            http_listen: "127.0.0.1:0".parse().unwrap(),
            api_token_file: Some(root.join("token")),
            tls: None,
            http_tls: None,
            data_node: None,
            metadata: None,
            max_request_bytes: 1 << 20,
        };
        let mut dcfg = base("d1", td.path());
        dcfg.data_node = Some(DataNodeRole {
            listen: data_l.local_addr().unwrap(),
            devices: Vec::new(),
        });
        let spec = DataNodeSpec {
            id: "d1".into(),
            addr: data_l.local_addr().unwrap().to_string(),
            zone: None,
            rack: None,
            host: None,
            free_bytes: 1 << 30,
            devices: 1,
        };
        let data = NativeNode::start_with(
            dcfg,
            Listeners {
                http: Some(bind()),
                data_node: Some(data_l),
                metadata: None,
            },
        )
        .unwrap();
        let mut mcfg = base("m1", td.path());
        mcfg.metadata = Some(MetadataRole {
            listen: meta_l.local_addr().unwrap(),
            peers: [("m1".to_string(), meta_l.local_addr().unwrap().to_string())].into(),
            bootstrap: None,
            data_nodes: vec![spec],
            replicas: 1,
            erasure: None,
            erasure_min_bytes: 64 << 10,
            compression: false,
            dedup: false,
            rebuild_delay_secs: 60,
            rebuild_bytes_per_sec: 0,
            scrub_bytes_per_sec: 0,
            tiering: None,
            extent_bytes: GRID as usize,
            tick_ms: 10,
            proposal_timeout_ms: 3000,
            repair_interval_secs: 0,
            gc_interval_secs: 0,
            groups: 1,
            cache_inodes: atlas_native::store::DEFAULT_CACHE_INODES,
            store_cache_bytes: atlas_native::store::DEFAULT_STORE_CACHE_BYTES,
        });
        let meta = NativeNode::start_with(
            mcfg,
            Listeners {
                http: Some(bind()),
                data_node: None,
                metadata: Some(meta_l),
            },
        )
        .unwrap();
        let endpoint = format!("http://{}", meta.http_addr());
        Self {
            _td: td,
            _nodes: vec![data, meta],
            endpoint,
        }
    }

    fn client(&self) -> Client {
        let mut cfg = ClientConfig::new(vec![self.endpoint.clone()]);
        cfg.token = Some(TOKEN.into());
        cfg.retry_for = Duration::from_secs(20);
        Client::new(cfg).unwrap()
    }

    /// A fresh mount, so nothing is served from an earlier one's caches.
    fn ops(&self, fs: &str) -> Ops {
        Ops::new(
            self.client(),
            fs,
            OpsConfig {
                max_io_bytes: 1 << 20,
                writeback_bytes: 0,
                ..OpsConfig::default()
            },
        )
    }

    fn call(&self, method: Method, path: &str, body: Value) -> Result<Vec<u8>, String> {
        self.client()
            .request(method, path, Body::Json(body), Retry::Idempotent)
            .map_err(|e| e.to_string())
    }

    fn snapshots(&self, fs: &str) -> Vec<String> {
        let v = self
            .client()
            .json(
                Method::GET,
                "/v1/fs-snapshots",
                Body::Empty,
                Retry::Idempotent,
            )
            .unwrap();
        let mut ids: Vec<String> = v["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["fs_id"] == fs)
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }
}

fn replicator(src: &Cluster, dst: &Cluster, keep: usize) -> Replicator {
    let mut cfg = ReplicateConfig::new("data", "data");
    cfg.keep = keep;
    cfg.max_io_bytes = 1 << 20;
    // Small pages and parts, so a round takes several of each.
    cfg.diff_limit = 3;
    cfg.batch_bytes = 512;
    Replicator::new(src.client(), dst.client(), cfg)
}

/// Every name, attribute, xattr, link target and byte under the root, keyed by path.
fn tree(ops: &Ops) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(ROOT_INO, String::from("/"))];
    while let Some((ino, path)) = todo.pop() {
        let a = ops.getattr(ino).unwrap();
        let mut desc = format!(
            "{} {:?} {:o} {}:{} n{} s{} m{} c{}",
            a.ino, a.kind, a.mode, a.uid, a.gid, a.nlink, a.size, a.mtime_ns, a.ctime_ns
        );
        for x in ops.listxattr(ino).unwrap() {
            desc.push_str(&format!(" {x}={:?}", ops.getxattr(ino, &x).unwrap()));
        }
        match a.kind {
            NodeType::Dir => {
                for e in ops.readdir(ino).unwrap() {
                    if e.name != "." && e.name != ".." {
                        todo.push((e.ino, format!("{path}{}/", e.name)));
                    }
                }
            }
            NodeType::File => {
                let data = ops.read(ino, 0, a.size as usize).unwrap();
                desc.push_str(&format!(" data={:x}", checksum(&data)));
            }
            NodeType::Symlink => desc.push_str(&format!(" -> {}", ops.readlink(ino).unwrap())),
            _ => {}
        }
        out.insert(path, desc);
    }
    out
}

fn checksum(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn write(ops: &Ops, ino: u64, off: u64, data: &[u8]) {
    ops.write(ino, off, data).unwrap();
    ops.flush(ino).unwrap();
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

fn assert_replica_matches(src: &Cluster, dst: &Cluster, snapshot: &str) {
    let want = tree(&src.ops(&format!("data@{snapshot}")));
    assert_eq!(tree(&dst.ops("data")), want);
}

#[test]
fn increments_failover_and_failback() {
    let (a, b) = (Cluster::start(), Cluster::start());
    a.call(
        Method::POST,
        "/v1/fs",
        json!({ "id": "data", "name": "data" }),
    )
    .unwrap();
    let src = a.ops("data");
    let dir = src
        .mknode(ROOT_INO, "dir", NodeType::Dir, None, 0o750, 1000, 1000)
        .unwrap();
    let big = src
        .mknode(dir.ino, "big.bin", NodeType::File, None, 0o644, 1000, 1000)
        .unwrap();
    write(&src, big.ino, 0, &pattern(5 * GRID as usize + 100, 1));
    let small = src
        .mknode(ROOT_INO, "small.txt", NodeType::File, None, 0o600, 0, 0)
        .unwrap();
    write(&src, small.ino, 0, b"hello replica");
    src.mknode(
        ROOT_INO,
        "link",
        NodeType::Symlink,
        Some("dir/big.bin".into()),
        0o777,
        0,
        0,
    )
    .unwrap();
    src.setxattr(small.ino, "user.tag", b"v1", false, false)
        .unwrap();
    for i in 0..8 {
        src.mknode(
            dir.ino,
            &format!("empty-{i}"),
            NodeType::File,
            None,
            0o644,
            0,
            0,
        )
        .unwrap();
    }

    // The first round copies everything.
    let r = replicator(&a, &b, 2);
    let first = r.run_once().unwrap();
    assert_eq!(first.from, None);
    assert!(!first.resumed);
    assert_eq!(first.bytes, 5 * GRID + 100 + 13);
    assert_replica_matches(&a, &b, &first.snapshot);
    // The replica is read-only to clients.
    let rep = b.ops("data");
    assert_eq!(
        rep.mknode(ROOT_INO, "nope", NodeType::File, None, 0o644, 0, 0)
            .unwrap_err(),
        libc::EROFS
    );
    assert_eq!(
        rep.write(small.ino, 0, b"x")
            .and_then(|_| rep.flush(small.ino)),
        Err(libc::EROFS)
    );

    // Nothing changed: an empty increment.
    let idle = r.run_once().unwrap();
    assert_eq!((idle.inodes, idle.removed, idle.bytes), (0, 0, 0));
    assert_eq!(idle.from.as_deref(), Some(first.snapshot.as_str()));

    // One grid cell rewritten, a file truncated, one removed, one renamed, one new.
    write(&src, big.ino, 2 * GRID + 10, b"changed");
    src.setattr(
        small.ino,
        SetAttr {
            size: Some(5),
            ..SetAttr::default()
        },
    )
    .unwrap();
    src.unlink(dir.ino, "empty-3").unwrap();
    src.rename(dir.ino, "empty-4", ROOT_INO, "moved").unwrap();
    let new = src
        .mknode(ROOT_INO, "new.bin", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    write(&src, new.ino, 0, &pattern(1000, 9));
    let inc = r.run_once().unwrap();
    assert_eq!(inc.from.as_deref(), Some(idle.snapshot.as_str()));
    assert_eq!(inc.removed, 1);
    // The truncated file's cell is resent only if the truncate rewrote it.
    assert!(
        (GRID + 1000..=GRID + 1005).contains(&inc.bytes),
        "only the changed cells and the new file: {}",
        inc.bytes
    );
    assert_replica_matches(&a, &b, &inc.snapshot);

    // The source keeps only the base; the target the newest `keep`.
    assert_eq!(a.snapshots("data"), vec![inc.snapshot.clone()]);
    let mut kept = vec![idle.snapshot.clone(), inc.snapshot.clone()];
    kept.sort();
    assert_eq!(b.snapshots("data"), kept);

    // A round cut short after its first part is finished by the next one.
    write(&src, small.ino, 0, b"HELLO");
    a.call(
        Method::POST,
        "/v1/fs/data/snapshots",
        json!({ "id": "repl-cut-short", "name": "repl-cut-short" }),
    )
    .unwrap();
    b.call(
        Method::POST,
        "/v1/fs/data/replica/apply",
        json!({ "from": inc.snapshot, "to": "repl-cut-short", "inodes": [], "removed": [] }),
    )
    .unwrap();
    let res = r.run_once().unwrap();
    assert!(res.resumed);
    assert_eq!(res.snapshot, "repl-cut-short");
    assert_eq!(res.bytes, 5);
    assert_replica_matches(&a, &b, &res.snapshot);
    let inc = res;

    // Failover: the replica is promoted and written to.
    b.call(Method::POST, "/v1/fs/data/promote", json!({}))
        .unwrap();
    let rep = b.ops("data");
    let late = rep
        .mknode(
            ROOT_INO,
            "written-at-dr.txt",
            NodeType::File,
            None,
            0o644,
            0,
            0,
        )
        .unwrap();
    write(&rep, late.ino, 0, b"dr site");
    rep.unlink(ROOT_INO, "moved").unwrap();
    // The old source changed too after the last increment; failback discards that.
    write(&src, small.ino, 0, b"lost");

    // Failback: the reverse direction refuses a writable target and names the common snapshot.
    let back = replicator(&b, &a, 2);
    let err = back.run_once().unwrap_err();
    assert!(
        matches!(&err, ReplicateError::State(m) if m.contains(&inc.snapshot) && m.contains("demote")),
        "{err}"
    );
    a.call(
        Method::POST,
        "/v1/fs/data/demote",
        json!({ "snapshot": inc.snapshot, "base": inc.snapshot }),
    )
    .unwrap();
    let rev = back.run_once().unwrap();
    assert_eq!(rev.from.as_deref(), Some(inc.snapshot.as_str()));
    assert_eq!((rev.bytes, rev.removed), (7, 1));
    assert_replica_matches(&b, &a, &rev.snapshot);
    assert_eq!(
        a.ops("data").read(small.ino, 0, 100).unwrap(),
        b"HELLO",
        "the old source's own late write is gone"
    );

    // Back to the original direction: promote the old source, demote the DR site.
    a.call(Method::POST, "/v1/fs/data/promote", json!({}))
        .unwrap();
    b.call(
        Method::POST,
        "/v1/fs/data/demote",
        json!({ "snapshot": rev.snapshot, "base": rev.snapshot }),
    )
    .unwrap();
    let again = a.ops("data");
    write(&again, new.ino, 0, b"home again");
    let fwd = r.run_once().unwrap();
    assert_eq!(fwd.from.as_deref(), Some(rev.snapshot.as_str()));
    assert_replica_matches(&a, &b, &fwd.snapshot);
}
