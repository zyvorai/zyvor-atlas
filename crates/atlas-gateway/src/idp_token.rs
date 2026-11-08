// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Identity-provider JWT access tokens on the MCP endpoint, and nowhere else.
//!
//! The MCP authorization spec (2026-07-28) has an MCP server act as an OAuth 2.1 *resource server*: it
//! validates access tokens issued by an authorization server it names in its Protected Resource
//! Metadata, and checks that a token was issued for it (RFC 8707 audience). This module is that
//! validation. It is **off unless `ATLAS_MCP_IDP_ISSUER` is set**, and then it only ever applies to
//! `/api/atlas/v1/mcp` (`auth::auth_middleware` is the single caller); REST, gRPC and `/metrics`
//! never see an IdP token. Atlas still has no authorization server of its own.
//!
//! # Configuration (read once at startup, see [`from_env`])
//! | Variable | Meaning |
//! |---|---|
//! | `ATLAS_MCP_IDP_ISSUER` | The issuer URL (`iss` must equal it byte for byte). Unset: feature off. |
//! | `ATLAS_MCP_IDP_JWKS_URL` | Where to fetch the signing keys. Unset: `<issuer>/.well-known/openid-configuration` is fetched and its `jwks_uri` used (its `issuer` must equal the configured one). |
//! | `ATLAS_MCP_IDP_MAX_LIFETIME_SECS` | Longest validity accepted from a token (default 86400). |
//! | `ATLAS_MCP_IDP_ALLOW_HTTP` | `1` lets the issuer, discovery and JWKS URLs be plain `http://` to a non-loopback host (lab IdPs such as `deploy/dex-lab`). Otherwise `https` is required; loopback `http` is always allowed. |
//! | `ATLAS_OIDC_ADMIN_GROUP`, `ATLAS_OIDC_OPERATOR_GROUP`, `ATLAS_OIDC_TENANT_CLAIM` | The same role and tenant mapping the console SSO login uses ([`atlas_common::config::IdentityMapping`], [`crate::auth::role_for_groups`]). |
//!
//! The audience is the MCP endpoint URL, `<ATLAS_PUBLIC_URL>/api/atlas/v1/mcp` (the `resource` of the
//! path-inserted metadata document). The feature **refuses to enable, and the gateway refuses to
//! start, when `ATLAS_PUBLIC_URL` is unset**: deriving the audience from request headers would let a
//! forged `Host` choose what a token has to say.
//!
//! # What is checked
//! * The JOSE header's `alg` must be `RS256` or `ES256` and the token must carry a `kid`. The key is
//!   looked up by `kid` in the issuer's JWKS and the algorithm is **pinned from the key** (an RSA key
//!   verifies only RS256, a P-256 key only ES256). `none`, every `HS*`, `PS*`/`ES384`/`EdDSA`, and a
//!   `jwk`/`jku`/`x5u` header (never consulted) can therefore not be used, and a symmetric (`oct`) JWKS
//!   entry is never imported, which closes the "public key as HMAC secret" confusion.
//! * Signature, `iss` (a single string equal to the configured issuer), `aud` (a string or an array
//!   containing the MCP URL), `exp` (required), `nbf` (when present), `sub` (required), with 30 s of leeway.
//! * Lifetime cap: `exp` may be at most `ATLAS_MCP_IDP_MAX_LIFETIME_SECS` in the future, and when the
//!   token has an `iat`, `exp - iat` may not exceed it either. A token cannot be revoked at the IdP
//!   and honoured here until it expires, so the cap bounds that window.
//!
//! # Keys
//! The JWKS is fetched lazily on first use (an IdP that is down never stops the gateway from starting;
//! that request gets a 401 and a log line), cached for one hour, and refetched when a token names an
//! unknown `kid` (key rotation) at most once per 30 s whatever the number of requests, failures
//! included, so unknown-`kid` traffic cannot make the gateway hammer the IdP. A successful fetch
//! replaces the whole key set. If a refetch fails, keys already cached are still used for up to one
//! more hour (stale-if-error), then requests get a 401 again. Responses are capped at 256 KiB, redirects
//! are not followed, and RSA keys under 2048 bits, keys with an `alg` or `use` that does not fit, keys
//! without a `kid`, and duplicate `kid`s are dropped.
//!
//! # The resulting actor
//! `id` is `idp-<8 hex of sha256(issuer)>:<sub>` so an IdP subject can never collide with an Atlas
//! subject (audit rows, rate limits, workspaces owned by the actor). The role comes from the token's
//! `groups` claim (never from a `role` claim), the tenant from `ATLAS_OIDC_TENANT_CLAIM` (else
//! `global`). The actor is **unscoped**: no tool allowlist and no resource scope, only its role limits
//! it, exactly like an Atlas operator token. A `jti` is recorded as `<tag>:<jti>` and checked against
//! the same deny-list `POST /auth/tokens/{jti}/revoke` writes to (revoke the namespaced value); a token
//! with no `jti` cannot be revoked here, only by the IdP and the lifetime cap.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use atlas_common::config::IdentityMapping;
use base64::Engine;
use jsonwebtoken::{
    decode, decode_header, errors::ErrorKind, get_current_timestamp, Algorithm, DecodingKey,
    Validation,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::oauth_resource::MCP_ENDPOINT_PATH;

/// Largest token accepted, so a hostile `Authorization` header cannot cost more than this to parse.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Largest JWKS or discovery response read.
const MAX_FETCH_BYTES: usize = 256 * 1024;
/// More keys than this in a JWKS are ignored (a real IdP has a handful).
const MAX_KEYS: usize = 64;
/// Smallest RSA modulus imported, in bits.
const MIN_RSA_BITS: usize = 2048;
const LEEWAY_SECS: u64 = 30;
/// Default for `ATLAS_MCP_IDP_MAX_LIFETIME_SECS`.
pub const DEFAULT_MAX_LIFETIME_SECS: u64 = 86_400;

/// Cache timing. [`Timing::default`] is what production uses; tests shorten it.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long fetched keys are trusted without a refetch.
    pub ttl: Duration,
    /// At most one fetch attempt (successful or not) per this interval.
    pub min_refetch: Duration,
    /// Per-request timeout of the JWKS and discovery fetches.
    pub http_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(3600),
            min_refetch: Duration::from_secs(30),
            http_timeout: Duration::from_secs(5),
        }
    }
}

