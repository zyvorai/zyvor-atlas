// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Lab-only OAuth authorization-server scaffolding. Off unless `ATLAS_MCP_AS_ENABLE` is set.
//!
//! It answers `client_credentials` for the configured client, a small Dynamic Client Registration
//! subset (`POST /oauth/register`), and an authorization-code grant with PKCE S256
//! (`GET /oauth/authorize`, `POST /oauth/token`). Every success mints an ordinary Atlas HS256 JWT.
//! Clients and codes live in memory and disappear on restart. Authorize auto-approves: there is
//! no login page. Redirects are loopback only.
//!
//! Dex (`deploy/dex-lab`) remains the authorization server for a real deployment. Leave this off
//! there. When it is on, the MCP protected-resource document lists this server's origin under
//! `authorization_servers`, and `GET /.well-known/oauth-authorization-server` names the endpoints.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde_json::{json, Value};

use sha2::Digest;

use crate::auth::{self, constant_time_eq};
use crate::state::AppState;

pub const TOKEN_PATH: &str = "/oauth/token";
pub const REGISTER_PATH: &str = "/oauth/register";
pub const AUTHORIZE_PATH: &str = "/oauth/authorize";
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";

const MAX_DYNAMIC_CLIENTS: usize = 32;
const MAX_CODES: usize = 128;
const CODE_TTL_SECS: u64 = 300;

/// Validated `ATLAS_MCP_AS_*` settings. The client secret is compared, never logged.
#[derive(Clone)]
pub struct LabAs {
    pub client_id: String,
    client_secret: String,
    pub role: &'static str,
    pub ttl_secs: u64,
    pub subject: String,
    pub tenant_id: String,
    pub tools: Vec<String>,
    dyn_state: Arc<Mutex<DynState>>,
}

#[derive(Default)]
struct DynState {
    clients: Vec<RegisteredClient>,
    codes: Vec<PendingCode>,
}

struct RegisteredClient {
    client_id: String,
    /// `None` is a public client: PKCE is the only authenticator.
    secret: Option<String>,
    redirect_uris: Vec<String>,
}

enum CodeAuth {
    /// The env-configured confidential client.
    Env,
    Public,
    Secret(String),
}

struct PendingCode {
    code: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    expires_unix: u64,
    auth: CodeAuth,
}

