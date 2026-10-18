// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{
    DataNodeRole, DataNodeSpec, DeviceConfig, Listeners, MetadataRole, NativeNode, NodeConfig,
    ObjectStoreConfig, TieringConfig,
};
use atlas_native::DeviceBackend;

const WAIT: Duration = Duration::from_secs(20);
const TOKEN: &str = "s3cret-token";

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{auth}Content-Length: {}\r\n\r\n",
        body.len()
    );
    // The server may refuse a request before reading its body; the reply is still readable.
    let _ = s
        .write_all(head.as_bytes())
        .and_then(|()| s.write_all(body));
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = std::str::from_utf8(&out[9..12]).unwrap().parse().unwrap();
    (status, out[split + 4..].to_vec())
}

fn api(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    http(addr, method, path, Some(TOKEN), body)
}

fn json(b: &[u8]) -> serde_json::Value {
    serde_json::from_slice(b).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(b)))
}

struct Cluster {
    _td: tempfile::TempDir,
    meta: BTreeMap<String, Option<NativeNode>>,
    data: BTreeMap<String, Option<NativeNode>>,
}

impl Cluster {
    /// `meta` metadata-only nodes and `data` data-only nodes, all on localhost.
    fn start(meta: usize, data: usize, repair_interval_secs: u64) -> Self {
        Self::start_groups(meta, data, repair_interval_secs, 1)
    }

    /// [`Self::start`] with the namespace sharded across `groups` Raft groups.
    fn start_groups(meta: usize, data: usize, repair_interval_secs: u64, groups: u32) -> Self {
        Self::start_full(meta, data, repair_interval_secs, groups, 60)
    }

    /// [`Self::start_groups`] with the rebuild controller's delay.
    fn start_full(
        meta: usize,
        data: usize,
        repair_interval_secs: u64,
        groups: u32,
        rebuild_delay_secs: u64,
    ) -> Self {
        Self::start_with(meta, data, |m| {
            m.repair_interval_secs = repair_interval_secs;
            m.groups = groups;
            m.rebuild_delay_secs = rebuild_delay_secs;
        })
    }

