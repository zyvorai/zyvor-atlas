// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! IdP-issued JWT access tokens on `/mcp` (`src/idp_token.rs`): an in-process fake identity provider
//! (discovery + JWKS over axum on a loopback port) and throwaway RS256/ES256 keys generated at test
//! time, driving a real gateway over HTTP. No real IdP is involved.
#![cfg(feature = "mcp")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::idp_token::{IdpSettings, IdpValidator, Timing};
use atlas_gateway::routes;
use atlas_gateway::startup::{build_state, BuildOptions};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};

mod common;

const PUBLIC_URL: &str = "https://atlas.example.com";
const AUDIENCE: &str = "https://atlas.example.com/api/atlas/v1/mcp";
const SECRET: &str = "mcp-idp-test-secret-at-least-32-bytes!!";
const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---------------------------------------------------------------------------------------------
// Throwaway signing keys
// ---------------------------------------------------------------------------------------------

struct TestKey {
    kid: String,
    alg: Algorithm,
    enc: EncodingKey,
    /// Public JWK (with `kid`, `alg`, `use`).
    jwk: Value,
    /// The public key as PEM, used as the HMAC secret in the algorithm-confusion test.
    public_pem: String,
}

fn rsa_key(idx: usize) -> &'static TestKey {
    static KEYS: [OnceLock<TestKey>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    KEYS[idx].get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        use rsa::traits::PublicKeyParts;
        let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
        let public = private.to_public_key();
        let kid = format!("rsa-{idx}");
        // jsonwebtoken (rust_crypto, no `use_pem` in this workspace) takes PKCS#1 DER for RSA.
        let der = private.to_pkcs1_der().unwrap();
        TestKey {
            alg: Algorithm::RS256,
            enc: EncodingKey::from_rsa_der(der.as_bytes()),
            jwk: json!({
                "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
                "n": B64.encode(public.n().to_bytes_be()),
                "e": B64.encode(public.e().to_bytes_be()),
            }),
            public_pem: public.to_public_key_pem(LineEnding::LF).unwrap(),
            kid,
        }
    })
}

fn ec_key() -> &'static TestKey {
    static KEY: OnceLock<TestKey> = OnceLock::new();
    KEY.get_or_init(|| {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
        let secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let point = secret.public_key().to_encoded_point(false);
        let der = secret.to_pkcs8_der().unwrap();
        TestKey {
            kid: "ec-0".into(),
            alg: Algorithm::ES256,
            enc: EncodingKey::from_ec_der(der.as_bytes()),
            jwk: json!({
                "kty": "EC", "use": "sig", "alg": "ES256", "kid": "ec-0", "crv": "P-256",
                "x": B64.encode(point.x().unwrap()),
                "y": B64.encode(point.y().unwrap()),
            }),
            public_pem: secret
                .public_key()
                .to_public_key_pem(LineEnding::LF)
                .unwrap(),
        }
    })
}

/// Sign `claims` with `key` under `alg` and `kid` (`None` omits the header).
fn sign_as(key: &TestKey, alg: Algorithm, kid: Option<&str>, claims: &Value) -> String {
    let mut h = Header::new(alg);
    h.kid = kid.map(str::to_owned);
    encode(&h, claims, &key.enc).unwrap()
}

fn sign(key: &TestKey, claims: &Value) -> String {
    sign_as(key, key.alg, Some(&key.kid), claims)
}

// ---------------------------------------------------------------------------------------------
// Fake identity provider
// ---------------------------------------------------------------------------------------------

const MODE_NORMAL: u8 = 0;
const MODE_DOWN: u8 = 1;
const MODE_BIG: u8 = 2;
const MODE_REDIRECT: u8 = 3;

struct FakeIdp {
    issuer: String,
    jwks: Mutex<Value>,
    jwks_hits: AtomicUsize,
    discovery_hits: AtomicUsize,
    mode: AtomicU8,
    /// What the discovery document claims as `issuer` (`None`: the real one).
    discovery_issuer: Mutex<Option<String>>,
}