/// Validated `ATLAS_MCP_IDP_*` settings.
#[derive(Debug, Clone)]
pub struct IdpSettings {
    /// Expected `iss`, exactly as configured.
    pub issuer: String,
    pub jwks_url: Option<String>,
    /// The MCP endpoint URL every token's `aud` must contain.
    pub audience: String,
    pub allow_http: bool,
    pub max_lifetime_secs: u64,
    pub mapping: IdentityMapping,
}

impl IdpSettings {
    /// Read the settings through `get` (the environment in production, a map in tests). `Ok(None)` when
    /// `ATLAS_MCP_IDP_ISSUER` is unset or blank; `Err` with an operator-facing message when it is set
    /// and anything is wrong, including a missing `ATLAS_PUBLIC_URL`.
    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        public_url: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let var = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(issuer) = var("ATLAS_MCP_IDP_ISSUER") else {
            return Ok(None);
        };
        let Some(public_url) = public_url else {
            return Err("ATLAS_MCP_IDP_ISSUER is set but ATLAS_PUBLIC_URL is not: the token audience is \
                 the MCP endpoint URL (<ATLAS_PUBLIC_URL>/api/atlas/v1/mcp) and is never taken from \
                 request headers, so set ATLAS_PUBLIC_URL (or unset ATLAS_MCP_IDP_ISSUER)"
                .into());
        };
        let allow_http = matches!(
            var("ATLAS_MCP_IDP_ALLOW_HTTP").as_deref(),
            Some("1" | "true" | "yes")
        );
        let issuer_url =
            check_url(&issuer, allow_http).map_err(|e| format!("ATLAS_MCP_IDP_ISSUER: {e}"))?;
        if issuer_url.query().is_some() || issuer_url.fragment().is_some() {
            return Err("ATLAS_MCP_IDP_ISSUER: must not have a query or fragment".into());
        }
        let jwks_url = var("ATLAS_MCP_IDP_JWKS_URL");
        if let Some(u) = &jwks_url {
            check_url(u, allow_http).map_err(|e| format!("ATLAS_MCP_IDP_JWKS_URL: {e}"))?;
        }
        let max_lifetime_secs = match var("ATLAS_MCP_IDP_MAX_LIFETIME_SECS") {
            None => DEFAULT_MAX_LIFETIME_SECS,
            Some(v) => match v.parse::<u64>() {
                Ok(n) if (1..=31 * 86_400).contains(&n) => n,
                _ => {
                    return Err(format!(
                        "ATLAS_MCP_IDP_MAX_LIFETIME_SECS: '{v}' is not a number of seconds from 1 to 2678400"
                    ))
                }
            },
        };
        Ok(Some(Self {
            issuer,
            jwks_url,
            audience: format!("{}{MCP_ENDPOINT_PATH}", public_url.trim_end_matches('/')),
            allow_http,
            max_lifetime_secs,
            mapping: IdentityMapping::from_lookup(get),
        }))
    }
}

