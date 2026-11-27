// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Block data reads against a lagging replica. Raft traffic between the metadata nodes goes
//! through proxies on localhost, so a follower can be cut off while its HTTP API keeps answering.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{Listeners, NativeNode, NodeConfig};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(20);
const BLOCK: usize = 8192;

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
    configs: BTreeMap<String, NodeConfig>,
    raft: BTreeMap<String, SocketAddr>,
    meta: BTreeMap<String, NativeNode>,
    _data: NativeNode,
}

impl Cluster {
    fn start() -> Self {
        let td = tempfile::tempdir().unwrap();
        let data_l = bind();
        let data_cfg: NodeConfig = serde_json::from_value(json!({
            "node_id": "d1",
            "data_dir": td.path().join("d1"),
            "http_listen": "127.0.0.1:0",
            "data_node": { "listen": data_l.local_addr().unwrap() },
        }))
        .unwrap();
        let data_addr = data_l.local_addr().unwrap();
        let data = NativeNode::start_with(
            data_cfg,
            Listeners {
                http: Some(bind()),
                data_node: Some(data_l),
                metadata: None,
            },
        )
        .unwrap();
        let net = Arc::new(Mutex::new(Net::default()));
        let listeners: BTreeMap<String, TcpListener> =
            (1..=3).map(|i| (format!("m{i}"), bind())).collect();
        let raft: BTreeMap<String, SocketAddr> = listeners
            .iter()
            .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
            .collect();
        let configs: BTreeMap<String, NodeConfig> = raft
            .iter()
            .map(|(id, addr)| {
                let peers: BTreeMap<&String, String> = raft
                    .iter()
                    .filter(|(p, _)| *p != id)
                    .map(|(p, target)| {
                        let via = link(net.clone(), id.clone(), p.clone(), *target);
                        (p, via.to_string())
                    })
                    .collect();
                let cfg = serde_json::from_value(json!({
                    "node_id": id,
                    "data_dir": td.path().join(id),
                    "http_listen": "127.0.0.1:0",
                    "metadata": {
                        "listen": addr,
                        "peers": peers,
                        "data_nodes": [{ "id": "d1", "addr": data_addr.to_string() }],
                        "replicas": 1,
                        "extent_bytes": 4096,
                        "tick_ms": 10,
                        "proposal_timeout_ms": 1000,
                        "repair_interval_secs": 0,
                        "gc_interval_secs": 0,
                    },
                }))
                .unwrap();
                (id.clone(), cfg)
            })
            .collect();
        let meta = listeners
            .into_iter()
            .map(|(id, l)| {
                let node = start_meta(&configs[&id], l);
                (id, node)
            })
            .collect();
        Self {
            _td: td,
            net,
            configs,
            raft,
            meta,
            _data: data,
        }
    }

    fn cut(&self, id: &str) {
        let mut n = self.net.lock().unwrap();
        n.cut.insert(id.into());
        n.open.retain(|(from, to, s)| {
            let gone = from == id || to == id;
            if gone {
                let _ = s.shutdown(Shutdown::Both);
            }
            !gone
        });
    }

    fn heal(&self, id: &str) {
        self.net.lock().unwrap().cut.remove(id);
    }

    /// Stops metadata node `id` and starts it again from its directory.
    fn restart(&mut self, id: &str) {
        drop(self.meta.remove(id).unwrap());
        let addr = self.raft[id];
        let deadline = Instant::now() + WAIT;
        let listener = loop {
            match TcpListener::bind(addr) {
                Ok(l) => break l,
                Err(e) => assert!(Instant::now() < deadline, "rebinding {addr}: {e}"),
            }
            thread::sleep(Duration::from_millis(20));
        };
        let node = start_meta(&self.configs[id], listener);
        self.meta.insert(id.into(), node);
    }

    fn call(&self, id: &str, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        http(self.meta[id].http_addr(), method, path, body)
    }

