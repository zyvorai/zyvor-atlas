// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! RFC 9728 OAuth 2.0 Protected Resource Metadata for the MCP endpoint, plus the `WWW-Authenticate`
//! challenge `auth_middleware` adds to 401s under `/api/atlas/v1/mcp`.
//!
//! What this is, and is not. The MCP authorization spec (2026-07-28) says an MCP server MUST serve
//! Protected Resource Metadata and answer unauthenticated requests with
//! `WWW-Authenticate: Bearer resource_metadata="<url>"`, and, as an OAuth resource server, must name
//! its authorization server(s) and accept only tokens issued for it. Atlas has **no OAuth
//! authorization server** of its own. What it can do, when `ATLAS_MCP_IDP_ISSUER` is set (see
//! `idp_token`), is validate JWT access tokens an external identity provider issued for the MCP
//! endpoint (issuer, audience = the MCP URL, expiry, signature from the issuer's JWKS), and then the
//! path-inserted document (the one whose `resource` is the MCP endpoint) carries
//! `authorization_servers: [<that issuer>]`. Without it the document has no `authorization_servers`:
//! the OIDC issuer used for the console's browser login is deliberately not advertised, because a token
//! it issues is not accepted by Atlas.
//!
//! The root-form document (`resource` = the origin) never lists the issuer: the audience Atlas enforces
//! is the MCP endpoint URL, so a client that took the origin as its RFC 8707 `resource` would get a
//! token Atlas refuses. Both documents publish `scopes_supported`: `atlas:role:viewer|operator|admin`
//! and one `atlas:tool:<name>` per MCP tool the process actually serves. The same list is the `scope`
//! parameter on the MCP endpoint's `WWW-Authenticate` challenge. Atlas's own HS256 tokens
//! (`POST /auth/login`, `POST /auth/tokens`, the OIDC console login, and the lab
//! `ATLAS_MCP_AS_ENABLE` client-credentials endpoint) carry no `aud`, are not audience-checked, and
//! keep working unchanged; the audience check applies to IdP tokens only. An IdP token's `scope`
//! claim, when present, maps onto a role and/or a tool allowlist (`map_scope_value`); a claim that
//! names none of these scopes is `403` with `error="insufficient_scope"`. A token with no `scope`
//! claim is unchanged (role from groups, no tool allowlist).
//!
//! # Where the resource URL comes from
//! Atlas had no public-URL setting and no `Forwarded` handling anywhere, so this adds one:
//!
//! 1. `ATLAS_PUBLIC_URL` (an origin such as `https://atlas.example.com`, no path/query/fragment),
//!    validated once at startup. When set it is the only source: request headers are ignored, so a
//!    forged `Host` can never change what the metadata or a challenge says. Set this in production.
//! 2. Otherwise the request's `Host` header, accepted only if it is a syntactically plain
//!    `host[:port]` (letters, digits, `.`, `-`, `:`, `[`, `]`), with the scheme from
//!    `X-Forwarded-Proto` when that is exactly `http` or `https`, else `http`. Both headers are
//!    chosen by the caller unless a proxy overwrites them, so this is a convenience for labs and
//!    NodePorts, not a trust boundary. Such documents are sent `Cache-Control: no-store`.
//!
//! A request with no usable `Host` and no configured URL gets `400` from the metadata routes and a
//! bare `WWW-Authenticate: Bearer` (no `resource_metadata`) from the middleware.

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::state::AppState;

/// Path of the MCP endpoint on the gateway.
pub const MCP_ENDPOINT_PATH: &str = "/api/atlas/v1/mcp";
/// RFC 9728 well-known path (root form: the resource is the origin).
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
/// RFC 9728 well-known path with the MCP endpoint path inserted (the resource is the MCP endpoint).
pub const MCP_METADATA_PATH: &str = "/.well-known/oauth-protected-resource/api/atlas/v1/mcp";

/// Validate `ATLAS_PUBLIC_URL`: `http(s)://host[:port]` with at most a trailing `/`. Returns the
/// normalised origin without the trailing slash.
pub fn parse_public_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let (scheme, rest) = if let Some(r) = raw.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = raw.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err("must start with http:// or https://".into());
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#', '@']) {
        return Err("must be an origin only: no path, query, fragment or userinfo".into());
    }
    if !valid_authority(authority) {
        return Err(
            "host[:port] is empty or has characters outside letters, digits . - : [ ]".into(),
        );
    }
    Ok(format!("{scheme}://{authority}"))
}