impl FakeIdp {
    fn set_keys(&self, keys: &[&TestKey]) {
        *self.jwks.lock().unwrap() =
            json!({ "keys": keys.iter().map(|k| k.jwk.clone()).collect::<Vec<_>>() });
    }
    fn jwks_hits(&self) -> usize {
        self.jwks_hits.load(Ordering::SeqCst)
    }
    fn set_mode(&self, m: u8) {
        self.mode.store(m, Ordering::SeqCst);
    }
    fn claims(&self) -> Value {
        let n = now();
        json!({ "iss": self.issuer, "aud": AUDIENCE, "sub": "alice", "exp": n + 600, "iat": n })
    }
}

async fn serve_jwks(State(s): State<Arc<FakeIdp>>) -> axum::response::Response {
    s.jwks_hits.fetch_add(1, Ordering::SeqCst);
    match s.mode.load(Ordering::SeqCst) {
        MODE_DOWN => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        MODE_BIG => {
            format!("{{\"keys\":[],\"pad\":\"{}\"}}", "x".repeat(400 * 1024)).into_response()
        }
        MODE_REDIRECT => (StatusCode::FOUND, [("location", "/jwks-moved")]).into_response(),
        _ => Json(s.jwks.lock().unwrap().clone()).into_response(),
    }
}

async fn serve_discovery(State(s): State<Arc<FakeIdp>>) -> axum::response::Response {
    s.discovery_hits.fetch_add(1, Ordering::SeqCst);
    let issuer = s
        .discovery_issuer
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| s.issuer.clone());
    Json(json!({ "issuer": issuer, "jwks_uri": format!("{}/jwks", s.issuer) })).into_response()
}

async fn spawn_idp(keys: &[&TestKey]) -> Arc<FakeIdp> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let idp = Arc::new(FakeIdp {
        issuer: format!("http://{}", listener.local_addr().unwrap()),
        jwks: Mutex::new(json!({ "keys": [] })),
        jwks_hits: AtomicUsize::new(0),
        discovery_hits: AtomicUsize::new(0),
        mode: AtomicU8::new(MODE_NORMAL),
        discovery_issuer: Mutex::new(None),
    });
    idp.set_keys(keys);
    let app = Router::new()
        .route("/jwks", get(serve_jwks))
        .route("/.well-known/openid-configuration", get(serve_discovery))
        .with_state(idp.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    idp
}

// ---------------------------------------------------------------------------------------------
// Gateway under test
// ---------------------------------------------------------------------------------------------

struct Gw {
    addr: SocketAddr,
    pool: sqlx::AnyPool,
    tag: String,
}

fn timing(min_refetch: Duration) -> Timing {
    Timing {
        ttl: Duration::from_secs(3600),
        min_refetch,
        http_timeout: Duration::from_secs(2),
    }
}

/// A gateway with auth on and the IdP feature configured for `issuer` (the way startup would from
/// the environment, so the settings parser is exercised too). `extra` are further `ATLAS_MCP_IDP_*`
/// variables (e.g. `ATLAS_MCP_IDP_JWKS_URL`).
async fn spawn_gw(issuer: &str, extra: &[(&str, &str)], t: Timing) -> Gw {
    let mut env: Vec<(String, String)> = vec![
        ("ATLAS_MCP_IDP_ISSUER".into(), issuer.into()),
        ("ATLAS_OIDC_ADMIN_GROUP".into(), "storage-admins".into()),
        ("ATLAS_OIDC_OPERATOR_GROUP".into(), "storage-ops,sre".into()),
        ("ATLAS_OIDC_TENANT_CLAIM".into(), "tenant".into()),
    ];
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let settings = IdpSettings::from_lookup(
        |k| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
        Some(PUBLIC_URL),
    )
    .unwrap()
    .unwrap();
    let validator = Arc::new(IdpValidator::new(settings, t).unwrap());
    let tag = validator.tag().to_string();

    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url: common::fresh_database_url("mcp-idp").await,
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
    let mut state = build_state(
        config,
        BuildOptions {
            enable_k8s: false,
            initial_discovery: true,
            enable_monitor: false,
        },
    )
    .await
    .unwrap();
    state.public_url = Some(PUBLIC_URL.into());
    state.mcp_idp = Some(validator);
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gw { addr, pool, tag }
}

/// A gateway wired to a fresh fake IdP serving `keys`, discovery on, refetch window 0 (every unknown
/// `kid` may refetch), unless a test says otherwise.
async fn setup(keys: &[&TestKey]) -> (Gw, Arc<FakeIdp>) {
    let idp = spawn_idp(keys).await;
    let gw = spawn_gw(&idp.issuer, &[], timing(Duration::ZERO)).await;
    (gw, idp)
}

