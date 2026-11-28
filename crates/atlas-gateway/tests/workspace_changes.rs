// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! `GET /workspaces/{id}/changes` and the `workspace_changes` MCP tool: tenant isolation, roles and
//! refusals over the fixture driver (which stores no file tree and says so), and the whole path over
//! the real driver against an in-process atlas-native node.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_driver_core::StorageDriver;
use atlas_driver_native::{FakeNativeDriver, HttpApi, HttpApiConfig, RealNativeDriver};
use atlas_gateway::routes;
use atlas_gateway::startup::{attach_native_driver, build_state, BuildOptions, NATIVE_BACKEND_ID};
use atlas_native::node::{
    DataNodeRole, DataNodeSpec, Listeners, MetadataRole, NativeNode, NodeConfig,
};
use reqwest::Method;
use serde_json::{json, Value};

mod common;

const SECRET: &str = "workspace-test-secret-at-least-32-bytes!!";
const NODE_TOKEN: &str = "gateway-test-node-token";
const MIB: u64 = 1 << 20;

async fn spawn(driver: Arc<dyn StorageDriver>) -> SocketAddr {
    let database_url = common::fresh_database_url("workspace_changes").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: SECRET.into(),
        jwt_secret_previous: None,
        auth_required: true,
        bootstrap_admin_token: None,
        admin_username: "admin".into(),
        admin_password: "Admin@321".into(),
        monitor_interval_secs: 0,
        ceph_prometheus_url: None,
        alert_webhook_url: None,
        backup_keep: 0,
        backup_max_age_secs: 0,
        rgw_public_endpoint: None,
        snapshot_tick_secs: 0,
        databridge_reconcile_secs: 0,
        job_poll_secs: 0,
        job_stale_secs: 0,
        https_addr: None,
        tls_cert_path: None,
        tls_key_path: None,
        tls_self_signed: false,
        disable_http: false,
        nfs_enable: false,
        nfs_server: None,
        nfs_exports: Vec::new(),
        nfs_driver_mode: atlas_common::config::DriverMode::Fake,
        zfs_enable: false,
        zfs_host: None,
        zfs_pools: Vec::new(),
        zfs_driver_mode: atlas_common::config::DriverMode::Fake,
        san_enable: false,
        san_server: None,
        san_targets: Vec::new(),
        san_driver_mode: atlas_common::config::DriverMode::Fake,
        cloudblock_enable: false,
        cloudblock_server: None,
        cloudblock_targets: Vec::new(),
        cloudblock_driver_mode: atlas_common::config::DriverMode::Fake,
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let state = build_state(
        config,
        BuildOptions {
            enable_k8s: false,
            initial_discovery: true,
            enable_monitor: false,
        },
    )
    .await
    .expect("build_state");
    attach_native_driver(&state.pool, &state.drivers, driver)
        .await
        .unwrap();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn token(subject: &str, role: &str, tenant: &str) -> String {
    atlas_gateway::auth::mint_token(SECRET, subject, role, tenant, 3600)
        .unwrap()
        .0
}

struct Api {
    base: String,
    c: reqwest::Client,
}

impl Api {
    fn new(addr: SocketAddr) -> Self {
        Self {
            base: format!("http://{addr}/api/atlas/v1"),
            c: reqwest::Client::new(),
        }
    }

    async fn send(&self, tok: &str, m: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .c
            .request(m, format!("{}{path}", self.base))
            .bearer_auth(tok);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, tok: &str, path: &str) -> (u16, Value) {
        self.send(tok, Method::GET, path, None).await
    }

    /// A native volume of `kind` owned by `tenant`.
    async fn volume(&self, admin: &str, tenant: &str, name: &str, kind: &str) -> String {
        let (st, v) = self
            .send(
                admin,
                Method::POST,
                "/volumes",
                Some(json!({
                    "tenant_id": tenant, "name": name, "size_bytes": 8 * MIB, "kind": kind,
                    "kubernetes": { "backend_id": NATIVE_BACKEND_ID }
                })),
            )
            .await;
        assert_eq!(st, 201, "{v}");
        v["volume_id"].as_str().unwrap().to_string()
    }

    /// A workspace of `source`: its id and volume id.
    async fn workspace(&self, tok: &str, source: &str) -> (String, String) {
        let (st, w) = self
            .send(
                tok,
                Method::POST,
                "/workspaces",
                Some(json!({ "source_volume_id": source })),
            )
            .await;
        assert_eq!(st, 201, "{w}");
        (
            w["id"].as_str().unwrap().to_string(),
            w["volume_id"].as_str().unwrap().to_string(),
        )
    }
}

fn message(v: &Value) -> &str {
    v["error"]["message"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn changes_follow_workspace_tenancy_and_the_fixture_invents_nothing() {
    let addr = spawn(Arc::new(FakeNativeDriver::new(NATIVE_BACKEND_ID))).await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let a = token("agent-a", "operator", "tenant-a");
    let a_viewer = token("view-a", "viewer", "tenant-a");
    let b = token("agent-b", "operator", "tenant-b");
    let b_viewer = token("view-b", "viewer", "tenant-b");
    let fs = api.volume(&admin, "tenant-a", "a-files", "filesystem").await;
    let block = api.volume(&admin, "tenant-a", "a-disk", "block").await;
    let (wfs, _) = api.workspace(&a, &fs).await;
    let (wblock, _) = api.workspace(&a, &block).await;
    let path = |w: &str| format!("/workspaces/{w}/changes");

    // The owner (any role) reaches the backend; the fixture refuses rather than make up a listing.
    for tok in [&a, &a_viewer, &admin] {
        let (st, v) = api.get(tok, &path(&wfs)).await;
        assert_eq!(st, 409, "{v}");
        assert!(message(&v).contains("fixture"), "{v}");
        assert!(v.get("changes").is_none() && v.get("total").is_none(), "{v}");
    }
    let (st, v) = api.get(&a, &path(&wblock)).await;
    assert_eq!(st, 400, "{v}");
    assert!(message(&v).contains("no paths"), "{v}");

    // Another tenant gets exactly what an unknown workspace gets, whatever it asks.
    let (st, unknown) = api.get(&b, &path("ws_000000000000")).await;
    assert_eq!(st, 404);
    for tok in [&b, &b_viewer] {
        for p in [
            path(&wfs),
            path(&wblock),
            format!("{}?cursor=abc.1&limit=5", path(&wfs)),
            format!("{}?limit=0", path(&wfs)),
        ] {
            let (st, v) = api.get(tok, &p).await;
            assert_eq!(st, 404, "{p}: {v}");
            assert_eq!(v["error"]["code"], unknown["error"]["code"]);
        }
    }
    for limit in ["0", "10001"] {
        let (st, v) = api.get(&a, &format!("{}?limit={limit}", path(&wfs))).await;
        assert_eq!(st, 400, "{v}");
    }
    let r = reqwest::get(format!("{}{}", api.base, path(&wfs)))
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // Read-only: there is nothing to POST.
    let (st, _) = api.send(&a, Method::POST, &path(&wfs), None).await;
    assert_eq!(st, 405);
}

/// One metadata node and one data node in this process.
struct Node {
    _td: tempfile::TempDir,
    _data: NativeNode,
    meta: NativeNode,
}

impl Node {
    fn start() -> Self {
        let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("token"), NODE_TOKEN).unwrap();
        let base = |id: &str| NodeConfig {
            node_id: id.into(),
            data_dir: td.path().join(id),
            http_listen: "127.0.0.1:0".parse().unwrap(),
            api_token_file: Some(td.path().join("token")),
            api_tokens_file: None,
            tls: None,
            http_tls: None,
            data_node: None,
            metadata: None,
            max_request_bytes: 1 << 20,
            grant_key_file: None,
        };
        let (data_l, meta_l) = (bind(), bind());
        let spec = DataNodeSpec {
            id: "d1".into(),
            addr: data_l.local_addr().unwrap().to_string(),
            zone: None,
            rack: None,
            host: None,
            free_bytes: 1 << 30,
            devices: 1,
        };
        let mut cfg = base("d1");
        cfg.data_node = Some(DataNodeRole {
            listen: data_l.local_addr().unwrap(),
            devices: Vec::new(),
            preallocate: Default::default(),
        });
        let data = NativeNode::start_with(
            cfg,
            Listeners {
                http: Some(bind()),
                data_node: Some(data_l),
                metadata: None,
            },
        )
        .unwrap();
        let mut cfg = base("m1");
        cfg.metadata = Some(MetadataRole {
            listen: meta_l.local_addr().unwrap(),
            peers: BTreeMap::from([("m1".into(), meta_l.local_addr().unwrap().to_string())]),
            bootstrap: None,
            data_nodes: vec![spec],
            replicas: 1,
            erasure: None,
            erasure_min_bytes: 64 << 10,
            compression: false,
            dedup: false,
            cross_group_dedup: false,
            concurrent_commits: None,
            parallel_log_sync: None,
            delta_writes: Default::default(),
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
        });
        let meta = NativeNode::start_with(
            cfg,
            Listeners {
                http: Some(bind()),
                data_node: None,
                metadata: Some(meta_l),
            },
        )
        .unwrap();
        Self {
            _td: td,
            _data: data,
            meta,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.meta.http_addr())
    }

    async fn driver(&self) -> Arc<RealNativeDriver> {
        let api = HttpApi::new(HttpApiConfig {
            endpoints: vec![self.endpoint()],
            token: Some(NODE_TOKEN.into()),
            ca_pem: None,
            identity_pem: None,
            timeout: Duration::from_secs(10),
        })
        .unwrap();
        let d = RealNativeDriver::new(NATIVE_BACKEND_ID, api);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !d
            .health()
            .await
            .is_ok_and(|h| h.status == atlas_api_types::Health::Ok)
        {
            assert!(Instant::now() < deadline, "node never became healthy");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Arc::new(d)
    }

    /// A node API call as a mount would make it.
    async fn call(&self, method: Method, path: &str, body: &[u8]) -> Value {
        let r = reqwest::Client::new()
            .request(method, format!("{}{path}", self.endpoint()))
            .bearer_auth(NODE_TOKEN)
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        let st = r.status();
        assert!(st.is_success(), "{path}: {st}");
        r.json().await.unwrap_or(Value::Null)
    }

    async fn create(&self, volume_id: &str, name: &str, content: &[u8]) {
        let fs = native_fs(volume_id);
        let body = json!({ "name": name, "op_id": format!("{fs}-{name}"), "kind": "file", "mode": 420 });
        let made = self
            .call(
                Method::POST,
                &format!("/v1/fs/{fs}/inodes/1/entries"),
                body.to_string().as_bytes(),
            )
            .await;
        self.call(
            Method::PUT,
            &format!("/v1/fs/{fs}/inodes/{}/data?offset=0", made["ino"]),
            content,
        )
        .await;
    }
}

fn native_fs(volume_id: &str) -> &str {
    volume_id
        .strip_prefix("vol_native_fs_")
        .expect("a native filesystem volume")
}

/// `change path` per entry of a response.
fn changes(v: &Value) -> Vec<String> {
    v["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("no changes in {v}"))
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["change"].as_str().unwrap(),
                c["path"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_of_a_real_filesystem_workspace_stay_with_its_tenant() {
    let node = Node::start();
    let addr = spawn(node.driver().await).await;
    let api = Api::new(addr);
    let admin = token("root", "admin", "global");
    let a = token("agent-a", "operator", "tenant-a");
    let b = token("agent-b", "operator", "tenant-b");

    let source = api.volume(&admin, "tenant-a", "a-files", "filesystem").await;
    node.create(&source, "input.txt", b"alpha-secret-input").await;
    let (ws, vol) = api.workspace(&a, &source).await;
    let path = format!("/workspaces/{ws}/changes");

    let (st, v) = api.get(&a, &path).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!((&v["total"], &v["next_cursor"]), (&json!(0), &Value::Null));
    assert_eq!(v["workspace_id"], ws.as_str());
    assert_eq!(v["volume_id"], vol.as_str());
    assert!(v["base_snapshot_id"]
        .as_str()
        .unwrap()
        .starts_with("snap_native_fs_"));

    node.create(&vol, "out-1", b"alpha-secret-output").await;
    node.create(&vol, "out-2", b"x").await;
    node.create(&source, "later-in-source", b"y").await;
    let (st, v) = api.get(&a, &path).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(changes(&v), ["created /out-1", "created /out-2"]);
    assert_eq!(v["summary"]["created"], 2);
    assert!(
        !v.to_string().contains("alpha-secret"),
        "no file contents: {v}"
    );

    let (st, first) = api.get(&a, &format!("{path}?limit=1")).await;
    assert_eq!(st, 200, "{first}");
    assert_eq!(changes(&first), ["created /out-1"]);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let (st, second) = api.get(&a, &format!("{path}?limit=1&cursor={cursor}")).await;
    assert_eq!(st, 200, "{second}");
    assert_eq!(changes(&second), ["created /out-2"]);
    assert_eq!(second["next_cursor"], Value::Null);

    // Tenant B: A's workspace is not there, and A's cursor opens nothing through B's own.
    assert_eq!(api.get(&b, &path).await.0, 404);
    assert_eq!(
        api.get(&b, &format!("{path}?limit=1&cursor={cursor}")).await.0,
        404
    );
    let source_b = api.volume(&admin, "tenant-b", "b-files", "filesystem").await;
    let (ws_b, _) = api.workspace(&b, &source_b).await;
    let (st, v) = api
        .get(
            &b,
            &format!("/workspaces/{ws_b}/changes?limit=1&cursor={cursor}"),
        )
        .await;
    assert_eq!(st, 409, "{v}");
    assert!(v.get("changes").is_none() && !v.to_string().contains("out-"));
    // Nor can a cursor smuggle query parameters to the node.
    let (st, v) = api
        .get(&a, &format!("{path}?cursor=abc.0%26limit%3D5"))
        .await;
    assert_eq!(st, 400, "{v}");

    // A reset starts over: nothing changed, and the old listing is not served for the new clone.
    let (st, reset) = api
        .send(&a, Method::POST, &format!("/workspaces/{ws}/reset"), None)
        .await;
    assert_eq!(st, 200, "{reset}");
    let (st, v) = api.get(&a, &path).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["total"], 0);
    assert_eq!(v["volume_id"], reset["volume_id"]);
    assert_eq!(
        api.get(&a, &format!("{path}?cursor={cursor}")).await.0,
        409
    );

    #[cfg(feature = "mcp")]
    mcp::tool_answers_the_owner_only(addr, &node, &ws, reset["volume_id"].as_str().unwrap()).await;
}

#[cfg(feature = "mcp")]
mod mcp {
    use super::*;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

    async fn connect(addr: SocketAddr, token: &str) -> Client {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let transport = StreamableHttpClientTransport::with_client(
            http,
            StreamableHttpClientTransportConfig::with_uri(format!(
                "http://{addr}/api/atlas/v1/mcp"
            )),
        );
        ().serve(transport).await.unwrap()
    }

    async fn call(c: &Client, args: Value) -> Result<Value, String> {
        let params = CallToolRequestParams::new("workspace_changes")
            .with_arguments(args.as_object().unwrap().clone());
        match c.call_tool(params).await {
            Err(e) => Err(e.to_string()),
            Ok(r) => {
                let text = r
                    .content
                    .first()
                    .and_then(|c| c.as_text())
                    .map(|t| t.text.clone())
                    .unwrap_or_default();
                if r.is_error == Some(true) {
                    Err(text)
                } else {
                    Ok(serde_json::from_str(&text).unwrap())
                }
            }
        }
    }

    pub(super) async fn tool_answers_the_owner_only(
        addr: SocketAddr,
        node: &Node,
        ws: &str,
        volume: &str,
    ) {
        node.create(volume, "from-agent", b"alpha-secret-mcp").await;
        let tok = |sub: &str, tenant: &str, tools: &[&str]| {
            let tools: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
            atlas_gateway::auth::mint_scoped_token(SECRET, sub, "viewer", tenant, 3600, &tools)
                .unwrap()
                .0
        };
        // Listed and callable without ATLAS_MCP_WRITE_ENABLE: it only reads.
        let a = connect(addr, &tok("agent-a", "tenant-a", &[])).await;
        let got = call(&a, json!({ "workspace_id": ws })).await.unwrap();
        assert_eq!(changes(&got), ["created /from-agent"]);
        assert!(!got.to_string().contains("alpha-secret"));
        assert!(call(&a, json!({ "workspace_id": ws, "limit": 1001 }))
            .await
            .is_err());
        let b = connect(addr, &tok("agent-b", "tenant-b", &[])).await;
        let denied = call(&b, json!({ "workspace_id": ws })).await.unwrap_err();
        assert!(!denied.contains("from-agent"), "{denied}");
        // A token limited to other tools cannot call it.
        let other = connect(addr, &tok("agent-a", "tenant-a", &["list_workspaces"])).await;
        assert!(call(&other, json!({ "workspace_id": ws })).await.is_err());
        for c in [a, b, other] {
            c.cancel().await.unwrap();
        }
    }
}
