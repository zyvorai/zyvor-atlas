// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The write confirm flow on MCP multi round-trip requests (`mcp/mrtr.rs`), driven by the rmcp client in
//! `server/discover` (2026-07-28, stateless) mode. Own test binary: the write gate and the call budget are
//! process-wide, so both are set once for every test in this file (a budget of 6 calls per token, and every
//! test uses a token of its own).
#![cfg(feature = "mcp")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Once};

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationCapability, FormElicitationCapability, Implementation, ProtocolVersion,
    UrlElicitationCapability,
};
use rmcp::service::{RequestContext, RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, ServiceExt};
use serde_json::{json, Value};

mod common;

const SECRET: &str = "mcp-mrtr-test-secret-at-least-32-bytes!!";

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
        std::env::set_var("ATLAS_MCP_CALLS_PER_MIN", "6");
        std::env::set_var("ATLAS_RATE_LIMIT_SYNC_SECS", "1");
    });
}

/// A running gateway: the address, its pool and the database URL (so a second one can share it).
struct Gw {
    addr: SocketAddr,
    pool: sqlx::AnyPool,
    url: String,
}

async fn spawn_with(secret: &str, database_url: Option<String>) -> Gw {
    init_env();
    let fresh = database_url.is_none();
    let database_url = match database_url {
        Some(u) => u,
        None => common::fresh_database_url("mcp_mrtr").await,
    };
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url: database_url.clone(),
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: secret.into(),
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
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let state = build_state(
        config,
        BuildOptions { enable_k8s: false, initial_discovery: fresh, enable_monitor: false },
    )
    .await
    .unwrap();
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gw { addr, pool, url: database_url }
}

async fn spawn() -> Gw {
    spawn_with(SECRET, None).await
}

async fn seed_volume(pool: &sqlx::AnyPool, id: &str, tenant: &str) {
    let vol = atlas_api_types::StorageVolume {
        id: id.into(),
        cluster_id: None,
        pool_id: None,
        name: format!("{id}-name"),
        kind: atlas_api_types::VolumeKind::Block,
        backend_native_id: Some(format!("rbd:pool/{id}")),
        size_bytes: 1073741824,
        used_bytes: None,
        state: "bound".into(),
        health: atlas_api_types::Health::Ok,
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some(format!("{id}-name")),
        storage_class_name: Some("zyvor-rbd-prod".into()),
    };
    atlas_inventory::upsert_volume(pool, "bkd_ceph_lab", tenant, &vol, None).await.unwrap();
}

async fn job_count(pool: &sqlx::AnyPool) -> usize {
    atlas_inventory::jobs::list_jobs(pool, 100).await.unwrap().len()
}

/// What the client's "user" does with an elicitation, and what the client declares.
#[derive(Clone)]
struct User {
    /// What the user answers; `None` declares no elicitation capability at all.
    caps: Caps,
    answer: Arc<Mutex<ElicitationAction>>,
    asked: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Copy)]
enum Caps {
    None,
    /// `elicitation: {}`, which the spec defines as form mode only (what rmcp's builder declares).
    Empty,
    Form,
    UrlOnly,
}

impl User {
    fn new(caps: Caps, answer: ElicitationAction) -> Self {
        Self { caps, answer: Arc::new(Mutex::new(answer)), asked: Default::default() }
    }
    fn answers(&self, a: ElicitationAction) {
        *self.answer.lock().unwrap() = a;
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl ClientHandler for User {
    fn get_info(&self) -> ClientConfig {
        let mut caps = ClientCapabilities::default();
        caps.elicitation = match self.caps {
            Caps::None => None,
            Caps::Empty => Some(ElicitationCapability::new()),
            Caps::Form => Some(ElicitationCapability::new().with_form(FormElicitationCapability::default())),
            Caps::UrlOnly => Some(ElicitationCapability::new().with_url(UrlElicitationCapability::new())),
        };
        ClientConfig::new(caps, Implementation::new("mrtr-test", "1.0.0"))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        if let ElicitRequestParams::FormElicitationParams { message, .. } = request {
            self.asked.lock().unwrap().push(message);
        }
        let action = self.answer.lock().unwrap().clone();
        let mut result = ElicitResult::new(action.clone());
        if action == ElicitationAction::Accept {
            result = result.with_content(json!({ "confirm": true }));
        }
        Ok(result)
    }
}

fn transport(addr: SocketAddr, secret: &str, actor: &str, role: &str, tenant: &str) -> StreamableHttpClientTransport<reqwest::Client> {
    let (token, _, _) = atlas_gateway::auth::mint_token(secret, actor, role, tenant, 3600).unwrap();
    let http = reqwest::Client::builder()
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(reqwest::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
            h
        })
        .build()
        .unwrap();
    StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/api/atlas/v1/mcp")),
    )
}