/// Build the validator from the environment, for `startup::build_state`. `Ok(None)` when the feature is
/// off. Errors (a bad setting, no `ATLAS_PUBLIC_URL`) stop the gateway; an unreachable IdP does not.
pub fn from_env(
    public_url: Option<&str>,
    auth_required: bool,
) -> anyhow::Result<Option<std::sync::Arc<IdpValidator>>> {
    let Some(settings) = IdpSettings::from_lookup(|k| std::env::var(k).ok(), public_url)
        .map_err(|e| anyhow::anyhow!(e))?
    else {
        return Ok(None);
    };
    let validator = IdpValidator::new(settings, Timing::default())?;
    if !auth_required {
        tracing::warn!(
            "ATLAS_MCP_IDP_ISSUER is set but ATLAS_AUTH_REQUIRED is off: no token is checked at all"
        );
    }
    if !cfg!(feature = "mcp") {
        tracing::warn!("ATLAS_MCP_IDP_ISSUER is set but this build has no `mcp` feature: there is no MCP endpoint");
    }
    tracing::info!(
        issuer = %validator.settings.issuer,
        audience = %validator.settings.audience,
        max_lifetime_secs = validator.settings.max_lifetime_secs,
        "MCP IdP access tokens enabled (keys are fetched on first use)"
    );
    Ok(Some(std::sync::Arc::new(validator)))
}

/// `https` to anything, or `http` to a loopback host (or any host when `allow_http`); no credentials
/// in the URL. Used for the issuer, the JWKS URL and the `jwks_uri` a discovery document returns.
fn check_url(raw: &str, allow_http: bool) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("not a valid URL ({e})"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not contain credentials".into());
    }
    let Some(host) = url.host_str() else {
        return Err("has no host".into());
    };
    match url.scheme() {
        "https" => Ok(url),
        "http" if allow_http || is_loopback(host) => Ok(url),
        "http" => Err(
            "must be https (plain http is allowed only to a loopback host, or anywhere with \
             ATLAS_MCP_IDP_ALLOW_HTTP=1)"
                .into(),
        ),
        other => Err(format!("scheme '{other}' is not allowed, use https")),
    }
}

fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// True when the token's header names an algorithm this module validates (`RS256`/`ES256`). The
/// middleware uses it to route: anything else, including an unparsable header, `alg: none` and `HS*`,
/// goes to the ordinary Atlas validation and is refused there.
pub fn is_idp_token(token: &str) -> bool {
    decode_header(token).is_ok_and(|h| matches!(h.alg, Algorithm::RS256 | Algorithm::ES256))
}

/// Why a token was refused. The text goes to the client in the 401 body and to the log, so it never
/// contains a token, a key or a URL.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdpError {
    #[error("token is too large")]
    TooLarge,
    #[error("token is malformed")]
    Malformed,
    #[error("token algorithm is not accepted (RS256 and ES256 only)")]
    UnsupportedAlgorithm,
    #[error("token has no usable kid header")]
    MissingKid,
    #[error("token is signed with an unknown key")]
    UnknownKey,
    #[error("token algorithm does not match its signing key")]
    KeyAlgorithmMismatch,
    #[error("identity provider keys are unavailable")]
    KeysUnavailable,
    #[error("token signature is invalid")]
    BadSignature,
    #[error("token has expired")]
    Expired,
    #[error("token is not valid yet")]
    NotYetValid,
    #[error("token issuer does not match")]
    WrongIssuer,
    #[error("token audience does not include this MCP server")]
    WrongAudience,
    #[error("token is missing the '{0}' claim")]
    MissingClaim(String),
    #[error("token lifetime exceeds the configured maximum")]
    LifetimeTooLong,
    #[error("token subject is not acceptable")]
    BadSubject,
    #[error("token is not acceptable")]
    Invalid,
}

fn map_jwt_error(e: &jsonwebtoken::errors::Error) -> IdpError {
    match e.kind() {
        ErrorKind::ExpiredSignature => IdpError::Expired,
        ErrorKind::ImmatureSignature => IdpError::NotYetValid,
        ErrorKind::InvalidIssuer => IdpError::WrongIssuer,
        ErrorKind::InvalidAudience => IdpError::WrongAudience,
        ErrorKind::InvalidSignature => IdpError::BadSignature,
        ErrorKind::MissingRequiredClaim(c) => IdpError::MissingClaim(c.clone()),
        ErrorKind::InvalidAlgorithm
        | ErrorKind::MissingAlgorithm
        | ErrorKind::UnsupportedAlgorithm => IdpError::UnsupportedAlgorithm,
        _ => IdpError::Invalid,
    }
}

/// A validated token's identity, ready to become an `auth::Actor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdpIdentity {
    /// `idp-<tag>:<sub>`.
    pub actor_id: String,
    /// `admin`, `operator` or `viewer`.
    pub role: &'static str,
    pub tenant_id: String,
    /// `<tag>:<jti>`, or empty when the token has no `jti` (not revocable here).
    pub jti: String,
}

#[derive(Clone)]
struct CachedKey {
    alg: Algorithm,
    key: DecodingKey,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, CachedKey>,
    /// When `keys` was last replaced by a successful fetch.
    fetched_at: Option<Instant>,
    /// When a fetch was last started (successful or not): the refetch rate limit.
    last_attempt: Option<Instant>,
    /// The JWKS URL, from configuration or discovery.
    jwks_url: Option<String>,
}

