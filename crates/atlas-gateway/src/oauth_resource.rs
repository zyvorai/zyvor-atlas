// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! RFC 9728 OAuth 2.0 Protected Resource Metadata for the MCP endpoint, plus the `WWW-Authenticate`
//! challenge `auth_middleware` adds to 401s under `/api/atlas/v1/mcp`.
//!
//! What this is, and is not. The MCP authorization spec (2026-07-28) says an MCP server MUST serve
//! Protected Resource Metadata and answer unauthenticated requests with
//! `WWW-Authenticate: Bearer resource_metadata="<url>"`. This module does exactly that and nothing
//! more. Atlas has **no OAuth authorization server**: tokens are HS256 JWTs Atlas mints itself
//! (`POST /auth/login`, `POST /auth/tokens`, the OIDC console login), so the metadata document
//! carries no `authorization_servers` and no `scopes_supported`. In particular it does not point at
//! the OIDC issuer: that is an identity provider for the console's browser login, and a token it
//! issues is not accepted by Atlas (only Atlas-signed JWTs are), so advertising it would send an
//! MCP client down a flow that ends in a 401. Tokens carry no `aud`, so the RFC 8707 audience check
//! the spec asks of a resource server is not performed.
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

/// The RFC 9728 document. `resource` is the origin for the root form and the MCP endpoint URL for
/// the path-inserted form (RFC 9728 section 3.3: it must equal the resource the URL was built
/// from). No `authorization_servers`, no `scopes_supported`: Atlas has neither (module docs).
pub fn metadata_document(resource: &str) -> Value {
    json!({
        "resource": resource,
        "bearer_methods_supported": ["header"],
        "resource_name": "Zyvor Atlas MCP server",
        "resource_documentation": "https://github.com/zyvorai/zyvor-atlas/blob/main/docs/MCP.md"
    })
}

/// The `WWW-Authenticate` value for a 401 on the MCP endpoint. `invalid_token` is set when a token
/// was presented and refused (RFC 6750 section 3.1) and left out when none was sent.
pub fn challenge(
    public_url: Option<&str>,
    headers: &HeaderMap,
    invalid_token: bool,
) -> HeaderValue {
    let mut v = String::from("Bearer");
    let mut sep = " ";
    if invalid_token {
        v.push_str(" error=\"invalid_token\"");
        sep = ", ";
    }
    if let Some(url) = metadata_url(public_url, headers) {
        v.push_str(&format!("{sep}resource_metadata=\"{url}\""));
    }
    // Every byte is from the validated authority plus constants, so this cannot fail; fall back to
    // the bare scheme rather than panic if that ever stops being true.
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
    let cache = if configured {
        "public, max-age=300"
    } else {
        "no-store"
    };
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static(cache))],
        Json(metadata_document(&resource)),
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
        assert_eq!(
            challenge(None, &hdrs, false).to_str().unwrap(),
            "Bearer resource_metadata=\"http://atlas.lab:30510/.well-known/oauth-protected-resource/api/atlas/v1/mcp\""
        );
        assert_eq!(
            challenge(Some("https://a.example"), &hdrs, true).to_str().unwrap(),
            "Bearer error=\"invalid_token\", resource_metadata=\"https://a.example/.well-known/oauth-protected-resource/api/atlas/v1/mcp\""
        );
        // No usable URL: bare scheme, never a malformed parameter.
        assert_eq!(
            challenge(None, &HeaderMap::new(), false).to_str().unwrap(),
            "Bearer"
        );
        assert_eq!(
            challenge(None, &HeaderMap::new(), true).to_str().unwrap(),
            "Bearer error=\"invalid_token\""
        );
    }

    #[test]
    fn document_has_only_what_atlas_can_stand_behind() {
        let d = metadata_document("https://a.example/api/atlas/v1/mcp");
        assert_eq!(d["resource"], "https://a.example/api/atlas/v1/mcp");
        assert_eq!(d["bearer_methods_supported"], json!(["header"]));
        assert!(d.get("authorization_servers").is_none());
        assert!(d.get("scopes_supported").is_none());
    }
}