async fn post_mcp(addr: SocketAddr, token: Option<&str>) -> reqwest::Response {
    let mut r = reqwest::Client::new()
        .post(format!("http://{addr}/api/atlas/v1/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(INIT);
    if let Some(t) = token {
        r = r.header("authorization", format!("Bearer {t}"));
    }
    r.send().await.unwrap()
}

async fn assert_accepted(addr: SocketAddr, token: &str, what: &str) {
    let resp = post_mcp(addr, Some(token)).await;
    assert!(resp.status().is_success(), "{what}: {}", resp.status());
    assert!(resp.headers().get("www-authenticate").is_none(), "{what}");
}

/// 401 with the `invalid_token` challenge and the resource-metadata pointer.
async fn assert_refused(addr: SocketAddr, token: &str, what: &str) -> String {
    let resp = post_mcp(addr, Some(token)).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "{what}");
    let ch = resp.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        ch.starts_with("Bearer error=\"invalid_token\""),
        "{what}: {ch}"
    );
    assert!(ch.contains("resource_metadata="), "{what}: {ch}");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "AUTH_ERROR", "{what}");
    body["error"]["message"].as_str().unwrap().to_string()
}

async fn connect_with_token(
    addr: SocketAddr,
    token: &str,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;
    let http_client = reqwest::Client::builder()
        .default_headers({
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            h
        })
        .build()
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http_client,
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            format!("http://{addr}/api/atlas/v1/mcp"),
        ),
    );
    ().serve(transport).await.unwrap()
}

fn text_of(r: &rmcp::model::CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

fn refused<E>(r: Result<rmcp::model::CallToolResult, E>) -> bool {
    match r {
        Err(_) => true,
        Ok(v) => v.is_error == Some(true),
    }
}

// ---------------------------------------------------------------------------------------------
// Accepting valid tokens
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn valid_rs256_token_is_accepted_and_keys_are_cached() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let token = sign(rsa_key(0), &idp.claims());
    assert_accepted(gw.addr, &token, "first").await;
    assert_accepted(gw.addr, &token, "second").await;
    assert_eq!(
        idp.discovery_hits.load(Ordering::SeqCst),
        1,
        "discovery once"
    );
    assert_eq!(idp.jwks_hits(), 1, "the JWKS is cached between requests");
}

#[tokio::test]
async fn valid_es256_token_is_accepted() {
    let (gw, idp) = setup(&[rsa_key(0), ec_key()]).await;
    assert_accepted(gw.addr, &sign(ec_key(), &idp.claims()), "es256").await;
    assert_accepted(
        gw.addr,
        &sign(rsa_key(0), &idp.claims()),
        "rs256 from the same set",
    )
    .await;
}

#[tokio::test]
async fn explicit_jwks_url_skips_discovery() {
    let idp = spawn_idp(&[rsa_key(0)]).await;
    let gw = spawn_gw(
        &idp.issuer,
        &[("ATLAS_MCP_IDP_JWKS_URL", &format!("{}/jwks", idp.issuer))],
        timing(Duration::ZERO),
    )
    .await;
    assert_accepted(gw.addr, &sign(rsa_key(0), &idp.claims()), "explicit").await;
    assert_eq!(idp.discovery_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn audience_may_be_an_array_containing_the_mcp_url() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let mut c = idp.claims();
    c["aud"] = json!(["account", AUDIENCE]);
    assert_accepted(gw.addr, &sign(rsa_key(0), &c), "aud array").await;
}

#[tokio::test]
async fn atlas_tokens_still_work_on_mcp_with_the_feature_on() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let (token, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "agent", "operator", "global", 3600).unwrap();
    assert_accepted(gw.addr, &token, "atlas hs256").await;
    assert_eq!(idp.jwks_hits(), 0, "an Atlas token never touches the IdP");
    // And a bad Atlas token is refused exactly as before.
    let (bad, _, _) = atlas_gateway::auth::mint_token(
        "another-secret-that-is-32-bytes-long!!",
        "x",
        "admin",
        "global",
        3600,
    )
    .unwrap();
    assert_refused(gw.addr, &bad, "wrong atlas secret").await;
    // No token: the plain challenge, no `invalid_token`.
    let resp = post_mcp(gw.addr, None).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(!resp.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("invalid_token"));
}