/// A plain `host[:port]` (also `[v6]:port`): nothing that could break out of a quoted header
/// parameter or a URL.
fn valid_authority(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 255
        && !a.starts_with(['-', '.', ':'])
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

/// The externally visible origin (`scheme://host[:port]`, no trailing slash) for this request, and
/// whether it came from configuration (true) or request headers (false). See the module docs.
pub fn origin(public_url: Option<&str>, headers: &HeaderMap) -> Option<(String, bool)> {
    if let Some(u) = public_url {
        return Some((u.trim_end_matches('/').to_string(), true));
    }
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if !valid_authority(host) {
        return None;
    }
    let scheme = match headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        Some(p) if p.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    Some((format!("{scheme}://{host}"), false))
}

/// URL of the metadata document a client should fetch for the MCP endpoint (the path-inserted form).
pub fn metadata_url(public_url: Option<&str>, headers: &HeaderMap) -> Option<String> {
    origin(public_url, headers).map(|(o, _)| format!("{o}{MCP_METADATA_PATH}"))
}

/// Role scopes published in protected-resource metadata and accepted from an IdP `scope` claim.
pub const ROLE_SCOPES: [&str; 3] = [
    "atlas:role:viewer",
    "atlas:role:operator",
    "atlas:role:admin",
];

/// What an OAuth `scope` claim grants. Absent means the token did not carry the claim (role stays
/// group-derived, tools stay empty). Insufficient means the claim was present and named no Atlas
/// role or tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeGrant {
    Absent,
    Mapped {
        /// Highest `atlas:role:*` (or a bare `viewer`/`operator`/`admin`) in the claim.
        /// `None` when the claim named tools only: keep the group-derived role.
        role: Option<&'static str>,
        /// `Some` when at least one `atlas:tool:<name>` was present (the allowlist, possibly after
        /// dropping names that are not tool-shaped). `None` when the claim named no tool scopes.
        tools: Option<Vec<String>>,
    },
    Insufficient,
}

/// `atlas:tool:<name>` when `name` is a plausible MCP tool name, else `None`.
pub fn tool_scope_name(name: &str) -> Option<String> {
    if (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        Some(format!("atlas:tool:{name}"))
    } else {
        None
    }
}

/// Map a JSON `scope` claim (string or array of strings). `None` and JSON null are [`ScopeGrant::Absent`].
pub fn map_scope_value(scope: Option<&Value>) -> ScopeGrant {
    match scope {
        None | Some(Value::Null) => ScopeGrant::Absent,
        Some(Value::String(s)) => map_scope_str(Some(s)),
        Some(Value::Array(items)) => {
            let parts: Vec<&str> = items.iter().take(128).filter_map(Value::as_str).collect();
            if parts.is_empty() {
                ScopeGrant::Insufficient
            } else {
                fold_scopes(&parts)
            }
        }
        Some(_) => ScopeGrant::Insufficient,
    }
}

/// Map a space-delimited scope string (RFC 6749). `None` is absent; empty or whitespace is insufficient.
pub fn map_scope_str(scope: Option<&str>) -> ScopeGrant {
    match scope {
        None => ScopeGrant::Absent,
        Some(s) => {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.is_empty() {
                ScopeGrant::Insufficient
            } else {
                fold_scopes(&parts)
            }
        }
    }
}

fn fold_scopes(parts: &[&str]) -> ScopeGrant {
    let mut role: Option<&'static str> = None;
    let mut role_rank = 0u8;
    let mut tools: Vec<String> = Vec::new();
    for part in parts {
        if let Some((name, rank)) = role_scope(part) {
            if rank >= role_rank {
                role = Some(name);
                role_rank = rank;
            }
            continue;
        }
        if let Some(name) = part.strip_prefix("atlas:tool:") {
            if tools.len() < 64
                && tool_scope_name(name).is_some()
                && !tools.iter().any(|t| t == name)
            {
                tools.push(name.to_string());
            }
        }
    }
    if role.is_none() && tools.is_empty() {
        ScopeGrant::Insufficient
    } else {
        ScopeGrant::Mapped {
            role,
            tools: if tools.is_empty() { None } else { Some(tools) },
        }
    }
}

