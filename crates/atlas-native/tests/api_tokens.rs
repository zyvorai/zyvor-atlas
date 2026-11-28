// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Scoped API tokens against a running node: what each role may and may not do, limits to named
//! filesystems and volumes, path and method confusion, the single administrator token, and what
//! the node writes to its log.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{Listeners, NativeNode, NodeConfig};
use serde_json::{json, Value};

const ADMIN: &str = "legacy-admin";

/// `(name, roles, filesystems, volumes)`; each token's value is `value-of-<name>-0123456789`.
type Spec = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

const TOKENS: &[Spec] = &[
    ("reader", &["read-only"], &[], &[]),
    ("reader-a", &["read-only"], &["a"], &[]),
    ("data-a", &["data"], &["a"], &[]),
    ("data-all", &["data"], &["*"], &[]),
    ("block-v1", &["data"], &[], &["v1"]),
    ("csi", &["lifecycle"], &[], &[]),
    ("gateway", &["lifecycle", "data"], &[], &[]),
    ("replicate", &["replication"], &[], &[]),
    ("ops", &["admin"], &[], &[]),
    ("officer", &["retention-unlock"], &[], &[]),
];

fn value(name: &str) -> String {
    match name {
        "admin" => ADMIN.into(),
        n => format!("value-of-{n}-0123456789"),
    }
}

fn write_credentials(dir: &Path) {
    std::fs::write(dir.join("token"), format!("{ADMIN}\n")).unwrap();
    let keys = dir.join("tokens");
    std::fs::create_dir_all(&keys).unwrap();
    let list: Vec<Value> = TOKENS
        .iter()
        .map(|(name, roles, fs, vols)| {
            std::fs::write(keys.join(name), format!("{}\n", value(name))).unwrap();
            let mut t = json!({ "name": name, "token_file": name, "roles": roles });
            if !fs.is_empty() {
                t["filesystems"] = json!(fs);
            }
            if !vols.is_empty() {
                t["volumes"] = json!(vols);
            }
            t
        })
        .collect();
    std::fs::write(
        keys.join("tokens.json"),
        json!({ "tokens": list }).to_string(),
    )
    .unwrap();
}

fn config(dir: &Path, http: &str, data: SocketAddr, raft: SocketAddr, legacy: bool) -> Value {
    let mut c = json!({
        "node_id": "n1",
        "data_dir": dir.join("state"),
        "http_listen": http,
        "api_tokens_file": dir.join("tokens/tokens.json"),
        "data_node": { "listen": data },
        "metadata": {
            "listen": raft,
            "peers": { "n1": raft.to_string() },
            "data_nodes": [{ "id": "n1", "addr": data.to_string() }],
            "replicas": 1,
            "extent_bytes": 4096,
            "tick_ms": 10,
            "repair_interval_secs": 0,
            "gc_interval_secs": 0,
        },
    });
    if legacy {
        c["api_token_file"] = json!(dir.join("token"));
    }
    c
}

/// One request with the target and header lines given verbatim.
fn raw(addr: SocketAddr, method: &str, target: &str, headers: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let head = format!(
        "{method} {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{headers}Content-Length: {}\r\n\r\n",
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

/// `method target` as the named token.
fn call(addr: SocketAddr, who: &str, method: &str, target: &str, body: &[u8]) -> (u16, Value) {
    let auth = format!("Authorization: Bearer {}\r\n", value(who));
    let (st, b) = raw(addr, method, target, &auth, body);
    let v = serde_json::from_slice(&b)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()));
    (st, v)
}

#[track_caller]
fn ok(addr: SocketAddr, who: &str, method: &str, target: &str, body: &[u8]) -> Value {
    let (st, v) = call(addr, who, method, target, body);
    assert!(
        (200..300).contains(&st),
        "{who} {method} {target}: {st} {v}"
    );
    v
}

/// The request is refused for the token, before anything looks at the object.
#[track_caller]
fn forbidden(addr: SocketAddr, who: &str, method: &str, target: &str) {
    let (st, v) = call(addr, who, method, target, b"{}");
    assert_eq!(st, 403, "{who} {method} {target}: {v}");
    if method != "HEAD" {
        assert_eq!(v["code"], "forbidden", "{who} {method} {target}: {v}");
    }
}

