// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The block layer against a live in-process cluster. Raft traffic between the metadata nodes
//! goes through proxies on localhost, so a follower can be cut off while it keeps serving HTTP.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{Listeners, NativeNode, NodeConfig};
use atlas_native_fuse::client::{Body, Client, ClientConfig, Retry};
use atlas_native_ublk::{
    dispatch::Dispatcher,
    volume::{Op, Request, Volume, VolumeConfig},
};
use reqwest::Method;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(30);
const EXTENT: usize = 64 << 10;
const BLOCK: usize = 4096;
/// The nodes' `max_request_bytes`.
const NODE_MAX: usize = 1 << 20;

/// The Raft links between metadata nodes; a node in `cut` has none.
#[derive(Default)]
struct Net {
    cut: BTreeSet<String>,
    open: Vec<(String, String, TcpStream)>,
}

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn pump(mut from: TcpStream, mut to: TcpStream) {
    thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = from.shutdown(Shutdown::Both);
        let _ = to.shutdown(Shutdown::Both);
    });
}

/// The address `from` dials to reach `to`, which listens at `target`.
fn link(net: Arc<Mutex<Net>>, from: String, to: String, target: SocketAddr) -> SocketAddr {
    let listener = bind();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for inbound in listener.incoming() {
            let Ok(inbound) = inbound else { return };
            let mut n = net.lock().unwrap();
            if n.cut.contains(&from) || n.cut.contains(&to) {
                continue;
            }
            let Ok(outbound) = TcpStream::connect(target) else {
                continue;
            };
            for s in [&inbound, &outbound] {
                n.open
                    .push((from.clone(), to.clone(), s.try_clone().unwrap()));
            }
            pump(inbound.try_clone().unwrap(), outbound.try_clone().unwrap());
            pump(outbound, inbound);
        }
    });
    addr
}

struct Cluster {
    _td: tempfile::TempDir,
    net: Arc<Mutex<Net>>,
    /// `m1`, `m2`, `m3`, in endpoint order.
    meta: BTreeMap<String, Option<NativeNode>>,
    endpoints: Vec<String>,
    data: Vec<NativeNode>,
}

impl Cluster {
    fn start() -> Self {
        Self::start_with(1, 1, 1)
    }

    fn start_with(data_nodes: usize, replicas: usize, groups: u32) -> Self {
        let td = tempfile::tempdir().unwrap();
        let mut data = Vec::new();
        let mut specs = Vec::new();
        for i in 1..=data_nodes {
            let listener = bind();
            let addr = listener.local_addr().unwrap();
            let id = format!("d{i}");
            let cfg: NodeConfig = serde_json::from_value(json!({
                "node_id": id,
                "data_dir": td.path().join(&id),
                "http_listen": "127.0.0.1:0",
                "data_node": { "listen": addr },
            }))
            .unwrap();
            data.push(
                NativeNode::start_with(
                    cfg,
                    Listeners {
                        http: Some(bind()),
                        data_node: Some(listener),
                        metadata: None,
                    },
                )
                .unwrap(),
            );
            specs.push(json!({ "id": id, "addr": addr.to_string() }));
        }
        let net = Arc::new(Mutex::new(Net::default()));
        let listeners: BTreeMap<String, TcpListener> =
            (1..=3).map(|i| (format!("m{i}"), bind())).collect();
        let raft: BTreeMap<String, SocketAddr> = listeners
            .iter()
            .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
            .collect();
        let meta: BTreeMap<String, Option<NativeNode>> = listeners
            .into_iter()
            .map(|(id, listener)| {
                let peers: BTreeMap<&String, String> = raft
                    .iter()
                    .filter(|(p, _)| **p != id)
                    .map(|(p, target)| {
                        let via = link(net.clone(), id.clone(), p.clone(), *target);
                        (p, via.to_string())
                    })
                    .collect();
                let cfg: NodeConfig = serde_json::from_value(json!({
                    "node_id": id,
                    "data_dir": td.path().join(&id),
                    "http_listen": "127.0.0.1:0",
                    "max_request_bytes": NODE_MAX,
                    "metadata": {
                        "listen": raft[&id],
                        "peers": peers,
                        "data_nodes": specs,
                        "replicas": replicas,
                        "groups": groups,
                        "extent_bytes": EXTENT,
                        "tick_ms": 10,
                        "proposal_timeout_ms": 1000,
                        "repair_interval_secs": 0,
                        "gc_interval_secs": 1,
                    },
                }))
                .unwrap();
                let node = NativeNode::start_with(
                    cfg,
                    Listeners {
                        http: Some(bind()),
                        data_node: None,
                        metadata: Some(listener),
                    },
                )
                .unwrap();
                (id, Some(node))
            })
            .collect();
        let endpoints = meta
            .values()
            .flatten()
            .map(|n| format!("http://{}", n.http_addr()))
            .collect();
        let cluster = Self {
            _td: td,
            net,
            meta,
            endpoints,
            data,
        };
        cluster.leader();
        cluster
    }