/// A 2026-07-28 (stateless, `server/discover`) client.
async fn modern(gw: &Gw, actor: &str, user: User) -> RunningService<RoleClient, User> {
    modern_on(gw.addr, SECRET, actor, user).await
}

async fn modern_on(addr: SocketAddr, secret: &str, actor: &str, user: User) -> RunningService<RoleClient, User> {
    user.serve_with_lifecycle(
        transport(addr, secret, actor, "operator", "tenant-a"),
        ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] },
    )
    .await
    .unwrap()
}

/// A legacy (`initialize`, 2025-11-25) client.
async fn legacy(gw: &Gw, actor: &str, user: User) -> RunningService<RoleClient, User> {
    user.serve(transport(gw.addr, SECRET, actor, "operator", "tenant-a")).await.unwrap()
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn snap(volume: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("create_snapshot").with_arguments(args(json!({ "volume_id": volume, "name": "mrtr-snap" })))
}

fn body(r: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(r.content.first().and_then(|c| c.as_text()).map(|t| t.text.as_str()).unwrap_or("null"))
        .unwrap_or(Value::Null)
}

/// The retry of a call that was answered with `input_required`, as a client would send it.
fn retry(base: CallToolRequestParams, state: &str, answer: Value) -> CallToolRequestParams {
    base.with_input_responses([("confirm".to_string(), answer)].into_iter().collect()).with_request_state(state)
}

fn accept() -> Value {
    json!({ "action": "accept", "content": { "confirm": true } })
}

/// The first round, driven by hand: the `input_required` result's request state.
async fn first_round(c: &RunningService<RoleClient, User>, base: CallToolRequestParams) -> (rmcp::model::InputRequiredResult, String) {
    match c.call_tool_once(base).await.unwrap() {
        CallToolResponse::InputRequired(r) => {
            let state = r.request_state.clone().expect("request state");
            (r, state)
        }
        other => panic!("expected input_required, got {other:?}"),
    }
}

/// The text of a refusal (a protocol error or an error result), or `None` when the call was not refused.
fn refusal(r: Result<CallToolResponse, rmcp::service::ServiceError>) -> Option<String> {
    match r {
        Err(e) => Some(format!("{e:?}")),
        Ok(CallToolResponse::Complete(r)) if r.is_error == Some(true) => Some(format!("{r:?}")),
        Ok(_) => None,
    }
}

/// Assert the call was refused, for the reason `why` (so a budget refusal cannot pass for the one under test).
fn assert_refused(r: Result<CallToolResponse, rmcp::service::ServiceError>, why: &str, what: &str) {
    match refusal(r) {
        Some(text) => assert!(text.contains(why), "{what}: refused, but not for '{why}': {text}"),
        None => panic!("{what}: was not refused"),
    }
}

/// `mcp.tool.call` / `mcp.<tool>` audit statuses of one actor, oldest first. The call row is written off the
/// request path, so wait (briefly) until `want` rows exist.
async fn statuses(pool: &sqlx::AnyPool, action: &str, actor: &str, want: usize) -> Vec<String> {
    for _ in 0..50 {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT status FROM storage_audit_logs WHERE action = $1 AND actor_id = $2 ORDER BY id")
                .bind(action)
                .bind(actor)
                .fetch_all(pool)
                .await
                .unwrap();
        if rows.len() >= want {
            return rows.into_iter().map(|r| r.0).collect();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("fewer than {want} {action} audit rows for {actor}");
}

// ---------------------------------------------------------------------------------------------------------
// Stateless serving and `server/discover` against the gateway (not MRTR itself).
// ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_2026_07_28_client_negotiates_by_discover_and_lists_tools_statelessly() {
    let gw = spawn().await;
    let c = modern(&gw, "agent-discover", User::new(Caps::Empty, ElicitationAction::Decline)).await;
    let info = c.peer_info().expect("discover result");
    assert_eq!(info.protocol_version, ProtocolVersion::V_2026_07_28);
    let listed = c.list_tools(Default::default()).await.unwrap();
    assert!(listed.tools.iter().any(|t| t.name == "create_snapshot"));
    // The list varies by the caller's token, so it must not be shared across authorization contexts.
    assert_eq!(listed.cache_scope, Some(rmcp::model::CacheScope::Private), "per-actor list must not be public");
    assert_eq!(listed.ttl_ms, Some(0));
    // An ordinary read tool works with no session at all.
    let r = c.call_tool(CallToolRequestParams::new("list_clusters")).await.unwrap();
    assert_eq!(r.is_error, Some(false));
    c.cancel().await.unwrap();

    // On the wire: server/discover is answered without a session id, and so is a tool call.
    let (token, _, _) = atlas_gateway::auth::mint_token(SECRET, "agent-raw", "operator", "tenant-a", 3600).unwrap();
    let http = reqwest::Client::new();
    let post = |method: &str, params: Value| {
        let mut params = params;
        params["_meta"] = json!({ "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                  "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "1" },
                                  "io.modelcontextprotocol/clientCapabilities": {} });
        http.post(format!("http://{}/api/atlas/v1/mcp", gw.addr))
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method.to_string())
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
    };
    let resp = post("server/discover", json!({})).await.unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert!(resp.headers().get("mcp-session-id").is_none(), "stateless: no session id");
    let text = resp.text().await.unwrap();
    assert!(text.contains("2026-07-28") && text.contains("supportedVersions"), "{text}");
}