    /// `meta` metadata nodes and `data` data nodes, with `tweak` applied to every metadata
    /// node's config.
    fn start_with(meta: usize, data: usize, tweak: impl Fn(&mut MetadataRole)) -> Self {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), format!("{TOKEN}\n")).unwrap();
        let data_l: BTreeMap<String, TcpListener> =
            (1..=data).map(|i| (format!("d{i}"), bind())).collect();
        let meta_l: BTreeMap<String, TcpListener> =
            (1..=meta).map(|i| (format!("m{i}"), bind())).collect();
        let specs: Vec<DataNodeSpec> = data_l
            .iter()
            .map(|(id, l)| DataNodeSpec {
                id: id.clone(),
                addr: l.local_addr().unwrap().to_string(),
                zone: None,
                rack: None,
                host: None,
                free_bytes: 1 << 30,
                devices: 2,
            })
            .collect();
        let raft_addrs: BTreeMap<String, String> = meta_l
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
        };
        let mut data_nodes = BTreeMap::new();
        for (id, l) in data_l {
            let mut cfg = base(&id, td.path());
            cfg.data_node = Some(DataNodeRole {
                listen: l.local_addr().unwrap(),
                devices: vec![
                    DeviceConfig {
                        path: td.path().join(&id).join("disk0"),
                        backend: DeviceBackend::File,
                    },
                    DeviceConfig {
                        path: td.path().join(&id).join("disk1"),
                        backend: DeviceBackend::Aligned,
                    },
                ],
            });
            let n = NativeNode::start_with(
                cfg,
                Listeners {
                    http: Some(bind()),
                    data_node: Some(l),
                    metadata: None,
                },
            )
            .unwrap();
            data_nodes.insert(id, Some(n));
        }
        let mut meta_nodes = BTreeMap::new();
        for (id, l) in meta_l {
            let mut cfg = base(&id, td.path());
            let mut role = MetadataRole {
                listen: l.local_addr().unwrap(),
                // Every voter, this node included: the node ignores its own entry.
                peers: raft_addrs.clone(),
                bootstrap: None,
                data_nodes: specs.clone(),
                replicas: 3,
                erasure: None,
                erasure_min_bytes: 64 << 10,
                compression: false,
                rebuild_delay_secs: 60,
                rebuild_bytes_per_sec: 0,
                scrub_bytes_per_sec: 0,
                tiering: None,
                extent_bytes: 4096,
                tick_ms: 10,
                proposal_timeout_ms: 3000,
                repair_interval_secs: 0,
                gc_interval_secs: 0,
                groups: 1,
                cache_inodes: atlas_native::store::DEFAULT_CACHE_INODES,
                store_cache_bytes: atlas_native::store::DEFAULT_STORE_CACHE_BYTES,
            };
            tweak(&mut role);
            cfg.metadata = Some(role);
            let n = NativeNode::start_with(
                cfg,
                Listeners {
                    http: Some(bind()),
                    data_node: None,
                    metadata: Some(l),
                },
            )
            .unwrap();
            meta_nodes.insert(id, Some(n));
        }
        Self {
            _td: td,
            meta: meta_nodes,
            data: data_nodes,
        }
    }

    fn meta_addrs(&self) -> Vec<(String, SocketAddr)> {
        self.meta
            .iter()
            .filter_map(|(id, n)| n.as_ref().map(|n| (id.clone(), n.http_addr())))
            .collect()
    }

    /// Retries `method path` across metadata nodes until one (the leader) answers `want`.
    fn on_leader(&self, method: &str, path: &str, body: &[u8], want: u16) -> (String, Vec<u8>) {
        let deadline = Instant::now() + WAIT;
        loop {
            for (id, addr) in self.meta_addrs() {
                let (st, b) = api(addr, method, path, body);
                if st == want {
                    return (id, b);
                }
                assert!(
                    st == 421 || st == 503,
                    "{id} {method} {path}: {st} {}",
                    String::from_utf8_lossy(&b)
                );
            }
            assert!(
                Instant::now() < deadline,
                "no leader answered {method} {path}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends `method path` to whichever metadata node is the leader and returns its answer.
    fn on_leader_any(&self, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let deadline = Instant::now() + WAIT;
        loop {
            for (_, addr) in self.meta_addrs() {
                let (st, b) = api(addr, method, path, body);
                if st != 421 && st != 503 {
                    return (st, b);
                }
            }
            assert!(
                Instant::now() < deadline,
                "no leader answered {method} {path}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_read(&self, path: &str, want: &[u8]) {
        for (id, addr) in self.meta_addrs() {
            let deadline = Instant::now() + WAIT;
            loop {
                let (st, b) = api(addr, "GET", path, b"");
                if st == 200 && b == want {
                    break;
                }
                assert!(Instant::now() < deadline, "{id} never served {path} ({st})");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn http_api_round_trip_with_auth_and_leader_redirect() {
    let c = Cluster::start(3, 3, 0);
    let (_, first) = c.meta_addrs()[0].clone();
    assert_eq!(http(first, "GET", "/healthz", None, b"").0, 200);
    assert_eq!(http(first, "GET", "/v1/status", None, b"").0, 401);
    assert_eq!(http(first, "GET", "/v1/status", Some("wrong"), b"").0, 401);
    assert_eq!(api(first, "GET", "/v1/status", b"").0, 200);

    for (id, addr) in c.meta_addrs() {
        let deadline = Instant::now() + WAIT;
        while http(addr, "GET", "/readyz", None, b"").0 != 200 {
            assert!(Instant::now() < deadline, "{id} never became ready");
            thread::sleep(Duration::from_millis(20));
        }
    }

    let (leader, body) = c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"vol","size_bytes":8192}"#,
        201,
    );
    let v = json(&body)["id"].as_str().unwrap().to_string();

    // A follower refuses mutations and names the leader.
    let (fid, faddr) = c
        .meta_addrs()
        .into_iter()
        .find(|(id, _)| *id != leader)
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let (st, b) = api(
            faddr,
            "POST",
            "/v1/volumes",
            br#"{"name":"x","size_bytes":4096}"#,
        );
        assert_eq!(st, 421, "{fid}: {}", String::from_utf8_lossy(&b));
        if json(&b)["leader"] == leader.as_str() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{fid} never named leader {leader}"
        );
        thread::sleep(Duration::from_millis(20));
    }

    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=0"),
        &[9u8; 4096],
        204,
    );
    c.wait_read(
        &format!("/v1/volumes/{v}/data?offset=0&len=4096"),
        &[9u8; 4096],
    );
    let (_, snap) = c.on_leader(
        "POST",
        &format!("/v1/volumes/{v}/snapshots"),
        br#"{"name":"s1"}"#,
        201,
    );
    let s = json(&snap)["id"].as_str().unwrap().to_string();
    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=0"),
        &[1u8; 4096],
        204,
    );
    c.wait_read(
        &format!("/v1/snapshots/{s}/data?offset=0&len=4096"),
        &[9u8; 4096],
    );
    c.wait_read(
        &format!("/v1/volumes/{v}/data?offset=0&len=4096"),
        &[1u8; 4096],
    );

    let (st, list) = api(faddr, "GET", "/v1/volumes", b"");
    assert_eq!(st, 200);
    assert_eq!(json(&list)["volumes"][0]["size_bytes"], 8192);

    // Bad requests are rejected without touching storage.
    let laddr = c.meta[&leader].as_ref().unwrap().http_addr();
    assert_eq!(
        api(laddr, "PUT", &format!("/v1/volumes/{v}/data"), b"x").0,
        400
    );
    assert_eq!(api(laddr, "POST", "/v1/volumes", b"not json").0, 400);
    assert_eq!(
        api(laddr, "GET", "/v1/volumes/nope/data?offset=0&len=1", b"").0,
        404
    );
    assert_eq!(
        api(
            laddr,
            "PUT",
            &format!("/v1/volumes/{v}/data?offset=0"),
            &vec![0u8; 2 << 20]
        )
        .0,
        413
    );
    assert_eq!(
        api(
            laddr,
            "GET",
            &format!("/v1/volumes/{v}/data?offset=0&len={}", 2 << 20),
            b""
        )
        .0,
        413
    );
    assert_eq!(
        api(
            laddr,
            "GET",
            &format!("/v1/volumes/{v}/data?offset=8000&len=193"),
            b""
        )
        .0,
        400,
        "read past the end of the volume"
    );

    // Unaligned writes and reads that cross extent boundaries.
    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=4000"),
        &[7u8; 200],
        204,
    );
    let mut want = vec![1u8; 96];
    want.extend_from_slice(&[7u8; 200]);
    want.extend_from_slice(&[0u8; 4]);
    c.wait_read(&format!("/v1/volumes/{v}/data?offset=3904&len=300"), &want);

    // Clone from the snapshot and grow the source.
    let (_, cl) = c.on_leader(
        "POST",
        &format!("/v1/snapshots/{s}/clone"),
        br#"{"name":"copy","size_bytes":12288}"#,
        201,
    );
    let cv = json(&cl)["id"].as_str().unwrap().to_string();
    let mut want = vec![9u8; 4096];
    want.extend_from_slice(&[0u8; 8192]);
    c.wait_read(&format!("/v1/volumes/{cv}/data?offset=0&len=12288"), &want);
    c.on_leader(
        "POST",
        &format!("/v1/volumes/{v}/resize"),
        br#"{"size_bytes":16384}"#,
        200,
    );
    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=16000"),
        &[3u8; 384],
        204,
    );
    c.on_leader(
        "POST",
        &format!("/v1/volumes/{v}/resize"),
        br#"{"size_bytes":4096}"#,
        409,
    );
    c.on_leader(
        "POST",
        "/v1/volumes/nope/resize",
        br#"{"size_bytes":1}"#,
        404,
    );
    c.on_leader("POST", "/v1/snapshots/nope/clone", br#"{"name":"x"}"#, 404);
    assert_eq!(
        api(
            laddr,
            "POST",
            &format!("/v1/snapshots/{s}/clone"),
            br#"{"name":"x","size_bytes":"big"}"#
        )
        .0,
        400
    );

    let (st, m) = http(laddr, "GET", "/metrics", None, b"");
    let m = String::from_utf8(m).unwrap();
    assert_eq!(st, 200);
    for series in [
        "atlas_native_raft_role{",
        "atlas_native_volumes ",
        "atlas_native_node_up{node=\"d1\"} 1",
        "atlas_native_repair_runs_total{",
    ] {
        assert!(m.contains(series), "missing {series}:\n{m}");
    }
    let d1 = c.data["d1"].as_ref().unwrap().http_addr();
    let (_, dm) = http(d1, "GET", "/metrics", None, b"");
    assert!(String::from_utf8(dm)
        .unwrap()
        .contains("atlas_native_data_fence{node=\"d1\",group=\"0\"}"));
    assert_eq!(api(d1, "GET", "/v1/volumes", b"").0, 404);
}

#[test]
fn background_repair_restores_replicas_after_losing_a_data_node() {
    let mut c = Cluster::start(3, 4, 1);
    let (_, body) = c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"vol","size_bytes":4096}"#,
        201,
    );
    let v = json(&body)["id"].as_str().unwrap().to_string();
    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=0"),
        &[5u8; 4096],
        204,
    );

    // d1..d3 hold the replicas (placement order); losing d2 leaves the spare d4 to repair onto.
    c.data.insert("d2".into(), None);
    let deadline = Instant::now() + WAIT;
    loop {
        let repaired = c.meta_addrs().into_iter().any(|(_, addr)| {
            let (_, b) = api(addr, "GET", "/v1/status", b"");
            json(&b)["last_repair"]["replicas_repaired"].as_u64() == Some(1)
        });
        if repaired {
            break;
        }
        assert!(Instant::now() < deadline, "background repair never ran");
        thread::sleep(Duration::from_millis(100));
    }
    c.data.insert("d1".into(), None);
    c.wait_read(
        &format!("/v1/volumes/{v}/data?offset=0&len=4096"),
        &[5u8; 4096],
    );
}