// ---------------------------------------------------------------------------------------------
// Refusing bad tokens
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_tokens_are_refused() {
    let (gw, idp) = setup(&[rsa_key(0), ec_key()]).await;
    let key = rsa_key(0);
    let ok = idp.claims();
    let n = now();
    let with = |f: &dyn Fn(&mut Value)| {
        let mut c = ok.clone();
        f(&mut c);
        sign(key, &c)
    };

    // Sanity: the unmodified claims are accepted, so every refusal below is the one change.
    assert_accepted(gw.addr, &sign(key, &ok), "baseline").await;

    let cases: Vec<(&str, String)> = vec![
        (
            "wrong audience",
            with(&|c| c["aud"] = json!("https://other.example.com/api/atlas/v1/mcp")),
        ),
        (
            "origin as audience",
            with(&|c| c["aud"] = json!(PUBLIC_URL)),
        ),
        (
            "audience array without ours",
            with(&|c| c["aud"] = json!(["a", "b"])),
        ),
        (
            "no audience",
            with(&|c| {
                c.as_object_mut().unwrap().remove("aud");
            }),
        ),
        (
            "wrong issuer",
            with(&|c| c["iss"] = json!("https://evil.example.com")),
        ),
        (
            "issuer with trailing slash",
            with(&|c| c["iss"] = json!(format!("{}/", idp.issuer))),
        ),
        (
            "issuer array",
            with(&|c| c["iss"] = json!([idp.issuer.clone(), "https://evil.example.com"])),
        ),
        (
            "no issuer",
            with(&|c| {
                c.as_object_mut().unwrap().remove("iss");
            }),
        ),
        ("expired", with(&|c| c["exp"] = json!(n - 3600))),
        (
            "no exp",
            with(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
        ),
        (
            "exp as a string",
            with(&|c| c["exp"] = json!((n + 600).to_string())),
        ),
        ("nbf in the future", with(&|c| c["nbf"] = json!(n + 3600))),
        (
            "no subject",
            with(&|c| {
                c.as_object_mut().unwrap().remove("sub");
            }),
        ),
        (
            "lifetime over the cap",
            with(&|c| c["exp"] = json!(n + 3 * 86_400)),
        ),
        (
            "exp - iat over the cap",
            with(&|c| {
                c["iat"] = json!(n - 3 * 86_400);
            }),
        ),
        // Signed by a key the IdP does not publish, under a kid it does.
        (
            "attacker key under a real kid",
            sign_as(rsa_key(1), Algorithm::RS256, Some(&key.kid), &ok),
        ),
        ("no kid", sign_as(key, Algorithm::RS256, None, &ok)),
        (
            "RS256 token naming the EC key",
            sign_as(key, Algorithm::RS256, Some(&ec_key().kid), &ok),
        ),
        (
            "ES256 token naming the RSA key",
            sign_as(ec_key(), Algorithm::ES256, Some(&key.kid), &ok),
        ),
    ];
    for (what, token) in cases {
        let msg = assert_refused(gw.addr, &token, what).await;
        assert!(
            !msg.contains(&token),
            "{what}: the token must never be echoed"
        );
    }
}

#[tokio::test]
async fn alg_none_is_refused() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let head = B64.encode(json!({"alg": "none", "typ": "JWT", "kid": "rsa-0"}).to_string());
    let body = B64.encode(idp.claims().to_string());
    for token in [
        format!("{head}.{body}."),
        format!("{head}.{body}.AAAA"),
        format!("{head}.{body}"),
    ] {
        assert_refused(gw.addr, &token, "alg none").await;
    }
    // "None"/"NONE" spellings and an empty alg are not a way around it either.
    for alg in ["None", "NONE", ""] {
        let head = B64.encode(json!({"alg": alg, "kid": "rsa-0"}).to_string());
        assert_refused(gw.addr, &format!("{head}.{body}."), alg).await;
    }
}