/// The request gets past authorization (whatever the node then makes of it).
#[track_caller]
fn admitted(addr: SocketAddr, who: &str, method: &str, target: &str, body: &[u8]) -> (u16, Value) {
    let (st, v) = call(addr, who, method, target, body);
    assert!(st != 401 && st != 403, "{who} {method} {target}: {st} {v}");
    (st, v)
}

fn wait_ready(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(addr).is_ok() && raw(addr, "GET", "/readyz", "", b"").0 == 200 {
            return;
        }
        assert!(Instant::now() < deadline, "node never became ready");
        thread::sleep(Duration::from_millis(20));
    }
}

fn start(dir: &Path, legacy: bool) -> NativeNode {
    write_credentials(dir);
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (data, raft) = (bind(), bind());
    let cfg: NodeConfig = serde_json::from_value(config(
        dir,
        "127.0.0.1:0",
        data.local_addr().unwrap(),
        raft.local_addr().unwrap(),
        legacy,
    ))
    .unwrap();
    let node = NativeNode::start_with(
        cfg,
        Listeners {
            http: Some(bind()),
            data_node: Some(data),
            metadata: Some(raft),
        },
    )
    .unwrap();
    wait_ready(node.http_addr());
    node
}

/// Filesystems `a` and `b` with a file each, and volumes `v1` and `v2`. Returns the file inodes.
fn populate(a: SocketAddr, who: &str) -> (u64, u64) {
    let mut inos = Vec::new();
    for fs in ["a", "b"] {
        let body = json!({ "id": fs, "name": fs }).to_string();
        ok(a, who, "POST", "/v1/fs", body.as_bytes());
        let file = ok(
            a,
            who,
            "POST",
            &format!("/v1/fs/{fs}/inodes/1/entries"),
            br#"{"name":"f","op_id":"f","kind":"file","mode":420}"#,
        );
        let ino = file["ino"].as_u64().unwrap();
        ok(
            a,
            who,
            "PUT",
            &format!("/v1/fs/{fs}/inodes/{ino}/data?offset=0"),
            format!("secret of {fs}").as_bytes(),
        );
        inos.push(ino);
    }
    for v in ["v1", "v2"] {
        let body = json!({ "id": v, "name": v, "size_bytes": 8192 }).to_string();
        ok(a, who, "POST", "/v1/volumes", body.as_bytes());
    }
    (inos[0], inos[1])
}

fn ids(v: &Value, list: &str, field: &str) -> Vec<String> {
    v[list]
        .as_array()
        .unwrap_or_else(|| panic!("no {list} in {v}"))
        .iter()
        .map(|i| i[field].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn only_a_known_token_gets_in() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    let good = value("reader");
    for headers in [
        String::new(),
        "Authorization: Bearer wrong\r\n".into(),
        "Authorization: Bearer \r\n".into(),
        format!("Authorization: bearer {good}\r\n"),
        format!("Authorization: Basic {good}\r\n"),
        format!("Authorization: {good}\r\n"),
        format!("Authorization: Bearer  {good}\r\n"),
        format!("Authorization: Bearer {good}x\r\n"),
        format!("Authorization: Bearer {}\r\n", &good[..good.len() - 1]),
        format!("Authorization: Bearer {}\r\n", good.to_uppercase()),
        format!("Authorization: Bearer {ADMIN}x\r\n"),
        format!("Authorization: Bearer {ADMIN} {good}\r\n"),
        // The last of repeated headers is the one read.
        format!("Authorization: Bearer {good}\r\nAuthorization: Bearer wrong\r\n"),
        format!("X-Authorization: Bearer {good}\r\n"),
    ] {
        for target in ["/v1/status", "/v1/nothing", "//v1/status", "/healthz/"] {
            let (st, body) = raw(a, "GET", target, &headers, b"");
            assert_eq!(st, 401, "{target} {headers:?}");
            assert_eq!(body, b"missing or invalid bearer token");
        }
    }
    // A token in the query or body is not a credential.
    let (st, _) = raw(
        a,
        "GET",
        &format!("/v1/status?token={ADMIN}"),
        "",
        ADMIN.as_bytes(),
    );
    assert_eq!(st, 401);
    for path in ["/healthz", "/readyz", "/metrics"] {
        assert_eq!(raw(a, "GET", path, "", b"").0, 200, "{path}");
    }
}

#[test]
fn the_single_token_is_still_the_administrator() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    populate(a, "admin");
    ok(a, "admin", "GET", "/v1/status", b"");
    ok(a, "admin", "POST", "/v1/repair", b"");
    ok(a, "admin", "POST", "/v1/gc", b"");
    let snap = ok(
        a,
        "admin",
        "POST",
        "/v1/volumes/v1/snapshots",
        br#"{"id":"s1","name":"s1"}"#,
    );
    assert_eq!(snap["id"], "s1");
    ok(
        a,
        "admin",
        "POST",
        "/v1/snapshots/s1/lock",
        br#"{"retain_secs":3600}"#,
    );
    ok(a, "admin", "POST", "/v1/snapshots/s1/unlock", b"");
    ok(a, "admin", "DELETE", "/v1/snapshots/s1", b"");
    // Unknown routes still answer as before.
    let (st, v) = call(a, "admin", "GET", "/v1/nothing", b"");
    assert_eq!((st, v), (404, json!("no such route")));
}