fn role_scope(part: &str) -> Option<(&'static str, u8)> {
    match part {
        "atlas:role:admin" | "admin" => Some(("admin", 3)),
        "atlas:role:operator" | "operator" => Some(("operator", 2)),
        "atlas:role:viewer" | "viewer" => Some(("viewer", 1)),
        _ => None,
    }
}

/// Scopes the running process can stand behind: the three roles, then one `atlas:tool:*` per MCP
/// tool this build is serving (write and data tools appear only when their gates are on).
pub fn supported_scopes(state: &AppState) -> Vec<String> {
    #[cfg(feature = "mcp")]
    {
        crate::mcp::scopes_supported(state)
    }
    #[cfg(not(feature = "mcp"))]
    {
        let _ = state;
        ROLE_SCOPES.iter().map(|s| (*s).to_string()).collect()
    }
}

/// Space-delimited `scope` parameter for `WWW-Authenticate`.
pub fn bearer_scope(state: &AppState) -> String {
    supported_scopes(state).join(" ")
}

/// Which RFC 6750 `error` a challenge carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeKind {
    /// No token was sent.
    None,
    /// A token was sent and refused (bad signature, expired, inactive, …).
    InvalidToken,
    /// The token is valid and its `scope` claim names no Atlas role or tool.
    InsufficientScope,
}

/// The RFC 9728 document. `resource` is the origin for the root form and the MCP endpoint URL for
/// the path-inserted form (RFC 9728 section 3.3: it must equal the resource the URL was built
/// from). `authorization_servers` lists the IdP issuer and, when the lab token endpoint is on, this
/// server's origin. `scopes` is [`supported_scopes`].
pub fn metadata_document(
    resource: &str,
    authorization_servers: &[String],
    scopes: &[String],
) -> Value {
    let mut doc = json!({
        "resource": resource,
        "bearer_methods_supported": ["header"],
        "resource_name": "Zyvor Atlas MCP server",
        "resource_documentation": "https://github.com/zyvorai/zyvor-atlas/blob/main/docs/MCP.md"
    });
    if !authorization_servers.is_empty() {
        doc["authorization_servers"] = json!(authorization_servers);
    }
    if !scopes.is_empty() {
        doc["scopes_supported"] = json!(scopes);
    }
    doc
}

/// The `WWW-Authenticate` value for a 401 or an insufficient-scope 403 on the MCP endpoint.
/// `scope` is the space-delimited list from [`bearer_scope`]; characters outside the scope alphabet
/// are dropped so a header parameter cannot break out of its quotes.
pub fn challenge(
    public_url: Option<&str>,
    headers: &HeaderMap,
    kind: ChallengeKind,
    scope: &str,
) -> HeaderValue {
    let mut parts: Vec<String> = Vec::new();
    match kind {
        ChallengeKind::None => {}
        ChallengeKind::InvalidToken => parts.push("error=\"invalid_token\"".into()),
        ChallengeKind::InsufficientScope => parts.push("error=\"insufficient_scope\"".into()),
    }
    let clean = scope
        .split_whitespace()
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
        })
        .collect::<Vec<_>>()
        .join(" ");
    if !clean.is_empty() {
        parts.push(format!("scope=\"{clean}\""));
    }
    if let Some(url) = metadata_url(public_url, headers) {
        parts.push(format!("resource_metadata=\"{url}\""));
    }
    let v = if parts.is_empty() {
        "Bearer".to_string()
    } else {
        format!("Bearer {}", parts.join(", "))
    };
    // Every byte is from the validated authority plus the scope alphabet, so this cannot fail;
    // fall back to the bare scheme rather than panic if that ever stops being true.
    HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
}