#[test]
fn a_lost_data_node_is_rebuilt_without_waiting_for_a_scrub() {
    // A scrub would take an hour to come round; only the rebuild path can restore replicas.
    let mut c = Cluster::start_full(3, 4, 3600, 1, 1);
    let (_, body) = c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"vol","size_bytes":16384}"#,
        201,
    );
    let v = json(&body)["id"].as_str().unwrap().to_string();
    c.on_leader(
        "PUT",
        &format!("/v1/volumes/{v}/data?offset=0"),
        &[6u8; 16384],
        204,
    );

    c.data.insert("d2".into(), None);
    let deadline = Instant::now() + WAIT;
    loop {
        let rebuilt = c.meta_addrs().into_iter().find_map(|(_, addr)| {
            let (_, b) = api(addr, "GET", "/v1/status", b"");
            let r = json(&b)["rebuild"][0].clone();
            // The same node's metrics, before leadership can move on a loaded host.
            let (_, m) = api(addr, "GET", "/metrics", b"");
            (r["lost_nodes"] == serde_json::json!(["d2"])
                && r["degraded_extents"] == 0
                && r["rebuilt"]["replicas_repaired"].as_u64() >= Some(1))
            .then(|| (r, String::from_utf8_lossy(&m).into_owned()))
        });
        if let Some((r, m)) = rebuilt {
            assert_eq!(r["unrecoverable_extents"], 0, "{r}");
            assert!(m.contains("atlas_native_lost_nodes{node="), "{m}");
            assert!(m.contains("atlas_native_degraded_extents{node="), "{m}");
            break;
        }
        assert!(Instant::now() < deadline, "the lost node was never rebuilt");
        thread::sleep(Duration::from_millis(100));
    }
    c.data.insert("d1".into(), None);
    c.wait_read(
        &format!("/v1/volumes/{v}/data?offset=0&len=16384"),
        &[6u8; 16384],
    );
}

#[test]
fn tiered_extents_are_read_from_objects_with_every_data_node_gone() {
    let bucket = tempfile::tempdir().unwrap();
    let path = bucket.path().to_path_buf();
    let mut c = Cluster::start_with(3, 3, |m| {
        m.tiering = Some(TieringConfig {
            store: ObjectStoreConfig::Dir { path: path.clone() },
            prefix: "cluster-a/".into(),
            cold_after_secs: 0,
            interval_secs: 0,
            bytes_per_sec: 0,
            min_extent_bytes: 0,
        });
    });
    let (_, body) = c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"vol","size_bytes":16384}"#,
        201,
    );
    let v = json(&body)["id"].as_str().unwrap().to_string();
    let data: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
    c.on_leader("PUT", &format!("/v1/volumes/{v}/data?offset=0"), &data, 204);

    let (leader, body) = c.on_leader("POST", "/v1/tier", b"", 200);
    assert_eq!(
        json(&body)["tiered"],
        4,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let objects = std::fs::read_dir(bucket.path().join("cluster-a/g0/extents"))
        .unwrap()
        .count();
    assert_eq!(objects, 4);

    // Every replica of the metadata applied the move, so any new leader reads the objects.
    for id in ["d1", "d2", "d3"] {
        c.data.insert(id.into(), None);
    }
    c.meta.insert(leader, None);
    c.wait_read(&format!("/v1/volumes/{v}/data?offset=0&len=16384"), &data);
}