impl LabAs {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let enabled =
            var("ATLAS_MCP_AS_ENABLE").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        if !enabled {
            return Ok(None);
        }
        let client_id = var("ATLAS_MCP_AS_CLIENT_ID")
            .ok_or("ATLAS_MCP_AS_ENABLE is set but ATLAS_MCP_AS_CLIENT_ID is not")?;
        if !valid_client_id(&client_id) {
            return Err(
                "ATLAS_MCP_AS_CLIENT_ID must be 1 to 128 characters of letters, digits, '.', '_' or '-'"
                    .into(),
            );
        }
        let client_secret = var("ATLAS_MCP_AS_CLIENT_SECRET")
            .ok_or("ATLAS_MCP_AS_ENABLE is set but ATLAS_MCP_AS_CLIENT_SECRET is not")?;
        if !(8..=256).contains(&client_secret.len()) || client_secret.chars().any(char::is_control)
        {
            return Err(
                "ATLAS_MCP_AS_CLIENT_SECRET must be 8 to 256 characters and contain no control characters"
                    .into(),
            );
        }
        let role = match var("ATLAS_MCP_AS_ROLE").as_deref() {
            None => "operator",
            Some(r) => {
                auth::normalize_console_role(r).map_err(|e| format!("ATLAS_MCP_AS_ROLE: {e}"))?
            }
        };
        let ttl_secs = match var("ATLAS_MCP_AS_TTL_SECS") {
            None => 3600,
            Some(v) => match v.parse::<u64>() {
                Ok(n) if (60..=86_400).contains(&n) => n,
                _ => {
                    return Err(
                        "ATLAS_MCP_AS_TTL_SECS must be a number of seconds from 60 to 86400".into(),
                    )
                }
            },
        };
        let subject = var("ATLAS_MCP_AS_SUBJECT").unwrap_or_else(|| client_id.clone());
        if subject.is_empty() || subject.len() > 128 || subject.chars().any(char::is_control) {
            return Err(
                "ATLAS_MCP_AS_SUBJECT must be 1 to 128 characters with no control characters"
                    .into(),
            );
        }
        let tenant_id = var("ATLAS_MCP_AS_TENANT").unwrap_or_else(|| "global".into());
        if tenant_id.is_empty() || tenant_id.len() > 128 || tenant_id.chars().any(char::is_control)
        {
            return Err(
                "ATLAS_MCP_AS_TENANT must be 1 to 128 characters with no control characters".into(),
            );
        }
        let mut tools = Vec::new();
        if let Some(raw) = var("ATLAS_MCP_AS_TOOLS") {
            for name in raw.split(',') {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                if crate::oauth_resource::tool_scope_name(name).is_none() {
                    return Err(format!(
                        "ATLAS_MCP_AS_TOOLS: '{name}' is not an MCP tool name (lowercase letters, digits, '_')"
                    ));
                }
                if tools.len() == 64 {
                    return Err("ATLAS_MCP_AS_TOOLS: at most 64 tools".into());
                }
                if !tools.iter().any(|t: &String| t == name) {
                    tools.push(name.to_string());
                }
            }
        }
        Ok(Some(Self {
            client_id,
            client_secret,
            role,
            ttl_secs,
            subject,
            tenant_id,
            tools,
            dyn_state: Arc::new(Mutex::new(DynState::default())),
        }))
    }

    /// Scope string returned to the client. The JWT itself carries `role` and, when set, `tools`.
    pub fn scope(&self) -> String {
        let mut parts = vec![format!("atlas:role:{}", self.role)];
        for tool in &self.tools {
            parts.push(format!("atlas:tool:{tool}"));
        }
        parts.join(" ")
    }

    fn credentials_ok(&self, client_id: &str, client_secret: &str) -> bool {
        let id_ok = constant_time_eq(client_id.as_bytes(), self.client_id.as_bytes());
        let secret_ok = constant_time_eq(client_secret.as_bytes(), self.client_secret.as_bytes());
        id_ok & secret_ok
    }
}