fn respond(state: &AppState, headers: &HeaderMap, path: &str) -> Response {
    let Some((origin, configured)) = origin(state.public_url.as_deref(), headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": "BAD_REQUEST", "message":
                "cannot determine this server's URL: send a plain Host header or set ATLAS_PUBLIC_URL" } })),
        )
            .into_response();
    };
    let resource = format!("{origin}{path}");
    // Only the document whose resource is the MCP endpoint names an authorization server: that is
    // the URL the IdP validator's audience is built from. The lab token endpoint's issuer is this
    // origin (it mints ordinary Atlas JWTs; see mcp_as).
    let mut servers = Vec::new();
    if path == MCP_ENDPOINT_PATH {
        if let Some(idp) = &state.mcp_idp {
            servers.push(idp.issuer().to_string());
        }
        if state.mcp_as.is_some() {
            servers.push(origin.clone());
        }
    }
    let scopes = supported_scopes(state);
    let cache = if configured {
        "public, max-age=300"
    } else {
        "no-store"
    };
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static(cache))],
        Json(metadata_document(&resource, &servers, &scopes)),
    )
        .into_response()
}

/// `GET /.well-known/oauth-protected-resource` (resource = the origin).
pub async fn root_metadata(State(s): State<AppState>, headers: HeaderMap) -> Response {
    respond(&s, &headers, "")
}