    fn client(&self, endpoints: Vec<String>, request_timeout: Duration) -> Client {
        let mut cfg = ClientConfig::new(endpoints);
        cfg.retry_for = WAIT;
        cfg.request_timeout = request_timeout;
        Client::new(cfg).unwrap()
    }

    fn admin(&self) -> Client {
        self.client(self.endpoints.clone(), Duration::from_secs(10))
    }

    fn create_volume(&self, id: &str, size: usize) {
        self.admin()
            .json(
                Method::POST,
                "/v1/volumes",
                Body::Json(json!({ "id": id, "name": id, "size_bytes": size })),
                Retry::Idempotent,
            )
            .unwrap();
    }

    fn open(&self, id: &str, cfg: VolumeConfig) -> Volume {
        Volume::open(self.admin(), id, cfg).unwrap()
    }

    /// The index (in endpoint order) of the node leading group 0.
    fn leader(&self) -> usize {
        let deadline = Instant::now() + WAIT;
        loop {
            let found = self.meta.values().position(|n| {
                n.as_ref().is_some_and(|n| {
                    let one = self.client(
                        vec![format!("http://{}", n.http_addr())],
                        Duration::from_secs(2),
                    );
                    one.json(Method::GET, "/v1/status", Body::Empty, Retry::Idempotent)
                        .is_ok_and(|s: Value| s["metadata"]["role"] == "leader")
                })
            });
            if let Some(i) = found {
                return i;
            }
            assert!(Instant::now() < deadline, "no leader");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn id(&self, node: usize) -> String {
        self.meta.keys().nth(node).unwrap().clone()
    }

    fn kill(&mut self, node: usize) {
        let id = self.id(node);
        drop(self.meta.get_mut(&id).unwrap().take());
    }

    fn cut(&self, node: usize) {
        let id = self.id(node);
        let mut n = self.net.lock().unwrap();
        n.cut.insert(id.clone());
        n.open.retain(|(from, to, s)| {
            let gone = *from == id || *to == id;
            if gone {
                let _ = s.shutdown(Shutdown::Both);
            }
            !gone
        });
    }
}

/// Bytes that differ per position and per `seed`.
fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 32) as u8
        })
        .collect()
}