/// Validates IdP access tokens against a cached JWKS. See the module docs.
pub struct IdpValidator {
    settings: IdpSettings,
    /// `idp-<8 hex of sha256(issuer)>`.
    tag: String,
    http: reqwest::Client,
    timing: Timing,
    cache: Mutex<KeyCache>,
    /// Serialises refetches, so a burst of requests with an unknown `kid` makes one attempt.
    refresh: tokio::sync::Mutex<()>,
}

impl IdpValidator {
    pub fn new(settings: IdpSettings, timing: Timing) -> anyhow::Result<Self> {
        let digest = Sha256::digest(settings.issuer.as_bytes());
        let tag = format!(
            "idp-{}",
            digest
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        // No redirects: a JWKS or discovery URL that redirects is refused rather than followed to an
        // address nobody configured.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timing.http_timeout)
            .connect_timeout(timing.http_timeout.min(Duration::from_secs(3)))
            .user_agent(concat!("atlas-gateway/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let cache = KeyCache {
            jwks_url: settings.jwks_url.clone(),
            ..KeyCache::default()
        };
        Ok(Self {
            settings,
            tag,
            http,
            timing,
            cache: Mutex::new(cache),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    /// The configured issuer, for the metadata document.
    pub fn issuer(&self) -> &str {
        &self.settings.issuer
    }

    /// The audience a token must carry.
    pub fn audience(&self) -> &str {
        &self.settings.audience
    }

    /// The namespace prefix of this issuer's actor ids (`idp-<8 hex>`).
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Validate `token` and map it to an Atlas identity.
    pub async fn validate(&self, token: &str) -> Result<IdpIdentity, IdpError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(IdpError::TooLarge);
        }
        let header = decode_header(token).map_err(|_| IdpError::Malformed)?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(IdpError::UnsupportedAlgorithm);
        }
        let kid = header
            .kid
            .as_deref()
            .filter(|k| !k.is_empty() && k.len() <= 256)
            .ok_or(IdpError::MissingKid)?;
        let key = self.key_for(kid).await?;
        // The algorithm is the key's, not the token's to choose.
        if key.alg != header.alg {
            return Err(IdpError::KeyAlgorithmMismatch);
        }
        let mut v = Validation::new(key.alg);
        v.leeway = LEEWAY_SECS;
        v.validate_exp = true;
        v.validate_nbf = true;
        v.validate_aud = true;
        v.set_issuer(&[&self.settings.issuer]);
        v.set_audience(&[&self.settings.audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let data = decode::<Value>(token, &key.key, &v).map_err(|e| map_jwt_error(&e))?;
        self.identity(&data.claims, get_current_timestamp())
    }

    /// Checks beyond `jsonwebtoken`'s, and the mapping to an identity. Split out so the claim rules are
    /// unit-testable without a signature.
    fn identity(&self, claims: &Value, now: u64) -> Result<IdpIdentity, IdpError> {
        // `jsonwebtoken` accepts an `iss` array that contains the issuer; an access token has one issuer.
        if claims.get("iss").and_then(Value::as_str) != Some(self.settings.issuer.as_str()) {
            return Err(IdpError::WrongIssuer);
        }
        let max = self.settings.max_lifetime_secs;
        let exp = claims
            .get("exp")
            .and_then(Value::as_f64)
            .map(|f| f.max(0.0) as u64)
            .ok_or_else(|| IdpError::MissingClaim("exp".into()))?;
        if exp > now.saturating_add(max).saturating_add(LEEWAY_SECS) {
            return Err(IdpError::LifetimeTooLong);
        }
        if let Some(iat) = claims
            .get("iat")
            .and_then(Value::as_f64)
            .map(|f| f.max(0.0) as u64)
        {
            if exp.saturating_sub(iat) > max.saturating_add(LEEWAY_SECS) {
                return Err(IdpError::LifetimeTooLong);
            }
        }
        let sub = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .ok_or(IdpError::BadSubject)?;

        let groups: Vec<String> = match claims.get("groups") {
            Some(Value::Array(a)) => a
                .iter()
                .take(512)
                .filter_map(|g| g.as_str().map(str::to_owned))
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        let role = crate::auth::role_for_groups(
            &groups,
            &self.settings.mapping.admin_groups,
            &self.settings.mapping.operator_groups,
        );
        let tenant_id = self
            .settings
            .mapping
            .tenant_claim
            .as_deref()
            .and_then(|c| claims.get(c)?.as_str())
            .filter(|t| !t.is_empty() && t.len() <= 128 && !t.chars().any(char::is_control))
            .map_or_else(|| "global".to_string(), str::to_owned);
        let jti = match claims
            .get("jti")
            .and_then(Value::as_str)
            .filter(|j| !j.is_empty())
        {
            None => String::new(),
            Some(j) if j.len() <= 200 => format!("{}:{j}", self.tag),
            Some(j) => format!(
                "{}:sha256-{}",
                self.tag,
                Sha256::digest(j.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
        };
        Ok(IdpIdentity {
            actor_id: format!("{}:{sub}", self.tag),
            role,
            tenant_id,
            jti,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KeyCache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached key for `kid` if the cache is younger than `max_age`.
    fn cached(&self, kid: &str, max_age: Duration) -> Option<CachedKey> {
        let c = self.lock();
        if c.fetched_at?.elapsed() >= max_age {
            return None;
        }
        c.keys.get(kid).cloned()
    }

    /// Claim the right to make a fetch attempt: true at most once per `min_refetch`.
    fn begin_attempt(&self) -> bool {
        let mut c = self.lock();
        if c.last_attempt
            .is_some_and(|t| t.elapsed() < self.timing.min_refetch)
        {
            return false;
        }
        c.last_attempt = Some(Instant::now());
        true
    }

    async fn key_for(&self, kid: &str) -> Result<CachedKey, IdpError> {
        if let Some(k) = self.cached(kid, self.timing.ttl) {
            return Ok(k);
        }
        // Single flight: whoever holds the gate fetches; the others wake up to a fresh cache or to a
        // closed rate-limit window.
        let _gate = self.refresh.lock().await;
        if let Some(k) = self.cached(kid, self.timing.ttl) {
            return Ok(k);
        }
        let mut fetch_failed = false;
        if self.begin_attempt() {
            match self.fetch_keys().await {
                Ok((keys, jwks_url)) => {
                    let n = keys.len();
                    {
                        let mut c = self.lock();
                        c.keys = keys;
                        c.fetched_at = Some(Instant::now());
                        c.jwks_url = Some(jwks_url);
                    }
                    if n == 0 {
                        tracing::warn!(issuer = %self.settings.issuer, "identity provider JWKS has no usable signing keys");
                    } else {
                        tracing::debug!(issuer = %self.settings.issuer, keys = n, "identity provider JWKS refreshed");
                    }
                    if let Some(k) = self.cached(kid, self.timing.ttl) {
                        return Ok(k);
                    }
                }
                Err(e) => {
                    fetch_failed = true;
                    tracing::warn!(issuer = %self.settings.issuer, "identity provider keys could not be fetched: {e}");
                }
            }
        }
        // Not found. A key we already hold stays usable for one more TTL while the IdP cannot be reached
        // (this also covers a refetch skipped by the rate limit right after a failed one); otherwise say
        // whether the key set is unavailable or merely lacks this key.
        if let Some(k) = self.cached(kid, self.timing.ttl.saturating_mul(2)) {
            return Ok(k);
        }
        let fresh = self
            .lock()
            .fetched_at
            .is_some_and(|t| t.elapsed() < self.timing.ttl);
        if fetch_failed || !fresh {
            Err(IdpError::KeysUnavailable)
        } else {
            Err(IdpError::UnknownKey)
        }
    }

    async fn fetch_keys(&self) -> Result<(HashMap<String, CachedKey>, String), String> {
        let known = self.lock().jwks_url.clone();
        let jwks_url = match known {
            Some(u) => u,
            None => self.discover().await?,
        };
        let body = self.get_limited(&jwks_url).await?;
        let doc: Value =
            serde_json::from_slice(&body).map_err(|e| format!("JWKS is not JSON: {e}"))?;
        Ok((parse_jwks(&doc), jwks_url))
    }

    /// OIDC discovery: `<issuer>/.well-known/openid-configuration` -> `jwks_uri`.
    async fn discover(&self) -> Result<String, String> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.settings.issuer.trim_end_matches('/')
        );
        let body = self.get_limited(&url).await?;
        let doc: Value = serde_json::from_slice(&body)
            .map_err(|e| format!("discovery document is not JSON: {e}"))?;
        // OIDC Discovery 4.3: the document's issuer must equal the one it was fetched for.
        if doc.get("issuer").and_then(Value::as_str) != Some(self.settings.issuer.as_str()) {
            return Err("discovery document's issuer does not match ATLAS_MCP_IDP_ISSUER".into());
        }
        let jwks_uri = doc
            .get("jwks_uri")
            .and_then(Value::as_str)
            .ok_or("discovery document has no jwks_uri")?;
        check_url(jwks_uri, self.settings.allow_http)
            .map_err(|e| format!("discovered jwks_uri: {e}"))?;
        Ok(jwks_uri.to_string())
    }

    /// GET with the size cap and no redirects.
    async fn get_limited(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut resp = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{url} answered {}", resp.status()));
        }
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_FETCH_BYTES as u64)
        {
            return Err(format!(
                "{url} response is larger than {MAX_FETCH_BYTES} bytes"
            ));
        }
        let mut buf = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("reading {url} failed: {e}"))?
        {
            if buf.len() + chunk.len() > MAX_FETCH_BYTES {
                return Err(format!(
                    "{url} response is larger than {MAX_FETCH_BYTES} bytes"
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }
}

/// The usable signature-verification keys of a JWKS document, by `kid`. Tolerant of what it does not
/// understand (one odd key never costs the rest), strict about what it keeps: only RSA >= 2048 bits
/// (RS256) and P-256 (ES256), never a symmetric key, and a key whose `kid` repeats is dropped.
fn parse_jwks(doc: &Value) -> HashMap<String, CachedKey> {
    let mut out: HashMap<String, CachedKey> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let Some(keys) = doc.get("keys").and_then(Value::as_array) else {
        return out;
    };
    for jwk in keys.iter().take(MAX_KEYS) {
        let s = |f: &str| jwk.get(f).and_then(Value::as_str);
        let Some(kid) = s("kid").filter(|k| !k.is_empty() && k.len() <= 256) else {
            continue;
        };
        if s("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let parsed = match s("kty") {
            Some("RSA") if s("alg").is_none_or(|a| a == "RS256") => {
                let (Some(n), Some(e)) = (s("n"), s("e")) else {
                    continue;
                };
                let (n, e) = (n.trim_end_matches('='), e.trim_end_matches('='));
                let Ok(raw_n) = b64.decode(n) else { continue };
                let bits = raw_n.iter().skip_while(|b| **b == 0).count() * 8;
                if bits < MIN_RSA_BITS {
                    continue;
                }
                DecodingKey::from_rsa_components(n, e)
                    .ok()
                    .map(|key| CachedKey {
                        alg: Algorithm::RS256,
                        key,
                    })
            }
            Some("EC") if s("crv") == Some("P-256") && s("alg").is_none_or(|a| a == "ES256") => {
                let (Some(x), Some(y)) = (s("x"), s("y")) else {
                    continue;
                };
                let (x, y) = (x.trim_end_matches('='), y.trim_end_matches('='));
                if b64.decode(x).map(|v| v.len()) != Ok(32)
                    || b64.decode(y).map(|v| v.len()) != Ok(32)
                {
                    continue;
                }
                DecodingKey::from_ec_components(x, y)
                    .ok()
                    .map(|key| CachedKey {
                        alg: Algorithm::ES256,
                        key,
                    })
            }
            // `oct` (symmetric), `OKP`, other curves, other algorithms: not imported.
            _ => None,
        };
        let Some(k) = parsed else { continue };
        if out.insert(kid.to_string(), k).is_some() {
            ambiguous.insert(kid.to_string());
        }
    }
    for kid in ambiguous {
        out.remove(&kid);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    const PUB: &str = "https://atlas.example.com";

    #[test]
    fn feature_is_off_without_an_issuer() {
        assert!(IdpSettings::from_lookup(env(&[]), None).unwrap().is_none());
        assert!(
            IdpSettings::from_lookup(env(&[("ATLAS_MCP_IDP_ISSUER", "  ")]), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn refuses_to_enable_without_a_public_url() {
        let e = IdpSettings::from_lookup(
            env(&[("ATLAS_MCP_IDP_ISSUER", "https://idp.example.com")]),
            None,
        )
        .unwrap_err();
        assert!(e.contains("ATLAS_PUBLIC_URL"), "{e}");
    }

    #[test]
    fn audience_is_the_mcp_url_and_defaults_apply() {
        let s = IdpSettings::from_lookup(
            env(&[
                (
                    "ATLAS_MCP_IDP_ISSUER",
                    "https://idp.example.com/realms/zyvor",
                ),
                ("ATLAS_OIDC_ADMIN_GROUP", "storage-admins, ops"),
                ("ATLAS_OIDC_TENANT_CLAIM", "tenant"),
            ]),
            Some(PUB),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.audience, "https://atlas.example.com/api/atlas/v1/mcp");
        assert_eq!(s.issuer, "https://idp.example.com/realms/zyvor");
        assert_eq!(s.max_lifetime_secs, 86_400);
        assert!(!s.allow_http);
        assert!(s.jwks_url.is_none());
        assert_eq!(s.mapping.admin_groups, ["storage-admins", "ops"]);
        assert_eq!(s.mapping.tenant_claim.as_deref(), Some("tenant"));
    }

    #[test]
    fn url_policy() {
        let try_cfg = |pairs: &[(&str, &str)]| {
            let mut v = vec![("ATLAS_MCP_IDP_ISSUER", "https://idp.example.com")];
            v.extend_from_slice(pairs);
            // later pairs win in tests by being found first
            v.reverse();
            IdpSettings::from_lookup(env(&v), Some(PUB))
        };
        // http to a remote host needs the explicit switch ...
        assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", "http://dex.lab:30556")]).is_err());
        assert!(try_cfg(&[
            ("ATLAS_MCP_IDP_ISSUER", "http://dex.lab:30556"),
            ("ATLAS_MCP_IDP_ALLOW_HTTP", "1")
        ])
        .is_ok());
        // ... but loopback http does not.
        assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", "http://127.0.0.1:5556")]).is_ok());
        assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", "http://localhost:5556")]).is_ok());
        assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", "http://[::1]:5556")]).is_ok());
        // A look-alike is not loopback.
        assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", "http://127.0.0.1.evil.example")]).is_err());
        // Other schemes, credentials, query and fragment, junk.
        for u in [
            "ftp://idp.example.com",
            "file:///etc/passwd",
            "idp.example.com",
            "https://u:p@idp.example.com",
            "https://idp.example.com/?a=1",
            "https://idp.example.com/#f",
        ] {
            assert!(try_cfg(&[("ATLAS_MCP_IDP_ISSUER", u)]).is_err(), "{u}");
        }
        // The JWKS URL follows the same policy.
        assert!(try_cfg(&[("ATLAS_MCP_IDP_JWKS_URL", "http://keys.example.com/jwks")]).is_err());
        assert!(try_cfg(&[("ATLAS_MCP_IDP_JWKS_URL", "https://keys.example.com/jwks")]).is_ok());
    }

    #[test]
    fn lifetime_cap_is_validated() {
        let with = |v: &'static str| {
            IdpSettings::from_lookup(
                env(&[
                    ("ATLAS_MCP_IDP_ISSUER", "https://idp.example.com"),
                    ("ATLAS_MCP_IDP_MAX_LIFETIME_SECS", v),
                ]),
                Some(PUB),
            )
        };
        assert_eq!(with("3600").unwrap().unwrap().max_lifetime_secs, 3600);
        for v in ["0", "-1", "abc", "99999999999"] {
            assert!(with(v).is_err(), "{v}");
        }
    }

    fn validator(extra: &[(&str, &str)]) -> IdpValidator {
        let mut pairs = vec![("ATLAS_MCP_IDP_ISSUER", "https://idp.example.com")];
        pairs.extend_from_slice(extra);
        let s = IdpSettings::from_lookup(env(&pairs), Some(PUB))
            .unwrap()
            .unwrap();
        IdpValidator::new(s, Timing::default()).unwrap()
    }

    #[test]
    fn identity_maps_groups_tenant_and_namespaces_ids() {
        let v = validator(&[
            ("ATLAS_OIDC_ADMIN_GROUP", "admins"),
            ("ATLAS_OIDC_OPERATOR_GROUP", "ops"),
            ("ATLAS_OIDC_TENANT_CLAIM", "tenant"),
        ]);
        let now = 1_000_000;
        let c = |extra: Value| {
            let mut c = json!({"iss": "https://idp.example.com", "sub": "alice", "exp": now + 600});
            c.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            c
        };
        let id = v
            .identity(
                &c(json!({"groups": ["x", "admins"], "tenant": "acme", "jti": "j1"})),
                now,
            )
            .unwrap();
        assert_eq!(id.role, "admin");
        assert_eq!(id.tenant_id, "acme");
        assert_eq!(id.actor_id, format!("{}:alice", v.tag()));
        assert_eq!(id.jti, format!("{}:j1", v.tag()));
        assert!(id.actor_id.starts_with("idp-"));

        assert_eq!(
            v.identity(&c(json!({"groups": ["ops"]})), now)
                .unwrap()
                .role,
            "operator"
        );
        let none = v.identity(&c(json!({})), now).unwrap();
        assert_eq!(
            (none.role, none.tenant_id.as_str(), none.jti.as_str()),
            ("viewer", "global", "")
        );
        // A `role` claim is never trusted; a non-string group is ignored; a bad tenant falls back.
        let id = v
            .identity(
                &c(json!({"role": "admin", "groups": [7, null], "tenant": 5})),
                now,
            )
            .unwrap();
        assert_eq!((id.role, id.tenant_id.as_str()), ("viewer", "global"));
        // A single string `groups` is one group.
        assert_eq!(
            v.identity(&c(json!({"groups": "admins"})), now)
                .unwrap()
                .role,
            "admin"
        );
        // A long jti is hashed, not truncated.
        let long = "j".repeat(500);
        assert!(v
            .identity(&c(json!({"jti": long})), now)
            .unwrap()
            .jti
            .contains("sha256-"));
    }

    #[test]
    fn identity_rejects_what_jsonwebtoken_lets_through() {
        let v = validator(&[]);
        let now = 1_000_000;
        let ok = json!({"iss": "https://idp.example.com", "sub": "a", "exp": now + 60});
        assert!(v.identity(&ok, now).is_ok());
        let mut c = ok.clone();
        c["iss"] = json!(["https://idp.example.com", "https://evil.example"]);
        assert_eq!(v.identity(&c, now), Err(IdpError::WrongIssuer));
        let mut c = ok.clone();
        c["iss"] = json!("https://idp.example.com/");
        assert_eq!(
            v.identity(&c, now),
            Err(IdpError::WrongIssuer),
            "the issuer is compared exactly"
        );
        let mut c = ok.clone();
        c["exp"] = json!(now + 86_400 + 31);
        assert_eq!(v.identity(&c, now), Err(IdpError::LifetimeTooLong));
        let mut c = ok.clone();
        c["exp"] = json!(now + 86_400);
        assert!(v.identity(&c, now).is_ok());
        let mut c = ok.clone();
        c["iat"] = json!(now - 90_000);
        c["exp"] = json!(now + 60);
        assert_eq!(
            v.identity(&c, now),
            Err(IdpError::LifetimeTooLong),
            "exp - iat is capped too"
        );
        for sub in [json!(""), json!(7), json!("a\nb"), json!("s".repeat(257))] {
            let mut c = ok.clone();
            c["sub"] = sub;
            assert_eq!(v.identity(&c, now), Err(IdpError::BadSubject));
        }
        let mut c = ok.clone();
        c.as_object_mut().unwrap().remove("sub");
        assert_eq!(v.identity(&c, now), Err(IdpError::BadSubject));
    }

    #[test]
    fn jwks_parsing_keeps_only_safe_keys() {
        let n2048 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xabu8; 256]);
        let n1024 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xabu8; 128]);
        let xy = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
        let doc = json!({"keys": [
            {"kty": "RSA", "kid": "rsa-ok", "n": n2048, "e": "AQAB", "alg": "RS256", "use": "sig"},
            {"kty": "RSA", "kid": "rsa-noalg", "n": n2048, "e": "AQAB"},
            {"kty": "RSA", "kid": "rsa-small", "n": n1024, "e": "AQAB"},
            {"kty": "RSA", "kid": "rsa-ps256", "n": n2048, "e": "AQAB", "alg": "PS256"},
            {"kty": "RSA", "kid": "rsa-enc", "n": n2048, "e": "AQAB", "use": "enc"},
            {"kty": "RSA", "n": n2048, "e": "AQAB"},
            {"kty": "oct", "kid": "hmac", "k": "c2VjcmV0", "alg": "HS256"},
            {"kty": "EC", "kid": "ec-ok", "crv": "P-256", "x": xy, "y": xy},
            {"kty": "EC", "kid": "ec-384", "crv": "P-384", "x": xy, "y": xy},
            {"kty": "EC", "kid": "ec-short", "crv": "P-256", "x": "AQ", "y": "AQ"},
            {"kty": "OKP", "kid": "ed", "crv": "Ed25519", "x": xy},
            {"kty": "RSA", "kid": "dup", "n": n2048, "e": "AQAB"},
            {"kty": "RSA", "kid": "dup", "n": n2048, "e": "AQAB"},
            "not an object", 5
        ]});
        let keys = parse_jwks(&doc);
        let mut got: Vec<_> = keys.iter().map(|(k, v)| (k.as_str(), v.alg)).collect();
        got.sort_by_key(|(k, _)| *k);
        assert_eq!(
            got,
            [
                ("ec-ok", Algorithm::ES256),
                ("rsa-noalg", Algorithm::RS256),
                ("rsa-ok", Algorithm::RS256)
            ]
        );
        assert!(parse_jwks(&json!({})).is_empty());
        assert!(parse_jwks(&json!({"keys": "x"})).is_empty());
    }

    #[test]
    fn routing_only_claims_rs256_and_es256_headers() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B;
        let tok = |alg: &str| {
            format!(
                "{}.{}.sig",
                B.encode(format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#)),
                B.encode("{}")
            )
        };
        assert!(is_idp_token(&tok("RS256")));
        assert!(is_idp_token(&tok("ES256")));
        for alg in [
            "HS256", "HS384", "HS512", "none", "PS256", "ES384", "EdDSA", "RS512", "",
        ] {
            assert!(!is_idp_token(&tok(alg)), "{alg}");
        }
        assert!(!is_idp_token("not-a-jwt"));
        assert!(!is_idp_token(""));
    }

    #[tokio::test]
    async fn validate_refuses_non_idp_algorithms_before_any_fetch() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B;
        // No JWKS URL is reachable: if validate tried to fetch, the error would be KeysUnavailable.
        let v = validator(&[("ATLAS_MCP_IDP_JWKS_URL", "http://127.0.0.1:1/jwks")]);
        let tok = |alg: &str, kid: bool| {
            let h = if kid {
                format!(r#"{{"alg":"{alg}","kid":"k"}}"#)
            } else {
                format!(r#"{{"alg":"{alg}"}}"#)
            };
            format!("{}.{}.c2ln", B.encode(h), B.encode("{}"))
        };
        assert_eq!(
            v.validate(&tok("none", true)).await,
            Err(IdpError::Malformed)
        );
        assert_eq!(
            v.validate(&tok("HS256", true)).await,
            Err(IdpError::UnsupportedAlgorithm)
        );
        assert_eq!(
            v.validate(&tok("RS256", false)).await,
            Err(IdpError::MissingKid)
        );
        assert_eq!(v.validate("x.y.z").await, Err(IdpError::Malformed));
        assert_eq!(
            v.validate(&"a".repeat(MAX_TOKEN_BYTES + 1)).await,
            Err(IdpError::TooLarge)
        );
        // Reaching the fetch with an unreachable IdP is a 401, not a panic or a hang.
        assert_eq!(
            v.validate(&tok("RS256", true)).await,
            Err(IdpError::KeysUnavailable)
        );
    }
}
