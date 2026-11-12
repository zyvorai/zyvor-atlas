// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The FUSE operations layer against a live in-process cluster: 3 Raft metadata nodes and 3
//! data nodes on localhost.

use std::{collections::BTreeMap, net::TcpListener, path::Path, sync::Arc, time::Duration};

use atlas_native::{
    node::{DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig},
    NodeType, SetAttr, TlsIdentity, ROOT_INO,
};
use atlas_native_fuse::{
    client::{Body, Client, ClientConfig, Retry},
    ops::{DirectReads, Ops, OpsConfig},
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use reqwest::Method;
use serde_json::json;

const TOKEN: &str = "fuse-test-token";

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

struct Cluster {
    _td: tempfile::TempDir,
    meta: BTreeMap<String, Option<NativeNode>>,
    data: Vec<(String, NativeNode)>,
}

impl Cluster {
    fn start() -> Self {
        Self::start_with(None)
    }

    /// Three data nodes, three metadata voters; `erasure` codes extents of 64 KiB and up.
    fn start_with(erasure: Option<&str>) -> Self {
        Self::start_nodes(erasure, 3)
    }

    /// `data_nodes` data nodes (three replicas each), three metadata voters.
    fn start_nodes(erasure: Option<&str>, data_nodes: usize) -> Self {
        Self::start_cfg(erasure, data_nodes, false)
    }

    /// Data nodes that refuse writes without a grant, and leaders that can lease them.
    fn start_granted(data_nodes: usize) -> Self {
        Self::start_cfg(None, data_nodes, true)
    }

    fn start_cfg(erasure: Option<&str>, data_nodes: usize, grants: bool) -> Self {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), TOKEN).unwrap();
        std::fs::write(td.path().join("grant"), "fuse-test-grant-secret").unwrap();
        let data_l: Vec<(String, TcpListener)> = (1..=data_nodes)
            .map(|i| (format!("d{i}"), bind()))
            .collect();
        let meta_l: Vec<(String, TcpListener)> =
            (1..=3).map(|i| (format!("m{i}"), bind())).collect();
        let specs: Vec<DataNodeSpec> = data_l
            .iter()
            .map(|(id, l)| DataNodeSpec {
                id: id.clone(),
                addr: l.local_addr().unwrap().to_string(),
                zone: None,
                rack: None,
                host: None,
                free_bytes: 1 << 30,
                devices: 1,
            })
            .collect();
        let peers: BTreeMap<String, String> = meta_l
            .iter()
            .map(|(id, l)| (id.clone(), l.local_addr().unwrap().to_string()))
            .collect();
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
            grant_key_file: grants.then(|| root.join("grant")),
        };
        let data: Vec<(String, NativeNode)> = data_l
            .into_iter()
            .map(|(id, l)| {
                let mut cfg = base(&id, td.path());
                cfg.data_node = Some(DataNodeRole {
                    listen: l.local_addr().unwrap(),
                    devices: Vec::new(),
                });
                let node = NativeNode::start_with(
                    cfg,
                    Listeners {
                        http: Some(bind()),
                        data_node: Some(l),
                        metadata: None,
                    },
                )
                .unwrap();
                (id, node)
            })
            .collect();
        let meta = meta_l
            .into_iter()
            .map(|(id, l)| {
                let mut cfg = base(&id, td.path());
                cfg.metadata = Some(MetadataRole {
                    listen: l.local_addr().unwrap(),
                    peers: peers.clone(),
                    bootstrap: None,
                    data_nodes: specs.clone(),
                    replicas: 3,
                    erasure: erasure.map(|e| e.parse().unwrap()),
                    erasure_min_bytes: 64 << 10,
                    compression: false,
                    dedup: false,
                    cross_group_dedup: false,
                    rebuild_delay_secs: 60,
                    rebuild_bytes_per_sec: 0,
                    scrub_bytes_per_sec: 0,
                    tiering: None,
                    extent_bytes: 64 << 10,
                    tick_ms: 10,
                    proposal_timeout_ms: 3000,
                    repair_interval_secs: 0,
                    gc_interval_secs: 0,
                    groups: 1,
                    cache_inodes: atlas_native::store::DEFAULT_CACHE_INODES,
                    store_cache_bytes: atlas_native::store::DEFAULT_STORE_CACHE_BYTES,
                });
                let n = NativeNode::start_with(
                    cfg,
                    Listeners {
                        http: Some(bind()),
                        data_node: None,
                        metadata: Some(l),
                    },
                )
                .unwrap();
                (id, Some(n))
            })
            .collect();
        Self {
            _td: td,
            meta,
            data,
        }
    }

    /// Stops data node `id`.
    fn stop_data(&mut self, id: &str) {
        self.data.retain(|(n, _)| n != id);
    }

    fn endpoints(&self) -> Vec<String> {
        self.meta
            .values()
            .flatten()
            .map(|n| format!("http://{}", n.http_addr()))
            .collect()
    }

    fn client(&self) -> Client {
        let mut cfg = ClientConfig::new(self.endpoints());
        cfg.token = Some(TOKEN.into());
        cfg.retry_for = Duration::from_secs(20);
        Client::new(cfg).unwrap()
    }

    /// Stops the current Raft leader.
    fn kill_leader(&mut self) -> String {
        loop {
            let leader = self.meta.iter().find_map(|(id, n)| {
                let mut cfg =
                    ClientConfig::new(vec![format!("http://{}", n.as_ref()?.http_addr())]);
                cfg.token = Some(TOKEN.into());
                let s = Client::new(cfg)
                    .ok()?
                    .json(Method::GET, "/v1/status", Body::Empty, Retry::Idempotent)
                    .ok()?;
                (s["metadata"]["role"] == "leader").then(|| id.clone())
            });
            if let Some(id) = leader {
                self.meta.insert(id.clone(), None);
                return id;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The test nodes accept at most 1 MiB per request.
fn mount(c: &Cluster, fs: &str, cfg: OpsConfig) -> Ops {
    Ops::new(
        c.client(),
        fs,
        OpsConfig {
            max_io_bytes: 1 << 20,
            ..cfg
        },
    )
}

fn create_fs(c: &Cluster, id: &str) {
    c.client()
        .json(
            Method::POST,
            "/v1/fs",
            Body::Json(json!({ "id": id, "name": id })),
            Retry::Idempotent,
        )
        .unwrap();
}

#[test]
fn checkpoint_writes_go_in_the_background_and_report_failures_at_flush() {
    let c = Cluster::start();
    create_fs(&c, "ck");
    let cfg = OpsConfig {
        writeback_bytes: 256 << 10,
        writeback_parallel: 4,
        ..OpsConfig::default()
    };
    let ops = mount(&c, "ck", cfg.clone());
    let f = ops
        .mknode(ROOT_INO, "ckpt.pt", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    // A header, then kernel-sized chunks: runs after the first are whole extents.
    let mut want: Vec<u8> = b"header-100".repeat(10);
    want.extend((0..3u32 << 20).map(|i| (i ^ (i >> 11)) as u8));
    ops.write(f.ino, 0, &want[..100]).unwrap();
    for (i, chunk) in want[100..].chunks(128 << 10).enumerate() {
        ops.write(f.ino, 100 + (i * (128 << 10)) as u64, chunk)
            .unwrap();
    }
    // Bytes on the wire count in the size before they land.
    assert_eq!(ops.getattr(f.ino).unwrap().size, want.len() as u64);
    ops.flush(f.ino).unwrap();
    let other = mount(&c, "ck", OpsConfig::default());
    assert_eq!(other.getattr(f.ino).unwrap().size, want.len() as u64);
    assert_eq!(other.read(f.ino, 0, 4 << 20).unwrap(), want);

    // Another client removes the file: the background runs fail. Later writes to it fail
    // fast, and flush (fsync, close) reports the failure once.
    let g = ops
        .mknode(ROOT_INO, "gone.pt", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    other.unlink(ROOT_INO, "gone.pt").unwrap();
    for i in 0..8u64 {
        if ops.write(g.ino, i << 17, &[7u8; 128 << 10]) == Err(libc::ENOENT) {
            break;
        }
    }
    assert_eq!(ops.flush(g.ino), Err(libc::ENOENT));
    assert_eq!(ops.flush(g.ino), Ok(()));
}

/// One file written sequentially in kernel-sized chunks, with runs sent in the write call and
/// in the background.
/// `BENCH_MIB=256 cargo test --release -p atlas-native-fuse --test ops checkpoint_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn checkpoint_bench() {
    let mib: usize = std::env::var("BENCH_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let c = Cluster::start();
    create_fs(&c, "bench");
    let chunk = vec![0x5au8; 128 << 10];
    for parallel in [0, 4] {
        let ops = mount(
            &c,
            "bench",
            OpsConfig {
                writeback_bytes: 1 << 20,
                writeback_parallel: parallel,
                ..OpsConfig::default()
            },
        );
        let f = ops
            .mknode(
                ROOT_INO,
                &format!("ckpt-{parallel}"),
                NodeType::File,
                None,
                0o644,
                0,
                0,
            )
            .unwrap();
        let t = std::time::Instant::now();
        for i in 0..(mib << 20) / chunk.len() {
            ops.write(f.ino, (i * chunk.len()) as u64, &chunk).unwrap();
        }
        ops.flush(f.ino).unwrap();
        println!(
            "checkpoint through Ops: {mib} MiB, 1 MiB runs, {parallel} in the background: {:.0} MiB/s",
            mib as f64 / t.elapsed().as_secs_f64()
        );
    }
}

#[test]
fn posix_operations_through_the_ops_layer() {
    let c = Cluster::start();
    create_fs(&c, "f1");
    let ops = mount(&c, "f1", OpsConfig::default());

    let d = ops
        .mknode(ROOT_INO, "dir", NodeType::Dir, None, 0o755, 1000, 1000)
        .unwrap();
    assert_eq!((d.kind, d.mode, d.nlink), (NodeType::Dir, 0o755, 2));
    let f = ops
        .mknode(d.ino, "file.txt", NodeType::File, None, 0o644, 1000, 1000)
        .unwrap();
    assert_eq!(
        ops.mknode(d.ino, "file.txt", NodeType::File, None, 0o644, 0, 0),
        Err(libc::EEXIST)
    );

    // Many small sequential writes are buffered into one run; the size includes them at once.
    let mut want = Vec::new();
    for i in 0..300u32 {
        let chunk = format!("line {i}\n").into_bytes();
        assert_eq!(
            ops.write(f.ino, want.len() as u64, &chunk).unwrap(),
            chunk.len()
        );
        want.extend_from_slice(&chunk);
    }
    assert_eq!(ops.getattr(f.ino).unwrap().size, want.len() as u64);
    ops.flush(f.ino).unwrap();
    assert_eq!(ops.read(f.ino, 0, 1 << 20).unwrap(), want);
    // Another client sees the flushed data.
    let other = mount(&c, "f1", OpsConfig::default());
    assert_eq!(other.read(f.ino, 0, 1 << 20).unwrap(), want);

    // A large write crossing extents, then a read past EOF is short.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let g = ops
        .mknode(ROOT_INO, "big.bin", NodeType::File, None, 0o600, 0, 0)
        .unwrap();
    ops.write(g.ino, 0, &big).unwrap();
    ops.flush(g.ino).unwrap();
    assert_eq!(ops.read(g.ino, 299_990, 100).unwrap(), &big[299_990..]);
    assert_eq!(ops.read(g.ino, 0, 400_000).unwrap(), big);

    // Truncate drops buffered bytes past the new size too.
    ops.write(f.ino, 0, b"HEAD").unwrap();
    let a = ops
        .setattr(
            f.ino,
            SetAttr {
                size: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(a.size, 2);
    assert_eq!(ops.read(f.ino, 0, 100).unwrap(), b"HE");

    let l = ops
        .mknode(
            ROOT_INO,
            "link",
            NodeType::Symlink,
            Some("dir/file.txt".into()),
            0o777,
            0,
            0,
        )
        .unwrap();
    assert_eq!(ops.readlink(l.ino).unwrap(), "dir/file.txt");
    assert_eq!(ops.link(f.ino, ROOT_INO, "hard").unwrap().nlink, 2);
    ops.rename(d.ino, "file.txt", ROOT_INO, "moved.txt")
        .unwrap();
    assert_eq!(ops.lookup(d.ino, "file.txt"), Err(libc::ENOENT));
    assert_eq!(ops.lookup(ROOT_INO, "moved.txt").unwrap().ino, f.ino);
    let names: Vec<String> = ops
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["big.bin", "dir", "hard", "link", "moved.txt"]);
    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let (page, next) = ops.readdir_page(ROOT_INO, after.as_deref(), 2).unwrap();
        assert!(page.len() <= 2);
        paged.extend(page.into_iter().map(|e| e.name));
        match next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    assert_eq!(paged, names);

    // A file unlinked while open stays readable through the handle until the last close.
    let o = ops
        .mknode(ROOT_INO, "open.txt", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.opened(o.ino);
    ops.write(o.ino, 0, b"still here").unwrap();
    ops.unlink(ROOT_INO, "open.txt").unwrap();
    assert_eq!(ops.lookup(ROOT_INO, "open.txt"), Err(libc::ENOENT));
    assert_eq!(ops.getattr(o.ino).unwrap().nlink, 0);
    assert_eq!(ops.read(o.ino, 0, 64).unwrap(), b"still here");
    assert!(ops
        .readdir(ROOT_INO)
        .unwrap()
        .iter()
        .all(|e| e.name != "open.txt" && !e.name.starts_with(".atlas_hidden_")));
    ops.released(o.ino).unwrap();
    assert_eq!(ops.getattr(o.ino), Err(libc::ENOENT));

    assert_eq!(ops.rmdir(ROOT_INO, "moved.txt"), Err(libc::ENOTDIR));
    ops.mknode(d.ino, "x", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    assert_eq!(ops.rmdir(ROOT_INO, "dir"), Err(libc::ENOTEMPTY));
    assert_eq!(ops.unlink(ROOT_INO, "dir"), Err(libc::EISDIR));
    assert_eq!(
        ops.lookup(ROOT_INO, &"n".repeat(300)),
        Err(libc::ENAMETOOLONG)
    );
    ops.unlink(d.ino, "x").unwrap();
    ops.rmdir(ROOT_INO, "dir").unwrap();
    let st = ops.statfs().unwrap();
    assert_eq!(st.inodes, 4);

    // A snapshot mount is frozen and read-only.
    c.client()
        .json(
            Method::POST,
            "/v1/fs/f1/snapshots",
            Body::Json(json!({ "id": "s1", "name": "s1" })),
            Retry::Idempotent,
        )
        .unwrap();
    // g was read (and read ahead) above; this mount's own write must be visible at once.
    ops.write(g.ino, 0, b"changed").unwrap();
    assert_eq!(ops.read(g.ino, 0, 7).unwrap(), b"changed");
    let snap = mount(&c, "f1@s1", OpsConfig::default());
    assert!(snap.read_only());
    assert_eq!(snap.read(g.ino, 0, 7).unwrap(), &big[..7]);
    assert_eq!(snap.write(g.ino, 0, b"x"), Err(libc::EROFS));
    assert_eq!(
        snap.mknode(ROOT_INO, "n", NodeType::File, None, 0, 0, 0),
        Err(libc::EROFS)
    );
}

fn direct(identity: Option<Arc<TlsIdentity>>) -> OpsConfig {
    OpsConfig {
        readahead_bytes: 0,
        direct_reads: Some(DirectReads {
            identity,
            timeout: Duration::from_secs(5),
            prefer_host: None,
            rdma: None,
        }),
        ..OpsConfig::default()
    }
}

#[test]
fn direct_reads_fetch_extents_from_the_data_nodes() {
    let c = Cluster::start();
    create_fs(&c, "dr");
    let writer = mount(&c, "dr", OpsConfig::default());
    let f = writer
        .mknode(ROOT_INO, "data.bin", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    // Spans many 64 KiB extents and ends mid-extent.
    let data: Vec<u8> = (0..700_001u32).map(|i| (i * 31 % 251) as u8).collect();
    writer.write(f.ino, 0, &data).unwrap();
    writer.flush(f.ino).unwrap();

    let ops = mount(&c, "dr", direct(None));
    assert_eq!(ops.read(f.ino, 0, 1 << 20).unwrap(), data);
    assert_eq!(ops.read(f.ino, 65_530, 20).unwrap(), &data[65_530..65_550]);
    assert_eq!(ops.read(f.ino, 699_990, 100).unwrap(), &data[699_990..]);
    assert_eq!(ops.direct_fallbacks(), 0);

    // A rewrite moves extents to new replicas; a fresh layout follows it.
    writer.write(f.ino, 100_000, b"rewritten").unwrap();
    writer.flush(f.ino).unwrap();
    assert_eq!(ops.read(f.ino, 100_000, 9).unwrap(), b"rewritten");
    assert_eq!(ops.direct_fallbacks(), 0);
}

#[test]
fn locality_api_and_host_preferring_direct_reads() {
    let c = Cluster::start_nodes(None, 4);
    create_fs(&c, "loc");
    let writer = mount(&c, "loc", OpsConfig::default());
    let dir = writer
        .mknode(ROOT_INO, "train set", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    let f = writer
        .mknode(dir.ino, "shard-0", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 239) as u8).collect();
    writer.write(f.ino, 0, &data).unwrap();
    writer.flush(f.ino).unwrap();

    let l = c
        .client()
        .json(
            Method::GET,
            "/v1/fs/loc/locality?path=%2Ftrain%20set",
            Body::Empty,
            Retry::Idempotent,
        )
        .unwrap();
    assert_eq!(
        (l["files"].as_u64(), l["dirs"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(l["bytes"], 300_000);
    // Three replicas over four hosts; the last host listed holds the least.
    let hosts = l["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 4);
    let host_bytes = |l: &serde_json::Value, h: &str| {
        l["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["host"] == h)
            .unwrap()["bytes"]
            .as_u64()
            .unwrap()
    };
    let total: u64 = hosts.iter().map(|h| h["bytes"].as_u64().unwrap()).sum();
    assert_eq!(total, 3 * 300_000);
    let target = hosts[3]["host"].as_str().unwrap().to_string();
    assert!(host_bytes(&l, &target) < 300_000);

    let (mut moved, mut local, mut after) = (0, 0, serde_json::Value::Null);
    loop {
        let pin = c
            .client()
            .json(
                Method::POST,
                "/v1/fs/loc/locality/pin",
                Body::Json(json!({
                    "path": "/train set", "hosts": [target], "after": after, "max_extents": 2,
                })),
                Retry::Idempotent,
            )
            .unwrap();
        assert_eq!(pin["deferred"], 0);
        moved += pin["moved"].as_u64().unwrap();
        local += pin["local"].as_u64().unwrap();
        after = pin["next"].clone();
        if after.is_null() {
            break;
        }
    }
    assert!(moved > 0);
    assert_eq!(serde_json::Value::from(moved + local), l["extents"]);
    let l2 = c
        .client()
        .json(
            Method::GET,
            "/v1/fs/loc/locality?path=/train%20set",
            Body::Empty,
            Retry::Idempotent,
        )
        .unwrap();
    assert_eq!(host_bytes(&l2, &target), 300_000);
    let total: u64 = l2["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 3 * 300_000);
    assert_eq!(writer.read(f.ino, 0, 1 << 20).unwrap(), data);
    let err = c
        .client()
        .json(
            Method::POST,
            "/v1/fs/loc/locality/pin",
            Body::Json(json!({ "hosts": ["elsewhere"] })),
            Retry::Idempotent,
        )
        .unwrap_err();
    assert!(matches!(
        err,
        atlas_native_fuse::client::Error::Api { status: 400, .. }
    ));

    let layout = c
        .client()
        .json(
            Method::GET,
            &format!("/v1/fs/loc/inodes/{}/layout?offset=0&len=300000", f.ino),
            Body::Empty,
            Retry::Idempotent,
        )
        .unwrap();
    let replicas = layout["extents"][0]["replicas"].as_array().unwrap();
    assert!(replicas.iter().all(|r| r["host"] == r["node_id"]));

    let mut cfg = direct(None);
    if let Some(d) = cfg.direct_reads.as_mut() {
        d.prefer_host = Some(target.clone());
    }
    let ops = mount(&c, "loc", cfg);
    assert_eq!(ops.read(f.ino, 0, 1 << 20).unwrap(), data);
    assert_eq!(ops.direct_fallbacks(), 0);
}

#[test]
fn direct_reads_of_erasure_coded_extents_read_the_data_shards() {
    let c = Cluster::start_with(Some("2+1"));
    create_fs(&c, "ec");
    let writer = mount(&c, "ec", OpsConfig::default());
    let f = writer
        .mknode(ROOT_INO, "data.bin", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    // Ten coded 64 KiB extents and a replicated tail below erasure_min_bytes.
    let data: Vec<u8> = (0..700_001u32).map(|i| (i * 37 % 241) as u8).collect();
    writer.write(f.ino, 0, &data).unwrap();
    writer.flush(f.ino).unwrap();
    let layout = c
        .client()
        .json(
            Method::GET,
            &format!("/v1/fs/ec/inodes/{}/layout?offset=0&len=700001", f.ino),
            Body::Empty,
            Retry::Idempotent,
        )
        .unwrap();
    let extents = layout["extents"].as_array().unwrap();
    assert_eq!(extents.len(), 11);
    assert_eq!(extents[0]["ec"]["data"], 2);
    assert_eq!(extents[0]["ec"]["shard_len"], 32 << 10);
    assert!(extents[10].get("ec").is_none());

    let ops = mount(&c, "ec", direct(None));
    assert_eq!(ops.read(f.ino, 0, 1 << 20).unwrap(), data);
    assert_eq!(ops.read(f.ino, 65_530, 20).unwrap(), &data[65_530..65_550]);
    assert_eq!(ops.direct_fallbacks(), 0);
    // Through the leader too.
    assert_eq!(writer.read(f.ino, 0, 1 << 20).unwrap(), data);
}

#[test]
fn direct_reads_fall_back_to_the_leader() {
    let c = Cluster::start();
    create_fs(&c, "fb");
    let writer = mount(&c, "fb", OpsConfig::default());
    let f = writer
        .mknode(ROOT_INO, "x", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    writer.write(f.ino, 0, &data).unwrap();
    writer.flush(f.ino).unwrap();

    // The test data nodes speak plaintext, so a client that insists on TLS can't use them.
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = params.self_signed(&key).unwrap();
    let issuer = Issuer::new(params, key);
    let client_key = KeyPair::generate().unwrap();
    let mut cp = CertificateParams::new(vec!["client".to_string()]).unwrap();
    cp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = cp.signed_by(&client_key, &issuer).unwrap();
    let identity = TlsIdentity::from_pem(
        ca.pem().as_bytes(),
        cert.pem().as_bytes(),
        client_key.serialize_pem().as_bytes(),
    )
    .unwrap();

    let ops = mount(&c, "fb", direct(Some(Arc::new(identity))));
    assert_eq!(ops.read(f.ino, 0, 1 << 20).unwrap(), data);
    assert!(ops.direct_fallbacks() > 0);
}

#[test]
fn calls_ride_through_a_leader_failure() {
    let mut c = Cluster::start();
    create_fs(&c, "f2");
    let ops = mount(
        &c,
        "f2",
        OpsConfig {
            writeback_bytes: 0,
            ..OpsConfig::default()
        },
    );
    let f = ops
        .mknode(ROOT_INO, "a", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(f.ino, 0, b"before").unwrap();
    let killed = c.kill_leader();
    // The client still lists the dead node; it must skip it and find the new leader.
    let g = ops
        .mknode(ROOT_INO, "b", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(g.ino, 0, b"after").unwrap();
    assert_eq!(
        ops.read(f.ino, 0, 64).unwrap(),
        b"before",
        "killed {killed}"
    );
    assert_eq!(ops.read(g.ino, 0, 64).unwrap(), b"after");
    ops.unlink(ROOT_INO, "a").unwrap();
    assert_eq!(ops.lookup(ROOT_INO, "a"), Err(libc::ENOENT));
}

#[test]
fn snapshot_mounts_are_read_only_and_clones_are_isolated() {
    let c = Cluster::start();
    create_fs(&c, "src");
    let ops = mount(&c, "src", OpsConfig::default());
    let d = ops
        .mknode(ROOT_INO, "d", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    let f = ops
        .mknode(d.ino, "f", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    // Spans several 64 KiB extents so overwrites, truncates and clones share some of them.
    let original: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    ops.write(f.ino, 0, &original).unwrap();
    ops.flush(f.ino).unwrap();

    let snap = c
        .client()
        .json(
            Method::POST,
            "/v1/fs/src/snapshots",
            Body::Json(json!({ "id": "s1", "name": "before" })),
            Retry::Idempotent,
        )
        .unwrap();
    assert_eq!(snap["id"], "s1");

    // Change the source after the snapshot: overwrite the middle, truncate, add and remove names.
    ops.write(f.ino, 70_000, &[0xAA; 1000]).unwrap();
    ops.flush(f.ino).unwrap();
    ops.setattr(
        f.ino,
        SetAttr {
            size: Some(150_000),
            ..SetAttr::default()
        },
    )
    .unwrap();
    ops.mknode(ROOT_INO, "new", NodeType::File, None, 0o644, 0, 0)
        .unwrap();

    let ro = mount(&c, "src@s1", OpsConfig::default());
    assert!(ro.read_only());
    let rd = ro.lookup(ROOT_INO, "d").unwrap();
    let rf = ro.lookup(rd.ino, "f").unwrap();
    assert_eq!(rf.size, original.len() as u64);
    assert_eq!(ro.read(rf.ino, 0, original.len()).unwrap(), original);
    assert_eq!(ro.lookup(ROOT_INO, "new"), Err(libc::ENOENT));
    assert_eq!(
        ro.mknode(ROOT_INO, "x", NodeType::File, None, 0o644, 0, 0),
        Err(libc::EROFS)
    );
    assert_eq!(ro.unlink(rd.ino, "f"), Err(libc::EROFS));
    assert_eq!(ro.write(rf.ino, 0, b"no"), Err(libc::EROFS));

    // A clone starts from the snapshot; writes to it touch neither the snapshot nor the source.
    let cl = c
        .client()
        .json(
            Method::POST,
            "/v1/fs-snapshots/s1/clone",
            Body::Json(json!({ "id": "cl", "name": "clone" })),
            Retry::Idempotent,
        )
        .unwrap();
    assert_eq!(cl["id"], "cl");
    let clone = mount(&c, "cl", OpsConfig::default());
    let cd = clone.lookup(ROOT_INO, "d").unwrap();
    let cf = clone.lookup(cd.ino, "f").unwrap();
    assert_eq!(clone.read(cf.ino, 0, original.len()).unwrap(), original);
    clone.write(cf.ino, 0, &[0x55; 70_000]).unwrap();
    clone.flush(cf.ino).unwrap();
    clone
        .mknode(ROOT_INO, "only-in-clone", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    assert_eq!(ro.lookup(ROOT_INO, "only-in-clone"), Err(libc::ENOENT));
    assert_eq!(ops.lookup(ROOT_INO, "only-in-clone"), Err(libc::ENOENT));
    assert_eq!(clone.read(cf.ino, 0, 4).unwrap(), [0x55; 4]);
    assert_eq!(ro.read(rf.ino, 0, original.len()).unwrap(), original);
    let now = ops.read(f.ino, 0, 200_000).unwrap();
    assert_eq!(now.len(), 150_000);
    assert_eq!(now[..70_000], original[..70_000]);
    assert_eq!(now[70_000..71_000], [0xAA; 1000]);

    // Deleting the source keeps the snapshot readable; deleting the snapshot keeps the clone.
    c.client()
        .request(Method::DELETE, "/v1/fs/src", Body::Empty, Retry::Remove)
        .unwrap();
    let ro = mount(&c, "src@s1", OpsConfig::default());
    assert_eq!(ro.read(rf.ino, 0, original.len()).unwrap(), original);
    c.client()
        .request(
            Method::DELETE,
            "/v1/fs-snapshots/s1",
            Body::Empty,
            Retry::Remove,
        )
        .unwrap();
    assert_eq!(ro.getattr(rf.ino), Err(libc::ENOENT));
    let after = clone.read(cf.ino, 0, original.len()).unwrap();
    assert_eq!(after[..70_000], [0x55; 70_000]);
    assert_eq!(after[70_000..], original[70_000..]);
}

#[test]
fn concurrent_creates_do_not_starve_each_other() {
    let c = Cluster::start();
    create_fs(&c, "busy");
    // Retries ride out an election on a slow runner; a request starved behind the others kept
    // timing out for longer than the whole retry budget.
    let mut cfg = ClientConfig::new(c.endpoints());
    cfg.token = Some(TOKEN.into());
    let ops = std::sync::Arc::new(Ops::new(
        Client::new(cfg).unwrap(),
        "busy",
        OpsConfig::default(),
    ));
    let threads: Vec<_> = (0..16)
        .map(|t| {
            let ops = ops.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    ops.mknode(
                        ROOT_INO,
                        &format!("f{t}-{i}"),
                        NodeType::File,
                        None,
                        0o644,
                        0,
                        0,
                    )
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(ops.readdir(ROOT_INO).unwrap().len(), 16 * 25);
}

#[test]
fn extended_attributes_round_trip_and_follow_snapshots() {
    let c = Cluster::start();
    create_fs(&c, "x");
    let ops = mount(&c, "x", OpsConfig::default());
    let f = ops
        .mknode(ROOT_INO, "f", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    assert_eq!(ops.listxattr(f.ino).unwrap(), Vec::<String>::new());
    assert_eq!(ops.getxattr(f.ino, "user.a"), Err(libc::ENODATA));
    ops.setxattr(f.ino, "user.a", b"one", false, false).unwrap();
    ops.setxattr(f.ino, "user.b c", &[0, 255, 7], true, false)
        .unwrap();
    assert_eq!(ops.getxattr(f.ino, "user.a").unwrap(), b"one");
    assert_eq!(ops.getxattr(f.ino, "user.b c").unwrap(), [0, 255, 7]);
    assert_eq!(
        ops.setxattr(f.ino, "user.a", b"x", true, false),
        Err(libc::EEXIST)
    );
    assert_eq!(
        ops.setxattr(f.ino, "user.zz", b"x", false, true),
        Err(libc::ENODATA)
    );
    ops.setxattr(f.ino, "user.a", b"two", false, true).unwrap();
    assert_eq!(ops.getxattr(f.ino, "user.a").unwrap(), b"two");
    assert_eq!(
        ops.setxattr(f.ino, "system.posix_acl_access", b"x", false, false),
        Err(libc::EINVAL)
    );
    assert_eq!(
        ops.setxattr(f.ino, "system.other", b"x", false, false),
        Err(libc::EOPNOTSUPP)
    );
    assert_eq!(
        ops.setxattr(f.ino, "user.big", &vec![1; 65 << 10], false, false),
        Err(libc::E2BIG)
    );
    assert_eq!(ops.listxattr(f.ino).unwrap(), ["user.a", "user.b c"]);

    c.client()
        .json(
            Method::POST,
            "/v1/fs/x/snapshots",
            Body::Json(json!({ "id": "xs", "name": "s" })),
            Retry::Idempotent,
        )
        .unwrap();
    ops.removexattr(f.ino, "user.a").unwrap();
    assert_eq!(ops.removexattr(f.ino, "user.a"), Err(libc::ENODATA));
    assert_eq!(ops.listxattr(f.ino).unwrap(), ["user.b c"]);

    let snap = mount(&c, "x@xs", OpsConfig::default());
    assert_eq!(snap.getxattr(f.ino, "user.a").unwrap(), b"two");
    assert_eq!(
        snap.setxattr(f.ino, "user.n", b"x", false, false),
        Err(libc::EROFS)
    );
}

#[test]
fn file_locks_are_seen_by_every_mount() {
    let c = Cluster::start();
    create_fs(&c, "lk");
    let a = mount(&c, "lk", OpsConfig::default());
    let b = mount(&c, "lk", OpsConfig::default());
    let f = a
        .mknode(ROOT_INO, "f", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    use atlas_native_fuse::locks::{F_RDLCK as rd, F_UNLCK as un, F_WRLCK as wr};
    assert_eq!(a.lock_session(), None, "no session before the first lock");
    a.setlk(f.ino, 1, 0, 99, wr, 11, false).unwrap();
    assert!(a.lock_session().is_some());
    // Same lock owner number, other mount: still another owner.
    assert_eq!(b.setlk(f.ino, 1, 50, 50, rd, 22, false), Err(libc::EAGAIN));
    let conflict = b.getlk(f.ino, 1, 0, u64::MAX, rd).unwrap().unwrap();
    assert_eq!(
        (conflict.start, conflict.end, conflict.typ, conflict.pid),
        (0, 99, wr, 11)
    );
    assert_eq!(b.getlk(f.ino, 1, 100, 200, wr).unwrap(), None);
    b.setlk(f.ino, 1, 100, u64::MAX, rd, 22, false).unwrap();

    // A blocking request waits until the holder lets go (here: closes the file).
    let waiter = std::thread::scope(|s| {
        let t = s.spawn(|| b.setlk(f.ino, 2, 0, 9, wr, 22, true));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!t.is_finished());
        a.release_locks(f.ino, 1).unwrap();
        t.join().unwrap()
    });
    waiter.unwrap();
    assert_eq!(a.setlk(f.ino, 1, 0, 0, rd, 11, false), Err(libc::EAGAIN));
    b.setlk(f.ino, 2, 0, u64::MAX, un, 22, false).unwrap();
    a.setlk(f.ino, 1, 0, 0, rd, 11, false).unwrap();

    // Unmounting closes the session and releases what it held.
    a.close_session();
    drop(a);
    b.setlk(f.ino, 3, 0, u64::MAX, wr, 22, false).unwrap_err();
    b.setlk(f.ino, 1, 0, u64::MAX, wr, 22, false).unwrap();

    // A snapshot mount never conflicts: nothing writes there.
    let snap = mount(&c, "lk@none", OpsConfig::default());
    assert_eq!(snap.getlk(f.ino, 1, 0, 9, wr), Ok(None));
    assert_eq!(snap.lock_sessions_lost(), 0);
}

#[test]
fn cache_leases_are_recalled_before_another_mount_changes_anything() {
    let c = Cluster::start();
    create_fs(&c, "cl");
    let leased = OpsConfig {
        cache_leases: true,
        writeback_bytes: 0,
        ..OpsConfig::default()
    };
    let a = mount(&c, "cl", leased.clone());
    let b = mount(&c, "cl", leased);
    assert_eq!(a.kernel_ttl(), Duration::ZERO);
    let f = a
        .mknode(ROOT_INO, "f", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    assert_eq!(b.lookup(ROOT_INO, "f").unwrap().size, 0);
    assert_eq!(b.getattr(f.ino).unwrap().size, 0);
    let held: u64 = c
        .endpoints()
        .iter()
        .map(|e| {
            let mut cfg = ClientConfig::new(vec![e.clone()]);
            cfg.token = Some(TOKEN.into());
            let m = Client::new(cfg)
                .unwrap()
                .request(Method::GET, "/metrics", Body::Empty, Retry::Idempotent)
                .unwrap();
            String::from_utf8(m)
                .unwrap()
                .lines()
                .filter(|l| l.starts_with("atlas_native_cache_leases{"))
                .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
                .sum::<u64>()
        })
        .sum();
    assert!(held >= 2, "leases on the root and the file: {held}");

    // Each change waits for b to drop what it cached, so b never sees the old state, and b's
    // recall thread answers well within the lease.
    let started = std::time::Instant::now();
    a.write(f.ino, 0, b"hello").unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(b.getattr(f.ino).unwrap().size, 5);
    a.rename(ROOT_INO, "f", ROOT_INO, "g").unwrap();
    assert_eq!(b.lookup(ROOT_INO, "f"), Err(libc::ENOENT));
    assert_eq!(b.lookup(ROOT_INO, "g").unwrap().ino, f.ino);
    a.setattr(
        f.ino,
        SetAttr {
            mode: Some(0o600),
            ..SetAttr::default()
        },
    )
    .unwrap();
    assert_eq!(b.getattr(f.ino).unwrap().mode & 0o777, 0o600);
    a.unlink(ROOT_INO, "g").unwrap();
    assert_eq!(b.lookup(ROOT_INO, "g"), Err(libc::ENOENT));
    // A mount's own changes do not wait on its own leases and show at once.
    let h = b
        .mknode(ROOT_INO, "h", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    b.write(h.ino, 0, b"own").unwrap();
    assert_eq!(b.getattr(h.ino).unwrap().size, 3);
    assert_eq!(a.lookup(ROOT_INO, "h").unwrap().size, 3);
}

#[test]
fn quota_overruns_fail_with_edquot_directly_and_at_flush() {
    let c = Cluster::start();
    create_fs(&c, "q");
    c.client()
        .json(
            Method::PUT,
            "/v1/fs/q/quota",
            Body::Json(json!({ "max_bytes": 1 << 20 })),
            Retry::Idempotent,
        )
        .unwrap();
    let ops = mount(
        &c,
        "q",
        OpsConfig {
            writeback_bytes: 0,
            ..OpsConfig::default()
        },
    );
    assert_eq!(
        ops.statfs().unwrap().quota.and_then(|q| q.max_bytes),
        Some(1 << 20)
    );
    let f = ops
        .mknode(ROOT_INO, "a", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(f.ino, 0, &[1u8; 1 << 20]).unwrap();
    assert_eq!(ops.write(f.ino, 1 << 20, b"x"), Err(libc::EDQUOT));
    ops.unlink(ROOT_INO, "a").unwrap();

    // With writeback the overrun surfaces on a later write or at flush, never silently.
    let wb = mount(
        &c,
        "q",
        OpsConfig {
            writeback_bytes: 256 << 10,
            writeback_parallel: 4,
            ..OpsConfig::default()
        },
    );
    let g = wb
        .mknode(ROOT_INO, "b", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    let mut failed = None;
    for i in 0..16u64 {
        if let Err(e) = wb.write(g.ino, i << 17, &[2u8; 128 << 10]) {
            failed = Some(e);
            break;
        }
    }
    let failed = failed.or_else(|| wb.flush(g.ino).err());
    assert_eq!(failed, Some(libc::EDQUOT));
}

#[test]
fn direct_io_writes_through_and_reads_around_the_caches() {
    let c = Cluster::start();
    create_fs(&c, "dio");
    // Large write-back and read-ahead windows, so only the direct paths can explain what
    // another mount sees.
    let cfg = OpsConfig {
        writeback_bytes: 8 << 20,
        readahead_bytes: 4 << 20,
        ttl: Duration::from_secs(60),
        ..OpsConfig::default()
    };
    let a = mount(&c, "dio", cfg.clone());
    let b = mount(&c, "dio", cfg);
    let f = a
        .mknode(ROOT_INO, "db", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    // A buffered write stays in this mount until flushed...
    a.write(f.ino, 0, b"buffered").unwrap();
    assert!(b.read_direct(f.ino, 0, 64).unwrap().is_empty());
    // ...and a direct write sends it first, then lands before returning.
    a.write_through(f.ino, 8, b"-direct").unwrap();
    assert_eq!(b.read_direct(f.ino, 0, 64).unwrap(), b"buffered-direct");

    // A read-ahead window in `b` would serve stale bytes; a direct read doesn't.
    assert_eq!(b.read(f.ino, 0, 8).unwrap(), b"buffered");
    a.write_through(f.ino, 0, b"BUFFERED").unwrap();
    assert_eq!(b.read(f.ino, 0, 8).unwrap(), b"buffered");
    assert_eq!(b.read_direct(f.ino, 0, 8).unwrap(), b"BUFFERED");
    // Unaligned and past-the-end reads behave like any read.
    assert_eq!(b.read_direct(f.ino, 3, 4).unwrap(), b"FERE");
    assert!(b.read_direct(f.ino, 100, 4).unwrap().is_empty());
}

#[test]
fn dot_lookups_resolve_handles_by_inode() {
    let c = Cluster::start();
    create_fs(&c, "dots");
    let ops = mount(&c, "dots", OpsConfig::default());
    let a = ops
        .mknode(ROOT_INO, "a", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    let b = ops
        .mknode(a.ino, "b", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    let f = ops
        .mknode(b.ino, "f", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    assert_eq!(ops.lookup_dot(b.ino, ".").unwrap().ino, b.ino);
    assert_eq!(ops.lookup_dot(b.ino, "..").unwrap().ino, a.ino);
    assert_eq!(ops.lookup_dot(ROOT_INO, "..").unwrap().ino, ROOT_INO);
    assert_eq!(ops.lookup_dot(f.ino, "..").unwrap_err(), libc::ENOTDIR);
    // A rename moves `..`, and nothing cached the old parent.
    ops.rename(a.ino, "b", ROOT_INO, "b2").unwrap();
    assert_eq!(ops.lookup_dot(b.ino, "..").unwrap().ino, ROOT_INO);
}

// ---- Direct client writes -------------------------------------------------------------------
//
// The cluster's extent size here is 64 KiB. With `direct_writes` on, a mount sends whole
// extents to the data nodes itself under short-lived grants; the metadata leader only leases
// and commits them.

const EXT: usize = 64 << 10;

fn direct_writes() -> OpsConfig {
    OpsConfig {
        writeback_bytes: 1 << 20,
        readahead_bytes: 0,
        direct_writes: Some(atlas_native_fuse::direct_write::DirectWrites {
            identity: None,
            timeout: Duration::from_secs(5),
            rdma: None,
        }),
        ..OpsConfig::default()
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31) as u8) ^ ((i >> 9) as u8) ^ seed)
        .collect()
}

fn new_file(ops: &Ops, name: &str) -> u64 {
    ops.mknode(ROOT_INO, name, NodeType::File, None, 0o644, 0, 0)
        .unwrap()
        .ino
}

/// What a client holds after `POST .../direct-write`: the targets and their grants.
fn lease_over_http(
    c: &Cluster,
    fs: &str,
    ino: u64,
    offset: u64,
    len: u64,
) -> atlas_native::engine::DirectLease {
    let v = c
        .client()
        .json(
            Method::POST,
            &format!("/v1/fs/{fs}/inodes/{ino}/direct-write"),
            Body::Json(json!({ "offset": offset, "len": len })),
            Retry::Idempotent,
        )
        .unwrap();
    serde_json::from_value(v).unwrap()
}

/// Writes one target the way the mount's direct writer does; returns the error if refused.
fn write_target(
    lease: &atlas_native::engine::DirectLease,
    t: &atlas_native::engine::DirectTarget,
    data: &[u8],
) -> Result<(), atlas_native::NativeError> {
    let dev = atlas_native::RemoteDevice::new(&t.endpoint, Duration::from_secs(5))
        .on_device(t.device_index)
        .in_group(t.group);
    match t.offset {
        Some(o) => dev.write_at_granted(Some(t.grant.clone()), lease.term, o, data),
        None => dev
            .append_granted(Some(t.grant.clone()), lease.term, data)
            .map(|_| ()),
    }
}

#[test]
fn direct_writes_send_whole_extents_to_the_data_nodes() {
    let c = Cluster::start_granted(3);
    create_fs(&c, "dw");
    let ops = mount(&c, "dw", direct_writes());
    let f = new_file(&ops, "ckpt.bin");

    // Aligned: eight whole extents in one run go as one lease.
    let a = pattern(8 * EXT, 1);
    ops.write(f, 0, &a).unwrap();
    ops.flush(f).unwrap();
    let (direct, fallbacks) = ops.direct_write_counts();
    assert!(
        direct >= 1 && fallbacks == 0,
        "direct {direct}, fallbacks {fallbacks}"
    );

    // Unaligned: the head and tail go through the leader, the whole extents in between direct.
    let g = new_file(&ops, "ragged.bin");
    let b = pattern(5 * EXT + 1234, 2);
    ops.write(g, 100, &b).unwrap();
    ops.flush(g).unwrap();
    let (direct_after, fallbacks) = ops.direct_write_counts();
    assert!(direct_after > direct && fallbacks == 0);

    // A run smaller than an extent never leaves the leader path.
    let h = new_file(&ops, "small.bin");
    ops.write(h, 0, b"tiny").unwrap();
    ops.flush(h).unwrap();
    assert_eq!(ops.direct_write_counts().0, direct_after);

    // A plain mount (reading through the leader) sees every byte.
    let plain = mount(&c, "dw", OpsConfig::default());
    assert_eq!(plain.read(f, 0, a.len()).unwrap(), a);
    assert_eq!(plain.getattr(f).unwrap().size, a.len() as u64);
    let mut want = vec![0u8; 100];
    want.extend_from_slice(&b);
    assert_eq!(plain.read(g, 0, want.len()).unwrap(), want);
    assert_eq!(plain.read(h, 0, 16).unwrap(), b"tiny");

    // Overwriting committed extents directly replaces them whole.
    let a2 = pattern(2 * EXT, 3);
    ops.write(f, 2 * EXT as u64, &a2).unwrap();
    ops.flush(f).unwrap();
    let mut a = a;
    a[2 * EXT..4 * EXT].copy_from_slice(&a2);
    // A new mount: `plain` may still serve this file from its read-ahead window.
    let fresh = mount(&c, "dw", OpsConfig::default());
    assert_eq!(fresh.read(f, 0, a.len()).unwrap(), a);
    assert_eq!(ops.direct_write_counts().1, 0);
}

#[test]
fn direct_writes_fall_back_to_the_leader_when_the_cluster_cannot_grant() {
    // No grant key configured: the leader refuses every lease, the mount keeps working.
    let c = Cluster::start();
    create_fs(&c, "nb");
    let ops = mount(&c, "nb", direct_writes());
    let f = new_file(&ops, "a.bin");
    let want = pattern(40 * EXT, 4);
    ops.write(f, 0, &want).unwrap();
    ops.flush(f).unwrap();
    let (direct, fallbacks) = ops.direct_write_counts();
    assert_eq!(direct, 0);
    assert!(fallbacks >= 1);
    let plain = mount(&c, "nb", OpsConfig::default());
    assert_eq!(plain.read(f, 0, want.len()).unwrap(), want);
}

#[test]
fn a_down_data_node_is_covered_by_the_spare_replica() {
    let mut c = Cluster::start_granted(4);
    create_fs(&c, "dn");
    c.stop_data("d2");
    let ops = mount(&c, "dn", direct_writes());
    let f = new_file(&ops, "a.bin");
    let want = pattern(12 * EXT, 5);
    ops.write(f, 0, &want).unwrap();
    ops.flush(f).unwrap();
    let (direct, fallbacks) = ops.direct_write_counts();
    assert!(direct >= 1, "the write did not go direct");
    assert_eq!(fallbacks, 0, "a spare should have covered the dead node");
    let plain = mount(&c, "dn", OpsConfig::default());
    assert_eq!(plain.read(f, 0, want.len()).unwrap(), want);
    // Every extent has its three replicas on the live nodes.
    let layout: atlas_native::engine::FileLayout = serde_json::from_value(
        c.client()
            .json(
                Method::GET,
                &format!("/v1/fs/dn/inodes/{f}/layout?offset=0&len={}", want.len()),
                Body::Empty,
                Retry::Idempotent,
            )
            .unwrap(),
    )
    .unwrap();
    for ext in &layout.extents {
        assert_eq!(ext.replicas.len(), 3);
        assert!(ext.replicas.iter().all(|r| r.node_id != "d2"));
    }
}

#[test]
fn a_client_that_dies_before_committing_leaves_nothing_visible() {
    let c = Cluster::start_granted(3);
    create_fs(&c, "die");
    let ops = mount(&c, "die", OpsConfig::default());
    let f = new_file(&ops, "a.bin");
    let orphan = pattern(2 * EXT, 6);

    // The client leases, writes every replica of both extents, then dies before committing.
    let lease = lease_over_http(&c, "die", f, 0, orphan.len() as u64);
    assert_eq!(lease.cells.len(), 2);
    for (cell, bytes) in lease.cells.iter().zip(orphan.chunks(EXT)) {
        for t in cell.targets.iter().take(lease.needed) {
            write_target(&lease, t, bytes).unwrap();
        }
    }
    // Not visible: the file has no size and no data.
    assert_eq!(ops.getattr(f).unwrap().size, 0);
    assert!(ops.read(f, 0, orphan.len()).unwrap().is_empty());

    // The same range is written normally; the orphaned bytes never surface.
    let real = pattern(2 * EXT, 7);
    ops.write(f, 0, &real).unwrap();
    ops.flush(f).unwrap();
    assert_eq!(ops.read(f, 0, real.len()).unwrap(), real);
    let other = mount(&c, "die", OpsConfig::default());
    assert_eq!(other.read(f, 0, real.len()).unwrap(), real);

    // The dead client's lease is still outstanding until it expires, but it commits nothing
    // later either way: a commit naming it for different bytes is refused or ignored by
    // content, so we only require that the committed file is unchanged after trying.
    let _ = c.client().json(
        Method::POST,
        &format!("/v1/fs/die/inodes/{f}/direct-abort"),
        Body::Json(json!({ "lease": lease.lease })),
        Retry::Idempotent,
    );
    assert_eq!(other.read(f, 0, real.len()).unwrap(), real);
}

#[test]
fn a_stale_term_client_is_fenced_after_a_leader_failover() {
    let mut c = Cluster::start_granted(3);
    create_fs(&c, "fo");
    let ops = mount(&c, "fo", OpsConfig::default());
    let f = new_file(&ops, "a.bin");
    ops.write(f, 0, b"seed").unwrap();
    ops.flush(f).unwrap();

    // A client leases under the current leader's term...
    let lease = lease_over_http(&c, "fo", f, 0, EXT as u64);
    let stale = pattern(EXT, 8);

    // ...which then dies; the new leader's first full-extent write raises the data nodes' fence.
    c.kill_leader();
    let after = mount(&c, "fo", direct_writes());
    let g = new_file(&after, "b.bin");
    let fresh = pattern(4 * EXT, 9);
    after.write(g, 0, &fresh).unwrap();
    after.flush(g).unwrap();
    assert_eq!(after.read(g, 0, fresh.len()).unwrap(), fresh);

    // The stale client's grants are for the old term: every data node refuses them.
    let t = &lease.cells[0].targets[0];
    let err = write_target(&lease, t, &stale).unwrap_err();
    assert!(
        matches!(err, atlas_native::NativeError::Fenced { current } if current > lease.term),
        "{err}"
    );
    // And the new leader knows nothing of its lease, so a commit is refused.
    let commit = json!({
        "lease": lease.lease,
        "cells": [{
            "logical_offset": 0,
            "checksum": "00".repeat(32),
            "replicas": [],
        }],
    });
    let err = c
        .client()
        .json(
            Method::POST,
            &format!("/v1/fs/fo/inodes/{f}/direct-commit"),
            Body::Json(commit),
            Retry::Idempotent,
        )
        .unwrap_err();
    assert_eq!(err.errno(), libc::EINVAL, "{err}");
    assert_eq!(ops.read(f, 0, 4).unwrap(), b"seed");
}

// ---- RDMA to the data nodes, over a fake transport (no RDMA hardware needed) ----

fn rdma_cfg() -> (Arc<atlas_native::FakeTransport>, atlas_native::RdmaConfig) {
    let t = atlas_native::FakeTransport::new();
    let cfg = atlas_native::RdmaConfig::new(t.clone(), 1);
    (t, cfg)
}

fn offer_rdma(c: &Cluster, t: &Arc<atlas_native::FakeTransport>) {
    for (_, n) in &c.data {
        n.offer_rdma_transport(t.clone());
    }
}

#[test]
fn direct_reads_go_over_rdma_when_the_nodes_offer_it() {
    let c = Cluster::start();
    create_fs(&c, "rr");
    let writer = mount(&c, "rr", OpsConfig::default());
    let f = new_file(&writer, "data.bin");
    let data = pattern(11 * EXT + 77, 4);
    writer.write(f, 0, &data).unwrap();
    writer.flush(f).unwrap();

    let (t, rdma) = rdma_cfg();
    offer_rdma(&c, &t);
    let mut cfg = direct(None);
    cfg.direct_reads.as_mut().unwrap().rdma = Some(rdma);
    let ops = mount(&c, "rr", cfg);
    assert_eq!(ops.read(f, 0, data.len()).unwrap(), data);
    assert_eq!(ops.direct_fallbacks(), 0);
    let s = ops.cfg.rdma_stats().unwrap();
    assert!(s.requests >= 11 && s.connects >= 1, "{s:?}");
    assert_eq!((s.connect_failures, s.midflight_errors), (0, 0), "{s:?}");
    assert!(t.server_opens.load(std::sync::atomic::Ordering::SeqCst) >= 1);
}

#[test]
fn direct_reads_use_tcp_when_the_nodes_do_not_offer_rdma() {
    if std::env::var("ATLAS_NATIVE_RDMA").is_ok_and(|v| v == "1") {
        return;
    }
    let c = Cluster::start();
    create_fs(&c, "rt");
    let writer = mount(&c, "rt", OpsConfig::default());
    let f = new_file(&writer, "data.bin");
    let data = pattern(5 * EXT, 6);
    writer.write(f, 0, &data).unwrap();
    writer.flush(f).unwrap();

    let (_t, rdma) = rdma_cfg();
    let mut cfg = direct(None);
    cfg.direct_reads.as_mut().unwrap().rdma = Some(rdma);
    let ops = mount(&c, "rt", cfg);
    assert_eq!(ops.read(f, 0, data.len()).unwrap(), data);
    // The data came straight from the nodes over TCP, with RDMA tried and counted as failed.
    assert_eq!(ops.direct_fallbacks(), 0);
    let s = ops.cfg.rdma_stats().unwrap();
    assert_eq!(s.requests, 0, "{s:?}");
    assert!(
        s.connect_failures >= 1 && s.tcp_fallback_connects >= 1,
        "{s:?}"
    );
}

#[test]
fn direct_writes_go_over_rdma_to_nodes_that_require_grants() {
    let c = Cluster::start_granted(3);
    create_fs(&c, "rw");
    let (t, rdma) = rdma_cfg();
    offer_rdma(&c, &t);
    let mut cfg = direct_writes();
    cfg.direct_writes.as_mut().unwrap().rdma = Some(rdma);
    let ops = mount(&c, "rw", cfg);
    let f = new_file(&ops, "ckpt.bin");
    let a = pattern(8 * EXT, 7);
    ops.write(f, 0, &a).unwrap();
    ops.flush(f).unwrap();
    let (direct, fallbacks) = ops.direct_write_counts();
    assert!(
        direct >= 1 && fallbacks == 0,
        "direct {direct}, fallbacks {fallbacks}"
    );
    let s = ops.cfg.rdma_stats().unwrap();
    assert!(s.requests >= 8 && s.connects >= 1, "{s:?}");
    assert_eq!((s.connect_failures, s.midflight_errors), (0, 0), "{s:?}");

    let fresh = mount(&c, "rw", OpsConfig::default());
    assert_eq!(fresh.read(f, 0, a.len()).unwrap(), a);
}

#[test]
fn rdma_with_a_tls_identity_is_a_configuration_error() {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = params.self_signed(&key).unwrap();
    let issuer = Issuer::new(params, key);
    let client_key = KeyPair::generate().unwrap();
    let mut cp = CertificateParams::new(vec!["client".to_string()]).unwrap();
    cp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let cert = cp.signed_by(&client_key, &issuer).unwrap();
    let identity = Arc::new(
        TlsIdentity::from_pem(
            ca.pem().as_bytes(),
            cert.pem().as_bytes(),
            client_key.serialize_pem().as_bytes(),
        )
        .unwrap(),
    );
    let (_t, rdma) = rdma_cfg();

    let mut reads = direct(Some(identity.clone()));
    reads.direct_reads.as_mut().unwrap().rdma = Some(rdma.clone());
    let e = reads.validate().unwrap_err();
    assert!(e.contains("plaintext") && e.contains("direct reads"), "{e}");

    let mut writes = direct_writes();
    let w = writes.direct_writes.as_mut().unwrap();
    w.identity = Some(identity);
    w.rdma = Some(rdma.clone());
    assert!(writes.validate().unwrap_err().contains("direct writes"));

    // Plaintext RDMA and TLS without RDMA are both fine.
    let mut ok = direct(None);
    ok.direct_reads.as_mut().unwrap().rdma = Some(rdma);
    assert!(ok.validate().is_ok());
    assert!(direct(None).validate().is_ok());
}

// ---- RDMA to the data nodes over a REAL verbs device (SoftRoCE rxe0; hardware not required) ----
//
// These mirror the fake-transport tests above but use `rdma_link::verbs_transport()`, so they need
// a build with `--features rdma` on Linux and an RDMA device. SoftRoCE is enough and is not CI:
//   rdma link add rxe0 type rxe netdev lo
// GID index 1 unless ATLAS_RDMA_GID_INDEX is set. A pass on rxe0 checks plumbing only, not an
// RDMA NIC and not throughput. Every RDMA connection pins about
// 1 MiB on each end (and these tests hold both ends in one process), so the first two need a high
// RLIMIT_MEMLOCK (e.g. `ulimit -l unlimited`); the third runs under the default limit.
//   ATLAS_RDMA_GID_INDEX=<n> cargo test -p atlas-native-fuse --features rdma --test ops -- --ignored real_rdma

fn verbs() -> (Arc<dyn atlas_native::rdma_link::RdmaTransport>, u32) {
    let t = atlas_native::rdma_link::verbs_transport()
        .expect("no verbs transport: build with --features rdma on Linux");
    let gid = std::env::var("ATLAS_RDMA_GID_INDEX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    (t, gid)
}

#[test]
#[ignore = "needs an RDMA device; SoftRoCE is enough and hardware is not required (`rdma link add rxe0 type rxe netdev lo`, GID 1 or ATLAS_RDMA_GID_INDEX). Not CI. A pass on rxe0 is plumbing only, not NIC throughput. Also --features rdma and a high RLIMIT_MEMLOCK"]
fn real_rdma_direct_reads_use_the_verbs_transport() {
    let c = Cluster::start();
    create_fs(&c, "vr");
    let writer = mount(&c, "vr", OpsConfig::default());
    let f = new_file(&writer, "data.bin");
    let data = pattern(11 * EXT + 77, 4);
    writer.write(f, 0, &data).unwrap();
    writer.flush(f).unwrap();

    let (t, gid) = verbs();
    for (_, n) in &c.data {
        n.offer_rdma_transport(t.clone());
    }
    let mut cfg = direct(None);
    cfg.direct_reads.as_mut().unwrap().rdma = Some(atlas_native::RdmaConfig::new(t, gid));
    let ops = mount(&c, "vr", cfg);
    assert_eq!(ops.read(f, 0, data.len()).unwrap(), data);
    assert_eq!(ops.direct_fallbacks(), 0);
    let s = ops.cfg.rdma_stats().unwrap();
    assert!(s.requests >= 11 && s.connects >= 1, "{s:?}");
    assert_eq!((s.connect_failures, s.midflight_errors), (0, 0), "{s:?}");
}

#[test]
#[ignore = "needs an RDMA device; SoftRoCE is enough and hardware is not required (`rdma link add rxe0 type rxe netdev lo`, GID 1 or ATLAS_RDMA_GID_INDEX). Not CI. A pass on rxe0 is plumbing only, not NIC throughput. Also --features rdma and a high RLIMIT_MEMLOCK"]
fn real_rdma_direct_writes_use_the_verbs_transport() {
    let c = Cluster::start_granted(3);
    create_fs(&c, "vw");
    let (t, gid) = verbs();
    for (_, n) in &c.data {
        n.offer_rdma_transport(t.clone());
    }
    let mut cfg = direct_writes();
    cfg.direct_writes.as_mut().unwrap().rdma = Some(atlas_native::RdmaConfig::new(t, gid));
    let ops = mount(&c, "vw", cfg);
    let f = new_file(&ops, "ckpt.bin");
    let a = pattern(8 * EXT, 7);
    ops.write(f, 0, &a).unwrap();
    ops.flush(f).unwrap();
    let (direct, fallbacks) = ops.direct_write_counts();
    assert!(
        direct >= 1 && fallbacks == 0,
        "direct {direct}, fallbacks {fallbacks}"
    );
    let s = ops.cfg.rdma_stats().unwrap();
    assert!(s.requests >= 8 && s.connects >= 1, "{s:?}");
    assert_eq!((s.connect_failures, s.midflight_errors), (0, 0), "{s:?}");

    // Read it back over plain TCP from a fresh mount: the bytes the RDMA writes stored are intact.
    let fresh = mount(&c, "vw", OpsConfig::default());
    assert_eq!(fresh.read(f, 0, a.len()).unwrap(), a);
}

/// With a low `RLIMIT_MEMLOCK` (the common 8 MiB default) the connection cap keeps RDMA from
/// running out of pinnable memory: the surplus connections use TCP, nothing fails, nothing backs
/// off and the data is intact. Run it under the default limit; it does nothing when the limit is
/// high enough that the cap would not bite.
#[test]
#[ignore = "needs an RDMA device; SoftRoCE is enough and hardware is not required (`rdma link add rxe0 type rxe netdev lo`, GID 1 or ATLAS_RDMA_GID_INDEX). Not CI. A pass on rxe0 is plumbing only, not NIC throughput. Also --features rdma; run under a low RLIMIT_MEMLOCK"]
fn real_rdma_degrades_gracefully_under_a_low_memlock_limit() {
    use atlas_native::rdma_link::{soft_memlock, Memlock};
    let Some(Memlock::Bytes(limit)) = soft_memlock() else {
        return;
    };
    if limit >= 32 << 20 {
        return;
    }
    let c = Cluster::start();
    create_fs(&c, "vl");
    let writer = mount(&c, "vl", OpsConfig::default());
    let mut files = Vec::new();
    for i in 0..4u8 {
        let f = new_file(&writer, &format!("data{i}.bin"));
        let data = pattern(12 * EXT + 5, i);
        writer.write(f, 0, &data).unwrap();
        writer.flush(f).unwrap();
        files.push((f, data));
    }

    let (t, gid) = verbs();
    for (_, n) in &c.data {
        n.offer_rdma_transport(t.clone());
    }
    let rdma = atlas_native::RdmaConfig::new(t, gid);
    let cap = rdma.max_connections;
    assert!(cap >= 1 && (cap as u64) * 2 * atlas_native::rdma_link::VERBS_PINNED_BYTES <= limit);
    let mut cfg = direct(None);
    cfg.direct_reads.as_mut().unwrap().rdma = Some(rdma);
    let ops = Arc::new(mount(&c, "vl", cfg));
    // Concurrent readers want more connections than the cap allows.
    let readers: Vec<_> = files
        .into_iter()
        .map(|(f, data)| {
            let ops = ops.clone();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    assert_eq!(ops.read(f, 0, data.len()).unwrap(), data);
                }
            })
        })
        .collect();
    for r in readers {
        r.join().unwrap();
    }
    assert_eq!(ops.direct_fallbacks(), 0);
    let s = ops.cfg.rdma_stats().unwrap();
    assert_eq!(
        (s.connect_failures, s.midflight_errors, s.enomem_failures),
        (0, 0, 0),
        "{s:?}"
    );
    assert!(
        s.capped_fallbacks > 0,
        "the cap should have been reached: {s:?}"
    );
    assert!(
        s.live_connections <= cap as u64 && s.requests > 0,
        "cap {cap}: {s:?}"
    );
}
