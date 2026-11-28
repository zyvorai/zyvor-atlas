// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

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
            grant_key_file: None,
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
                    cross_group_dedup: false,
                    concurrent_commits: None,
                    delta_writes: Default::default(),
                    rebuild_delay_secs: 60,
                    rebuild_bytes_per_sec: 0,
                    scrub_bytes_per_sec: 0,
                    tiering: None,
                    extent_bytes: 64 << 10,
                    tick_ms: 10,
                    proposal_timeout_ms: 3000,
                    parallel_log_sync: None,
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
        assert_eq!(
            http.delete(&url).send().await.unwrap().status().as_u16(),
            403
        );
        assert_eq!(put("v2").send().await.unwrap().status().as_u16(), 403);
        assert_eq!(
            http.get(&url).send().await.unwrap().text().await.unwrap(),
            "v1"
        );

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
        assert_eq!(
            http.delete(&url).send().await.unwrap().status().as_u16(),
            403
        );

        // Other keys are unaffected.
        let other = format!("http://{addr}/vault/free.txt");
        assert!(http
            .put(&other)
            .body("x")
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        assert!(http
            .delete(&other)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    });
}

/// `n` gateways of their own (clients, caches and sessions) serving filesystem `fs`.
fn gateways(c: &Cluster, rt: &tokio::runtime::Runtime, fs: &str, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let g = Gateway::new(vec![c.bucket(fs, fs, false)], None);
            format!("http://{}/{fs}", serve(rt, g, false))
        })
        .collect()
}

/// Status, ETag and body of a request.
async fn send(req: reqwest::RequestBuilder) -> (u16, String, String) {
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let etag = r
        .headers()
        .get("etag")
        .map_or_else(String::new, |v| v.to_str().unwrap().to_string());
    (status, etag, r.text().await.unwrap())
}

async fn status(req: reqwest::RequestBuilder) -> u16 {
    send(req).await.0
}