#[test]
fn scoped_tokens_work_without_an_administrator_token() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), false);
    let a = node.http_addr();
    assert_eq!(raw(a, "GET", "/v1/status", "", b"").0, 401);
    assert_eq!(call(a, "admin", "GET", "/v1/status", b"").0, 401);
    populate(a, "gateway");
    ok(a, "ops", "POST", "/v1/repair", b"");
}

#[test]
fn a_data_token_stays_inside_its_filesystem() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    let (fa, fb) = populate(a, "admin");

    // What a mount of `a` does.
    ok(a, "data-a", "GET", "/v1/status", b"");
    ok(a, "data-a", "GET", "/v1/fs/a/statfs", b"");
    ok(a, "data-a", "GET", "/v1/fs/a/inodes/1", b"");
    ok(a, "data-a", "GET", "/v1/fs/a/inodes/1/lookup?name=f", b"");
    ok(a, "data-a", "GET", "/v1/fs/a/inodes/1/entries", b"");
    let (st, v) = call(
        a,
        "data-a",
        "GET",
        &format!("/v1/fs/a/inodes/{fa}/data?offset=0&len=64"),
        b"",
    );
    assert_eq!((st, v), (200, json!("secret of a")));
    ok(
        a,
        "data-a",
        "POST",
        "/v1/fs/a/sessions",
        br#"{"session":"s-a","ttl_ms":30000}"#,
    );
    ok(a, "data-a", "POST", "/v1/fs/a/sessions/s-a/renew", b"");
    ok(
        a,
        "data-a",
        "POST",
        "/v1/fs/a/rename",
        br#"{"parent":1,"name":"f","new_parent":1,"new_name":"g"}"#,
    );
    ok(
        a,
        "data-a",
        "PUT",
        &format!("/v1/fs/a/inodes/{fa}/xattrs/user.k"),
        b"v",
    );
    ok(a, "data-a", "DELETE", "/v1/fs/a/sessions/s-a", b"");

    // Listings show only what the token names.
    let v = ok(a, "data-a", "GET", "/v1/fs", b"");
    assert_eq!(ids(&v, "filesystems", "id"), ["a"]);
    let v = ok(a, "data-a", "GET", "/v1/volumes", b"");
    assert!(ids(&v, "volumes", "id").is_empty());
    let v = ok(a, "data-all", "GET", "/v1/fs", b"");
    assert_eq!(ids(&v, "filesystems", "id"), ["a", "b"]);

    // Nothing of `b`, by any inode route.
    for (method, rest) in [
        ("GET", "statfs"),
        ("POST", "sessions"),
        ("GET", "sessions/s-a/recalls"),
        ("POST", "sessions/s-a/recalls/done"),
        ("POST", "sessions/s-a/renew"),
        ("DELETE", "sessions/s-a"),
        ("GET", "locks"),
        ("POST", "rename"),
        ("GET", "inodes/1"),
        ("POST", "inodes/1/attr"),
        ("GET", "inodes/1/lookup?name=f"),
        ("GET", "inodes/1/entries"),
        ("POST", "inodes/1/entries"),
        ("POST", "inodes/1/unlink"),
        ("POST", "inodes/1/rmdir"),
        ("POST", "inodes/1/links"),
        ("GET", "inodes/1/xattrs"),
        ("GET", "inodes/1/xattrs/user.k"),
        ("PUT", "inodes/1/xattrs/user.k"),
        ("DELETE", "inodes/1/xattrs/user.k"),
        ("POST", "inodes/1/locks"),
        ("GET", "inodes/1/locks"),
        ("POST", "inodes/1/locks/release"),
        ("GET", "inodes/1/target"),
        ("GET", "inodes/1/data?offset=0&len=1"),
        ("PUT", "inodes/1/data?offset=0"),
        ("GET", "inodes/1/layout?offset=0&len=1"),
        ("POST", "inodes/1/direct-write"),
        ("POST", "inodes/1/direct-commit"),
        ("POST", "inodes/1/direct-abort"),
    ] {
        forbidden(a, "data-a", method, &format!("/v1/fs/b/{rest}"));
        admitted(a, "data-a", method, &format!("/v1/fs/a/{rest}"), b"{}");
    }
    // A filesystem that does not exist looks the same as one that is out of scope.
    forbidden(a, "data-a", "GET", "/v1/fs/nothing/inodes/1");
    let v = ok(
        a,
        "admin",
        "GET",
        &format!("/v1/fs/b/inodes/{fb}/data?offset=0&len=64"),
        b"",
    );
    assert_eq!(v, json!("secret of b"));

    // Not the data path: lifecycle, administration, other kinds of object.
    for (method, target) in [
        ("DELETE", "/v1/fs/a"),
        ("POST", "/v1/fs"),
        ("PUT", "/v1/fs/a/quota"),
        ("GET", "/v1/fs/a/quota"),
        ("POST", "/v1/fs/a/snapshots"),
        ("POST", "/v1/fs/a/promote"),
        ("POST", "/v1/fs/a/demote"),
        ("POST", "/v1/fs/a/replica/apply"),
        ("POST", "/v1/fs/a/locality/pin"),
        ("GET", "/v1/fs-snapshots"),
        ("GET", "/v1/members"),
        ("POST", "/v1/members"),
        ("GET", "/v1/reduction"),
        ("POST", "/v1/volumes"),
        ("GET", "/v1/volumes/v1/data?offset=0&len=1"),
        ("PUT", "/v1/volumes/v1/data?offset=0"),
        ("GET", "/v1/snapshots/s/data?offset=0&len=1"),
        ("POST", "/v1/repair"),
        ("GET", "/v1/exports"),
        ("GET", "/v1/transfers"),
    ] {
        forbidden(a, "data-a", method, target);
    }

    // Snapshot trees follow their filesystem.
    ok(
        a,
        "admin",
        "POST",
        "/v1/fs/a/snapshots",
        br#"{"id":"snap-a","name":"s"}"#,
    );
    ok(
        a,
        "admin",
        "POST",
        "/v1/fs/b/snapshots",
        br#"{"id":"snap-b","name":"s"}"#,
    );
    ok(a, "data-a", "GET", "/v1/fs/a@snap-a/inodes/1/entries", b"");
    forbidden(a, "data-a", "GET", "/v1/fs/b@snap-b/inodes/1/entries");
    forbidden(a, "data-a", "GET", "/v1/fs/b@snap-a/inodes/1/entries");
    let (st, v) = call(
        a,
        "data-a",
        "GET",
        &format!("/v1/fs/a@snap-b/inodes/{fb}/data?offset=0&len=64"),
        b"",
    );
    assert_eq!(st, 404, "{v}");
}