#[test]
fn block_requests_round_trip() {
    let c = Cluster::start();
    let size = 3 << 20;
    c.create_volume("v", size);
    // Requests of at most 256 KiB, so large block requests are split.
    let cfg = VolumeConfig {
        max_request_bytes: 256 << 10,
        ..VolumeConfig::default()
    };
    let v = c.open("v", cfg.clone());
    let g = v.geometry();
    assert_eq!(
        (g.size_bytes, g.block_bytes, g.extent_bytes),
        (size as u64, BLOCK as u32, EXTENT as u64)
    );
    let mut want = vec![0u8; size];
    assert_eq!(v.read(0, size as u64).unwrap(), want);

    // Inside one extent, across a boundary, and larger than a cluster request with partial
    // extents at both ends.
    for (seed, offset, len) in [
        (1, BLOCK, BLOCK),
        (2, EXTENT - BLOCK, 2 * BLOCK),
        (3, 5 * EXTENT - 3 * BLOCK, 1 << 20),
        (4, size - BLOCK, BLOCK),
    ] {
        let data = pattern(seed, len);
        v.write(offset as u64, &data).unwrap();
        want[offset..offset + len].copy_from_slice(&data);
        assert_eq!(v.read(offset as u64, len as u64).unwrap(), data);
    }
    v.flush().unwrap();
    assert_eq!(v.read(0, size as u64).unwrap(), want);

    let (offset, len) = (EXTENT - BLOCK, 3 * EXTENT);
    v.write_zeroes(offset as u64, len as u64).unwrap();
    want[offset..offset + len].fill(0);
    // A second attachment sees everything that completed.
    assert_eq!(c.open("v", cfg).read(0, size as u64).unwrap(), want);

    let discard = Request {
        op: Op::Discard,
        offset: 0,
        len: BLOCK as u64,
        data: Vec::new(),
    };
    assert_eq!(v.execute(discard), Err(libc::EOPNOTSUPP));
    let short = Request {
        op: Op::Write,
        offset: 0,
        len: 2 * BLOCK as u64,
        data: vec![0; BLOCK],
    };
    assert_eq!(v.execute(short), Err(libc::EINVAL));
    for (offset, len) in [
        (512, BLOCK),
        (0, 512),
        (0, 0),
        (size, BLOCK),
        (size - BLOCK, 2 * BLOCK),
        (u64::MAX as usize - BLOCK + 1, 2 * BLOCK),
    ] {
        assert_eq!(v.read(offset as u64, len as u64), Err(libc::EINVAL));
        assert_eq!(v.write_zeroes(offset as u64, len as u64), Err(libc::EINVAL));
    }
    assert_eq!(v.read(0, size as u64).unwrap(), want);

    let ro = c.open(
        "v",
        VolumeConfig {
            read_only: true,
            max_request_bytes: 256 << 10,
            ..VolumeConfig::default()
        },
    );
    assert_eq!(ro.write(0, &[1; BLOCK]), Err(libc::EROFS));
    assert_eq!(ro.write_zeroes(0, BLOCK as u64), Err(libc::EROFS));
    assert_eq!(ro.read(0, size as u64).unwrap(), want);
}

#[test]
fn open_reports_what_is_wrong() {
    let c = Cluster::start();
    c.create_volume("v", 2 << 20);
    let open = |id: &str, cfg: VolumeConfig| Volume::open(c.admin(), id, cfg).err().unwrap();
    let ok = VolumeConfig {
        max_request_bytes: NODE_MAX as u64,
        ..VolumeConfig::default()
    };
    let e = open("nope", ok.clone());
    assert!(e.contains("nope") && e.contains("not_found"), "{e}");
    let e = open("../v", ok.clone());
    assert!(e.contains("volume id"), "{e}");
    let e = open(
        "v",
        VolumeConfig {
            max_request_bytes: 2 * NODE_MAX as u64,
            ..ok.clone()
        },
    );
    assert!(e.contains("max_request_bytes"), "{e}");
    let e = open(
        "v",
        VolumeConfig {
            block_bytes: 8192,
            ..ok.clone()
        },
    );
    assert!(e.contains("block size"), "{e}");
    assert!(Volume::open(c.admin(), "v", ok).is_ok());
}

fn config() -> VolumeConfig {
    VolumeConfig {
        max_request_bytes: NODE_MAX as u64,
        ..VolumeConfig::default()
    }
}

