// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The single-node edge profile: one process is the only metadata voter and the only data node,
//! with one copy per extent and the smallest inode and store caches, through a restart.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{Listeners, NativeNode, NodeConfig};

const TOKEN: &str = "edge-test-token";

fn api(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nAuthorization: Bearer \
         {TOKEN}\r\nContent-Length: {}\r\n\r\n",
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

fn ok(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> serde_json::Value {
    let (st, b) = api(addr, method, path, body);
    assert!(
        (200..300).contains(&st),
        "{method} {path}: {st} {}",
        String::from_utf8_lossy(&b)
    );
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}

fn start(dir: &std::path::Path, data: TcpListener, raft: TcpListener) -> NativeNode {
    let cfg: NodeConfig = serde_json::from_value(serde_json::json!({
        "node_id": "edge-0",
        "data_dir": dir.join("state"),
        "http_listen": "127.0.0.1:0",
        "api_token_file": dir.join("token"),
        "data_node": { "listen": data.local_addr().unwrap() },
        "metadata": {
            "listen": raft.local_addr().unwrap(),
            "peers": { "edge-0": raft.local_addr().unwrap().to_string() },
            "data_nodes": [{ "id": "edge-0", "addr": data.local_addr().unwrap().to_string() }],
            "replicas": 1,
            "tick_ms": 10,
            "repair_interval_secs": 86400,
            "cache_inodes": 64,
            "store_cache_bytes": 1 << 20,
        },
    }))
    .unwrap();
    let node = NativeNode::start_with(
        cfg,
        Listeners {
            http: Some(TcpListener::bind("127.0.0.1:0").unwrap()),
            data_node: Some(data),
            metadata: Some(raft),
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while api(node.http_addr(), "GET", "/readyz", b"").0 != 200 {
        assert!(
            Instant::now() < deadline,
            "the edge node never became ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
    node
}

#[test]
fn a_single_node_with_minimal_caches_serves_and_survives_a_restart() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("token"), TOKEN).unwrap();
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (data, raft) = (bind(), bind());
    let (data_addr, raft_addr) = (data.local_addr().unwrap(), raft.local_addr().unwrap());
    let node = start(td.path(), data, raft);
    let a = node.http_addr();
    ok(a, "POST", "/v1/fs", br#"{"id":"edge","name":"edge"}"#);
    let dir = ok(
        a,
        "POST",
        "/v1/fs/edge/inodes/1/entries",
        br#"{"name":"d","op_id":"d","kind":"dir","mode":493}"#,
    )["ino"]
        .as_u64()
        .unwrap();
    // Enough commits for the Raft log to compact into the store (twice its 1024-entry
    // threshold), so later reads page inodes from the store through a 64-inode cache.
    let n = 2100;
    let mut inos = Vec::new();
    for i in 0..n {
        let body = format!(r#"{{"name":"f{i}","op_id":"f{i}","kind":"file","mode":420}}"#);
        let ino = ok(
            a,
            "POST",
            &format!("/v1/fs/edge/inodes/{dir}/entries"),
            body.as_bytes(),
        )["ino"]
            .as_u64()
            .unwrap();
        if i % 50 == 0 {
            ok(
                a,
                "PUT",
                &format!("/v1/fs/edge/inodes/{ino}/data?offset=0"),
                format!("payload {i}").as_bytes(),
            );
        }
        inos.push(ino);
    }
    let check = |a: SocketAddr| {
        let entries = ok(a, "GET", &format!("/v1/fs/edge/inodes/{dir}/entries"), b"");
        assert_eq!(entries["entries"].as_array().map(Vec::len), Some(n));
        for (i, ino) in inos.iter().enumerate().step_by(50) {
            let (st, b) = api(
                a,
                "GET",
                &format!("/v1/fs/edge/inodes/{ino}/data?offset=0&len=64"),
                b"",
            );
            assert_eq!((st, b), (200, format!("payload {i}").into_bytes()));
        }
    };
    check(a);
    drop(node);

    let node = start(
        td.path(),
        TcpListener::bind(data_addr).unwrap(),
        TcpListener::bind(raft_addr).unwrap(),
    );
    check(node.http_addr());
}
