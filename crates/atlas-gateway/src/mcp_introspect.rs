// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Optional RFC 7662 token introspection for opaque access tokens on the MCP endpoint.
//!
//! Off unless `ATLAS_MCP_INTROSPECTION_URL` is set (then the client id and secret are required).
//! The middleware calls this only when the presented bearer token is not a JWT. The gateway POSTs
//! the token with HTTP Basic client authentication, and accepts the token when the response says
//! `active: true`. A `scope` claim is mapped the same way as on an IdP JWT (`oauth_resource`). An
//! inactive token is `401 invalid_token`. A token that is active but whose `scope` names no Atlas
//! role or tool is `403 insufficient_scope`.
//!
//! REST, gRPC and `/metrics` never call this. JWT access tokens (Atlas HS256 or the IdP's RS256/ES256)
//! are validated locally and are not sent here.

use std::time::Duration;

use atlas_common::config::IdentityMapping;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::oauth_resource::{map_scope_str, map_scope_value, ScopeGrant};

const MAX_BODY: usize = 64 * 1024;

/// Why an opaque token was not accepted. The text is safe to log: it never includes the token.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntroError {
    #[error("introspection is unavailable")]
    Unavailable,
    #[error("token is not active")]
    Inactive,
    #[error("token subject is not acceptable")]
    BadSubject,
    #[error("token audience does not include this MCP server")]
    WrongAudience,
    #[error("token issuer does not match")]
    WrongIssuer,
    #[error("token scope does not include an Atlas role or tool")]
    InsufficientScope,
}

/// A validated opaque token, ready to become an `auth::Actor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueIdentity {
    pub actor_id: String,
    pub role: &'static str,
    pub tenant_id: String,
    pub tools: Vec<String>,
    pub jti: String,
}

/// Client of one introspection endpoint.
#[derive(Clone)]
pub struct Introspection {
    url: reqwest::Url,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    audience: Option<String>,
    expected_issuer: Option<String>,
    mapping: IdentityMapping,
    tag: String,
}

impl Introspection {
    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        public_url: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(url) = var("ATLAS_MCP_INTROSPECTION_URL") else {
            return Ok(None);
        };
        let allow_http = matches!(
            var("ATLAS_MCP_INTROSPECTION_ALLOW_HTTP").as_deref(),
            Some("1" | "true" | "yes")
        );
        let url = check_url(&url, allow_http)?;
        let client_id = var("ATLAS_MCP_INTROSPECTION_CLIENT_ID").ok_or(
            "ATLAS_MCP_INTROSPECTION_URL is set but ATLAS_MCP_INTROSPECTION_CLIENT_ID is not",
        )?;
        let client_secret = var("ATLAS_MCP_INTROSPECTION_CLIENT_SECRET").ok_or(
            "ATLAS_MCP_INTROSPECTION_URL is set but ATLAS_MCP_INTROSPECTION_CLIENT_SECRET is not",
        )?;
        if client_id.len() > 256
            || client_secret.len() > 512
            || client_id.chars().any(char::is_control)
            || client_secret.chars().any(char::is_control)
        {
            return Err(
                "ATLAS_MCP_INTROSPECTION_CLIENT_ID / CLIENT_SECRET is too long or contains a control character"
                    .into(),
            );
        }
        let audience = public_url.map(|u| {
            format!(
                "{}{}",
                u.trim_end_matches('/'),
                crate::oauth_resource::MCP_ENDPOINT_PATH
            )
        });
        Self::build(
            url,
            client_id,
            client_secret,
            audience,
            var("ATLAS_MCP_IDP_ISSUER"),
            IdentityMapping::from_lookup(get),
        )
        .map(Some)
    }

    fn build(
        url: reqwest::Url,
        client_id: String,
        client_secret: String,
        audience: Option<String>,
        expected_issuer: Option<String>,
        mapping: IdentityMapping,
    ) -> Result<Self, String> {
        let tag = format!(
            "intro-{}",
            Sha256::digest(url.as_str().as_bytes())
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("atlas-gateway/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("introspection http client: {e}"))?;
        Ok(Self {
            url,
            client_id,
            client_secret,
            http,
            audience,
            expected_issuer,
            mapping,
            tag,
        })
    }

    /// POST the token and map an active response. The token is not logged.
    pub async fn introspect(&self, token: &str) -> Result<OpaqueIdentity, IntroError> {
        if token.is_empty() || token.len() > 8 * 1024 {
            return Err(IntroError::Inactive);
        }
        let body = format!("token={}&token_type_hint=access_token", form_escape(token));
        let resp = self
            .http
            .post(self.url.clone())
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| IntroError::Unavailable)?;
        if !resp.status().is_success() {
            return Err(IntroError::Unavailable);
        }
        let bytes = resp.bytes().await.map_err(|_| IntroError::Unavailable)?;
        if bytes.len() > MAX_BODY {
            return Err(IntroError::Unavailable);
        }
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| IntroError::Inactive)?;
        identity_from_response(
            &doc,
            &self.mapping,
            self.audience.as_deref(),
            self.expected_issuer.as_deref(),
            &self.tag,
        )
    }
}