#[test]
fn snapshots_export_to_objects_and_import_into_a_new_volume() {
    let bucket = tempfile::tempdir().unwrap();
    let path = bucket.path().to_path_buf();
    let c = Cluster::start_with(3, 3, |m| {
        m.tiering = Some(TieringConfig {
            store: ObjectStoreConfig::Dir { path: path.clone() },
            prefix: "cluster-a/".into(),
            cold_after_secs: 3600,
            interval_secs: 0,
            bytes_per_sec: 0,
            min_extent_bytes: 0,
        });
    });
    let (_, body) = c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"vol","size_bytes":16384}"#,
        201,
    );
    let v = json(&body)["id"].as_str().unwrap().to_string();
    let data: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
    c.on_leader("PUT", &format!("/v1/volumes/{v}/data?offset=0"), &data, 204);
    let (_, body) = c.on_leader(
        "POST",
        &format!("/v1/volumes/{v}/snapshots"),
        br#"{"name":"nightly"}"#,
        201,
    );
    let s = json(&body)["id"].as_str().unwrap().to_string();

    let finish = |node: &str, body: &[u8]| -> serde_json::Value {
        let t = json(body)["transfer"].as_str().unwrap().to_string();
        let addr = c.meta[node].as_ref().unwrap().http_addr();
        let deadline = Instant::now() + WAIT;
        loop {
            let (st, b) = api(addr, "GET", &format!("/v1/transfers/{t}"), b"");
            assert_eq!(st, 200);
            let j = json(&b);
            match j["state"].as_str().unwrap() {
                "done" => return j,
                "failed" => panic!("transfer failed: {j}"),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "transfer {t} never finished: {j}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    };

    let (node, body) = c.on_leader(
        "POST",
        &format!("/v1/snapshots/{s}/export"),
        br#"{"name":"vol-nightly"}"#,
        202,
    );
    let t = finish(&node, &body);
    assert_eq!(
        (t["kind"].as_str(), t["done"].as_u64()),
        (Some("export"), Some(4))
    );
    assert_eq!(t["result"]["uploaded"], 4, "{t}");
    assert!(path
        .join("cluster-a/exports/vol-nightly/manifest.json")
        .exists());
    let (_, body) = c.on_leader("GET", "/v1/exports", b"", 200);
    assert_eq!(json(&body)["exports"][0]["snapshot_name"], "nightly");
    c.on_leader(
        "POST",
        &format!("/v1/snapshots/{s}/export"),
        br#"{"name":"../escape"}"#,
        400,
    );

    let (node, body) = c.on_leader(
        "POST",
        "/v1/volumes/import",
        br#"{"export":"vol-nightly","name":"restored"}"#,
        202,
    );
    let w = json(&body)["id"].as_str().unwrap().to_string();
    let t = finish(&node, &body);
    assert_eq!(t["result"]["bytes"], 16384, "{t}");
    c.wait_read(&format!("/v1/volumes/{w}/data?offset=0&len=16384"), &data);

    let (_, body) = c.on_leader("DELETE", "/v1/exports/vol-nightly", b"", 200);
    assert_eq!(json(&body)["blobs_deleted"], 4);
    c.on_leader("GET", "/v1/exports/vol-nightly", b"", 404);
}

#[test]
fn config_validation_rejects_bad_files() {
    let td = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        let p = td.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    };
    let ok = write(
        "ok.json",
        r#"{"node_id":"d1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0",
            "data_node":{"listen":"127.0.0.1:0"}}"#,
    );
    assert!(NodeConfig::from_file(&ok).is_ok());
    for (name, body) in [
        (
            "unknown.json",
            r#"{"node_id":"d1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0","bogus":1,
                "data_node":{"listen":"127.0.0.1:0"}}"#,
        ),
        (
            "norole.json",
            r#"{"node_id":"d1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0"}"#,
        ),
        (
            "replicas.json",
            r#"{"node_id":"m1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0",
                "metadata":{"listen":"127.0.0.1:0","peers":{},"replicas":3,
                "data_nodes":[{"id":"d1","addr":"127.0.0.1:1"}]}}"#,
        ),
        (
            "badaddr.json",
            r#"{"node_id":"m1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0",
                "metadata":{"listen":"127.0.0.1:0","peers":{"m2":"no-port"},"replicas":1,
                "data_nodes":[{"id":"d1","addr":"d1.svc:7481"}]}}"#,
        ),
        (
            "tierprefix.json",
            r#"{"node_id":"m1","data_dir":"/tmp/x","http_listen":"127.0.0.1:0",
                "metadata":{"listen":"127.0.0.1:0","peers":{},"replicas":1,
                "data_nodes":[{"id":"d1","addr":"127.0.0.1:1"}],
                "tiering":{"store":{"dir":{"path":"/tmp/b"}},"prefix":"../up/"}}}"#,
        ),
        (
            "unsetenv.json",
            r#"{"node_id":"${ATLAS_NATIVE_TEST_UNSET_VAR}","data_dir":"/tmp/x",
                "http_listen":"127.0.0.1:0","data_node":{"listen":"127.0.0.1:0"}}"#,
        ),
    ] {
        assert!(
            NodeConfig::from_file(write(name, body)).is_err(),
            "{name} accepted"
        );
    }
}

#[test]
fn a_lowered_metadata_group_count_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join("m1");
    std::fs::create_dir_all(dir.join("raft-g2")).unwrap();
    let cfg = |groups: u32| {
        let body = format!(
            r#"{{"node_id":"m1","data_dir":"{}","http_listen":"127.0.0.1:0",
                "metadata":{{"listen":"127.0.0.1:0","peers":{{}},"replicas":1,"groups":{groups},
                "data_nodes":[{{"id":"d1","addr":"127.0.0.1:1"}}]}}}}"#,
            dir.display()
        );
        let p = td.path().join("cfg.json");
        std::fs::write(&p, body).unwrap();
        NodeConfig::from_file(&p).unwrap()
    };
    let err = NativeNode::start(cfg(2))
        .err()
        .expect("started with fewer groups");
    assert!(err.to_string().contains("never lowered"), "{err}");
    NativeNode::start(cfg(3)).expect("same group count");
}

