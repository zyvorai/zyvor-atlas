// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The S3 front end against a live in-process cluster (3 Raft metadata nodes, 3 data nodes),
//! driven over HTTP: signed requests through Atlas's own S3 client, unsigned ones through
//! reqwest, and the same files checked through the FUSE client's operations layer.

use std::{
    collections::{BTreeMap, HashMap},
    net::{SocketAddr, TcpListener},
    path::Path,
    time::Duration,
};

use atlas_driver_rgw::S3Target;
use atlas_native::{
    node::{DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig},
    NodeType, ROOT_INO,
};
use atlas_native_fuse::{
    client::{Body, Client, ClientConfig, Retry},
    ops::{Ops, OpsConfig},
};
use atlas_native_s3::{
    service::{Gateway, Grant},
    store::Bucket,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ConnBuilder,
};
use reqwest::Method;
use s3s::{auth::SimpleAuth, service::S3ServiceBuilder};
use serde_json::json;

const TOKEN: &str = "s3-test-token";
const ADMIN: (&str, &str) = ("admin", "admin-secret-0123456789");
const READER: (&str, &str) = ("reader", "reader-secret-0123456789");

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

struct Cluster {
    _td: tempfile::TempDir,
    meta: Vec<NativeNode>,
    _data: Vec<NativeNode>,
}

impl Cluster {
    fn start() -> Self {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), TOKEN).unwrap();
        let data_l: Vec<(String, TcpListener)> =
            (1..=3).map(|i| (format!("d{i}"), bind())).collect();
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
        };
        let data = data_l
            .into_iter()
            .map(|(id, l)| {
                let mut cfg = base(&id, td.path());
                cfg.data_node = Some(DataNodeRole {
                    listen: l.local_addr().unwrap(),
                    devices: Vec::new(),
                });
                let listeners = Listeners {
                    http: Some(bind()),
                    data_node: Some(l),
                    metadata: None,
                };
                NativeNode::start_with(cfg, listeners).unwrap()
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
                    erasure: None,
                    erasure_min_bytes: 64 << 10,
                    compression: false,
                    dedup: false,
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
                let listeners = Listeners {
                    http: Some(bind()),
                    data_node: None,
                    metadata: Some(l),
                };
                NativeNode::start_with(cfg, listeners).unwrap()
            })
            .collect();
        Self {
            _td: td,
            meta,
            _data: data,
        }
    }

    fn client(&self) -> Client {
        let endpoints = self
            .meta
            .iter()
            .map(|n| format!("http://{}", n.http_addr()))
            .collect();
        let mut cfg = ClientConfig::new(endpoints);
        cfg.token = Some(TOKEN.into());
        cfg.retry_for = Duration::from_secs(20);
        Client::new(cfg).unwrap()
    }

    fn create_fs(&self, id: &str) {
        self.client()
            .json(
                Method::POST,
                "/v1/fs",
                Body::Json(json!({ "id": id, "name": id })),
                Retry::Idempotent,
            )
            .unwrap();
    }

    /// The test nodes accept at most 1 MiB per request.
    fn ops(&self, fs: &str) -> Ops {
        Ops::new(
            self.client(),
            fs,
            OpsConfig {
                max_io_bytes: 1 << 20,
                cache_leases: true,
                readahead_bytes: 0,
                ..OpsConfig::default()
            },
        )
    }

    fn bucket(&self, name: &str, fs: &str, read_only: bool) -> Bucket {
        Bucket {
            name: name.into(),
            ops: self.ops(fs),
            read_only,
            uid: 0,
            gid: 0,
        }
    }
}

/// Serves `gateway` on a local port until the runtime ends; with `auth`, the two test keys.
fn serve(rt: &tokio::runtime::Runtime, gateway: Gateway, auth: bool) -> SocketAddr {
    let mut b = S3ServiceBuilder::new(gateway);
    if auth {
        let mut a = SimpleAuth::new();
        a.register(ADMIN.0.into(), ADMIN.1.into());
        a.register(READER.0.into(), READER.1.into());
        b.set_auth(a);
    }
    let service = b.build();
    let listener = bind();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let http = ConnBuilder::new(TokioExecutor::new());
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let conn = http
                .serve_connection(TokioIo::new(socket), service.clone())
                .into_owned();
            tokio::spawn(async move {
                let _ = conn.await;
            });
        }
    });
    addr
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn target(addr: SocketAddr, bucket: &str, (ak, sk): (&str, &str)) -> S3Target {
    S3Target::new(&format!("http://{addr}"), "us-east-1", bucket, ak, sk).unwrap()
}