#[test]
fn concurrent_writes_to_shared_extents_are_all_kept() {
    let c = Cluster::start();
    let (workers, blocks) = (8usize, 64usize);
    c.create_volume("v", blocks * BLOCK);
    let v = Arc::new(c.open("v", config()));
    let pool = Dispatcher::new(workers, {
        let v = v.clone();
        Arc::new(move |r| v.execute(r))
    })
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let mut want = vec![0u8; blocks * BLOCK];
    for round in 0..4u64 {
        // Neighbouring blocks, so every extent is rewritten by several requests at once.
        for b in 0..blocks {
            let data = pattern(round * 1000 + b as u64 + 1, BLOCK);
            want[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&data);
            let tx = tx.clone();
            pool.submit(
                Request {
                    op: Op::Write,
                    offset: (b * BLOCK) as u64,
                    len: BLOCK as u64,
                    data,
                },
                move |r| tx.send(r).unwrap(),
            );
        }
        for _ in 0..blocks {
            rx.recv_timeout(WAIT).unwrap().unwrap();
        }
        assert_eq!(v.read(0, want.len() as u64).unwrap(), want, "round {round}");
    }
}

#[test]
fn leader_failover_loses_nothing() {
    let mut c = Cluster::start();
    let blocks = 32usize;
    c.create_volume("v", blocks * BLOCK);
    let v = Arc::new(c.open("v", {
        VolumeConfig {
            settle: Duration::from_millis(500),
            ..config()
        }
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (v, stop) = (v.clone(), stop.clone());
        thread::spawn(move || {
            let mut want = vec![0u8; blocks * BLOCK];
            let mut n = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let b = (n as usize * 7) % blocks;
                let data = pattern(n + 1, BLOCK);
                v.write((b * BLOCK) as u64, &data).unwrap();
                assert_eq!(v.read((b * BLOCK) as u64, BLOCK as u64).unwrap(), data);
                want[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&data);
                n += 1;
            }
            (want, n)
        })
    };
    thread::sleep(Duration::from_millis(300));
    let leader = c.leader();
    c.kill(leader);
    c.leader();
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::SeqCst);
    let (want, writes) = writer.join().unwrap();
    assert!(writes > 2, "{writes}");
    assert_eq!(v.read(0, want.len() as u64).unwrap(), want);
    assert_eq!(
        c.open("v", config()).read(0, want.len() as u64).unwrap(),
        want
    );
}

#[test]
fn a_lagging_follower_never_answers_a_read_with_old_bytes() {
    let c = Cluster::start();
    c.create_volume("v", 4 * BLOCK);
    let v = c.open("v", config());
    let old = pattern(1, BLOCK);
    let new = pattern(2, BLOCK);
    v.write(0, &old).unwrap();

    let leader = c.leader();
    let follower = (leader + 1) % 3;
    let path = format!("/v1/volumes/v/data?offset=0&len={BLOCK}");
    let one = c.client(vec![c.endpoints[follower].clone()], Duration::from_secs(5));
    let raw = |query: &str| one.attempt(0, &Method::GET, &format!("{path}{query}"), &Body::Empty);
    use atlas_native_fuse::client::Attempt;
    let deadline = Instant::now() + WAIT;
    while !matches!(raw("&stale=1"), Attempt::Done(b) if b == old) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }

    c.cut(follower);
    v.write(0, &new).unwrap();
    // It really is behind, and it refuses a normal read.
    assert!(matches!(raw("&stale=1"), Attempt::Done(b) if b == old));
    assert!(matches!(raw(""), Attempt::Refused { status, .. } if status.as_u16() == 503));

    for _ in 0..3 {
        v.client().prefer(follower);
        assert_eq!(v.read(0, BLOCK as u64).unwrap(), new);
        assert_ne!(v.client().preferred(), follower);
    }
    // Attaching with the lagging node listed first works from current data too.
    let mut endpoints = c.endpoints.clone();
    endpoints.rotate_left(follower);
    let again = Volume::open(c.client(endpoints, Duration::from_secs(5)), "v", config()).unwrap();
    assert_eq!(again.read(0, BLOCK as u64).unwrap(), new);

    // With only the lagging node to ask, a read fails rather than return what it has.
    let alone = c.client(vec![c.endpoints[follower].clone()], Duration::from_secs(5));
    let e = Volume::open(
        alone,
        "v",
        VolumeConfig {
            io_timeout: Some(Duration::from_secs(2)),
            ..config()
        },
    )
    .err()
    .unwrap();
    assert!(e.contains("503"), "{e}");
}