/// `GET /.well-known/oauth-protected-resource/api/atlas/v1/mcp` (resource = the MCP endpoint).
pub async fn mcp_metadata(State(s): State<AppState>, headers: HeaderMap) -> Response {
    respond(&s, &headers, MCP_ENDPOINT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn public_url_is_an_origin_only() {
        assert_eq!(
            parse_public_url("https://atlas.example.com").unwrap(),
            "https://atlas.example.com"
        );
        assert_eq!(
            parse_public_url(" https://atlas.example.com:8443/ ").unwrap(),
            "https://atlas.example.com:8443"
        );
        assert_eq!(
            parse_public_url("http://10.0.0.5:5110").unwrap(),
            "http://10.0.0.5:5110"
        );
        for bad in [
            "atlas.example.com",
            "ftp://x",
            "https://",
            "https://a/b",
            "https://a?x=1",
            "https://a#f",
            "https://u:p@a",
            "https://a\"b",
            "https://a b",
        ] {
            assert!(parse_public_url(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn configured_url_wins_over_request_headers() {
        let hdrs = h(&[("host", "evil.example"), ("x-forwarded-proto", "http")]);
        assert_eq!(
            origin(Some("https://atlas.example.com"), &hdrs),
            Some(("https://atlas.example.com".into(), true))
        );
    }

    #[test]
    fn host_fallback_is_validated() {
        assert_eq!(
            origin(None, &h(&[("host", "atlas.lab:30510")])),
            Some(("http://atlas.lab:30510".into(), false))
        );
        assert_eq!(
            origin(
                None,
                &h(&[("host", "atlas.lab"), ("x-forwarded-proto", "https")])
            ),
            Some(("https://atlas.lab".into(), false))
        );
        // Anything but http/https is ignored.
        assert_eq!(
            origin(
                None,
                &h(&[("host", "atlas.lab"), ("x-forwarded-proto", "javascript")])
            ),
            Some(("http://atlas.lab".into(), false))
        );
        // A Host that could break out of a URL or a quoted parameter is refused.
        for bad in ["a\"b", "a/b", "a b", "a@b", "a,b", "", "-a"] {
            let mut m = HeaderMap::new();
            if let Ok(v) = HeaderValue::from_str(bad) {
                m.insert(header::HOST, v);
            }
            assert_eq!(origin(None, &m), None, "{bad:?}");
        }
        assert_eq!(origin(None, &HeaderMap::new()), None);
    }

    #[test]
    fn challenge_shapes() {
        let hdrs = h(&[("host", "atlas.lab:30510")]);
        let meta = "http://atlas.lab:30510/.well-known/oauth-protected-resource/api/atlas/v1/mcp";
        assert_eq!(
            challenge(None, &hdrs, ChallengeKind::None, "atlas:role:viewer atlas:role:operator")
                .to_str()
                .unwrap(),
            format!("Bearer scope=\"atlas:role:viewer atlas:role:operator\", resource_metadata=\"{meta}\"")
        );
        assert_eq!(
            challenge(
                Some("https://a.example"),
                &hdrs,
                ChallengeKind::InvalidToken,
                "atlas:role:viewer"
            )
            .to_str()
            .unwrap(),
            "Bearer error=\"invalid_token\", scope=\"atlas:role:viewer\", resource_metadata=\"https://a.example/.well-known/oauth-protected-resource/api/atlas/v1/mcp\""
        );
        assert_eq!(
            challenge(
                None,
                &hdrs,
                ChallengeKind::InsufficientScope,
                "atlas:role:operator"
            )
            .to_str()
            .unwrap(),
            format!("Bearer error=\"insufficient_scope\", scope=\"atlas:role:operator\", resource_metadata=\"{meta}\"")
        );
        // A token that contains a quote is dropped whole, so it cannot break out of the parameter.
        assert_eq!(
            challenge(
                None,
                &HeaderMap::new(),
                ChallengeKind::None,
                "atlas:role:viewer\"evil"
            )
            .to_str()
            .unwrap(),
            "Bearer"
        );
        // No usable URL and no safe scope: bare scheme, never a malformed parameter.
        assert_eq!(
            challenge(None, &HeaderMap::new(), ChallengeKind::None, "")
                .to_str()
                .unwrap(),
            "Bearer"
        );
        assert_eq!(
            challenge(None, &HeaderMap::new(), ChallengeKind::InvalidToken, "")
                .to_str()
                .unwrap(),
            "Bearer error=\"invalid_token\""
        );
    }

    #[test]
    fn document_lists_scopes_and_only_real_authorization_servers() {
        let scopes = vec![
            "atlas:role:viewer".to_string(),
            "atlas:tool:list_clusters".to_string(),
        ];
        let d = metadata_document("https://a.example/api/atlas/v1/mcp", &[], &scopes);
        assert_eq!(d["resource"], "https://a.example/api/atlas/v1/mcp");
        assert_eq!(d["bearer_methods_supported"], json!(["header"]));
        assert!(d.get("authorization_servers").is_none());
        assert_eq!(
            d["scopes_supported"],
            json!(["atlas:role:viewer", "atlas:tool:list_clusters"])
        );
        let servers = vec!["https://idp.example.com".to_string()];
        let d = metadata_document("https://a.example/api/atlas/v1/mcp", &servers, &scopes);
        assert_eq!(
            d["authorization_servers"],
            json!(["https://idp.example.com"])
        );
    }

    #[test]
    fn scope_claim_maps_to_a_role_or_a_tool_allowlist() {
        assert_eq!(map_scope_str(None), ScopeGrant::Absent);
        assert_eq!(map_scope_value(None), ScopeGrant::Absent);
        assert_eq!(map_scope_str(Some("   ")), ScopeGrant::Insufficient);
        assert_eq!(
            map_scope_str(Some("openid profile email")),
            ScopeGrant::Insufficient
        );
        assert_eq!(
            map_scope_str(Some("openid atlas:role:operator")),
            ScopeGrant::Mapped {
                role: Some("operator"),
                tools: None
            }
        );
        // Several role scopes: the highest wins. A bare role name is accepted too.
        assert_eq!(
            map_scope_str(Some("atlas:role:viewer admin")),
            ScopeGrant::Mapped {
                role: Some("admin"),
                tools: None
            }
        );
        // A repeated name is kept once. A well-formed name that is not a current tool is still an
        // allowlist entry (it simply never matches). A name that is not tool-shaped is dropped.
        assert_eq!(
            map_scope_str(Some(
                "atlas:tool:list_alerts atlas:tool:list_alerts atlas:tool:nope atlas:tool:Bad"
            )),
            ScopeGrant::Mapped {
                role: None,
                tools: Some(vec!["list_alerts".into(), "nope".into()])
            }
        );
        let both = map_scope_value(Some(&json!([
            "openid",
            "atlas:role:viewer",
            "atlas:tool:cluster_health"
        ])));
        assert_eq!(
            both,
            ScopeGrant::Mapped {
                role: Some("viewer"),
                tools: Some(vec!["cluster_health".into()])
            }
        );
        assert_eq!(map_scope_value(Some(&json!(1))), ScopeGrant::Insufficient);
        assert_eq!(map_scope_value(Some(&Value::Null)), ScopeGrant::Absent);
        assert!(tool_scope_name("list_clusters").is_some());
        assert!(tool_scope_name("List").is_none());
        assert!(tool_scope_name("").is_none());
    }
}