    fn leader(&self) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let found = self.meta.keys().find(|id| {
                let (_, body) = self.call(id, "GET", "/v1/status", b"");
                serde_json::from_slice::<Value>(&body)
                    .is_ok_and(|v| v["metadata"]["role"] == "leader")
            });
            if let Some(id) = found {
                return id.clone();
            }
            assert!(Instant::now() < deadline, "no leader");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends a mutation to whichever node leads, expecting `status`.
    fn on_leader(&self, method: &str, path: &str, body: &[u8], status: u16) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        loop {
            let (got, reply) = self.call(&self.leader(), method, path, body);
            if got == status {
                return reply;
            }
            assert!(
                (got == 421 || got == 503) && Instant::now() < deadline,
                "{method} {path}: {got} {}",
                String::from_utf8_lossy(&reply)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn id_of(&self, reply: Vec<u8>) -> String {
        serde_json::from_slice::<Value>(&reply).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn wait_for(&self, id: &str, path: &str, want: &[u8]) {
        let deadline = Instant::now() + WAIT;
        loop {
            let (status, body) = self.call(id, "GET", path, b"");
            if status == 200 && body == want {
                return;
            }
            assert!(Instant::now() < deadline, "{id} {path}: {status}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn start_meta(cfg: &NodeConfig, listener: TcpListener) -> NativeNode {
    NativeNode::start_with(
        cfg.clone(),
        Listeners {
            http: Some(bind()),
            data_node: None,
            metadata: Some(listener),
        },
    )
    .unwrap()
}

fn http(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = std::str::from_utf8(&out[9..12]).unwrap().parse().unwrap();
    (status, out[split + 4..].to_vec())
}

#[test]
fn a_lagging_follower_never_serves_stale_block_data() {
    let mut c = Cluster::start();
    let v = c.id_of(c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"v","size_bytes":16384}"#,
        201,
    ));
    let old = vec![1u8; BLOCK];
    let new = vec![2u8; BLOCK];
    c.on_leader("PUT", &format!("/v1/volumes/{v}/data?offset=0"), &old, 204);
    let s = c.id_of(c.on_leader(
        "POST",
        &format!("/v1/volumes/{v}/snapshots"),
        br#"{"name":"s"}"#,
        201,
    ));
    let volume = format!("/v1/volumes/{v}/data?offset=0&len={BLOCK}");
    let snapshot = format!("/v1/snapshots/{s}/data?offset=0&len={BLOCK}");
    let stale_volume = format!("{volume}&stale=1");
    let stale_snapshot = format!("{snapshot}&stale=1");

    let leader = c.leader();
    let follower = c.meta.keys().find(|id| **id != leader).unwrap().clone();
    c.wait_for(&follower, &stale_volume, &old);
    c.wait_for(&follower, &stale_snapshot, &old);

    c.cut(&follower);
    c.on_leader("PUT", &format!("/v1/volumes/{v}/data?offset=0"), &new, 204);
    c.on_leader("DELETE", &format!("/v1/snapshots/{s}"), b"", 204);

    // The follower is behind: that is what opting out of the barrier shows.
    assert_eq!(
        c.call(&follower, "GET", &stale_volume, b""),
        (200, old.clone())
    );
    assert_eq!(
        c.call(&follower, "GET", &stale_snapshot, b""),
        (200, old.clone())
    );
    // Without the opt-out it refuses rather than answer with what it has.
    for path in [&volume, &snapshot] {
        let (status, body) = c.call(&follower, "GET", path, b"");
        assert_eq!(status, 503, "{path}: {}", String::from_utf8_lossy(&body));
    }
    // Every replica that can reach the leader answers with the new state.
    for id in c.meta.keys().filter(|id| **id != follower) {
        assert_eq!(c.call(id, "GET", &volume, b""), (200, new.clone()), "{id}");
        assert_eq!(c.call(id, "GET", &snapshot, b"").0, 404, "{id}");
    }

    // A restarted replica starts from its checkpoint, which is older still.
    c.restart(&follower);
    assert_ne!(
        c.call(&follower, "GET", &stale_volume, b""),
        (200, new.clone())
    );
    assert_eq!(c.call(&follower, "GET", &volume, b"").0, 503);
    assert_eq!(c.call(&follower, "GET", &snapshot, b"").0, 503);

    // Once reconnected, nothing but the new bytes is ever served.
    c.heal(&follower);
    let deadline = Instant::now() + WAIT;
    loop {
        let (status, body) = c.call(&follower, "GET", &volume, b"");
        if status == 200 {
            assert_eq!(body, new);
            break;
        }
        assert_eq!(status, 503, "{}", String::from_utf8_lossy(&body));
        assert!(Instant::now() < deadline, "the follower never caught up");
    }
    assert_eq!(c.call(&follower, "GET", &snapshot, b"").0, 404);

    // With no leader reachable by anyone, every replica refuses.
    let ids: Vec<String> = c.meta.keys().cloned().collect();
    for id in &ids {
        c.cut(id);
    }
    // Past the old leader's check-quorum, which makes it step down.
    thread::sleep(Duration::from_millis(500));
    for id in &ids {
        let (status, body) = c.call(id, "GET", &volume, b"");
        assert_eq!(status, 503, "{id}: {}", String::from_utf8_lossy(&body));
        assert_eq!(c.call(id, "GET", &stale_volume, b"").0, 200, "{id}");
    }
}