/// Read the settings from the environment. `Ok(None)` when the endpoint is off. `Err` stops startup.
pub fn from_env() -> anyhow::Result<Option<std::sync::Arc<LabAs>>> {
    match LabAs::from_lookup(|k| std::env::var(k).ok()) {
        Ok(Some(lab)) => {
            if !cfg!(feature = "mcp") {
                tracing::warn!(
                    "ATLAS_MCP_AS_ENABLE is set but this build has no `mcp` feature: the token endpoint is not served"
                );
            }
            tracing::info!(
                client_id = %lab.client_id,
                role = lab.role,
                ttl_secs = lab.ttl_secs,
                "lab MCP authorization server enabled (DCR + PKCE scaffolding; Dex remains the production server)"
            );
            Ok(Some(std::sync::Arc::new(lab)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Mint a token when the presented client credentials and `grant_type` are acceptable.
pub fn issue(
    lab: &LabAs,
    secret: &str,
    client_id: &str,
    client_secret: &str,
    grant_type: &str,
) -> Result<Value, &'static str> {
    if grant_type != "client_credentials" {
        return Err("unsupported_grant_type");
    }
    if !lab.credentials_ok(client_id, client_secret) {
        return Err("invalid_client");
    }
    let (token, exp, _) = auth::mint_scoped_token(
        secret,
        &lab.subject,
        lab.role,
        &lab.tenant_id,
        lab.ttl_secs,
        &lab.tools,
    )
    .map_err(|_| "server_error")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as usize)
        .unwrap_or(0);
    let expires_in = exp.saturating_sub(now);
    Ok(json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": expires_in,
        "scope": lab.scope(),
    }))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

/// RFC 7636 appendix B vector is covered by the unit test.
pub(crate) fn pkce_s256(verifier: &str) -> String {
    use base64::Engine;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn valid_verifier(s: &str) -> bool {
    (43..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

fn valid_challenge(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Loopback `http` or `https` only, so a lab client cannot turn authorize into an open redirect.
pub(crate) fn loopback_redirect(uri: &str) -> bool {
    if uri.len() > 256
        || uri.contains('#')
        || uri.contains('@')
        || uri.contains('\\')
        || uri.chars().any(char::is_control)
    {
        return false;
    }
    let rest = if let Some(r) = uri.strip_prefix("https://") {
        r
    } else if let Some(r) = uri.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let host = rest.split(['/', '?', ':']).next().unwrap_or("");
    host == "localhost" || host == "127.0.0.1"
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn append_query(uri: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = uri.to_string();
    let mut first = !uri.contains('?');
    for (k, v) in pairs {
        if v.is_empty() && *k == "state" {
            continue;
        }
        out.push(if first { '?' } else { '&' });
        first = false;
        out.push_str(k);
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    out
}

impl LabAs {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, DynState>, &'static str> {
        self.dyn_state.lock().map_err(|_| "server_error")
    }

    /// RFC 7591 subset. In memory, loopback redirects, authorization_code only.
    pub fn register_client(&self, body: &Value) -> Result<Value, &'static str> {
        let redirects = body
            .get("redirect_uris")
            .and_then(Value::as_array)
            .ok_or("invalid_client_metadata")?;
        if redirects.is_empty() || redirects.len() > 4 {
            return Err("invalid_redirect_uri");
        }
        let mut redirect_uris = Vec::new();
        for uri in redirects {
            let uri = uri.as_str().ok_or("invalid_redirect_uri")?;
            if !loopback_redirect(uri) {
                return Err("invalid_redirect_uri");
            }
            if !redirect_uris.iter().any(|u: &String| u == uri) {
                redirect_uris.push(uri.to_string());
            }
        }
        if let Some(grants) = body.get("grant_types").and_then(Value::as_array) {
            if grants.len() != 1
                || grants.first().and_then(Value::as_str) != Some("authorization_code")
            {
                return Err("invalid_client_metadata");
            }
        }
        if let Some(types) = body.get("response_types").and_then(Value::as_array) {
            if types.len() != 1 || types.first().and_then(Value::as_str) != Some("code") {
                return Err("invalid_client_metadata");
            }
        }
        let method = body
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let secret = match method {
            "none" => None,
            "client_secret_post" | "client_secret_basic" => Some(random_id("")),
            _ => return Err("invalid_client_metadata"),
        };
        if let Some(name) = body.get("client_name").and_then(Value::as_str) {
            if name.len() > 64 || name.chars().any(char::is_control) {
                return Err("invalid_client_metadata");
            }
        }
        let mut state = self.lock()?;
        if state.clients.len() >= MAX_DYNAMIC_CLIENTS {
            return Err("temporarily_unavailable");
        }
        let client_id = random_id("dyn_");
        state.clients.push(RegisteredClient {
            client_id: client_id.clone(),
            secret: secret.clone(),
            redirect_uris: redirect_uris.clone(),
        });
        let mut doc = json!({
            "client_id": client_id,
            "redirect_uris": redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": method,
        });
        if let Some(secret) = secret {
            doc["client_secret"] = json!(secret);
        }
        Ok(doc)
    }

    /// Auto-approve a code for a loopback redirect. Returns the redirect URL, not an HTTP response.
    pub fn authorize(&self, q: &HashMap<String, String>) -> Result<String, &'static str> {
        if q.get("response_type").map(String::as_str) != Some("code") {
            return Err("unsupported_response_type");
        }
        let client_id = q.get("client_id").map(String::as_str).unwrap_or("");
        let redirect = q.get("redirect_uri").map(String::as_str).unwrap_or("");
        let challenge = q.get("code_challenge").map(String::as_str).unwrap_or("");
        let method = q
            .get("code_challenge_method")
            .map(String::as_str)
            .unwrap_or("");
        let state_param = q.get("state").map(String::as_str).unwrap_or("");
        if client_id.is_empty() || !loopback_redirect(redirect) {
            return Err("invalid_request");
        }
        if method != "S256" || !valid_challenge(challenge) {
            return Err("invalid_request");
        }
        if state_param.len() > 256 || state_param.chars().any(char::is_control) {
            return Err("invalid_request");
        }
        let auth = if client_id == self.client_id {
            CodeAuth::Env
        } else {
            let state = self.lock()?;
            let Some(client) = state.clients.iter().find(|c| c.client_id == client_id) else {
                return Err("invalid_request");
            };
            if !client.redirect_uris.iter().any(|u| u == redirect) {
                return Err("invalid_request");
            }
            match &client.secret {
                None => CodeAuth::Public,
                Some(secret) => CodeAuth::Secret(secret.clone()),
            }
        };
        let code = random_id("code_");
        let expires = now_unix().saturating_add(CODE_TTL_SECS);
        let mut state = self.lock()?;
        let now = now_unix();
        state.codes.retain(|c| c.expires_unix > now);
        if state.codes.len() >= MAX_CODES {
            state.codes.remove(0);
        }
        state.codes.push(PendingCode {
            code: code.clone(),
            client_id: client_id.to_string(),
            redirect_uri: redirect.to_string(),
            challenge: challenge.to_string(),
            expires_unix: expires,
            auth,
        });
        Ok(append_query(
            redirect,
            &[("code", &code), ("state", state_param)],
        ))
    }

    /// Exchange a PKCE code for an Atlas JWT. The code is single-use.
    pub fn redeem_authorization_code(
        &self,
        jwt_secret: &str,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Value, &'static str> {
        if !valid_verifier(verifier) {
            return Err("invalid_grant");
        }
        let mut state = self.lock()?;
        let now = now_unix();
        let idx = state
            .codes
            .iter()
            .position(|c| c.code == code)
            .ok_or("invalid_grant")?;
        let pending = state.codes.remove(idx);
        if pending.expires_unix <= now
            || pending.client_id != client_id
            || pending.redirect_uri != redirect_uri
        {
            return Err("invalid_grant");
        }
        if pkce_s256(verifier) != pending.challenge {
            return Err("invalid_grant");
        }
        let secret_ok = match &pending.auth {
            CodeAuth::Env => self.credentials_ok(client_id, client_secret),
            CodeAuth::Public => client_secret.is_empty(),
            CodeAuth::Secret(expected) => {
                constant_time_eq(client_secret.as_bytes(), expected.as_bytes())
            }
        };
        if !secret_ok {
            return Err("invalid_client");
        }
        drop(state);
        self.mint(jwt_secret, client_id)
    }

    fn mint(&self, jwt_secret: &str, client_id: &str) -> Result<Value, &'static str> {
        let subject = if client_id == self.client_id {
            self.subject.clone()
        } else {
            client_id.to_string()
        };
        let (token, exp, _) = auth::mint_scoped_token(
            jwt_secret,
            &subject,
            self.role,
            &self.tenant_id,
            self.ttl_secs,
            &self.tools,
        )
        .map_err(|_| "server_error")?;
        let now = now_unix() as usize;
        Ok(json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": exp.saturating_sub(now),
            "scope": self.scope(),
        }))
    }
}

fn valid_client_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn form_map(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

fn percent_decode(raw: &str) -> String {
    let raw = raw.replace('+', " ");
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basic_client(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((id.to_string(), secret.to_string()))
}

fn oauth_error(status: StatusCode, error: &str, basic: bool) -> Response {
    let mut resp = (status, Json(json!({ "error": error }))).into_response();
    if basic {
        if let Ok(v) = HeaderValue::from_str("Basic realm=\"atlas\"") {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
    }
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// `POST /oauth/token`. 404 when the lab endpoint is off.
pub async fn token(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(lab) = state.mcp_as.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if body.len() > 8 * 1024 {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", false);
    }
    let Ok(text) = std::str::from_utf8(&body) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", false);
    };
    let form = form_map(text);
    let (client_id, client_secret, from_basic) = match basic_client(&headers) {
        Some((id, secret)) => (id, secret, true),
        None => (
            form.get("client_id").cloned().unwrap_or_default(),
            form.get("client_secret").cloned().unwrap_or_default(),
            false,
        ),
    };
    let grant = form.get("grant_type").map(String::as_str).unwrap_or("");
    let result = match grant {
        "authorization_code" => lab.redeem_authorization_code(
            &state.config.jwt_secret,
            &client_id,
            &client_secret,
            form.get("code").map(String::as_str).unwrap_or(""),
            form.get("redirect_uri").map(String::as_str).unwrap_or(""),
            form.get("code_verifier").map(String::as_str).unwrap_or(""),
        ),
        _ => issue(
            lab,
            &state.config.jwt_secret,
            &client_id,
            &client_secret,
            grant,
        ),
    };
    match result {
        Ok(doc) => {
            let mut resp = Json(doc).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err("invalid_client") => {
            oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", from_basic)
        }
        Err("unsupported_grant_type") => {
            oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", false)
        }
        Err("invalid_grant") => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", false),
        Err(_) => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", false),
    }
}

/// `POST /oauth/register`. 404 when the lab endpoint is off.
pub async fn register(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(lab) = state.mcp_as.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if body.len() > 8 * 1024 {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", false);
    }
    let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", false);
    };
    match lab.register_client(&doc) {
        Ok(out) => {
            let mut resp = (StatusCode::CREATED, Json(out)).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => oauth_error(StatusCode::BAD_REQUEST, e, false),
    }
}

/// `GET /oauth/authorize`. Auto-approves a loopback PKCE request. 404 when the lab endpoint is off.
pub async fn authorize(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let Some(lab) = state.mcp_as.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match lab.authorize(&q) {
        Ok(location) => {
            let mut resp = StatusCode::FOUND.into_response();
            if let Ok(v) = HeaderValue::from_str(&location) {
                resp.headers_mut().insert(header::LOCATION, v);
            }
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => oauth_error(StatusCode::BAD_REQUEST, e, false),
    }
}

/// `GET /.well-known/oauth-authorization-server`. 404 when the lab endpoint is off.
pub async fn metadata(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(lab) = state.mcp_as.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((origin, configured)) =
        crate::oauth_resource::origin(state.public_url.as_deref(), &headers)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cannot determine this server's URL" })),
        )
            .into_response();
    };
    let scopes = crate::oauth_resource::supported_scopes(&state);
    let cache = if configured {
        "public, max-age=300"
    } else {
        "no-store"
    };
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static(cache))],
        Json(json!({
            "issuer": origin,
            "token_endpoint": format!("{origin}{TOKEN_PATH}"),
            "authorization_endpoint": format!("{origin}{AUTHORIZE_PATH}"),
            "registration_endpoint": format!("{origin}{REGISTER_PATH}"),
            "grant_types_supported": ["client_credentials", "authorization_code"],
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
            "scopes_supported": scopes,
            // Lab scaffolding. Dex remains the production authorization server.
            "scopes_default": lab.scope(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    fn lab() -> LabAs {
        LabAs::from_lookup(env(&[
            ("ATLAS_MCP_AS_ENABLE", "1"),
            ("ATLAS_MCP_AS_CLIENT_ID", "lab-agent"),
            ("ATLAS_MCP_AS_CLIENT_SECRET", "lab-secret-value"),
            ("ATLAS_MCP_AS_ROLE", "operator"),
            ("ATLAS_MCP_AS_TOOLS", "list_alerts,cluster_health"),
        ]))
        .unwrap()
        .unwrap()
    }

    #[test]
    fn disabled_unless_the_flag_is_set_and_rejects_a_short_secret() {
        assert!(LabAs::from_lookup(env(&[])).unwrap().is_none());
        assert!(LabAs::from_lookup(env(&[("ATLAS_MCP_AS_ENABLE", "1")])).is_err());
        assert!(LabAs::from_lookup(env(&[
            ("ATLAS_MCP_AS_ENABLE", "1"),
            ("ATLAS_MCP_AS_CLIENT_ID", "lab"),
            ("ATLAS_MCP_AS_CLIENT_SECRET", "short"),
        ]))
        .is_err());
    }

    #[test]
    fn client_credentials_mints_an_atlas_jwt_with_the_configured_role_and_tools() {
        let lab = lab();
        let secret = "lab-jwt-secret-at-least-32-bytes!!";
        assert_eq!(
            issue(
                &lab,
                secret,
                "lab-agent",
                "wrong-secret-value",
                "client_credentials"
            ),
            Err("invalid_client")
        );
        assert_eq!(
            issue(
                &lab,
                secret,
                "lab-agent",
                "lab-secret-value",
                "authorization_code"
            ),
            Err("unsupported_grant_type")
        );
        let doc = issue(
            &lab,
            secret,
            "lab-agent",
            "lab-secret-value",
            "client_credentials",
        )
        .unwrap();
        assert_eq!(doc["token_type"], "Bearer");
        assert!(doc["scope"]
            .as_str()
            .unwrap()
            .contains("atlas:role:operator"));
        assert!(doc["scope"]
            .as_str()
            .unwrap()
            .contains("atlas:tool:list_alerts"));
        let token = doc["access_token"].as_str().unwrap();
        let data = auth::decode_token(token, secret, None).unwrap();
        assert_eq!(data.claims.role, "operator");
        assert_eq!(data.claims.sub, "lab-agent");
        assert_eq!(
            data.claims.tools,
            vec!["list_alerts".to_string(), "cluster_health".to_string()]
        );
    }

    #[test]
    fn pkce_s256_matches_the_rfc7636_vector_and_rejects_open_redirects() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(loopback_redirect("http://127.0.0.1:8080/cb"));
        assert!(loopback_redirect("https://localhost/cb"));
        assert!(!loopback_redirect("https://evil.example/cb"));
        assert!(!loopback_redirect("http://127.0.0.1.evil/cb"));
        assert!(!loopback_redirect("http://user@127.0.0.1/cb"));
    }

    #[test]
    fn dcr_and_pkce_mint_an_atlas_jwt_and_a_code_is_single_use() {
        let lab = lab();
        let secret = "lab-jwt-secret-at-least-32-bytes!!";
        let registered = lab
            .register_client(&json!({
                "redirect_uris": ["http://127.0.0.1:9/cb"],
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code"],
            }))
            .unwrap();
        let client_id = registered["client_id"].as_str().unwrap();
        assert!(registered.get("client_secret").is_none());
        assert!(lab
            .register_client(&json!({ "redirect_uris": ["https://evil.example/cb"] }))
            .is_err());
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut q = HashMap::new();
        q.insert("response_type".into(), "code".into());
        q.insert("client_id".into(), client_id.into());
        q.insert("redirect_uri".into(), "http://127.0.0.1:9/cb".into());
        q.insert("code_challenge".into(), pkce_s256(verifier));
        q.insert("code_challenge_method".into(), "S256".into());
        q.insert("state".into(), "xyz".into());
        let loc = lab.authorize(&q).unwrap();
        assert!(loc.starts_with("http://127.0.0.1:9/cb?"));
        assert!(loc.contains("state=xyz"));
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let wrong = "a".repeat(43);
        assert_eq!(
            lab.redeem_authorization_code(
                secret,
                client_id,
                "",
                code,
                "http://127.0.0.1:9/cb",
                &wrong
            ),
            Err("invalid_grant")
        );
        // The bad verifier consumed the code. A fresh code with the right verifier mints a JWT.
        let loc = lab.authorize(&q).unwrap();
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        let doc = lab
            .redeem_authorization_code(
                secret,
                client_id,
                "",
                code,
                "http://127.0.0.1:9/cb",
                verifier,
            )
            .unwrap();
        assert_eq!(doc["token_type"], "Bearer");
        let token = doc["access_token"].as_str().unwrap();
        let data = auth::decode_token(token, secret, None).unwrap();
        assert_eq!(data.claims.sub, client_id);
        assert_eq!(data.claims.role, "operator");
        assert_eq!(
            lab.redeem_authorization_code(
                secret,
                client_id,
                "",
                code,
                "http://127.0.0.1:9/cb",
                verifier,
            ),
            Err("invalid_grant")
        );
    }

    #[test]
    fn form_decoding_handles_plus_and_percent() {
        let m = form_map("grant_type=client_credentials&client_id=a%2Bb&client_secret=x+y");
        assert_eq!(m.get("client_id").map(String::as_str), Some("a+b"));
        assert_eq!(m.get("client_secret").map(String::as_str), Some("x y"));
    }
}