#[test]
fn a_volume_token_stays_inside_its_volume() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    populate(a, "admin");
    ok(a, "block-v1", "PUT", "/v1/volumes/v1/data?offset=0", b"abc");
    let (st, v) = call(
        a,
        "block-v1",
        "GET",
        "/v1/volumes/v1/data?offset=0&len=3",
        b"",
    );
    assert_eq!((st, v), (200, json!("abc")));
    forbidden(a, "block-v1", "PUT", "/v1/volumes/v2/data?offset=0");
    forbidden(a, "block-v1", "GET", "/v1/volumes/v2/data?offset=0&len=3");
    forbidden(a, "block-v1", "GET", "/v1/fs/a/inodes/1");
    forbidden(a, "block-v1", "DELETE", "/v1/volumes/v1");
    forbidden(a, "block-v1", "POST", "/v1/volumes/v1/resize");
    let v = ok(a, "block-v1", "GET", "/v1/volumes", b"");
    assert_eq!(ids(&v, "volumes", "id"), ["v1"]);
    let v = ok(a, "block-v1", "GET", "/v1/fs", b"");
    assert!(ids(&v, "filesystems", "id").is_empty());
}

#[test]
fn paths_and_methods_cannot_be_dressed_up() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    populate(a, "admin");
    for target in [
        "/v1/fs/%62/inodes/1",
        "/v1/fs/%61/inodes/1",
        "/v1/fs/a%40x/inodes/1",
        "/v1/fs/a%2F..%2Fb/inodes/1",
        "/v1/fs/a/../b/inodes/1",
        "/v1/fs/./a/inodes/1",
        "/v1/fs//inodes/1",
        "/v1//fs/a/inodes/1",
        "/v1/fs/a//inodes/1",
        "//v1/fs/b/inodes/1//",
        "/v1/fs/b/inodes/1?x=/v1/fs/a",
        "/v1/fs/b/inodes/1#/v1/fs/a",
        "/v1/fs/b;/v1/fs/a",
        "/v1/fs/a;b/inodes/1",
        "/v1/fs/A/inodes/1",
        "/V1/fs/a/inodes/1",
        "/v1/FS/a/inodes/1",
        "/%76%31/fs/a/inodes/1",
        "/v1%2Ffs%2Fa/inodes/1",
        "http://test/v1/fs/a/inodes/1",
        "/x/../v1/fs/a/inodes/1",
        "*",
    ] {
        for who in ["data-a", "reader-a"] {
            forbidden(a, who, "GET", target);
        }
    }
    // Surrounding slashes are ignored by the router and by authorization alike.
    ok(a, "data-a", "GET", "//v1/fs/a/inodes/1/", b"");
    forbidden(a, "data-a", "GET", "//v1/fs/b/inodes/1/");

    for method in ["get", "Get", "HEAD", "OPTIONS", "PATCH", "TRACE", "FOO"] {
        for who in ["data-a", "reader", "csi", "ops", "officer"] {
            forbidden(a, who, method, "/v1/fs/a/inodes/1");
            forbidden(a, who, method, "/v1/volumes");
        }
    }
    // A read-only token changes nothing, whatever the headers claim.
    let auth = format!(
        "Authorization: Bearer {}\r\nX-HTTP-Method-Override: GET\r\n",
        value("reader")
    );
    assert_eq!(raw(a, "DELETE", "/v1/fs/a", &auth, b"").0, 403);
    for (method, target) in [
        ("DELETE", "/v1/fs/a"),
        ("DELETE", "/v1/fs/a?method=GET"),
        ("PUT", "/v1/fs/a/inodes/1/data?offset=0"),
        ("POST", "/v1/fs/a/inodes/1/entries"),
        ("POST", "/v1/fs/a/sessions"),
        ("DELETE", "/v1/volumes/v1"),
        ("PUT", "/v1/volumes/v1/data?offset=0"),
        ("POST", "/v1/repair"),
    ] {
        forbidden(a, "reader", method, target);
    }
    ok(a, "reader", "GET", "/v1/fs/b/inodes/1/entries", b"");
    ok(a, "reader", "GET", "/v1/members", b"");
    ok(a, "reader-a", "GET", "/v1/fs/a/quota", b"");
    forbidden(a, "reader-a", "GET", "/v1/fs/b/quota");
    forbidden(a, "reader-a", "GET", "/v1/members");
    let v = ok(a, "reader-a", "GET", "/v1/fs-snapshots", b"");
    assert!(ids(&v, "snapshots", "id").is_empty());
    // Everything is still there.
    let v = ok(a, "admin", "GET", "/v1/fs", b"");
    assert_eq!(ids(&v, "filesystems", "id"), ["a", "b"]);
}