/// The current content and ETag. A read can fail while its object is being replaced (the old
/// file is gone before it is read), so it is repeated.
async fn current(http: &reqwest::Client, url: &str) -> (String, String) {
    for _ in 0..200 {
        if let Ok(r) = http.get(url).send().await {
            let etag = r.headers().get("etag").cloned();
            if let (200, Some(etag)) = (r.status().as_u16(), etag) {
                if let Ok(body) = r.text().await {
                    return (etag.to_str().unwrap().to_string(), body);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{url} could not be read");
}

/// Waits out the time until the cluster's leader has heard every metadata node, before which
/// conditional writes are refused as unavailable.
async fn conditions_available(http: &reqwest::Client, bucket_url: &str) {
    for _ in 0..200 {
        let req = http
            .put(format!("{bucket_url}/warm"))
            .header("if-none-match", "*");
        if status(req.body("")).await != 503 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("conditional writes never became available");
}

/// Starts a multipart upload of `url` and uploads `body` as its only part. Returns the upload id
/// and the request body that completes it.
async fn start_upload(
    http: &reqwest::Client,
    url: &str,
    tagging: Option<&str>,
    body: &str,
) -> (String, String) {
    let mut req = http.post(format!("{url}?uploads"));
    if let Some(t) = tagging {
        req = req.header("x-amz-tagging", t);
    }
    let (s, _, xml) = send(req).await;
    assert_eq!(s, 200, "{xml}");
    let id = tags(&xml, "UploadId").pop().unwrap();
    let part = http
        .put(format!("{url}?partNumber=1&uploadId={id}"))
        .body(body.to_string());
    let (s, etag, text) = send(part).await;
    assert_eq!(s, 200, "{text}");
    let done = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part>\
         </CompleteMultipartUpload>"
    );
    (id, done)
}

fn complete(
    http: &reqwest::Client,
    url: &str,
    (id, body): &(String, String),
) -> reqwest::RequestBuilder {
    http.post(format!("{url}?uploadId={id}")).body(body.clone())
}

/// Whether a multipart completion succeeded: a failure after the copy started is a 200 too.
fn completed((status, _, body): &(u16, String, String)) -> bool {
    *status == 200 && body.contains("<CompleteMultipartUploadResult")
}

fn no_temporary_files(ops: &Ops) {
    let mut dirs = vec![ROOT_INO];
    while let Some(dir) = dirs.pop() {
        for e in ops.readdir(dir).unwrap() {
            assert!(!e.name.starts_with(".atlas_s3_tmp_"), "{}", e.name);
            if e.kind == NodeType::Dir {
                dirs.push(e.ino);
            }
        }
    }
}

#[test]
fn concurrent_conditional_writes_through_two_gateways_have_one_winner() {
    let c = Cluster::start();
    c.create_fs("cas");
    let rt = runtime();
    let g = gateways(&c, &rt, "cas", 2);
    let http = reqwest::Client::new();
    rt.block_on(async {
        conditions_available(&http, &g[0]).await;

        for round in 0..8 {
            let writers: Vec<_> = (0..8)
                .map(|i| {
                    let url = format!("{}/lock/{round}", g[i % 2]);
                    let req = http.put(url).header("if-none-match", "*");
                    tokio::spawn(send(req.body(format!("writer {i}"))))
                })
                .collect();
            let mut won = Vec::new();
            for (i, w) in writers.into_iter().enumerate() {
                let (s, _, text) = w.await.unwrap();
                match s {
                    200 => won.push(i),
                    412 => {}
                    _ => panic!("round {round} writer {i}: {s} {text}"),
                }
            }
            assert_eq!(won.len(), 1, "round {round}: {won:?}");
            for base in &g {
                let (_, _, text) = send(http.get(format!("{base}/lock/{round}"))).await;
                assert_eq!(text, format!("writer {}", won[0]));
            }
        }

        // A counter incremented by read, then write if unchanged: no increment is lost.
        const WRITERS: usize = 6;
        const EACH: usize = 5;
        let counter = format!("{}/counter", g[0]);
        assert_eq!(status(http.put(counter).body("0")).await, 200);
        let writers: Vec<_> = (0..WRITERS)
            .map(|i| {
                let (http, url) = (http.clone(), format!("{}/counter", g[i % 2]));
                tokio::spawn(async move {
                    for _ in 0..EACH {
                        let mut tries = 0;
                        loop {
                            tries += 1;
                            assert!(tries < 500, "writer {i} made no progress");
                            // The ETag of the very bytes read.
                            let (etag, text) = current(&http, &url).await;
                            let next = text.parse::<usize>().unwrap() + 1;
                            let put = http.put(&url).header("if-match", etag);
                            let (s, _, text) = send(put.body(next.to_string())).await;
                            match s {
                                200 => break,
                                412 | 409 => {}
                                _ => panic!("{s} {text}"),
                            }
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.await.unwrap();
        }
        for base in &g {
            let (_, _, text) = send(http.get(format!("{base}/counter"))).await;
            assert_eq!(text, (WRITERS * EACH).to_string());
        }

        // Multipart completions of one new key race through both gateways.
        for round in 0..4 {
            let urls = [
                format!("{}/mp/{round}", g[0]),
                format!("{}/mp/{round}", g[1]),
            ];
            let a = start_upload(&http, &urls[0], None, "first").await;
            let b = start_upload(&http, &urls[1], None, "second").await;
            let (ra, rb) = tokio::join!(
                send(complete(&http, &urls[0], &a).header("if-none-match", "*")),
                send(complete(&http, &urls[1], &b).header("if-none-match", "*")),
            );
            assert!(completed(&ra) != completed(&rb), "{ra:?} {rb:?}");
            let (lost, lost_url, lost_upload, content) = if completed(&ra) {
                (rb, &urls[1], &b, "first")
            } else {
                (ra, &urls[0], &a, "second")
            };
            // Judged before the copy (412) or at the publish (in the body of a 200).
            assert!(
                lost.2.contains("<Code>PreconditionFailed</Code>"),
                "{lost:?}"
            );
            let (_, etag, text) = send(http.get(&urls[0])).await;
            assert_eq!(text, content);
            // The loser's upload is intact and can replace the winner by naming its ETag.
            let stale = complete(&http, lost_url, lost_upload).header("if-match", "\"x\"");
            assert_eq!(status(stale).await, 412);
            let r = send(complete(&http, lost_url, lost_upload).header("if-match", etag)).await;
            assert!(completed(&r), "{r:?}");
            assert_ne!(send(http.get(&urls[0])).await.2, content);
        }
    });
    drop(rt);
    no_temporary_files(&c.ops("cas"));
}

#[test]
fn write_conditions_are_applied_or_refused_never_ignored() {
    let c = Cluster::start();
    c.create_fs("cond");
    let rt = runtime();
    let g = gateways(&c, &rt, "cond", 1).remove(0);
    let http = reqwest::Client::new();
    let url = |k: &str| format!("{g}/{k}");
    rt.block_on(async {
        conditions_available(&http, &g).await;
        let put = |k: &str, body: &'static str| http.put(url(k)).body(body);
        let (s, first, _) = send(put("a", "v1")).await;
        assert_eq!(s, 200);
        let on_match = |k: &str, etag: &str, body: &'static str| {
            put(k, body).header("if-match", etag.to_string())
        };

        assert_eq!(status(on_match("nope/x", &first, "v")).await, 404);
        assert_eq!(status(on_match("a", "\"x\"", "v2")).await, 412);
        assert_eq!(
            status(on_match("a", &format!("W/{first}"), "v2")).await,
            412
        );
        // Unquoted ETags are accepted, as some SDKs send them.
        let (s, second, _) = send(on_match("a", first.trim_matches('"'), "v2")).await;
        assert_eq!(s, 200);
        assert_eq!(status(on_match("a", &first, "v3")).await, 412);
        assert_eq!(
            status(put("a", "v3").header("if-none-match", "*")).await,
            412
        );
        for (name, value) in [("if-none-match", second.as_str()), ("if-match", "*")] {
            assert_eq!(status(put("a", "v3").header(name, value)).await, 501);
        }
        let both = put("b", "v")
            .header("if-match", &second)
            .header("if-none-match", "*");
        assert_eq!(status(both).await, 501);
        assert_eq!(status(http.head(url("b"))).await, 404);
        assert_eq!(send(http.get(url("a"))).await.2, "v2");

        // Copies take the same conditions on their destination.
        let copy = |to: &str| http.put(url(to)).header("x-amz-copy-source", "/cond/a");
        assert_eq!(status(put("old", "old")).await, 200);
        assert_eq!(status(copy("old").header("if-none-match", "*")).await, 412);
        assert_eq!(status(copy("old").header("if-match", "\"x\"")).await, 412);
        assert_eq!(send(http.get(url("old"))).await.2, "old");
        assert_eq!(status(copy("new").header("if-none-match", "*")).await, 200);
        assert_eq!(status(copy("old").header("if-match", "*")).await, 501);
        let onto_self = copy("a")
            .header("x-amz-metadata-directive", "REPLACE")
            .header("if-match", &second);
        assert_eq!(status(onto_self).await, 501);

        // Multipart completion: judged before the parts are copied.
        let upload = start_upload(&http, &url("a"), None, "parts").await;
        let done = |name: &'static str, v: &str| {
            complete(&http, &url("a"), &upload).header(name, v.to_string())
        };
        assert_eq!(status(done("if-none-match", "*")).await, 412);
        assert_eq!(status(done("if-match", &first)).await, 412);
        assert_eq!(status(done("if-none-match", &second)).await, 501);
        let missing = start_upload(&http, &url("missing"), None, "parts").await;
        let r = complete(&http, &url("missing"), &missing).header("if-match", &second);
        assert_eq!(status(r).await, 404);
        let r = send(done("if-match", &second)).await;
        assert!(completed(&r), "{r:?}");
        assert_eq!(send(http.get(url("a"))).await.2, "parts");

        // Conditional deletes are not supported: refused, and nothing deleted.
        let r = http.delete(url("a")).header("if-match", "\"x\"");
        assert_eq!(status(r).await, 501);
        let batch = "<Delete><Object><Key>old</Key></Object><Object><Key>a</Key>\
                     <ETag>\"x\"</ETag></Object></Delete>";
        let r = http.post(format!("{g}?delete")).body(batch);
        assert_eq!(status(r).await, 501);
        assert_eq!(status(http.head(url("a"))).await, 200);
        assert_eq!(status(http.head(url("old"))).await, 200);

        // Directory markers.
        let marker = || put("dir/", "");
        assert_eq!(status(marker().header("if-match", &second)).await, 404);
        assert_eq!(status(marker().header("if-none-match", "*")).await, 200);
        assert_eq!(status(marker().header("if-none-match", "*")).await, 412);
        assert_eq!(status(marker().header("if-match", "\"x\"")).await, 412);
        let empty = "\"d41d8cd98f00b204e9800998ecf8427e\"";
        assert_eq!(status(marker().header("if-match", empty)).await, 200);
    });
    drop(rt);
    let ops = c.ops("cond");
    // A refused `If-Match` on a missing key made no directories.
    assert_eq!(ops.lookup(ROOT_INO, "nope").unwrap_err(), libc::ENOENT);
    no_temporary_files(&ops);
}

#[test]
fn objects_carry_tags_within_aws_limits() {
    let c = Cluster::start();
    c.create_fs("tags");
    let gateway = Gateway::new(
        vec![
            c.bucket("tags", "tags", false),
            c.bucket("frozen", "tags", true),
        ],
        None,
    );
    let rt = runtime();
    let addr = serve(&rt, gateway, false);
    let http = reqwest::Client::new();
    let url = |k: &str| format!("http://{addr}/tags/{k}");
    let xml = |pairs: &[(&str, &str)]| {
        let tags: String = pairs
            .iter()
            .map(|(k, v)| format!("<Tag><Key>{k}</Key><Value>{v}</Value></Tag>"))
            .collect();
        format!("<Tagging><TagSet>{tags}</TagSet></Tagging>")
    };
    let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    rt.block_on(async {
        let read = |key: &str| {
            let req = http.get(format!("{}?tagging", url(key)));
            async move {
                let (s, _, text) = send(req).await;
                assert_eq!(s, 200, "{text}");
                tags(&text, "Key")
                    .into_iter()
                    .zip(tags(&text, "Value"))
                    .collect::<Vec<_>>()
            }
        };
        let tagged = |k: &str, header: &str| {
            http.put(url(k))
                .header("x-amz-tagging", header.to_string())
                .body("data")
        };
        let put_tags = |k: &str, body: String| http.put(format!("{}?tagging", url(k))).body(body);

        let (s, etag, text) = send(tagged("a", "team=ml&note=x+y%2Bz&empty=")).await;
        assert_eq!(s, 200, "{text}");
        assert_eq!(
            read("a").await,
            pairs(&[("team", "ml"), ("note", "x y+z"), ("empty", "")])
        );

        // Replacing and removing tags changes neither the object nor its ETag.
        assert_eq!(status(put_tags("a", xml(&[("k", "v")]))).await, 200);
        assert_eq!(read("a").await, pairs(&[("k", "v")]));
        let (_, now, text) = send(http.get(url("a"))).await;
        assert_eq!((now, text.as_str()), (etag.clone(), "data"));
        for _ in 0..2 {
            let r = http.delete(format!("{}?tagging", url("a")));
            assert_eq!(status(r).await, 204);
            assert!(read("a").await.is_empty());
        }

        // Limits: refused, and nothing changes.
        assert_eq!(status(put_tags("a", xml(&[("keep", "1")]))).await, 200);
        let keys: Vec<String> = (0..11).map(|i| format!("k{i}")).collect();
        let header: Vec<String> = keys.iter().map(|k| format!("{k}=v")).collect();
        let long_key = "k".repeat(129);
        let long_value = "v".repeat(257);
        for bad in [
            header.join("&"),
            "aws:x=1".to_string(),
            "a=1&a=2".to_string(),
            format!("{long_key}=1"),
            format!("k={long_value}"),
            "k=a*b".to_string(),
        ] {
            let (s, _, text) = send(tagged("refused", &bad)).await;
            assert_eq!(s, 400, "{bad}: {text}");
            assert!(text.contains("InvalidTag"), "{text}");
        }
        assert_eq!(status(http.head(url("refused"))).await, 404);
        let eleven: Vec<(&str, &str)> = keys.iter().map(|k| (k.as_str(), "v")).collect();
        for bad in [
            xml(&eleven),
            xml(&[("a", "1"), ("a", "2")]),
            xml(&[("", "1")]),
        ] {
            assert_eq!(status(put_tags("a", bad)).await, 400);
        }
        assert_eq!(read("a").await, pairs(&[("keep", "1")]));
        assert_eq!(status(tagged("ten", &header[..10].join("&"))).await, 200);
        assert_eq!(read("ten").await.len(), 10);

        let r = http.get(format!("{}?tagging", url("none")));
        assert_eq!(status(r).await, 404);
        assert_eq!(status(put_tags("none", xml(&[]))).await, 404);
        let r = http.get(format!("{}?tagging&versionId=abc", url("a")));
        assert_eq!(status(r).await, 501);
        // A read-only bucket serves tags but doesn't change them.
        let frozen = format!("http://{addr}/frozen/a?tagging");
        assert_eq!(status(http.get(&frozen)).await, 200);
        assert_eq!(status(http.put(&frozen).body(xml(&[]))).await, 403);
        assert_eq!(status(http.delete(&frozen)).await, 403);

        // Copies keep the source's tags unless told to replace them.
        let copy = |from: &str, to: &str| {
            http.put(url(to))
                .header("x-amz-copy-source", format!("/tags/{from}"))
        };
        assert_eq!(status(copy("a", "kept")).await, 200);
        assert_eq!(read("kept").await, pairs(&[("keep", "1")]));
        let ignored = copy("a", "ignored").header("x-amz-tagging", "other=1");
        assert_eq!(status(ignored).await, 200);
        assert_eq!(read("ignored").await, pairs(&[("keep", "1")]));
        let replaced = copy("a", "replaced")
            .header("x-amz-tagging-directive", "REPLACE")
            .header("x-amz-tagging", "other=2");
        assert_eq!(status(replaced).await, 200);
        assert_eq!(read("replaced").await, pairs(&[("other", "2")]));
        let cleared = copy("a", "cleared").header("x-amz-tagging-directive", "REPLACE");
        assert_eq!(status(cleared).await, 200);
        assert!(read("cleared").await.is_empty());
        let onto_self = || copy("a", "a").header("x-amz-metadata-directive", "REPLACE");
        assert_eq!(status(onto_self()).await, 200);
        assert_eq!(read("a").await, pairs(&[("keep", "1")]));
        let retag = onto_self()
            .header("x-amz-tagging-directive", "REPLACE")
            .header("x-amz-tagging", "self=1");
        assert_eq!(status(retag).await, 200);
        assert_eq!(read("a").await, pairs(&[("self", "1")]));

        // An overwrite replaces the tags with its own, here none.
        assert_eq!(status(http.put(url("a")).body("new")).await, 200);
        assert!(read("a").await.is_empty());

        // Multipart: tags given at creation arrive with the object.
        let upload = start_upload(&http, &url("big"), Some("mp=1"), "parts").await;
        let r = send(complete(&http, &url("big"), &upload)).await;
        assert!(completed(&r), "{r:?}");
        assert_eq!(read("big").await, pairs(&[("mp", "1")]));

        // A directory marker's tags go with it, though its directory stays.
        assert_eq!(status(tagged("m/", "m=1").body("")).await, 200);
        assert_eq!(read("m/").await, pairs(&[("m", "1")]));
        assert_eq!(status(http.put(url("m/child")).body("x")).await, 200);
        assert_eq!(status(http.delete(url("m/"))).await, 204);
        assert_eq!(status(put_tags("m/", xml(&[]))).await, 404);
        assert_eq!(status(http.put(url("m/")).body("")).await, 200);
        assert!(read("m/").await.is_empty());
    });
    drop(rt);
}

/// A `COMPLIANCE` retention document ending an hour from now.
fn locked_for_an_hour() -> String {
    let until = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let until = until
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    format!(
        "<Retention><Mode>COMPLIANCE</Mode><RetainUntilDate>{until}</RetainUntilDate></Retention>"
    )
}

#[test]
fn retention_holds_against_conditional_and_concurrent_writes() {
    let c = Cluster::start();
    c.create_fs("worm");
    let rt = runtime();
    let g = gateways(&c, &rt, "worm", 2);
    let http = reqwest::Client::new();
    let url = |k: &str| format!("{}/{k}", g[0]);
    rt.block_on(async {
        conditions_available(&http, &g[0]).await;
        let lock = |url: String| {
            http.put(format!("{url}?retention"))
                .body(locked_for_an_hour())
        };
        let (s, etag, _) = send(http.put(url("a")).body("v1")).await;
        assert_eq!(s, 200);
        let upload = start_upload(&http, &url("a"), None, "parts").await;
        assert_eq!(status(lock(url("a"))).await, 200);
        let put = || http.put(url("a")).body("v2");
        assert_eq!(status(put().header("if-none-match", "*")).await, 412);
        assert_eq!(status(put().header("if-match", &etag)).await, 403);
        let (s, _, text) = send(complete(&http, &url("a"), &upload)).await;
        assert!(text.contains("<Code>AccessDenied</Code>"), "{s} {text}");
        let r = complete(&http, &url("a"), &upload).header("if-match", &etag);
        let (s, _, text) = send(r).await;
        assert!(text.contains("<Code>AccessDenied</Code>"), "{s} {text}");
        let copy = || http.put(url("a")).header("x-amz-copy-source", "/worm/a");
        let retag = copy().header("x-amz-metadata-directive", "REPLACE");
        assert_eq!(status(retag).await, 403);
        assert_eq!(status(http.put(url("b")).body("b")).await, 200);
        let over = http.put(url("a")).header("x-amz-copy-source", "/worm/b");
        assert_eq!(status(over).await, 403);
        // Tags are not the object's data: a locked object can be tagged.
        let tag = "<Tagging><TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet></Tagging>";
        let r = http.put(format!("{}?tagging", url("a"))).body(tag);
        assert_eq!(status(r).await, 200);
        assert_eq!(send(http.get(url("a"))).await.2, "v1");

        // A directory marker can be locked too.
        assert_eq!(status(http.put(url("held/")).body("")).await, 200);
        assert_eq!(status(lock(url("held/"))).await, 200);
        assert_eq!(status(http.put(url("held/")).body("")).await, 403);
        assert_eq!(status(http.delete(url("held/"))).await, 403);
        assert_eq!(status(http.head(url("held/"))).await, 200);

        // Once a lock is confirmed, no write racing it through either gateway lands.
        for round in 0..6u64 {
            let key = format!("race/{round}");
            assert_eq!(status(http.put(url(&key)).body("start")).await, 200);
            let writers: Vec<_> = (0..4)
                .map(|i| {
                    let (http, url) = (http.clone(), format!("{}/{key}", g[i % 2]));
                    tokio::spawn(async move {
                        for n in 0..12 {
                            let req = http.put(&url).body(format!("writer {i} write {n}"));
                            let (s, _, text) = send(req).await;
                            match s {
                                200 => {}
                                403 => break,
                                _ => panic!("{s} {text}"),
                            }
                        }
                    })
                })
                .collect();
            tokio::time::sleep(Duration::from_millis(20 * round)).await;
            let target = format!("{}/{key}", g[1]);
            let (s, _, text) = send(lock(target.clone())).await;
            assert_eq!(s, 200, "{text}");
            let held = current(&http, &target).await.1;
            for w in writers {
                w.await.unwrap();
            }
            for base in &g {
                let now = send(http.get(format!("{base}/{key}"))).await.2;
                assert_eq!(now, held, "round {round}: replaced after it was locked");
            }
        }
    });
    drop(rt);
    no_temporary_files(&c.ops("worm"));
}

/// Forwards to one metadata node, but answers a conditional rename as a cluster that can't do
/// it would: `refuse` 404 (a node from before the route) or 409 `unsupported` (not every node
/// known to support it).
fn refusing_proxy(upstream: SocketAddr, refuse: u16) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    fn relay(mut client: TcpStream, upstream: SocketAddr, refuse: u16) -> std::io::Result<()> {
        let mut reader = BufReader::new(client.try_clone()?);
        let (mut head, mut len) = (String::new(), 0usize);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == "\r\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            if !lower.starts_with("connection:") {
                head.push_str(&line);
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body)?;
        if head.lines().next().unwrap_or("").contains("/rename-if") {
            let (reason, text) = match refuse {
                404 => ("Not Found", "no such route"),
                _ => ("Conflict", r#"{"code":"unsupported","error":"not yet"}"#),
            };
            return write!(
                client,
                "HTTP/1.1 {refuse} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
        let mut node = TcpStream::connect(upstream)?;
        write!(node, "{head}Connection: close\r\n\r\n")?;
        node.write_all(&body)?;
        std::io::copy(&mut node, &mut client)?;
        Ok(())
    }
    let listener = bind();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || relay(conn, upstream, refuse));
        }
    });
    format!("http://{addr}")
}

#[test]
fn a_cluster_without_conditional_renames_refuses_conditions_and_keeps_plain_writes() {
    let c = Cluster::start();
    c.create_fs("older");
    let rt = runtime();
    for (refuse, code, name) in [
        (404u16, 501u16, "NotImplemented"),
        (409, 503, "ServiceUnavailable"),
    ] {
        let endpoints = c
            .meta
            .iter()
            .map(|n| refusing_proxy(n.http_addr(), refuse))
            .collect();
        let mut cfg = ClientConfig::new(endpoints);
        cfg.token = Some(TOKEN.into());
        cfg.retry_for = Duration::from_secs(20);
        let ops = Ops::new(
            Client::new(cfg).unwrap(),
            "older",
            OpsConfig {
                max_io_bytes: 1 << 20,
                cache_leases: true,
                readahead_bytes: 0,
                ..OpsConfig::default()
            },
        );
        let bucket = Bucket {
            name: "older".into(),
            ops,
            read_only: false,
            uid: 0,
            gid: 0,
        };
        let addr = serve(&rt, Gateway::new(vec![bucket], None), false);
        let http = reqwest::Client::new();
        let url = |k: &str| format!("http://{addr}/older/{refuse}/{k}");
        rt.block_on(async {
            // Unconditional writes go ahead as before, retention included.
            assert_eq!(status(http.put(url("a")).body("v1")).await, 200);
            let (s, etag, _) = send(http.put(url("a")).body("v2")).await;
            assert_eq!(s, 200);
            // Conditional ones are refused, whether or not they would hold.
            let create = http.put(url("new")).header("if-none-match", "*");
            assert_eq!(status(create.body("v")).await, code);
            assert_eq!(status(http.head(url("new"))).await, 404);
            let replace = http.put(url("a")).header("if-match", &etag);
            assert_eq!(status(replace.body("v3")).await, code);
            assert_eq!(send(http.get(url("a"))).await.2, "v2");
            let upload = start_upload(&http, &url("mp"), None, "parts").await;
            let done = complete(&http, &url("mp"), &upload).header("if-none-match", "*");
            let (_, _, text) = send(done).await;
            assert!(text.contains(&format!("<Code>{name}</Code>")), "{text}");
            assert_eq!(status(http.head(url("mp"))).await, 404);
            let r = send(complete(&http, &url("mp"), &upload)).await;
            assert!(completed(&r), "{r:?}");
            let lock = http
                .put(format!("{}?retention", url("a")))
                .body(locked_for_an_hour());
            assert_eq!(status(lock).await, 200);
            assert_eq!(status(http.put(url("a")).body("v4")).await, 403);
        });
    }
    drop(rt);
    no_temporary_files(&c.ops("older"));
}