/// The classic algorithm confusion: an HS256 token whose MAC key is the IdP's *public* key (PEM, and
/// the JWK's raw modulus), naming a real kid. Both the Atlas path and the IdP path must refuse it, and
/// a symmetric `oct` JWKS entry must not be importable either.
#[tokio::test]
async fn hs256_with_the_public_key_as_secret_is_refused() {
    let key = rsa_key(0);
    let idp = spawn_idp(&[key]).await;
    // Also publish a symmetric key, as a hostile or sloppy IdP might.
    idp.jwks.lock().unwrap()["keys"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kty": "oct", "kid": "oct-1", "alg": "HS256", "use": "sig", "k": B64.encode("a-shared-secret-of-sufficient-length")}));
    let gw = spawn_gw(&idp.issuer, &[], timing(Duration::ZERO)).await;
    let claims = idp.claims();

    let mac = |secret: &[u8], kid: &str, alg: Algorithm| {
        let mut h = Header::new(alg);
        h.kid = Some(kid.into());
        encode(&h, &claims, &EncodingKey::from_secret(secret)).unwrap()
    };
    let n_bytes = B64.decode(key.jwk["n"].as_str().unwrap()).unwrap();
    for (what, token) in [
        (
            "HS256, PEM as secret",
            mac(key.public_pem.as_bytes(), &key.kid, Algorithm::HS256),
        ),
        (
            "HS384, PEM as secret",
            mac(key.public_pem.as_bytes(), &key.kid, Algorithm::HS384),
        ),
        (
            "HS512, modulus as secret",
            mac(&n_bytes, &key.kid, Algorithm::HS512),
        ),
        (
            "HS256 against an oct JWKS entry",
            mac(
                b"a-shared-secret-of-sufficient-length",
                "oct-1",
                Algorithm::HS256,
            ),
        ),
    ] {
        assert_refused(gw.addr, &token, what).await;
    }

    // The validator itself refuses them too, independent of the router in front of it.
    let settings = IdpSettings::from_lookup(
        |k| (k == "ATLAS_MCP_IDP_ISSUER").then(|| idp.issuer.clone()),
        Some(PUBLIC_URL),
    )
    .unwrap()
    .unwrap();
    let v = IdpValidator::new(settings, timing(Duration::ZERO)).unwrap();
    let hs = mac(key.public_pem.as_bytes(), &key.kid, Algorithm::HS256);
    assert!(v.validate(&hs).await.is_err());
    let oct = mac(
        b"a-shared-secret-of-sufficient-length",
        "oct-1",
        Algorithm::HS256,
    );
    assert!(v.validate(&oct).await.is_err());
    // An RS256 header over the same oct key is refused as well (the key is not even imported).
    let sneaky = sign_as(key, Algorithm::RS256, Some("oct-1"), &claims);
    assert!(v.validate(&sneaky).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Key refresh, rotation, rate limit, IdP trouble
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn unknown_kid_triggers_a_refetch_and_a_rotated_key_validates() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    assert_accepted(gw.addr, &sign(rsa_key(0), &idp.claims()), "old key").await;
    assert_eq!(idp.jwks_hits(), 1);

    // The IdP rotates: the old key is gone, a new one is published.
    idp.set_keys(&[rsa_key(1)]);
    assert_accepted(gw.addr, &sign(rsa_key(1), &idp.claims()), "rotated key").await;
    assert_eq!(idp.jwks_hits(), 2, "one refetch for the unknown kid");
    // The replaced key set no longer trusts the retired key.
    assert_refused(gw.addr, &sign(rsa_key(0), &idp.claims()), "retired key").await;
}

#[tokio::test]
async fn unknown_kid_refetches_are_rate_limited() {
    let idp = spawn_idp(&[rsa_key(0)]).await;
    let gw = spawn_gw(&idp.issuer, &[], timing(Duration::from_secs(3600))).await;
    assert_accepted(gw.addr, &sign(rsa_key(0), &idp.claims()), "first").await;
    assert_eq!(idp.jwks_hits(), 1);

    // An attacker sprays tokens with made-up kids; a real new key is published meanwhile.
    idp.set_keys(&[rsa_key(0), rsa_key(1)]);
    for i in 0..25 {
        let t = sign_as(
            rsa_key(1),
            Algorithm::RS256,
            Some(&format!("made-up-{i}")),
            &idp.claims(),
        );
        assert_refused(gw.addr, &t, "unknown kid").await;
    }
    assert_eq!(
        idp.jwks_hits(),
        1,
        "25 unknown kids must not cause a single refetch inside the window"
    );
    // The legitimately rotated key waits for the window; it is refused, not fetched, until then.
    assert_refused(
        gw.addr,
        &sign(rsa_key(1), &idp.claims()),
        "new key inside the window",
    )
    .await;
    assert_eq!(idp.jwks_hits(), 1);
    // The cached key keeps working throughout.
    assert_accepted(gw.addr, &sign(rsa_key(0), &idp.claims()), "cached key").await;
}

#[tokio::test]
async fn concurrent_unknown_kids_make_one_fetch() {
    let idp = spawn_idp(&[rsa_key(0)]).await;
    let gw = spawn_gw(&idp.issuer, &[], timing(Duration::from_secs(3600))).await;
    let claims = idp.claims();
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let addr = gw.addr;
            let t = sign_as(
                rsa_key(0),
                Algorithm::RS256,
                Some(&format!("k{i}")),
                &claims,
            );
            tokio::spawn(async move { post_mcp(addr, Some(&t)).await.status() })
        })
        .collect();
    for t in tasks {
        assert_eq!(t.await.unwrap(), reqwest::StatusCode::UNAUTHORIZED);
    }
    assert_eq!(idp.jwks_hits(), 1);
}