#[test]
fn clearing_a_retention_lock_needs_its_own_capability() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    populate(a, "admin");
    ok(
        a,
        "csi",
        "POST",
        "/v1/volumes/v1/snapshots",
        br#"{"id":"block","name":"s"}"#,
    );
    ok(
        a,
        "csi",
        "POST",
        "/v1/fs/a/snapshots",
        br#"{"id":"tree","name":"s"}"#,
    );
    let lock = br#"{"retain_secs":3600}"#;
    ok(a, "csi", "POST", "/v1/snapshots/block/lock", lock);
    ok(a, "gateway", "POST", "/v1/fs-snapshots/tree/lock", lock);
    for base in ["/v1/snapshots/block", "/v1/fs-snapshots/tree"] {
        for who in [
            "reader",
            "reader-a",
            "data-a",
            "data-all",
            "block-v1",
            "csi",
            "gateway",
            "replicate",
            "ops",
        ] {
            forbidden(a, who, "POST", &format!("{base}/unlock"));
        }
        // The lock holds against those who may delete snapshots.
        let (st, v) = call(a, "csi", "DELETE", base, b"");
        assert_eq!(st, 409, "{base}: {v}");
        assert!(v.to_string().contains("retention-locked"), "{base}: {v}");
        // The unlock capability is that and nothing else.
        forbidden(a, "officer", "DELETE", base);
        forbidden(a, "officer", "POST", &format!("{base}/lock"));
        forbidden(a, "officer", "GET", "/v1/status");
        forbidden(a, "officer", "GET", "/v1/fs");
        ok(a, "officer", "POST", &format!("{base}/unlock"), b"");
        ok(a, "csi", "DELETE", base, b"");
    }
}