pub fn from_env(public_url: Option<&str>) -> anyhow::Result<Option<std::sync::Arc<Introspection>>> {
    match Introspection::from_lookup(|k| std::env::var(k).ok(), public_url) {
        Ok(Some(intro)) => {
            tracing::info!("MCP opaque-token introspection enabled");
            Ok(Some(std::sync::Arc::new(intro)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(e)),
    }
}

/// Map an introspection JSON document. `now` is unused: expiry is checked when `exp` is present
/// against the clock inside this function's caller via the document only when we pass `now`.
pub fn identity_from_response(
    doc: &Value,
    mapping: &IdentityMapping,
    audience: Option<&str>,
    expected_issuer: Option<&str>,
    tag: &str,
) -> Result<OpaqueIdentity, IntroError> {
    if doc.get("active").and_then(Value::as_bool) != Some(true) {
        return Err(IntroError::Inactive);
    }
    if let Some(iss) = doc.get("iss").and_then(Value::as_str) {
        if let Some(want) = expected_issuer {
            if iss != want {
                return Err(IntroError::WrongIssuer);
            }
        }
    }
    if let Some(want) = audience {
        if let Some(aud) = doc.get("aud") {
            let ok = match aud {
                Value::String(s) => s == want,
                Value::Array(items) => items.iter().any(|v| v.as_str() == Some(want)),
                _ => false,
            };
            if !ok {
                return Err(IntroError::WrongAudience);
            }
        }
    }
    if let Some(exp) = doc.get("exp").and_then(Value::as_f64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if (exp.max(0.0) as u64) + 30 < now {
            return Err(IntroError::Inactive);
        }
    }
    let sub = doc
        .get("sub")
        .and_then(Value::as_str)
        .or_else(|| doc.get("client_id").and_then(Value::as_str))
        .or_else(|| doc.get("username").and_then(Value::as_str))
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or(IntroError::BadSubject)?;
    let groups: Vec<String> = match doc.get("groups") {
        Some(Value::Array(a)) => a
            .iter()
            .take(512)
            .filter_map(|g| g.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => s.split_whitespace().map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    let mut role =
        crate::auth::role_for_groups(&groups, &mapping.admin_groups, &mapping.operator_groups);
    let tools = match doc.get("scope") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => match map_scope_str(Some(s)) {
            ScopeGrant::Absent => Vec::new(),
            ScopeGrant::Insufficient => return Err(IntroError::InsufficientScope),
            ScopeGrant::Mapped {
                role: scoped,
                tools,
            } => {
                if let Some(scoped) = scoped {
                    role = scoped;
                }
                tools.unwrap_or_default()
            }
        },
        Some(other) => match map_scope_value(Some(other)) {
            ScopeGrant::Absent => Vec::new(),
            ScopeGrant::Insufficient => return Err(IntroError::InsufficientScope),
            ScopeGrant::Mapped {
                role: scoped,
                tools,
            } => {
                if let Some(scoped) = scoped {
                    role = scoped;
                }
                tools.unwrap_or_default()
            }
        },
    };
    let tenant_id = mapping
        .tenant_claim
        .as_deref()
        .and_then(|c| doc.get(c)?.as_str())
        .filter(|t| !t.is_empty() && t.len() <= 128 && !t.chars().any(char::is_control))
        .map_or_else(|| "global".to_string(), str::to_owned);
    let jti = match doc
        .get("jti")
        .and_then(Value::as_str)
        .filter(|j| !j.is_empty())
    {
        None => String::new(),
        Some(j) if j.len() <= 200 => format!("{tag}:{j}"),
        Some(j) => format!(
            "{tag}:sha256-{}",
            Sha256::digest(j.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ),
    };
    Ok(OpaqueIdentity {
        actor_id: format!("{tag}:{sub}"),
        role,
        tenant_id,
        tools,
        jti,
    })
}

fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn check_url(raw: &str, allow_http: bool) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("ATLAS_MCP_INTROSPECTION_URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("ATLAS_MCP_INTROSPECTION_URL must not contain credentials".into());
    }
    let Some(host) = url.host_str() else {
        return Err("ATLAS_MCP_INTROSPECTION_URL has no host".into());
    };
    let loopback = {
        let h = host.trim_start_matches('[').trim_end_matches(']');
        h.eq_ignore_ascii_case("localhost")
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if allow_http || loopback => Ok(url),
        "http" => Err(
            "ATLAS_MCP_INTROSPECTION_URL must be https (plain http is allowed to a loopback host, \
             or anywhere with ATLAS_MCP_INTROSPECTION_ALLOW_HTTP=1)"
                .into(),
        ),
        other => Err(format!(
            "ATLAS_MCP_INTROSPECTION_URL scheme '{other}' is not allowed"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapping() -> IdentityMapping {
        IdentityMapping {
            admin_groups: vec!["admins".into()],
            operator_groups: vec!["ops".into()],
            tenant_claim: Some("tenant".into()),
        }
    }

    #[test]
    fn active_scope_and_groups_map_and_inactive_is_refused() {
        let m = mapping();
        assert_eq!(
            identity_from_response(
                &json!({"active": false, "sub": "a"}),
                &m,
                None,
                None,
                "intro-abcd"
            ),
            Err(IntroError::Inactive)
        );
        assert_eq!(
            identity_from_response(&json!({"active": true}), &m, None, None, "intro-abcd"),
            Err(IntroError::BadSubject)
        );
        let id = identity_from_response(
            &json!({"active": true, "sub": "alice", "groups": ["ops"], "tenant": "acme"}),
            &m,
            None,
            None,
            "intro-abcd",
        )
        .unwrap();
        assert_eq!(id.role, "operator");
        assert_eq!(id.tenant_id, "acme");
        assert!(id.tools.is_empty());
        assert_eq!(id.actor_id, "intro-abcd:alice");
        let id = identity_from_response(
            &json!({"active": true, "sub": "alice", "groups": ["admins"], "scope": "atlas:role:viewer atlas:tool:list_alerts"}),
            &m,
            None,
            None,
            "intro-abcd",
        )
        .unwrap();
        assert_eq!(id.role, "viewer");
        assert_eq!(id.tools, vec!["list_alerts".to_string()]);
        assert_eq!(
            identity_from_response(
                &json!({"active": true, "sub": "alice", "scope": "openid email"}),
                &m,
                None,
                None,
                "intro-abcd",
            ),
            Err(IntroError::InsufficientScope)
        );
        assert_eq!(
            identity_from_response(
                &json!({"active": true, "sub": "alice", "aud": "https://other/mcp"}),
                &m,
                Some("https://atlas.example/api/atlas/v1/mcp"),
                None,
                "intro-abcd",
            ),
            Err(IntroError::WrongAudience)
        );
        // No `aud` is accepted: many introspection responses omit it.
        assert!(identity_from_response(
            &json!({"active": true, "client_id": "agent"}),
            &m,
            Some("https://atlas.example/api/atlas/v1/mcp"),
            None,
            "intro-abcd",
        )
        .is_ok());
    }

    #[test]
    fn settings_require_a_client_when_the_url_is_set() {
        let get = |k: &str| {
            (k == "ATLAS_MCP_INTROSPECTION_URL").then(|| "https://idp.example/introspect".into())
        };
        assert!(Introspection::from_lookup(get, None).is_err());
        assert!(Introspection::from_lookup(|_| None, None)
            .unwrap()
            .is_none());
        assert!(Introspection::from_lookup(
            |k| match k {
                "ATLAS_MCP_INTROSPECTION_URL" => Some("http://idp.example/introspect".into()),
                "ATLAS_MCP_INTROSPECTION_CLIENT_ID" => Some("c".into()),
                "ATLAS_MCP_INTROSPECTION_CLIENT_SECRET" => Some("s".into()),
                _ => None,
            },
            None,
        )
        .is_err());
    }

    #[tokio::test]
    async fn posts_the_token_and_accepts_an_active_response() {
        use axum::body::Bytes;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::Router;
        let app = Router::new().route(
            "/introspect",
            post(|headers: HeaderMap, body: Bytes| async move {
                assert!(headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .starts_with("Basic "));
                let text = String::from_utf8(body.to_vec()).unwrap();
                assert!(text.contains("token="));
                let scope = if text.contains("token=opaque-token") {
                    "atlas:role:operator"
                } else {
                    "openid email"
                };
                axum::Json(json!({
                    "active": true,
                    "sub": "bob",
                    "scope": scope
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let intro = Introspection::from_lookup(
            |k| match k {
                "ATLAS_MCP_INTROSPECTION_URL" => Some(format!("http://{addr}/introspect")),
                "ATLAS_MCP_INTROSPECTION_CLIENT_ID" => Some("cid".into()),
                "ATLAS_MCP_INTROSPECTION_CLIENT_SECRET" => Some("csecret".into()),
                _ => None,
            },
            Some("https://atlas.example"),
        )
        .unwrap()
        .unwrap();
        let id = intro.introspect("opaque-token").await.unwrap();
        assert_eq!(id.role, "operator");
        assert_eq!(id.actor_id.split(':').next_back(), Some("bob"));
        assert_eq!(
            intro.introspect("other").await.unwrap_err(),
            IntroError::InsufficientScope
        );
    }
}