// ---------------------------------------------------------------------------------------------------------
// The confirm flow.
// ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn accepting_the_elicitation_executes_the_snapshot_once() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_acc", "tenant-a").await;
    let before = job_count(&gw.pool).await;
    // `elicitation: {}` is what rmcp's own builder declares; the spec defines it as form mode.
    let user = User::new(Caps::Empty, ElicitationAction::Accept);
    let c = modern(&gw, "agent-accept", user.clone()).await;

    // The client library drives the rounds: plan -> elicit -> retry. The user was asked once, with the plan.
    let done = c.call_tool(snap("vol_acc")).await.unwrap();
    assert_eq!(done.is_error, Some(false), "{done:?}");
    let out = body(&done);
    assert_eq!(out["executed"], true, "{out}");
    assert!(out["result"]["job_id"].is_string(), "{out}");
    let asked = user.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("create_snapshot") && asked[0].contains("vol_acc"), "{}", asked[0]);
    assert_eq!(job_count(&gw.pool).await, before + 1, "exactly one snapshot job");
    // And it is really there over REST.
    let job_id = out["result"]["job_id"].as_str().unwrap();
    let (token, _, _) = atlas_gateway::auth::mint_token(SECRET, "agent-accept", "operator", "tenant-a", 60).unwrap();
    let rest = reqwest::Client::new()
        .get(format!("http://{}/api/atlas/v1/jobs/{job_id}", gw.addr))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert!(rest.status().is_success(), "{}", rest.status());

    // By hand: the first round is an input_required result with the state, no plan JSON and no token argument.
    let (first, state) = first_round(&c, snap("vol_acc")).await;
    let wire = serde_json::to_value(&first).unwrap();
    assert_eq!(wire["resultType"], "input_required");
    assert_eq!(wire["inputRequests"]["confirm"]["method"], "elicitation/create");
    assert_eq!(wire["inputRequests"]["confirm"]["params"]["requestedSchema"]["properties"]["confirm"]["type"], "boolean");
    assert!(!wire.to_string().contains("confirmation_token"));
    // The retry executes; replaying the same state is refused and runs nothing.
    let ok = c.call_tool_once(retry(snap("vol_acc"), &state, accept())).await.unwrap();
    assert!(matches!(&ok, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{ok:?}");
    assert_eq!(job_count(&gw.pool).await, before + 2);
    assert_refused(c.call_tool_once(retry(snap("vol_acc"), &state, accept())).await, "already used", "replay");
    assert_eq!(job_count(&gw.pool).await, before + 2, "a replay must not enqueue another");
    c.cancel().await.unwrap();

    // Audit: each round is a call row; the asking round is `proposed`, not `error`; the executing one `success`.
    let calls = statuses(&gw.pool, "mcp.tool.call", "agent-accept", 5).await;
    assert_eq!(calls, vec!["proposed", "success", "proposed", "success", "error"], "{calls:?}");
    let tool_rows = statuses(&gw.pool, "mcp.create_snapshot", "agent-accept", 5).await;
    assert_eq!(tool_rows, vec!["proposed", "success", "proposed", "success", "rejected"], "{tool_rows:?}");
}

#[tokio::test]
async fn declining_or_cancelling_executes_nothing_and_consumes_nothing() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_dec", "tenant-a").await;
    let before = job_count(&gw.pool).await;
    let user = User::new(Caps::Form, ElicitationAction::Decline);
    let c = modern(&gw, "agent-decline", user.clone()).await;

    let declined = c.call_tool(snap("vol_dec")).await.unwrap();
    assert_eq!(declined.is_error, Some(false), "a decline is not an error: {declined:?}");
    assert_eq!(body(&declined)["executed"], false);
    assert_eq!(body(&declined)["outcome"], "declined");
    user.answers(ElicitationAction::Cancel);
    let cancelled = c.call_tool(snap("vol_dec")).await.unwrap();
    assert_eq!(body(&cancelled)["outcome"], "cancelled");
    assert_eq!(job_count(&gw.pool).await, before);
    let calls = statuses(&gw.pool, "mcp.tool.call", "agent-decline", 4).await;
    assert_eq!(calls, vec!["proposed", "declined", "proposed", "cancelled"], "{calls:?}");
    c.cancel().await.unwrap();

    // A decline consumes nothing: the same state still executes on a later accept (own token: fresh budget).
    let c = modern(&gw, "agent-decline", User::new(Caps::Form, ElicitationAction::Decline)).await;
    let (_, state) = first_round(&c, snap("vol_dec")).await;
    let r = c
        .call_tool_once(retry(snap("vol_dec"), &state, json!({ "action": "decline" })))
        .await
        .unwrap();
    assert!(matches!(&r, CallToolResponse::Complete(r) if body(r)["outcome"] == "declined"));
    assert_eq!(job_count(&gw.pool).await, before);
    let r = c.call_tool_once(retry(snap("vol_dec"), &state, accept())).await.unwrap();
    assert!(matches!(&r, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{r:?}");
    assert_eq!(job_count(&gw.pool).await, before + 1);
    // `accept` with confirm false is a decline too.
    let (_, state) = first_round(&c, snap("vol_dec")).await;
    let r = c
        .call_tool_once(retry(snap("vol_dec"), &state, json!({ "action": "accept", "content": { "confirm": false } })))
        .await
        .unwrap();
    assert!(matches!(&r, CallToolResponse::Complete(r) if body(r)["outcome"] == "declined"));
    assert_eq!(job_count(&gw.pool).await, before + 1);
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn a_retry_without_a_usable_answer_asks_again_instead_of_executing() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_ask", "tenant-a").await;
    let before = job_count(&gw.pool).await;
    let c = modern(&gw, "agent-ask", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    let (_, state) = first_round(&c, snap("vol_ask")).await;
    for garbled in [json!("yes"), json!({ "action": "maybe" })] {
        let r = c.call_tool_once(retry(snap("vol_ask"), &state, garbled)).await.unwrap();
        assert!(matches!(r, CallToolResponse::InputRequired(_)), "must ask again, got {r:?}");
    }
    assert_eq!(job_count(&gw.pool).await, before);
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn clients_that_cannot_elicit_keep_the_confirmation_token_flow() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_leg", "tenant-a").await;
    let before = job_count(&gw.pool).await;

    // 2026-07-28 without the elicitation capability, and with URL mode only: the plan JSON, as before.
    for (i, caps) in [Caps::None, Caps::UrlOnly].into_iter().enumerate() {
        let c = modern(&gw, &format!("agent-nocap-{i}"), User::new(caps, ElicitationAction::Accept)).await;
        let plan = c.call_tool(snap("vol_leg")).await.unwrap();
        let p = body(&plan);
        assert_eq!(p["executed"], false, "{p}");
        let token = p["confirmation_token"].as_str().expect("legacy plan carries the token").to_string();
        assert_eq!(job_count(&gw.pool).await, before + i, "a plan changes nothing");
        let mut a = args(json!({ "volume_id": "vol_leg", "name": "mrtr-snap" }));
        a.insert("confirmation_token".into(), token.into());
        let done = c.call_tool(CallToolRequestParams::new("create_snapshot").with_arguments(a)).await.unwrap();
        assert_eq!(body(&done)["executed"], true);
        c.cancel().await.unwrap();
    }
    assert_eq!(job_count(&gw.pool).await, before + 2);

    // A legacy `initialize` client that declares elicitation is not on 2026-07-28: still the plan JSON.
    let user = User::new(Caps::Empty, ElicitationAction::Accept);
    let c = legacy(&gw, "agent-initialize", user.clone()).await;
    assert_ne!(c.peer_info().map(|i| i.protocol_version.clone()), Some(ProtocolVersion::V_2026_07_28));
    let plan = c.call_tool(snap("vol_leg")).await.unwrap();
    assert_eq!(body(&plan)["executed"], false);
    assert!(body(&plan)["confirmation_token"].is_string());
    assert!(user.asked().is_empty(), "a legacy client is never elicited");
    assert_eq!(job_count(&gw.pool).await, before + 2);
    c.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------------------------------------
// requestState is attacker-controlled input.
// ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn tampered_foreign_mismatched_and_expired_state_is_refused() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_sec", "tenant-a").await;
    seed_volume(&gw.pool, "vol_other", "tenant-a").await;
    atlas_inventory::alerts::upsert_open(&gw.pool, "alert_m", "warning", "test", "pool", "pool_x", "t", "d", &json!({}))
        .await
        .unwrap();
    let before = job_count(&gw.pool).await;
    let user = || User::new(Caps::Empty, ElicitationAction::Accept);
    let a = modern(&gw, "agent-a", user()).await;
    let (_, state) = first_round(&a, snap("vol_sec")).await;

    // Tampered: a flipped character, an appended one, garbage, and nothing at all.
    let mut flipped = state.clone().into_bytes();
    let i = flipped.len() / 2;
    flipped[i] = if flipped[i] == b'A' { b'B' } else { b'A' };
    for bad in [String::from_utf8(flipped).unwrap(), format!("{state}x"), "not-a-token".to_string(), String::new()] {
        assert_refused(a.call_tool_once(retry(snap("vol_sec"), &bad, accept())).await, "invalid or expired", "tampered");
    }
    a.cancel().await.unwrap();

    // Each refusal below is on a token of its own, so the call budget never stands in for the reason.
    // Different arguments, another volume, and another tool, with a valid state.
    let a2 = modern(&gw, "agent-a", user()).await;
    let other_args = CallToolRequestParams::new("create_snapshot")
        .with_arguments(args(json!({ "volume_id": "vol_sec", "name": "another-name" })));
    assert_refused(a2.call_tool_once(retry(other_args, &state, accept())).await, "different caller or arguments", "other arguments");
    assert_refused(a2.call_tool_once(retry(snap("vol_other"), &state, accept())).await, "different caller or arguments", "other volume");
    let other_tool = CallToolRequestParams::new("ack_alert").with_arguments(args(json!({ "alert_id": "alert_m" })));
    assert_refused(a2.call_tool_once(retry(other_tool, &state, accept())).await, "different caller or arguments", "other tool");
    a2.cancel().await.unwrap();
    // A different principal cannot use A's state.
    let b = modern(&gw, "agent-b", user()).await;
    assert_refused(b.call_tool_once(retry(snap("vol_sec"), &state, accept())).await, "different caller or arguments", "other principal");
    b.cancel().await.unwrap();
    assert_eq!(job_count(&gw.pool).await, before, "nothing ran");

    // None of that consumed the state: the right caller with the right arguments still executes it.
    let a3 = modern(&gw, "agent-a", user()).await;
    let ok = a3.call_tool_once(retry(snap("vol_sec"), &state, accept())).await.unwrap();
    assert!(matches!(&ok, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{ok:?}");
    assert_eq!(job_count(&gw.pool).await, before + 1);
    a3.cancel().await.unwrap();

    // Expired: signed with the right key but past its expiry; and signed with another secret.
    let a4 = modern(&gw, "agent-a", user()).await;
    let params = json!({ "volume_id": "vol_sec", "name": "mrtr-snap" }).to_string();
    let expired = forge(SECRET, "agent-a", "create_snapshot", &params, now() - 10);
    assert_refused(a4.call_tool_once(retry(snap("vol_sec"), &expired, accept())).await, "invalid or expired", "expired");
    let wrong_key = forge("some-other-secret-at-least-32-bytes!!!", "agent-a", "create_snapshot", &params, now() + 200);
    assert_refused(a4.call_tool_once(retry(snap("vol_sec"), &wrong_key, accept())).await, "invalid or expired", "other secret");
    // The forging helper is sound: the same call with a live token signed with the gateway's key works.
    let live = forge(SECRET, "agent-a", "create_snapshot", &params, now() + 200);
    let ok = a4.call_tool_once(retry(snap("vol_sec"), &live, accept())).await.unwrap();
    assert!(matches!(&ok, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{ok:?}");
    assert_eq!(job_count(&gw.pool).await, before + 2);
    a4.cancel().await.unwrap();
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// A confirmation token as the gateway signs them (key = SHA-256 of a label and the JWT secret), with a chosen expiry.
fn forge(secret: &str, sub: &str, tool: &str, params: &str, exp: u64) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"atlas.mcp.confirmation-token.v1\0");
    h.update(secret.as_bytes());
    let key: [u8; 32] = h.finalize().into();
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &json!({ "jti": uuid_like(), "sub": sub, "tool": tool, "params": params, "exp": exp }),
        &jsonwebtoken::EncodingKey::from_secret(&key),
    )
    .unwrap()
}

fn uuid_like() -> String {
    format!("forged-{}-{}", std::process::id(), now())
}

// ---------------------------------------------------------------------------------------------------------
// Tokens are portable and single use is durable.
// ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_token_survives_a_restart_and_is_single_use_across_instances_sharing_the_database() {
    let a = spawn().await;
    seed_volume(&a.pool, "vol_rst", "tenant-a").await;
    let before = job_count(&a.pool).await;

    // Two plans on instance A, then "restart": a second gateway on the same database and secret.
    let ca = modern_on(a.addr, SECRET, "agent-rst", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    let (_, state) = first_round(&ca, snap("vol_rst")).await;
    let second = ca
        .call_tool_once(snap("vol_rst"))
        .await
        .map(|r| match r {
            CallToolResponse::InputRequired(r) => r.request_state.unwrap(),
            other => panic!("{other:?}"),
        })
        .unwrap();
    ca.cancel().await.unwrap();
    let b = spawn_with(SECRET, Some(a.url.clone())).await;

    // A gateway with another secret cannot honour it.
    let other_secret = "an-entirely-different-secret-32-bytes!!";
    let c = spawn_with(other_secret, Some(a.url.clone())).await;
    let cc = modern_on(c.addr, other_secret, "agent-rst", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    assert_refused(cc.call_tool_once(retry(snap("vol_rst"), &state, accept())).await, "invalid or expired", "different secret");
    cc.cancel().await.unwrap();

    let cb = modern_on(b.addr, SECRET, "agent-rst", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    let ok = cb.call_tool_once(retry(snap("vol_rst"), &state, accept())).await.unwrap();
    assert!(matches!(&ok, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{ok:?}");
    assert_eq!(job_count(&b.pool).await, before + 1);
    // Replay on B and on A (A never saw the redemption in memory): refused, the database decides.
    assert_refused(cb.call_tool_once(retry(snap("vol_rst"), &state, accept())).await, "already used", "replay on B");
    cb.cancel().await.unwrap();
    let ca = modern_on(a.addr, SECRET, "agent-rst", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    assert_refused(ca.call_tool_once(retry(snap("vol_rst"), &state, accept())).await, "already used", "replay on A");
    // The second state, also issued by A, executes on B once and not again on A.
    let cb = modern_on(b.addr, SECRET, "agent-rst", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    let ok = cb.call_tool_once(retry(snap("vol_rst"), &second, accept())).await.unwrap();
    assert!(matches!(&ok, CallToolResponse::Complete(r) if body(r)["executed"] == true), "{ok:?}");
    assert_refused(ca.call_tool_once(retry(snap("vol_rst"), &second, accept())).await, "already used", "replay of the second on A");
    assert_eq!(job_count(&a.pool).await, before + 2);
    ca.cancel().await.unwrap();
    cb.cancel().await.unwrap();
    let used: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mcp_confirm_used").fetch_one(&a.pool).await.unwrap();
    assert_eq!(used.0, 2, "one row per redeemed token");
}

#[tokio::test]
async fn a_legacy_token_issued_before_a_restart_still_executes_once_on_the_new_instance() {
    let a = spawn().await;
    seed_volume(&a.pool, "vol_lgc", "tenant-a").await;
    let before = job_count(&a.pool).await;
    let c = modern_on(a.addr, SECRET, "agent-lgc", User::new(Caps::None, ElicitationAction::Accept)).await;
    let plan = body(&c.call_tool(snap("vol_lgc")).await.unwrap());
    let token = plan["confirmation_token"].as_str().unwrap().to_string();
    c.cancel().await.unwrap();

    let b = spawn_with(SECRET, Some(a.url.clone())).await;
    let c = modern_on(b.addr, SECRET, "agent-lgc", User::new(Caps::None, ElicitationAction::Accept)).await;
    let mut a_args = args(json!({ "volume_id": "vol_lgc", "name": "mrtr-snap" }));
    a_args.insert("confirmation_token".into(), token.into());
    let call = || CallToolRequestParams::new("create_snapshot").with_arguments(a_args.clone());
    let done = c.call_tool(call()).await.unwrap();
    assert_eq!(body(&done)["executed"], true, "{done:?}");
    assert!(c.call_tool(call()).await.map(|r| r.is_error == Some(true)).unwrap_or(true), "replay");
    assert_eq!(job_count(&b.pool).await, before + 1);
    c.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------------------------------------
// Budgets.
// ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_round_spends_the_call_budget() {
    let gw = spawn().await;
    seed_volume(&gw.pool, "vol_bud", "tenant-a").await;
    let c = modern(&gw, "agent-budget", User::new(Caps::Empty, ElicitationAction::Accept)).await;
    // The budget is 6 calls a minute per token: six asking rounds are allowed, the seventh is not.
    for round in 1..=6 {
        let r = c.call_tool_once(snap("vol_bud")).await;
        assert!(matches!(r, Ok(CallToolResponse::InputRequired(_))), "round {round}: {r:?}");
    }
    assert_refused(c.call_tool_once(snap("vol_bud")).await, "budget", "round 7 is over budget");
    c.cancel().await.unwrap();
    let calls = statuses(&gw.pool, "mcp.tool.call", "agent-budget", 6).await;
    assert_eq!(calls, vec!["proposed"; 6]);
    let limited = statuses(&gw.pool, "mcp.tool.rate_limited", "agent-budget", 1).await;
    assert_eq!(limited, vec!["denied"]);
}