#[tokio::test]
async fn unreachable_idp_gives_401_and_recovers_without_a_restart() {
    let idp = spawn_idp(&[rsa_key(0)]).await;
    idp.set_mode(MODE_DOWN);
    // The gateway starts fine with the IdP down (spawn_gw does not talk to it) ...
    let gw = spawn_gw(&idp.issuer, &[], timing(Duration::from_millis(1000))).await;
    let token = sign(rsa_key(0), &idp.claims());
    let msg = assert_refused(gw.addr, &token, "idp down").await;
    assert!(msg.contains("unavailable"), "{msg}");
    // ... Atlas tokens are unaffected ...
    let (atlas, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "agent", "operator", "global", 3600).unwrap();
    assert_accepted(gw.addr, &atlas, "atlas token while the idp is down").await;
    // ... a failed attempt also counts against the rate limit ...
    idp.set_mode(MODE_NORMAL);
    let hits = idp.jwks_hits();
    assert_refused(gw.addr, &token, "still inside the window").await;
    assert_eq!(idp.jwks_hits(), hits);
    // ... and once the window passes the next request recovers.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_accepted(gw.addr, &token, "after recovery").await;
}

#[tokio::test]
async fn issuer_that_refuses_connections_is_a_401_not_a_hang() {
    // Port 1 on loopback: connection refused.
    let gw = spawn_gw("http://127.0.0.1:1", &[], timing(Duration::ZERO)).await;
    let token = sign(
        rsa_key(0),
        &json!({"iss": "http://127.0.0.1:1", "aud": AUDIENCE, "sub": "a", "exp": now() + 60}),
    );
    let started = std::time::Instant::now();
    assert_refused(gw.addr, &token, "refused").await;
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn hostile_idp_responses_are_not_trusted() {
    // An oversized JWKS is refused (not buffered without bound).
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    idp.set_mode(MODE_BIG);
    assert_refused(gw.addr, &sign(rsa_key(0), &idp.claims()), "oversized jwks").await;

    // A redirect is not followed.
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    idp.set_mode(MODE_REDIRECT);
    assert_refused(gw.addr, &sign(rsa_key(0), &idp.claims()), "redirect").await;

    // A discovery document that names a different issuer is refused.
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    *idp.discovery_issuer.lock().unwrap() = Some("https://someone-else.example.com".into());
    assert_refused(
        gw.addr,
        &sign(rsa_key(0), &idp.claims()),
        "discovery issuer mismatch",
    )
    .await;
    assert_eq!(
        idp.jwks_hits(),
        0,
        "the JWKS of a mismatching discovery document is never fetched"
    );
}

// ---------------------------------------------------------------------------------------------
// Identity, roles, tenants, revocation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn groups_map_to_roles_and_the_tenant_claim_scopes_reads() {
    use rmcp::model::CallToolRequestParams;
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    for tenant in ["acme", "globex"] {
        let id = format!("vol_{tenant}");
        let vol = atlas_api_types::StorageVolume {
            id: id.clone(),
            cluster_id: None,
            pool_id: None,
            name: format!("{tenant}-vol"),
            kind: atlas_api_types::VolumeKind::Block,
            backend_native_id: Some(format!("rbd:pool/{id}")),
            size_bytes: 1073741824,
            used_bytes: None,
            state: "bound".into(),
            health: atlas_api_types::Health::Ok,
            kubernetes_namespace: Some("default".into()),
            pvc_name: Some(format!("{tenant}-vol")),
            storage_class_name: Some("zyvor-rbd-prod".into()),
        };
        atlas_inventory::upsert_volume(&gw.pool, "bkd_ceph_lab", tenant, &vol, None)
            .await
            .unwrap();
    }
    let by_id = |id: &str| {
        CallToolRequestParams::new("get_volume")
            .with_arguments(json!({ "id": id }).as_object().unwrap().clone())
    };
    let token_for = |groups: Value, tenant: Option<&str>| {
        let mut c = idp.claims();
        c["groups"] = groups;
        if let Some(t) = tenant {
            c["tenant"] = json!(t);
        }
        sign(rsa_key(0), &c)
    };

    // No groups: viewer, refused on an operator tool.
    let viewer = connect_with_token(gw.addr, &token_for(json!([]), Some("acme"))).await;
    assert!(
        refused(viewer.call_tool(by_id("vol_acme")).await),
        "viewer is refused"
    );
    viewer.cancel().await.unwrap();
    // An unrelated group changes nothing.
    let other = connect_with_token(gw.addr, &token_for(json!(["everyone"]), Some("acme"))).await;
    assert!(refused(other.call_tool(by_id("vol_acme")).await));
    other.cancel().await.unwrap();

    // An operator group: allowed on operator tools, and confined to the tenant from the claim.
    let op = connect_with_token(gw.addr, &token_for(json!(["sre"]), Some("acme"))).await;
    let own = op.call_tool(by_id("vol_acme")).await.unwrap();
    assert_eq!(own.is_error, Some(false), "{}", text_of(&own));
    assert!(
        refused(op.call_tool(by_id("vol_globex")).await),
        "another tenant's volume"
    );
    op.cancel().await.unwrap();

    // The admin group sees every tenant.
    let admin =
        connect_with_token(gw.addr, &token_for(json!(["storage-admins"]), Some("acme"))).await;
    for id in ["vol_acme", "vol_globex"] {
        let r = admin.call_tool(by_id(id)).await.unwrap();
        assert_eq!(r.is_error, Some(false), "{id}: {}", text_of(&r));
    }
    admin.cancel().await.unwrap();

    // A `role` claim in the token is never read: an admin-looking claim without the group is a viewer.
    let mut c = idp.claims();
    c["role"] = json!("admin");
    c["roles"] = json!(["admin"]);
    let forged = connect_with_token(gw.addr, &sign(rsa_key(0), &c)).await;
    assert!(refused(forged.call_tool(by_id("vol_acme")).await));
    forged.cancel().await.unwrap();
}