#[test]
fn http_members_list_and_remove_a_voter() {
    let c = Cluster::start(3, 3, 0);
    let (_, first) = c.meta_addrs()[0].clone();
    let (st, b) = api(first, "GET", "/v1/members", b"");
    assert_eq!(st, 200);
    let m = json(&b);
    assert_eq!(m["membership"]["type"], "stable");
    assert_eq!(
        m["membership"]["voters"],
        serde_json::json!(["m1", "m2", "m3"])
    );

    for bad in [
        &br#"{"voters": ["m1"]}"#[..],
        br#"{"voters": {"m1": "x", "m9": "no-port"}}"#,
    ] {
        let (st, b) = api(first, "POST", "/v1/members", bad);
        assert_eq!(st, 400, "{}", String::from_utf8_lossy(&b));
    }

    let addrs = m["addrs"].as_object().unwrap().clone();
    let keep: serde_json::Map<String, serde_json::Value> =
        addrs.into_iter().filter(|(id, _)| id != "m3").collect();
    let mut body = serde_json::json!({ "voters": keep });
    // The local node's own address is optional.
    for id in ["m1", "m2"] {
        body["voters"]
            .as_object_mut()
            .unwrap()
            .entry(id)
            .or_insert("127.0.0.1:1".into());
    }
    let (_, b) = c.on_leader("POST", "/v1/members", body.to_string().as_bytes(), 200);
    assert_eq!(
        json(&b)["membership"]["voters"],
        serde_json::json!(["m1", "m2"])
    );

    for (id, addr) in c.meta_addrs().into_iter().filter(|(id, _)| id != "m3") {
        let deadline = Instant::now() + WAIT;
        loop {
            let m = json(&api(addr, "GET", "/v1/members", b"").1);
            if m["membership"]["voters"] == serde_json::json!(["m1", "m2"]) {
                break;
            }
            assert!(Instant::now() < deadline, "{id}: {m}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"name":"after","size_bytes":4096}"#,
        201,
    );
}

#[test]
fn http_file_api_on_a_three_node_cluster() {
    let c = Cluster::start(3, 3, 0);
    let _ = c.on_leader("POST", "/v1/fs", br#"{"id":"f1","name":"home"}"#, 201);
    let call = |method: &str, path: &str, body: &[u8]| {
        // Leadership may move under load; follow it like a client would.
        c.on_leader_any(method, path, body)
    };
    let code = |method: &str, path: &str, body: &[u8]| {
        let (st, b) = c.on_leader_any(method, path, body);
        (
            st,
            json(&b)["code"].as_str().unwrap_or_default().to_string(),
        )
    };
    // A retried create is a no-op.
    assert_eq!(
        call("POST", "/v1/fs", br#"{"id":"f1","name":"home"}"#).0,
        201
    );

    let mk = |parent: u64, body: &str| {
        let (st, b) = call(
            "POST",
            &format!("/v1/fs/f1/inodes/{parent}/entries"),
            body.as_bytes(),
        );
        assert_eq!(st, 201, "{}", String::from_utf8_lossy(&b));
        json(&b)["ino"].as_u64().unwrap()
    };
    let d = mk(1, r#"{"name":"d","op_id":"o1","kind":"dir","mode":493}"#);
    let a = mk(
        d,
        r#"{"name":"my file é","op_id":"o2","kind":"file","mode":420}"#,
    );
    assert_eq!(
        mk(
            d,
            r#"{"name":"my file é","op_id":"o2","kind":"file","mode":420}"#
        ),
        a
    );
    let l = mk(
        1,
        r#"{"name":"ln","op_id":"o3","kind":"symlink","target":"d/my file é"}"#,
    );

    // Unaligned write across extent boundaries, then read it back from every replica.
    let payload: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let (st, b) = call(
        "PUT",
        &format!("/v1/fs/f1/inodes/{a}/data?offset=100"),
        &payload,
    );
    assert_eq!(st, 200);
    assert_eq!(json(&b)["size"], 6100);
    let mut want = vec![0u8; 100];
    want.extend_from_slice(&payload);
    c.wait_read(
        &format!("/v1/fs/f1/inodes/{a}/data?offset=0&len=9000&stale=1"),
        &want,
    );

    let (st, b) = call(
        "GET",
        &format!("/v1/fs/f1/inodes/{d}/lookup?name=my%20file+%C3%A9"),
        b"",
    );
    assert_eq!((st, json(&b)["ino"].as_u64()), (200, Some(a)));
    let (_, b) = call("GET", "/v1/fs/f1/inodes/1/entries", b"");
    let names = json(&b)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["d", "ln"]);
    let page = |q: &str| {
        let (st, b) = call("GET", &format!("/v1/fs/f1/inodes/1/entries?{q}"), b"");
        assert_eq!(st, 200, "{q}");
        json(&b)["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(page("limit=1"), ["d"]);
    assert_eq!(page("limit=1&after=d"), ["ln"]);
    assert!(page("limit=5&after=ln").is_empty());
    assert_eq!(
        call("GET", "/v1/fs/f1/inodes/1/entries?limit=x", b"").0,
        400
    );

    // After a read barrier any replica serves a read that sees the latest write, at once.
    let (st, _) = call(
        "POST",
        "/v1/fs/f1/inodes/1/entries",
        br#"{"name":"fresh","op_id":"o9","kind":"dir","mode":493}"#,
    );
    assert_eq!(st, 201);
    for (id, addr) in c.meta_addrs() {
        let (st, b) = api(
            addr,
            "GET",
            "/v1/fs/f1/inodes/1/lookup?name=fresh&barrier=1",
            b"",
        );
        assert_eq!(st, 200, "{id}: {}", String::from_utf8_lossy(&b));
    }
    assert_eq!(
        call("POST", "/v1/fs/f1/inodes/1/rmdir", br#"{"name":"fresh"}"#).0,
        204
    );
    let (_, b) = call("GET", &format!("/v1/fs/f1/inodes/{l}/target"), b"");
    assert_eq!(json(&b)["target"], "d/my file é");

    // Errors carry a stable code for errno mapping.
    let entries = format!("/v1/fs/f1/inodes/{d}/entries");
    assert_eq!(
        code(
            "POST",
            &entries,
            br#"{"name":"my file \u00e9","op_id":"x","kind":"file"}"#
        ),
        (409, "exists".into())
    );
    assert_eq!(
        code("POST", "/v1/fs/f1/inodes/1/rmdir", br#"{"name":"d"}"#),
        (409, "not_empty".into())
    );
    assert_eq!(
        code("POST", "/v1/fs/f1/inodes/1/unlink", br#"{"name":"d"}"#),
        (409, "is_dir".into())
    );
    assert_eq!(
        code("GET", "/v1/fs/f1/inodes/1/lookup?name=nope", b""),
        (404, "not_found".into())
    );
    assert_eq!(
        code(
            "POST",
            &format!("/v1/fs/f1/inodes/{a}/entries"),
            br#"{"name":"x","op_id":"y","kind":"file"}"#
        ),
        (409, "not_dir".into())
    );
    assert_eq!(
        code(
            "POST",
            "/v1/fs/f1/inodes/1/entries",
            br#"{"name":"a/b","op_id":"z","kind":"file"}"#
        ),
        (409, "invalid".into())
    );

    // Hard link, rename, truncate.
    let (st, b) = call(
        "POST",
        &format!("/v1/fs/f1/inodes/{a}/links"),
        br#"{"parent":1,"name":"hard"}"#,
    );
    assert_eq!((st, json(&b)["nlink"].as_u64()), (200, Some(2)));
    let rename =
        format!(r#"{{"parent":{d},"name":"my file é","new_parent":1,"new_name":"moved"}}"#);
    assert_eq!(call("POST", "/v1/fs/f1/rename", rename.as_bytes()).0, 204);
    let (st, b) = call(
        "POST",
        &format!("/v1/fs/f1/inodes/{a}/attr"),
        br#"{"size":50,"mode":384}"#,
    );
    assert_eq!(st, 200);
    assert_eq!(
        (json(&b)["size"].as_u64(), json(&b)["mode"].as_u64()),
        (Some(50), Some(0o600))
    );

    // Snapshots are frozen, readable as fs@snap, and refuse writes.
    let (st, b) = call(
        "POST",
        "/v1/fs/f1/snapshots",
        br#"{"id":"s1","name":"before"}"#,
    );
    assert_eq!((st, json(&b)["id"].as_str()), (201, Some("s1")));
    assert_eq!(
        call(
            "PUT",
            &format!("/v1/fs/f1/inodes/{a}/data?offset=0"),
            b"CHANGED"
        )
        .0,
        200
    );
    c.wait_read(
        &format!("/v1/fs/f1@s1/inodes/{a}/data?offset=0&len=50&stale=1"),
        &want[..50],
    );
    assert_eq!(
        code(
            "PUT",
            &format!("/v1/fs/f1@s1/inodes/{a}/data?offset=0"),
            b"x"
        ),
        (409, "read_only".into())
    );
    let (st, _) = call(
        "POST",
        "/v1/fs-snapshots/s1/clone",
        br#"{"id":"c1","name":"copy"}"#,
    );
    assert_eq!(st, 201);
    c.wait_read(
        &format!("/v1/fs/c1/inodes/{a}/data?offset=0&len=50&stale=1"),
        &want[..50],
    );
    let (_, b) = call("GET", "/v1/fs", b"");
    assert_eq!(json(&b)["filesystems"].as_array().unwrap().len(), 2);

    assert_eq!(call("DELETE", "/v1/fs/c1", b"").0, 204);
    assert_eq!(call("DELETE", "/v1/fs-snapshots/s1", b"").0, 204);
    assert_eq!(
        call("POST", "/v1/fs/f1/inodes/1/unlink", br#"{"name":"moved"}"#).0,
        204
    );
    assert_eq!(
        call("POST", "/v1/fs/f1/inodes/1/unlink", br#"{"name":"hard"}"#).0,
        204
    );
    let (st, b) = call("POST", "/v1/gc", b"");
    assert_eq!(st, 200);
    assert!(json(&b)["reclaimed"].as_u64().unwrap() > 0);

    // Quotas: the limits are enforced over the API and reported by statfs.
    assert_eq!(
        call("POST", "/v1/fs", br#"{"id":"fq","name":"quota"}"#).0,
        201
    );
    let (st, b) = call("GET", "/v1/fs/fq/quota", b"");
    assert_eq!((st, json(&b)), (200, serde_json::json!({})));
    assert_eq!(
        code("PUT", "/v1/fs/fq/quota", br#"{"max_byte":1}"#),
        (400, "invalid".into())
    );
    let (st, b) = call(
        "PUT",
        "/v1/fs/fq/quota",
        br#"{"max_bytes":4096,"max_inodes":3}"#,
    );
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    let mkq = |body: &str| call("POST", "/v1/fs/fq/inodes/1/entries", body.as_bytes());
    let (st, b) = mkq(r#"{"name":"f","op_id":"q1","kind":"file","mode":420}"#);
    assert_eq!(st, 201);
    let qf = json(&b)["ino"].as_u64().unwrap();
    assert_eq!(
        mkq(r#"{"name":"d","op_id":"q2","kind":"dir","mode":493}"#).0,
        201
    );
    let (st, b) = mkq(r#"{"name":"x","op_id":"q3","kind":"file","mode":420}"#);
    assert_eq!(
        (st, json(&b)["code"].as_str()),
        (409, Some("quota")),
        "{}",
        String::from_utf8_lossy(&b)
    );
    let data = format!("/v1/fs/fq/inodes/{qf}/data?offset=");
    assert_eq!(call("PUT", &format!("{data}0"), &[1u8; 4096]).0, 200);
    // A rewrite at the limit is fine; growing past it is not.
    assert_eq!(call("PUT", &format!("{data}0"), &[2u8; 4096]).0, 200);
    assert_eq!(
        code("PUT", &format!("{data}4096"), b"z"),
        (409, "quota".into())
    );
    let (_, b) = call("GET", "/v1/fs/fq/statfs", b"");
    let s = json(&b);
    assert_eq!(
        (s["used_bytes"].as_u64(), s["quota"]["max_bytes"].as_u64()),
        (Some(4096), Some(4096))
    );
    assert_eq!(call("PUT", "/v1/fs/fq/quota", b"{}").0, 200);
    assert_eq!(call("PUT", &format!("{data}4096"), b"z").0, 200);
    assert_eq!(call("DELETE", "/v1/fs/fq", b"").0, 204);
}

#[test]
fn http_namespace_sharded_across_raft_groups() {
    const GROUPS: usize = 4;
    let mut c = Cluster::start_groups(3, 3, 0, GROUPS as u32);
    let ids: Vec<String> = (0..12).map(|i| format!("v{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        let body = format!(r#"{{"id":"{id}","name":"vol{i}","size_bytes":8192}}"#);
        c.on_leader("POST", "/v1/volumes", body.as_bytes(), 201);
        c.on_leader(
            "PUT",
            &format!("/v1/volumes/{id}/data?offset=0"),
            &[i as u8 + 1; 4096],
            204,
        );
    }

    // Every node replicates every group, each with its own leader.
    let (_, addr) = c.meta_addrs()[0].clone();
    let deadline = Instant::now() + WAIT;
    loop {
        let (_, b) = api(addr, "GET", "/v1/status", b"");
        let groups = json(&b)["metadata_groups"].as_array().unwrap().clone();
        assert_eq!(groups.len(), GROUPS);
        if groups.iter().all(|g| g["leader"].is_string()) {
            break;
        }
        assert!(Instant::now() < deadline, "a group has no leader");
        thread::sleep(Duration::from_millis(20));
    }

    // The volumes spread over the groups; each group's engine reports its own share.
    let deadline = Instant::now() + WAIT;
    loop {
        let (_, b) = api(addr, "GET", "/metrics", b"");
        let m = String::from_utf8(b).unwrap();
        assert_eq!(m.matches("# TYPE atlas_native_volumes ").count(), 1, "{m}");
        let per_group: Vec<u64> = (0..GROUPS)
            .map(|g| {
                let key = format!("atlas_native_volumes{{group=\"{g}\"}} ");
                m.lines()
                    .find_map(|l| l.strip_prefix(&key))
                    .map_or(0, |v| v.parse().unwrap())
            })
            .collect();
        if per_group.iter().sum::<u64>() == 12 {
            assert!(
                per_group.iter().filter(|n| **n > 0).count() >= 2,
                "{per_group:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "volumes never replicated: {per_group:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    // A snapshot joins its volume's group and a clone its snapshot's, wherever their ids hash.
    c.on_leader(
        "POST",
        "/v1/volumes/v3/snapshots",
        br#"{"id":"s3","name":"snap"}"#,
        201,
    );
    c.on_leader(
        "POST",
        "/v1/snapshots/s3/clone",
        br#"{"id":"c3","name":"clone"}"#,
        201,
    );
    c.wait_read("/v1/volumes/c3/data?offset=0&len=4096", &[4u8; 4096]);
    // An id another group already holds is refused, not duplicated.
    for id in &ids {
        let body = format!(r#"{{"id":"{id}","name":"dup"}}"#);
        let (st, _) = c.on_leader_any("POST", "/v1/snapshots/s3/clone", body.as_bytes());
        assert_eq!(st, 409, "clone over {id}");
    }
    // Any node lists every group's volumes.
    for (id, addr) in c.meta_addrs() {
        let deadline = Instant::now() + WAIT;
        loop {
            let (_, b) = api(addr, "GET", "/v1/volumes", b"");
            if json(&b)["volumes"].as_array().unwrap().len() == 13 {
                break;
            }
            assert!(Instant::now() < deadline, "{id} never listed every volume");
            thread::sleep(Duration::from_millis(20));
        }
    }

    // Filesystems shard the same way; a listing needs no node to lead every group.
    for f in 0..4 {
        let body = format!(r#"{{"id":"f{f}","name":"fs{f}"}}"#);
        c.on_leader("POST", "/v1/fs", body.as_bytes(), 201);
        let (_, b) = c.on_leader(
            "POST",
            &format!("/v1/fs/f{f}/inodes/1/entries"),
            br#"{"name":"a","op_id":"o1","kind":"file","mode":420}"#,
            201,
        );
        let ino = json(&b)["ino"].as_u64().unwrap();
        c.on_leader(
            "PUT",
            &format!("/v1/fs/f{f}/inodes/{ino}/data?offset=0"),
            format!("hello {f}").as_bytes(),
            200,
        );
        c.on_leader(
            "POST",
            &format!("/v1/fs/f{f}/snapshots"),
            format!(r#"{{"id":"fs{f}","name":"snap"}}"#).as_bytes(),
            201,
        );
        c.on_leader(
            "POST",
            &format!("/v1/fs-snapshots/fs{f}/clone"),
            format!(r#"{{"id":"g{f}","name":"clone"}}"#).as_bytes(),
            201,
        );
        c.wait_read(
            &format!("/v1/fs/g{f}/inodes/{ino}/data?offset=0&len=7&stale=1"),
            format!("hello {f}").as_bytes(),
        );
    }
    for (id, addr) in c.meta_addrs() {
        let (st, b) = api(addr, "GET", "/v1/fs", b"");
        assert_eq!(st, 200, "{id}: {}", String::from_utf8_lossy(&b));
        assert_eq!(json(&b)["filesystems"].as_array().unwrap().len(), 8);
        let (st, b) = api(addr, "GET", "/v1/fs-snapshots", b"");
        assert_eq!(st, 200, "{id}");
        assert_eq!(json(&b)["snapshots"].as_array().unwrap().len(), 4);
    }

    // Losing a metadata node moves its groups' leadership; every group keeps serving.
    let gone = c.meta_addrs()[0].0.clone();
    c.meta.insert(gone, None);
    for (i, id) in ids.iter().enumerate() {
        c.wait_read(
            &format!("/v1/volumes/{id}/data?offset=0&len=4096"),
            &[i as u8 + 1; 4096],
        );
    }
    c.on_leader(
        "POST",
        "/v1/volumes",
        br#"{"id":"after","name":"after","size_bytes":4096}"#,
        201,
    );
}

#[test]
fn http_members_change_reaches_every_raft_group() {
    let c = Cluster::start_groups(3, 3, 0, 3);
    let (_, first) = c.meta_addrs()[0].clone();
    let m = json(&api(first, "GET", "/v1/members", b"").1);
    assert_eq!(m["groups"].as_array().unwrap().len(), 3);
    let keep: serde_json::Map<String, serde_json::Value> = m["addrs"]
        .as_object()
        .unwrap()
        .clone()
        .into_iter()
        .filter(|(id, _)| id != "m3")
        .collect();
    let mut body = serde_json::json!({ "voters": keep });
    for id in ["m1", "m2"] {
        body["voters"]
            .as_object_mut()
            .unwrap()
            .entry(id)
            .or_insert("127.0.0.1:1".into());
    }
    // Each node changes the groups it leads; repeating the request across nodes finishes it.
    c.on_leader("POST", "/v1/members", body.to_string().as_bytes(), 200);
    for (id, addr) in c.meta_addrs().into_iter().filter(|(id, _)| id != "m3") {
        let deadline = Instant::now() + WAIT;
        loop {
            let m = json(&api(addr, "GET", "/v1/members", b"").1);
            let done = m["groups"]
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["membership"]["voters"] == serde_json::json!(["m1", "m2"]));
            if done {
                break;
            }
            assert!(Instant::now() < deadline, "{id}: {m}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    for v in 0..6 {
        let body = format!(r#"{{"id":"after{v}","name":"after","size_bytes":4096}}"#);
        c.on_leader("POST", "/v1/volumes", body.as_bytes(), 201);
    }
}

#[test]
fn http_sessions_and_file_locks_survive_failover_and_expire() {
    let mut c = Cluster::start(3, 3, 0);
    c.on_leader("POST", "/v1/fs", br#"{"id":"lk","name":"locks"}"#, 201);
    let (_, b) = c.on_leader(
        "POST",
        "/v1/fs/lk/inodes/1/entries",
        br#"{"name":"f","op_id":"o1","kind":"file","mode":420}"#,
        201,
    );
    let ino = json(&b)["ino"].as_u64().unwrap();
    let locks = format!("/v1/fs/lk/inodes/{ino}/locks");
    let lock = |session: &str, kind: &str, start: u64, end: u64| {
        format!(
            r#"{{"session":"{session}","owner":1,"kind":"{kind}","start":{start},"end":{end},"pid":7}}"#
        )
    };
    for s in ["a", "b"] {
        let body = format!(r#"{{"session":"{s}","ttl_ms":60000}}"#);
        let (_, b) = c.on_leader("POST", "/v1/fs/lk/sessions", body.as_bytes(), 200);
        assert_eq!(json(&b)["ttl_ms"], 60000);
    }

    let (leader, _) = c.on_leader("POST", &locks, lock("a", "write", 0, 99).as_bytes(), 204);
    let (st, b) = c.on_leader_any("POST", &locks, lock("b", "write", 50, 60).as_bytes());
    assert_eq!((st, json(&b)["code"].as_str()), (409, Some("locked")));
    c.on_leader("POST", &locks, lock("b", "read", 100, 199).as_bytes(), 204);
    let (st, b) = c.on_leader_any("POST", &locks, lock("zz", "read", 0, 0).as_bytes());
    assert_eq!((st, json(&b)["code"].as_str()), (410, Some("no_session")));
    let (st, b) = c.on_leader_any(
        "POST",
        "/v1/fs/lk/inodes/1/locks",
        lock("a", "read", 0, 0).as_bytes(),
    );
    assert_eq!(st, 409, "a directory: {}", String::from_utf8_lossy(&b));
    let test = format!("{locks}?session=b&owner=1&kind=write&start=0&end=10");
    let (_, b) = c.on_leader("GET", &test, b"", 200);
    let conflict = &json(&b)["conflict"];
    assert_eq!(
        (&conflict["session"], &conflict["pid"]),
        (&serde_json::json!("a"), &serde_json::json!(7))
    );

    // Locks are replicated: a new leader enforces them.
    c.meta.insert(leader, None);
    let (st, _) = c.on_leader_any("POST", &locks, lock("b", "write", 50, 60).as_bytes());
    assert_eq!(st, 409);
    // Closing a session releases its locks.
    c.on_leader("DELETE", "/v1/fs/lk/sessions/a", b"", 204);
    c.on_leader("POST", &locks, lock("b", "write", 50, 60).as_bytes(), 204);
    c.on_leader("DELETE", "/v1/fs/lk/sessions/b", b"", 204);

    // A session that stops renewing is expired with its locks.
    c.on_leader(
        "POST",
        "/v1/fs/lk/sessions",
        br#"{"session":"c","ttl_ms":1000}"#,
        200,
    );
    c.on_leader("POST", &locks, lock("c", "write", 0, 9).as_bytes(), 204);
    let (_, b) = c.on_leader("GET", "/v1/fs/lk/locks", b"", 200);
    assert_eq!(json(&b)["locks"].as_array().unwrap().len(), 1);
    let deadline = Instant::now() + WAIT;
    loop {
        let (_, b) = c.on_leader("GET", "/v1/fs/lk/locks", b"", 200);
        if json(&b)["locks"].as_array().unwrap().is_empty() {
            assert_eq!(json(&b)["sessions"], 0);
            break;
        }
        assert!(Instant::now() < deadline, "session c never expired");
        thread::sleep(Duration::from_millis(100));
    }
    let (st, _) = c.on_leader_any("POST", "/v1/fs/lk/sessions/c/renew", b"");
    assert_eq!(st, 410);
    let expired: u64 = c
        .meta_addrs()
        .into_iter()
        .map(|(_, addr)| {
            let m = String::from_utf8(api(addr, "GET", "/metrics", b"").1).unwrap();
            m.lines()
                .find(|l| l.starts_with("atlas_native_client_sessions_expired_total"))
                .and_then(|l| l.rsplit(' ').next()?.parse().ok())
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(expired, 1);
}

#[test]
fn http_cache_leases_are_recalled_and_outlive_a_leader_change() {
    let mut c = Cluster::start(3, 3, 0);
    c.on_leader("POST", "/v1/fs", br#"{"id":"cl","name":"leases"}"#, 201);
    let (_, b) = c.on_leader(
        "POST",
        "/v1/fs/cl/inodes/1/entries",
        br#"{"name":"f","op_id":"o1","kind":"file","mode":420}"#,
        201,
    );
    let ino = json(&b)["ino"].as_u64().unwrap();
    c.on_leader(
        "POST",
        "/v1/fs/cl/sessions",
        br#"{"session":"x","ttl_ms":60000,"cache":true}"#,
        200,
    );
    // Without a session nothing is leased; with one the reply says for how long.
    let (_, b) = c.on_leader("GET", &format!("/v1/fs/cl/inodes/{ino}?lease=1"), b"", 200);
    assert!(json(&b).get("lease_ms").is_none());
    let (st, _) = c.on_leader_any(
        "GET",
        &format!("/v1/fs/cl/inodes/{ino}?lease=1&session=nope"),
        b"",
    );
    assert_eq!(st, 410);
    let (leader, b) = c.on_leader(
        "GET",
        "/v1/fs/cl/inodes/1/lookup?name=f&lease=1&session=x",
        b"",
        200,
    );
    assert_eq!(json(&b)["lease_ms"], 5000);
    let addr = c
        .meta_addrs()
        .into_iter()
        .find(|(id, _)| *id == leader)
        .unwrap()
        .1;

    // A write waits until the holder gives its lease back.
    let started = Instant::now();
    let writer = thread::spawn(move || {
        let (st, _) = api(
            addr,
            "PUT",
            &format!("/v1/fs/cl/inodes/{ino}/data?offset=0"),
            b"new",
        );
        (st, started.elapsed())
    });
    let (st, b) = api(
        addr,
        "GET",
        "/v1/fs/cl/sessions/x/recalls?wait_ms=5000",
        b"",
    );
    assert_eq!(st, 200);
    assert_eq!(json(&b)["inos"], serde_json::json!([ino]));
    thread::sleep(Duration::from_millis(200));
    assert!(
        !writer.is_finished(),
        "the write did not wait for the recall"
    );
    let body = format!(r#"{{"inos":[{ino}]}}"#);
    let (st, _) = api(
        addr,
        "POST",
        "/v1/fs/cl/sessions/x/recalls/done",
        body.as_bytes(),
    );
    assert_eq!(st, 204);
    let (st, waited) = writer.join().unwrap();
    assert_eq!(st, 200);
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // The holder's own changes are not held up by its lease.
    c.on_leader(
        "GET",
        &format!("/v1/fs/cl/inodes/{ino}?lease=1&session=x"),
        b"",
        200,
    );
    let started = Instant::now();
    c.on_leader(
        "PUT",
        &format!("/v1/fs/cl/inodes/{ino}/data?offset=0&session=x"),
        b"own",
        200,
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    // A new leader cannot know its predecessor's leases, so it holds changes back for one lease.
    c.meta.insert(leader, None);
    let started = Instant::now();
    c.on_leader(
        "PUT",
        &format!("/v1/fs/cl/inodes/{ino}/data?offset=0"),
        b"after",
        200,
    );
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}