#[test]
fn a_write_that_cannot_be_made_durable_is_not_acknowledged() {
    let mut c = Cluster::start_with(3, 3, 1);
    c.create_volume("v", 4 * EXTENT);
    let v = Arc::new(c.open(
        "v",
        VolumeConfig {
            io_timeout: Some(Duration::from_secs(2)),
            ..config()
        },
    ));
    let old = pattern(1, BLOCK);
    v.write(0, &old).unwrap();
    // Three copies on three nodes: without one, no write can be stored.
    drop(c.data.pop());
    let started = Instant::now();
    assert_eq!(v.write(0, &pattern(2, BLOCK)), Err(libc::EIO));
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(v.write_zeroes(0, BLOCK as u64), Err(libc::EIO));
    assert_eq!(v.read(0, BLOCK as u64).unwrap(), old);

    // Giving up ends a request that would otherwise wait for ever.
    let forever = Arc::new(c.open(
        "v",
        VolumeConfig {
            io_timeout: None,
            ..config()
        },
    ));
    let writer = {
        let v = forever.clone();
        thread::spawn(move || v.write(0, &[9; BLOCK]))
    };
    thread::sleep(Duration::from_millis(500));
    assert!(!writer.is_finished());
    forever.abort();
    assert_eq!(writer.join().unwrap(), Err(libc::EIO));
    assert_eq!(forever.read(0, BLOCK as u64), Err(libc::EIO));
}

#[test]
fn a_deleted_volume_fails_at_once() {
    let c = Cluster::start();
    c.create_volume("v", 4 * BLOCK);
    let v = c.open("v", config());
    c.admin()
        .request(Method::DELETE, "/v1/volumes/v", Body::Empty, Retry::Remove)
        .unwrap();
    let started = Instant::now();
    assert_eq!(v.read(0, BLOCK as u64), Err(libc::EIO));
    assert_eq!(v.write(0, &[1; BLOCK]), Err(libc::EIO));
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// A node that accepts requests and never answers.
fn black_hole() -> String {
    let listener = bind();
    let url = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming() {
            held.push(s);
        }
    });
    url
}

#[test]
fn an_unanswered_write_is_held_for_the_settle_time() {
    let c = Cluster::start();
    c.create_volume("v", 4 * BLOCK);
    let settle = Duration::from_secs(2);
    let timeout = Duration::from_millis(500);
    // A port nothing listens on: the request cannot have reached anyone.
    let refused = {
        let l = bind();
        format!("http://{}", l.local_addr().unwrap())
    };
    for (first, held) in [(black_hole(), true), (refused, false)] {
        let mut endpoints = vec![first];
        endpoints.extend(c.endpoints.clone());
        let v = Volume::open(
            c.client(endpoints, timeout),
            "v",
            VolumeConfig { settle, ..config() },
        )
        .unwrap();
        v.client().prefer(0);
        let started = Instant::now();
        // Reads are never held.
        v.read(0, BLOCK as u64).unwrap();
        assert!(started.elapsed() < settle, "read: {:?}", started.elapsed());
        v.client().prefer(0);
        let started = Instant::now();
        let data = pattern(u64::from(held) + 1, BLOCK);
        v.write(0, &data).unwrap();
        let took = started.elapsed();
        if held {
            assert!(took >= timeout + settle, "{took:?}");
        } else {
            assert!(took < settle, "{took:?}");
        }
        assert_eq!(v.read(0, BLOCK as u64).unwrap(), data);
    }
}

#[test]
fn volumes_in_every_metadata_group_work() {
    let c = Cluster::start_with(1, 1, 2);
    // Ids are spread over the groups by hash; eight of them cover both.
    for i in 0..8u64 {
        let id = format!("vol-{i}");
        c.create_volume(&id, 4 * BLOCK);
        let v = c.open(&id, config());
        let data = pattern(i + 1, BLOCK);
        v.write(BLOCK as u64, &data).unwrap();
        for node in 0..3 {
            v.client().prefer(node);
            assert_eq!(v.read(BLOCK as u64, BLOCK as u64).unwrap(), data);
        }
    }
}