/// The actor is namespaced by issuer, unscoped, and its calls are audited under that id.
#[tokio::test]
async fn idp_actors_are_namespaced_and_audited() {
    use rmcp::model::CallToolRequestParams;
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let mut c = idp.claims();
    c["sub"] = json!("atlas-admin"); // would collide with an Atlas subject without the namespace
    c["groups"] = json!(["sre"]);
    let client = connect_with_token(gw.addr, &sign(rsa_key(0), &c)).await;
    let r = client
        .call_tool(CallToolRequestParams::new("list_clusters"))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(false));
    // Unscoped: it sees the full role-based tool list, not a filtered one.
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(
        tools.tools.len() > 10,
        "an IdP actor is unscoped, role-limited only"
    );
    client.cancel().await.unwrap();

    let expected = format!("{}:atlas-admin", gw.tag);
    let mut seen = false;
    for _ in 0..40 {
        let rows = atlas_inventory::audit::list(
            &gw.pool,
            Some("idp-"),
            Some("mcp.tool.call"),
            None,
            None,
            20,
        )
        .await
        .unwrap();
        if rows.iter().any(|r| r["actor_id"] == json!(expected)) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "expected an mcp.tool.call audit row for {expected}");
    let atlas_rows = atlas_inventory::audit::list(
        &gw.pool,
        Some("atlas-admin"),
        Some("mcp.tool.call"),
        None,
        None,
        20,
    )
    .await
    .unwrap();
    assert!(atlas_rows
        .iter()
        .all(|r| r["actor_id"] != json!("atlas-admin")));
}

