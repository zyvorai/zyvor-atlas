// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Lab-only OAuth token endpoint. Off unless `ATLAS_MCP_AS_ENABLE` is set.
//!
//! This is not an authorization server in the MCP sense: there is no Dynamic Client Registration,
//! no authorization-code grant and no PKCE. It answers `client_credentials` by minting an ordinary
//! Atlas HS256 JWT (the same tokens `POST /auth/tokens` mints), so a lab client can complete a
//! token request without Dex. A real deployment keeps using an identity provider such as the Dex
//! lab (`deploy/dex-lab`) and leaves this off. When it is on, the MCP protected-resource document
//! lists this server's origin under `authorization_servers`, and
//! `GET /.well-known/oauth-authorization-server` names the token endpoint.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde_json::{json, Value};

use crate::auth::{self, constant_time_eq};
use crate::state::AppState;

pub const TOKEN_PATH: &str = "/oauth/token";
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";

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
}

impl LabAs {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let enabled = var("ATLAS_MCP_AS_ENABLE").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
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
        if !(8..=256).contains(&client_secret.len()) || client_secret.chars().any(char::is_control) {
            return Err(
                "ATLAS_MCP_AS_CLIENT_SECRET must be 8 to 256 characters and contain no control characters"
                    .into(),
            );
        }
        let role = match var("ATLAS_MCP_AS_ROLE").as_deref() {
            None => "operator",
            Some(r) => auth::normalize_console_role(r)
                .map_err(|e| format!("ATLAS_MCP_AS_ROLE: {e}"))?,
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
        if subject.is_empty()
            || subject.len() > 128
            || subject.chars().any(char::is_control)
        {
            return Err("ATLAS_MCP_AS_SUBJECT must be 1 to 128 characters with no control characters".into());
        }
        let tenant_id = var("ATLAS_MCP_AS_TENANT").unwrap_or_else(|| "global".into());
        if tenant_id.is_empty()
            || tenant_id.len() > 128
            || tenant_id.chars().any(char::is_control)
        {
            return Err("ATLAS_MCP_AS_TENANT must be 1 to 128 characters with no control characters".into());
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
                "lab MCP client_credentials token endpoint enabled (not a full authorization server)"
            );
            Ok(Some(std::sync::Arc::new(lab)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Mint a token when the presented client credentials and `grant_type` are acceptable.
pub fn issue(lab: &LabAs, secret: &str, client_id: &str, client_secret: &str, grant_type: &str) -> Result<Value, &'static str> {
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
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
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
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
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
    match issue(lab, &state.config.jwt_secret, &client_id, &client_secret, grant) {
        Ok(doc) => {
            let mut resp = Json(doc).into_response();
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            resp
        }
        Err("invalid_client") => oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", from_basic),
        Err("unsupported_grant_type") => {
            oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", false)
        }
        Err(_) => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", false),
    }
}

/// `GET /.well-known/oauth-authorization-server`. 404 when the lab endpoint is off.
pub async fn metadata(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(lab) = state.mcp_as.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((origin, configured)) = crate::oauth_resource::origin(state.public_url.as_deref(), &headers)
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "cannot determine this server's URL" })),
        )
            .into_response();
    };
    let scopes = crate::oauth_resource::supported_scopes(&state);
    let cache = if configured { "public, max-age=300" } else { "no-store" };
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static(cache))],
        Json(json!({
            "issuer": origin,
            "token_endpoint": format!("{origin}{TOKEN_PATH}"),
            "grant_types_supported": ["client_credentials"],
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
            "scopes_supported": scopes,
            "response_types_supported": ["token"],
            // The minted token's scope. There is no registration_endpoint: this is not DCR.
            "scopes_default": lab.scope(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_string())
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
            issue(&lab, secret, "lab-agent", "wrong-secret-value", "client_credentials"),
            Err("invalid_client")
        );
        assert_eq!(
            issue(&lab, secret, "lab-agent", "lab-secret-value", "authorization_code"),
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
        assert!(doc["scope"].as_str().unwrap().contains("atlas:role:operator"));
        assert!(doc["scope"].as_str().unwrap().contains("atlas:tool:list_alerts"));
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
    fn form_decoding_handles_plus_and_percent() {
        let m = form_map("grant_type=client_credentials&client_id=a%2Bb&client_secret=x+y");
        assert_eq!(m.get("client_id").map(String::as_str), Some("a+b"));
        assert_eq!(m.get("client_secret").map(String::as_str), Some("x y"));
    }
}