/// Reads `path` (`a/b/c`) through the operations layer, as a FUSE or NFS client sees it.
fn read_path(ops: &Ops, path: &str) -> Vec<u8> {
    let mut ino = ROOT_INO;
    for part in path.split('/') {
        ino = ops.lookup(ino, part).unwrap().ino;
    }
    let size = ops.getattr(ino).unwrap().size;
    ops.read(ino, 0, size as usize).unwrap()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The text of every `<tag>` element (no nesting of the same tag).
fn tags(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    xml.split(&open)
        .skip(1)
        .filter_map(|s| s.split_once(&close).map(|(v, _)| v.to_string()))
        .collect()
}

/// Keys and common prefixes of every page of a ListObjectsV2 listing.
async fn list_all(
    http: &reqwest::Client,
    addr: SocketAddr,
    bucket: &str,
    query: &str,
    page: usize,
) -> (Vec<String>, Vec<String>) {
    let (mut keys, mut prefixes) = (Vec::new(), Vec::new());
    let mut token: Option<String> = None;
    loop {
        let mut url = format!("http://{addr}/{bucket}?list-type=2&max-keys={page}&{query}");
        if let Some(t) = &token {
            url.push_str(&format!("&continuation-token={t}"));
        }
        let xml = http.get(&url).send().await.unwrap().text().await.unwrap();
        assert!(xml.contains("<ListBucketResult"), "{xml}");
        keys.extend(tags(&xml, "Key"));
        for cp in tags(&xml, "CommonPrefixes") {
            prefixes.extend(tags(&cp, "Prefix"));
        }
        token = tags(&xml, "NextContinuationToken").pop();
        if token.is_none() {
            assert!(xml.contains("<IsTruncated>false</IsTruncated>"), "{xml}");
            return (keys, prefixes);
        }
    }
}

#[test]
fn signed_objects_round_trip_and_share_the_filesystem() {
    let c = Cluster::start();
    c.create_fs("share");
    c.create_fs("other");
    let grants = HashMap::from([
        (ADMIN.0.to_string(), Grant::default()),
        (
            READER.0.to_string(),
            Grant {
                buckets: Some(["share".to_string()].into()),
                read_only: true,
            },
        ),
    ]);
    let gateway = Gateway::new(
        vec![
            c.bucket("share", "share", false),
            c.bucket("other", "other", false),
        ],
        Some(grants),
    );
    let _keep = gateway.clone();
    let ops = c.ops("share");
    let rt = runtime();
    let addr = serve(&rt, gateway, true);
    rt.block_on(async {
        let admin = target(addr, "share", ADMIN);
        let small = pattern(300_000, 7);
        admin
            .put_object("docs/2026/report.bin", small.clone())
            .await
            .unwrap();
        assert_eq!(
            admin.get_object("docs/2026/report.bin").await.unwrap(),
            small
        );

        // 12 MiB in 5 MiB parts: assembled from three parts.
        let big = pattern(12 << 20, 3);
        let (n, _) = admin
            .put_multipart_streaming("models/ckpt.bin", 5 << 20, &big[..])
            .await
            .unwrap();
        assert_eq!(n, big.len() as u64);
        assert_eq!(admin.get_object("models/ckpt.bin").await.unwrap(), big);

        let listed = admin.list_objects(None).await.unwrap();
        assert_eq!(
            listed,
            vec![
                ("docs/2026/report.bin".to_string(), small.len() as u64),
                ("models/ckpt.bin".to_string(), big.len() as u64)
            ]
        );

        // Read-only key: reads its bucket, cannot write it, cannot see the other bucket.
        let reader = target(addr, "share", READER);
        assert_eq!(
            reader.get_object("docs/2026/report.bin").await.unwrap(),
            small
        );
        assert!(reader.put_object("nope", b"x".to_vec()).await.is_err());
        assert!(target(addr, "other", READER)
            .list_objects(None)
            .await
            .is_err());
        assert!(target(addr, "share", ("admin", "wrong-secret-0123456789"))
            .get_object("models/ckpt.bin")
            .await
            .is_err());
        let unsigned = reqwest::get(format!("http://{addr}/share/models/ckpt.bin"))
            .await
            .unwrap();
        assert_eq!(unsigned.status(), 403);

        admin.delete_object("docs/2026/report.bin").await.unwrap();
        assert!(admin.get_object("docs/2026/report.bin").await.is_err());
    });
    drop(rt);
    // The same bytes through the filesystem, and the emptied `docs/2026/` directories are gone.
    assert_eq!(read_path(&ops, "models/ckpt.bin"), pattern(12 << 20, 3));
    assert_eq!(ops.lookup(ROOT_INO, "docs").unwrap_err(), libc::ENOENT);
    let names: Vec<String> = ops
        .readdir(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(
        names.iter().all(|n| !n.starts_with(".atlas_s3_tmp_")),
        "{names:?}"
    );
}

#[test]
fn listings_follow_key_order_delimiters_and_pages() {
    let c = Cluster::start();
    c.create_fs("list");
    let gateway = Gateway::new(vec![c.bucket("list", "list", false)], None);
    let _keep = gateway.clone();
    let rt = runtime();
    let addr = serve(&rt, gateway, false);
    rt.block_on(async {
        let http = reqwest::Client::new();
        let keys = ["a-b", "a/b/c", "a/b/d", "a/e", "a0", "b/", "b/x", "c d+e"];
        for k in keys {
            let body = if k.ends_with('/') {
                Vec::new()
            } else {
                k.as_bytes().to_vec()
            };
            let r = http
                .put(format!("http://{addr}/list/{k}"))
                .body(body)
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "{k}: {}", r.text().await.unwrap());
        }
        // Lexicographic key order ("a-b" < "a/..." < "a0"), the marker "b/" included, at every
        // page size.
        for page in [1, 2, 3, 1000] {
            let (k, p) = list_all(&http, addr, "list", "", page).await;
            assert_eq!(k, keys.map(String::from), "page {page}");
            assert!(p.is_empty());
        }
        for page in [1, 2, 1000] {
            let (k, p) = list_all(&http, addr, "list", "delimiter=/", page).await;
            assert_eq!(k, ["a-b", "a0", "c d+e"], "page {page}");
            assert_eq!(p, ["a/", "b/"], "page {page}");
        }
        let (k, p) = list_all(&http, addr, "list", "prefix=a/&delimiter=/", 1000).await;
        assert_eq!((k, p), (vec!["a/e".to_string()], vec!["a/b/".to_string()]));
        let (k, _) = list_all(&http, addr, "list", "prefix=a/b", 1000).await;
        assert_eq!(k, ["a/b/c", "a/b/d"]);
        let (k, _) = list_all(&http, addr, "list", "prefix=b/&delimiter=/", 1000).await;
        assert_eq!(k, ["b/", "b/x"]);
        let (k, p) = list_all(&http, addr, "list", "delimiter=-", 1000).await;
        assert_eq!(p, ["a-"]);
        assert!(!k.contains(&"a-b".to_string()) && k.contains(&"a/b/c".to_string()));
        let (k, _) = list_all(&http, addr, "list", "start-after=a/e", 1000).await;
        assert_eq!(k, ["a0", "b/", "b/x", "c d+e"]);
        let (k, _) = list_all(&http, addr, "list", "prefix=missing/", 1000).await;
        assert!(k.is_empty());

        let xml = http
            .get(format!(
                "http://{addr}/list?list-type=2&encoding-type=url&prefix=c"
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(tags(&xml, "Key"), ["c%20d%2Be"]);

        // ListObjects (v1) with a marker.
        let xml = http
            .get(format!("http://{addr}/list?max-keys=2&marker=a/b/d"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(tags(&xml, "Key"), ["a/e", "a0"]);
        assert!(xml.contains("<IsTruncated>true</IsTruncated>"), "{xml}");
    });
    drop(rt);
}

#[test]
fn objects_carry_headers_ranges_copies_and_refuse_what_a_filesystem_cannot_hold() {
    let c = Cluster::start();
    c.create_fs("objs");
    let gateway = Gateway::new(vec![c.bucket("objs", "objs", false)], None);
    let _keep = gateway.clone();
    let ops = c.ops("objs");
    let rt = runtime();
    let addr = serve(&rt, gateway, false);
    let url = |k: &str| format!("http://{addr}/objs/{k}");
    rt.block_on(async {
        let http = reqwest::Client::new();
        let body = pattern(100_000, 1);
        let r = http
            .put(url("img/cat.png"))
            .header("content-type", "image/png")
            .header("x-amz-meta-color", "orange")
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let etag = r.headers()["etag"].to_str().unwrap().to_string();

        let h = http.head(url("img/cat.png")).send().await.unwrap();
        assert_eq!(h.headers()["content-type"], "image/png");
        assert_eq!(h.headers()["x-amz-meta-color"], "orange");
        assert_eq!(h.headers()["content-length"], "100000");
        assert_eq!(h.headers()["etag"].to_str().unwrap(), etag);

        let r = http.get(url("img/cat.png")).header("range", "bytes=10-19").send().await.unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.bytes().await.unwrap().to_vec(), body[10..20].to_vec());
        let r = http.get(url("img/cat.png")).header("range", "bytes=-5").send().await.unwrap();
        assert_eq!(r.bytes().await.unwrap().to_vec(), body[body.len() - 5..].to_vec());

        // Conditional reads and copies.
        let status = |req: reqwest::RequestBuilder| async move { req.send().await.unwrap().status() };
        assert_eq!(status(http.get(url("img/cat.png")).header("if-none-match", &etag)).await, 304);
        assert_eq!(status(http.head(url("img/cat.png")).header("if-none-match", "\"x\"")).await, 200);
        assert_eq!(status(http.get(url("img/cat.png")).header("if-match", "\"x\"")).await, 412);
        assert_eq!(status(http.get(url("img/cat.png")).header("if-match", &etag)).await, 200);
        let future = "Fri, 01 Jan 2100 00:00:00 GMT";
        let past = "Mon, 01 Jan 2001 00:00:00 GMT";
        assert_eq!(status(http.get(url("img/cat.png")).header("if-modified-since", future)).await, 304);
        assert_eq!(status(http.get(url("img/cat.png")).header("if-unmodified-since", past)).await, 412);
        let copy = |cond: &'static str, v: String| {
            http.put(url("img/kitten.png"))
                .header("x-amz-copy-source", "/objs/img/cat.png")
                .header(cond, v)
        };
        assert_eq!(status(copy("x-amz-copy-source-if-match", "\"x\"".into())).await, 412);
        assert_eq!(status(copy("x-amz-copy-source-if-none-match", etag.clone())).await, 412);
        assert_eq!(http.head(url("img/kitten.png")).send().await.unwrap().status(), 404);

        // Content-MD5 is checked: base64 of a wrong digest is refused and nothing is stored.
        let r = http
            .put(url("bad-md5"))
            .header("content-md5", "AAAAAAAAAAAAAAAAAAAAAA==")
            .body("data")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(http.head(url("bad-md5")).send().await.unwrap().status(), 404);

        // Copy to a new key keeps the headers; a copy onto itself replaces them.
        let r = http
            .put(url("backup/cat.png"))
            .header("x-amz-copy-source", "/objs/img/cat.png")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        assert_eq!(http.get(url("backup/cat.png")).send().await.unwrap().bytes().await.unwrap().to_vec(), body);
        let r = http
            .put(url("img/cat.png"))
            .header("x-amz-copy-source", "/objs/img/cat.png")
            .header("x-amz-metadata-directive", "REPLACE")
            .header("x-amz-meta-color", "black")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let h = http.head(url("img/cat.png")).send().await.unwrap();
        assert_eq!(h.headers()["x-amz-meta-color"], "black");
        assert_eq!(h.headers()["etag"].to_str().unwrap(), etag);

        // A key below an object, an object over a directory, and keys with no file path.
        assert_eq!(http.put(url("img/cat.png/whiskers")).body("x").send().await.unwrap().status(), 409);
        assert_eq!(http.put(url("img")).body("x").send().await.unwrap().status(), 409);
        for bad in ["a//b", "y/.atlas_hidden_1", ".atlas_s3_uploads/z"] {
            let s = http.put(url(bad)).body("x").send().await.unwrap().status();
            assert_eq!(s, 400, "{bad}");
        }
        assert_eq!(http.get(url("missing")).send().await.unwrap().status(), 404);
        assert_eq!(http.get(url("img")).send().await.unwrap().status(), 404);
        assert_eq!(http.put(format!("http://{addr}/newbucket")).send().await.unwrap().status(), 501);
        assert_eq!(http.get(format!("http://{addr}/nobucket?list-type=2")).send().await.unwrap().status(), 404);

        // Batch delete.
        let xml = "<Delete><Object><Key>img/cat.png</Key></Object><Object><Key>never</Key></Object></Delete>";
        let r = http
            .post(format!("http://{addr}/objs?delete"))
            .body(xml)
            .send()
            .await
            .unwrap();
        let text = r.text().await.unwrap();
        assert_eq!(tags(&text, "Key"), ["img/cat.png", "never"], "{text}");
        assert_eq!(http.head(url("img/cat.png")).send().await.unwrap().status(), 404);
    });
    // `img/` emptied and pruned; `backup/` still holds its copy.
    assert_eq!(ops.lookup(ROOT_INO, "img").unwrap_err(), libc::ENOENT);
    assert_eq!(read_path(&ops, "backup/cat.png"), pattern(100_000, 1));
    drop(rt);
}

#[test]
fn files_written_through_the_filesystem_are_objects() {
    let c = Cluster::start();
    c.create_fs("mixed");
    let gateway = Gateway::new(
        vec![
            c.bucket("mixed", "mixed", false),
            c.bucket("frozen", "mixed", true),
        ],
        None,
    );
    let _keep = gateway.clone();
    let ops = c.ops("mixed");
    let dir = ops
        .mknode(ROOT_INO, "nfs", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    let f = ops
        .mknode(dir.ino, "report.txt", NodeType::File, None, 0o644, 0, 0)
        .unwrap();
    ops.write(f.ino, 0, b"written over NFS").unwrap();
    ops.flush(f.ino).unwrap();
    let rt = runtime();
    let addr = serve(&rt, gateway, false);
    let first = rt.block_on(async {
        let r = reqwest::get(format!("http://{addr}/mixed/nfs/report.txt"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let etag = r.headers()["etag"].to_str().unwrap().to_string();
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"written over NFS");
        etag
    });
    // A change through the filesystem changes the ETag.
    ops.write(f.ino, 0, b"WRITTEN").unwrap();
    ops.flush(f.ino).unwrap();
    rt.block_on(async {
        let http = reqwest::Client::new();
        let r = http
            .get(format!("http://{addr}/mixed/nfs/report.txt"))
            .send()
            .await
            .unwrap();
        assert_ne!(r.headers()["etag"].to_str().unwrap(), first);
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"WRITTEN over NFS");
        // A read-only bucket of the same filesystem serves it but refuses writes.
        let r = http
            .get(format!("http://{addr}/frozen/nfs/report.txt"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = http
            .put(format!("http://{addr}/frozen/x"))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    });
    drop(rt);
    // The bucket's creation date is stamped once and doesn't move with the top directory.
    let probe = c.bucket("mixed", "mixed", false);
    probe.stamp_created().unwrap();
    let created = probe.created_ns().unwrap();
    probe.stamp_created().unwrap();
    ops.mknode(ROOT_INO, "later", NodeType::Dir, None, 0o755, 0, 0)
        .unwrap();
    assert_eq!(probe.created_ns().unwrap(), created);
    assert_ne!(ops.getattr(ROOT_INO).unwrap().ctime_ns, created);
}

#[test]
fn retention_locked_objects_refuse_delete_and_overwrite_and_only_extend() {
    let c = Cluster::start();
    c.create_fs("vault");
    let gateway = Gateway::new(vec![c.bucket("vault", "vault", false)], None);
    let _keep = gateway.clone();
    let rt = runtime();
    let addr = serve(&rt, gateway, false);
    let url = format!("http://{addr}/vault/ledger/2026.csv");
    let retention = |secs_from_now: i64, mode: &str| {
        let until = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now);
        let until = until
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        format!(
            "<Retention xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Mode>{mode}</Mode>\
             <RetainUntilDate>{until}</RetainUntilDate></Retention>"
        )
    };
    rt.block_on(async {
        let http = reqwest::Client::new();
        let put = |body: &'static str| http.put(&url).body(body);
        assert!(put("v1").send().await.unwrap().status().is_success());

        // No retention yet.
        let r = http.get(format!("{url}?retention")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 404);

        // GOVERNANCE is refused (it could be bypassed); COMPLIANCE is accepted.
        let r = http
            .put(format!("{url}?retention"))
            .body(retention(3600, "GOVERNANCE"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{}", r.text().await.unwrap());
        let r = http
            .put(format!("{url}?retention"))
            .body(retention(3600, "COMPLIANCE"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        let r = http.get(format!("{url}?retention")).send().await.unwrap();
        let xml = r.text().await.unwrap();
        assert!(xml.contains("COMPLIANCE"), "{xml}");

        // Locked: neither delete nor overwrite works, and the content is unchanged.
        assert_eq!(http.delete(&url).send().await.unwrap().status().as_u16(), 403);
        assert_eq!(put("v2").send().await.unwrap().status().as_u16(), 403);
        assert_eq!(http.get(&url).send().await.unwrap().text().await.unwrap(), "v1");

        // A lock can be extended but never shortened.
        let r = http
            .put(format!("{url}?retention"))
            .body(retention(60, "COMPLIANCE"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        let r = http
            .put(format!("{url}?retention"))
            .body(retention(7200, "COMPLIANCE"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        assert_eq!(http.delete(&url).send().await.unwrap().status().as_u16(), 403);

        // Other keys are unaffected.
        let other = format!("http://{addr}/vault/free.txt");
        assert!(http.put(&other).body("x").send().await.unwrap().status().is_success());
        assert!(http.delete(&other).send().await.unwrap().status().is_success());
    });
}