#[tokio::test]
async fn jti_is_namespaced_and_revocable_and_missing_jti_is_accepted() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let mut c = idp.claims();
    c["jti"] = json!("tok-1");
    let token = sign(rsa_key(0), &c);
    assert_accepted(gw.addr, &token, "before revoke").await;

    // Revoking the bare IdP jti does nothing (different namespace); the namespaced one does.
    atlas_inventory::tokens::revoke(&gw.pool, "tok-1", "admin")
        .await
        .unwrap();
    assert_accepted(gw.addr, &token, "bare jti is another namespace").await;
    atlas_inventory::tokens::revoke(&gw.pool, &format!("{}:tok-1", gw.tag), "admin")
        .await
        .unwrap();
    let msg = assert_refused(gw.addr, &token, "revoked").await;
    assert!(msg.contains("revoked"), "{msg}");

    // No jti: accepted, and there is nothing to revoke (documented).
    assert_accepted(gw.addr, &sign(rsa_key(0), &idp.claims()), "no jti").await;
}

// ---------------------------------------------------------------------------------------------
// An IdP token never leaves /mcp
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn idp_tokens_are_refused_on_rest_and_metrics() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let mut c = idp.claims();
    c["groups"] = json!(["storage-admins"]);
    let token = sign(rsa_key(0), &c);
    assert_accepted(gw.addr, &token, "control: the same token works on /mcp").await;
    let before = idp.jwks_hits();

    for path in [
        "/api/atlas/v1/clusters",
        "/api/atlas/v1/volumes",
        "/api/atlas/v1/auth/tokens/revoked",
        "/metrics",
    ] {
        let resp = reqwest::Client::new()
            .get(format!("http://{}{path}", gw.addr))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        assert!(
            resp.headers().get("www-authenticate").is_none(),
            "{path}: only /mcp challenges"
        );
    }
    assert_eq!(
        idp.jwks_hits(),
        before,
        "the IdP validator is never consulted off /mcp"
    );

    // gRPC validates with `auth::decode_token` (HS256 only): an IdP token cannot pass it either.
    assert!(atlas_gateway::auth::decode_token(&token, SECRET, None).is_err());
    assert!(atlas_gateway::auth::decode_token(
        &token,
        SECRET,
        Some("previous-secret-32-bytes-or-more!!!")
    )
    .is_err());
}

// ---------------------------------------------------------------------------------------------
// Discovery metadata
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn metadata_advertises_the_issuer_on_the_mcp_document_only() {
    let (gw, idp) = setup(&[rsa_key(0)]).await;
    let base = format!("http://{}", gw.addr);
    let doc: Value = reqwest::get(format!(
        "{base}/.well-known/oauth-protected-resource/api/atlas/v1/mcp"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        doc["resource"], AUDIENCE,
        "the resource is exactly the audience tokens must carry"
    );
    assert_eq!(doc["authorization_servers"], json!([idp.issuer.clone()]));
    assert!(doc["scopes_supported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "atlas:role:operator"), "{doc}");
    assert_eq!(doc["bearer_methods_supported"], json!(["header"]));

    // The root form's resource is the origin, which Atlas would refuse as an audience: not advertised.
    let root: Value = reqwest::get(format!("{base}/.well-known/oauth-protected-resource"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(root["resource"], PUBLIC_URL);
    assert!(root.get("authorization_servers").is_none(), "{root}");

    let card: Value = reqwest::get(format!("{base}/.well-known/mcp"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        card["authentication"]["authorization_servers"],
        json!([idp.issuer.clone()])
    );
    assert_eq!(card["authentication"]["type"], "bearer");

    // The challenge still points at the metadata document.
    let resp = post_mcp(gw.addr, None).await;
    let ch = resp.headers()["www-authenticate"].to_str().unwrap();
    assert!(
        ch.contains(&format!(
            "resource_metadata=\"{PUBLIC_URL}/.well-known/oauth-protected-resource/api/atlas/v1/mcp\""
        )),
        "{ch}"
    );
    assert!(ch.contains("atlas:role:viewer") && ch.contains("scope=\""), "{ch}");
    assert!(!ch.contains("error="), "{ch}");
}