#[test]
fn lifecycle_replication_and_admin_roles_are_separate() {
    let td = tempfile::tempdir().unwrap();
    let node = start(td.path(), true);
    let a = node.http_addr();
    let (fa, _) = populate(a, "gateway");

    // What the CSI controller does, without touching data.
    ok(a, "csi", "PUT", "/v1/fs/a/quota", br#"{"max_bytes":1000000}"#);
    ok(a, "csi", "GET", "/v1/fs/a/quota", b"");
    ok(a, "csi", "GET", "/v1/fs/a/statfs", b"");
    ok(
        a,
        "csi",
        "POST",
        "/v1/fs/a/snapshots",
        br#"{"id":"sa","name":"sa"}"#,
    );
    ok(a, "csi", "GET", "/v1/fs-snapshots", b"");
    ok(
        a,
        "csi",
        "POST",
        "/v1/fs-snapshots/sa/clone",
        br#"{"id":"c","name":"c"}"#,
    );
    ok(a, "csi", "DELETE", "/v1/fs/c", b"");
    for target in [
        format!("/v1/fs/a/inodes/{fa}/data?offset=0&len=64"),
        "/v1/volumes/v1/data?offset=0&len=1".into(),
        "/v1/fs/a/inodes/1/entries".into(),
    ] {
        forbidden(a, "csi", "GET", &target);
    }
    forbidden(a, "csi", "POST", "/v1/repair");
    forbidden(a, "csi", "POST", "/v1/members");
    forbidden(a, "csi", "POST", "/v1/snapshots/x/export");

    // The Atlas gateway also moves block data.
    ok(a, "gateway", "PUT", "/v1/volumes/v1/data?offset=0", b"x");
    forbidden(a, "gateway", "POST", "/v1/gc");

    // Replication: snapshots, diffs and reads, not removal of filesystems or live writes.
    ok(
        a,
        "replicate",
        "POST",
        "/v1/fs/a/snapshots",
        br#"{"id":"r1","name":"r1"}"#,
    );
    ok(a, "replicate", "GET", "/v1/fs-snapshots/r1/diff", b"");
    ok(
        a,
        "replicate",
        "GET",
        &format!("/v1/fs/a@r1/inodes/{fa}/data?offset=0&len=64"),
        b"",
    );
    ok(
        a,
        "replicate",
        "POST",
        "/v1/fs",
        br#"{"id":"copy","name":"copy","replica":true,"extent_bytes":4096}"#,
    );
    ok(a, "replicate", "DELETE", "/v1/fs-snapshots/r1", b"");
    forbidden(a, "replicate", "DELETE", "/v1/fs/a");
    forbidden(a, "replicate", "PUT", "/v1/fs/a/inodes/1/data?offset=0");
    forbidden(a, "replicate", "POST", "/v1/fs/copy/promote");
    forbidden(a, "replicate", "DELETE", "/v1/volumes/v1");

    // A scoped administrator may do all but unlock; a route nobody classified stays closed.
    ok(a, "ops", "POST", "/v1/repair", b"");
    ok(a, "ops", "POST", "/v1/gc", b"");
    ok(a, "ops", "GET", "/v1/members", b"");
    ok(a, "ops", "DELETE", "/v1/fs/copy", b"");
    forbidden(a, "ops", "GET", "/v1/nothing");
    forbidden(a, "ops", "POST", "/v1/fs/a/nothing");

    let (_, metrics) = raw(a, "GET", "/metrics", "", b"");
    let metrics = String::from_utf8(metrics).unwrap();
    let denied = metrics
        .lines()
        .find(|l| {
            l.starts_with("atlas_native_api_denied_total") && l.contains("reason=\"forbidden\"")
        })
        .unwrap_or_else(|| panic!("{metrics}"));
    assert!(!denied.ends_with(" 0"), "{denied}");
}

struct Killed(Option<Child>);

impl Drop for Killed {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn the_node_logs_denials_and_never_a_token() {
    let td = tempfile::tempdir().unwrap();
    write_credentials(td.path());
    // The binary takes its addresses from the file: reserve three free ports.
    let ports: Vec<SocketAddr> = {
        let held: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        held.iter().map(|l| l.local_addr().unwrap()).collect()
    };
    let cfg = config(
        td.path(),
        &ports[0].to_string(),
        ports[1],
        ports[2],
        true,
    );
    std::fs::write(td.path().join("node.json"), cfg.to_string()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_atlas-native-node"))
        .arg("--config")
        .arg(td.path().join("node.json"))
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Killed(Some(child));
    let a = ports[0];
    wait_ready(a);

    let secret = value("data-a");
    assert_eq!(call(a, "data-a", "DELETE", "/v1/fs/b", b"").0, 403);
    // Tokens put where a log might repeat them.
    forbidden(a, "data-a", "GET", &format!("/v1/fs/{secret}/statfs"));
    forbidden(a, "data-a", "GET", &format!("/v1/{secret}?t={secret}"));
    forbidden(a, "data-a", "GET", &format!("/v1/fs/{ADMIN}/statfs"));
    let almost = format!("Authorization: Bearer {secret}x\r\n");
    assert_eq!(
        raw(a, "GET", &format!("/v1/fs/{secret}"), &almost, b"").0,
        401
    );
    assert_eq!(call(a, "csi", "POST", "/v1/snapshots/s/unlock", b"").0, 403);
    call(a, "officer", "POST", "/v1/snapshots/s/unlock", b"");

    let mut c = child.0.take().unwrap();
    c.kill().unwrap();
    let out = c.wait_with_output().unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let records: Vec<Value> = log
        .lines()
        .filter_map(|l| l.strip_prefix("atlas-native-audit "))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let has = |event: &str, token: Value, route: &str| {
        records
            .iter()
            .any(|r| r["event"] == event && r["token"] == token && r["route"] == route)
    };
    assert!(has("denied", json!("data-a"), "v1/fs/{fs}"), "{log}");
    assert!(has("denied", json!("data-a"), "unclassified"), "{log}");
    assert!(has("denied", Value::Null, "unclassified"), "{log}");
    assert!(
        has("denied", json!("csi"), "v1/snapshots/{id}/unlock"),
        "{log}"
    );
    assert!(
        has(
            "retention_unlock",
            json!("officer"),
            "v1/snapshots/{id}/unlock"
        ),
        "{log}"
    );
    assert!(!log.contains(ADMIN), "{log}");
    for (name, ..) in TOKENS {
        assert!(!log.contains(&value(name)), "{name}: {log}");
    }
}
