// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! JWT auth middleware. When `config.auth_required` is false (dev default), requests pass through
//! with an anonymous actor; when true, a valid HS256 Bearer token is required.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::AppState;

/// JWT claims. `sub` is the actor id used in audit logs; `role` gates write actions; `jti` is the
/// token id used for revocation (empty on legacy tokens, which are simply not revocable);
/// `tenant_id` scopes read access (see `tenant_scope`/`require_tenant`) — defaults to `"global"`
/// so tokens minted before this field existed keep decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub exp: usize,
    #[serde(default)]
    pub jti: String,
    #[serde(default = "default_tenant")]
    pub tenant_id: String,
    /// MCP tool allowlist. Empty means an ordinary, unscoped token. A non-empty list makes this a
    /// scoped agent token: it may call only these MCP tools, and is refused on every other route
    /// (REST, gRPC, `/metrics`), because a list that only the MCP edge honours would be bypassed
    /// by using the same token elsewhere. A scope narrows the role; it never widens it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

fn default_tenant() -> String {
    "global".to_string()
}

/// The authenticated actor, injected as a request extension.
#[derive(Debug, Clone)]
pub struct Actor {
    pub id: String,
    pub role: String,
    pub tenant_id: String,
    /// See [`Claims::tools`]; empty = unscoped.
    pub tools: Vec<String>,
}

impl Actor {
    fn anonymous() -> Self {
        Self {
            id: "anonymous".into(),
            role: "viewer".into(),
            tenant_id: default_tenant(),
            tools: Vec::new(),
        }
    }

    /// True for a scoped agent token (see [`Claims::tools`]).
    pub fn is_scoped(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Whether this actor may call the MCP tool `name`: always for an unscoped actor, otherwise
    /// only when listed.
    pub fn may_call_tool(&self, name: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == name)
    }

    pub fn level(&self) -> u8 {
        role_level(&self.role)
    }
}

/// Role levels (PDF §14.2). Higher = more privilege; each level includes the ones below it.
pub const ROLE_VIEWER: u8 = 0;
pub const ROLE_OPERATOR: u8 = 1;
pub const ROLE_ADMIN: u8 = 2;

/// Map a JWT `role` claim to a privilege level. Product service accounts get operator-level access
/// for volume lifecycle. Unknown roles are viewer (read-only).
pub fn role_level(role: &str) -> u8 {
    match role {
        "admin" | "storage.admin" | "storage.security" | "storage.breakglass" => ROLE_ADMIN,
        "operator" | "storage.operator" => ROLE_OPERATOR,
        r if r.starts_with("product.service.") => ROLE_OPERATOR,
        _ => ROLE_VIEWER,
    }
}

/// Mint a signed HS256 service-account token for `subject` with `role`, expiring in `ttl_secs`.
/// Returns `(token, exp_unix_secs, jti)`. The `jti` identifies the token for later revocation. Used
/// by the token-issuance endpoint so products (Veyron, Hyper2KVM, …) get least-privilege credentials
/// without the shared secret ever leaving Atlas.
pub fn mint_token(
    secret: &str,
    subject: &str,
    role: &str,
    tenant_id: &str,
    ttl_secs: u64,
) -> atlas_common::AppResult<(String, usize, String)> {
    mint_scoped_token(secret, subject, role, tenant_id, ttl_secs, &[])
}

/// [`mint_token`] with an MCP tool allowlist (see [`Claims::tools`]); an empty slice mints an
/// ordinary unscoped token.
pub fn mint_scoped_token(
    secret: &str,
    subject: &str,
    role: &str,
    tenant_id: &str,
    ttl_secs: u64,
    tools: &[String],
) -> atlas_common::AppResult<(String, usize, String)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| atlas_common::AppError::Internal(e.to_string()))?
        .as_secs();
    let exp = (now + ttl_secs) as usize;
    let jti = atlas_common::ids::token_jti();
    let claims = Claims {
        sub: subject.to_string(),
        role: role.to_string(),
        exp,
        jti: jti.clone(),
        tenant_id: tenant_id.to_string(),
        tools: tools.to_vec(),
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| atlas_common::AppError::Internal(format!("mint token: {e}")))?;
    Ok((token, exp, jti))
}

/// Decode + validate an HS256 JWT, trying `secret` first and falling back to `previous_secret`
/// (if set) on a signature mismatch — this is what makes JWT secret rotation possible without
/// forcing every outstanding session to re-login the instant the secret changes (see
/// `Config::jwt_secret_previous`). Tokens are only ever *minted* with the current secret; this
/// fallback exists purely for validating tokens issued before a rotation. Shared by both the REST
/// `auth_middleware` and the gRPC interceptor so the two edges can't drift.
pub fn decode_token(
    token: &str,
    secret: &str,
    previous_secret: Option<&str>,
) -> jsonwebtoken::errors::Result<jsonwebtoken::TokenData<Claims>> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let primary = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    );
    match (primary, previous_secret) {
        (Ok(data), _) => Ok(data),
        (Err(_), Some(prev)) => decode::<Claims>(
            token,
            &DecodingKey::from_secret(prev.as_bytes()),
            &validation,
        ),
        (Err(e), None) => Err(e),
    }
}

/// Enforce a minimum role. No-op when auth is disabled (dev), so open-dev keeps working.
pub fn require_role(auth_required: bool, actor: &Actor, min: u8) -> atlas_common::AppResult<()> {
    if auth_required && actor.level() < min {
        return Err(atlas_common::AppError::Forbidden(format!(
            "action requires a higher role; actor '{}' has role '{}'",
            actor.id, actor.role
        )));
    }
    Ok(())
}

/// The tenant a non-admin actor is restricted to on read/list endpoints, or `None` when the
/// actor is unscoped (admin role, or auth disabled) and may see every tenant's resources.
pub fn tenant_scope(auth_required: bool, actor: &Actor) -> Option<&str> {
    if !auth_required || actor.level() >= ROLE_ADMIN {
        None
    } else {
        Some(actor.tenant_id.as_str())
    }
}

/// Enforce that a non-admin actor may only fetch a single resource belonging to their own
/// tenant. Returns 404 rather than 403 so a scoped actor can't distinguish "doesn't exist" from
/// "exists in another tenant" by probing IDs.
pub fn require_tenant(
    auth_required: bool,
    actor: &Actor,
    resource_tenant: &str,
    not_found_msg: impl Into<String>,
) -> atlas_common::AppResult<()> {
    if !auth_required || actor.level() >= ROLE_ADMIN || actor.tenant_id == resource_tenant {
        return Ok(());
    }
    Err(atlas_common::AppError::NotFound(not_found_msg.into()))
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Resolve the actor: anonymous when auth is disabled, else a valid non-revoked Bearer token.
    let actor = if !state.config.auth_required {
        Actor::anonymous()
    } else {
        let token = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        let Some(token) = token else {
            return unauthorized("missing bearer token");
        };
        // Bootstrap admin: raw bearer accepted until the operator mints real JWTs and unsets
        // ATLAS_BOOTSTRAP_ADMIN_TOKEN. Checked before JWT decode so it need not be a valid JWT.
        // Compared in constant time: this is a long-lived shared secret, and `==` on `&str`
        // short-circuits on the first mismatched byte, which leaks timing an attacker could use
        // to recover it byte-by-byte.
        if state
            .config
            .bootstrap_admin_token
            .as_deref()
            .is_some_and(|boot| constant_time_eq(boot.as_bytes(), token.as_bytes()))
        {
            Actor {
                id: "bootstrap".into(),
                role: "admin".into(),
                tenant_id: default_tenant(),
                tools: Vec::new(),
            }
        } else {
            match decode_token(
                token,
                &state.config.jwt_secret,
                state.config.jwt_secret_previous.as_deref(),
            ) {
                Ok(data) => {
                    // Deny-list check: a revoked token is rejected even before it expires. Fail open on a
                    // DB error (readiness already gates on the DB) so a transient blip can't lock everyone out.
                    if !data.claims.jti.is_empty() {
                        match atlas_inventory::tokens::is_revoked(&state.pool, &data.claims.jti)
                            .await
                        {
                            Ok(true) => return unauthorized("token has been revoked"),
                            Ok(false) => {}
                            Err(e) => tracing::warn!("token revocation check failed: {e}"),
                        }
                    }
                    Actor {
                        id: data.claims.sub,
                        role: data.claims.role,
                        tenant_id: data.claims.tenant_id,
                        tools: data.claims.tools,
                    }
                }
                Err(e) => return unauthorized(&format!("invalid token: {e}")),
            }
        }
    };

    // A scoped agent token is valid only on the MCP edge (`/mcp`, or the full path when the layer
    // sees it unstripped); everywhere else it is refused, not silently given its role's reach.
    if actor.is_scoped() {
        let path = req.uri().path();
        let on_mcp = ["/mcp", "/api/atlas/v1/mcp"]
            .iter()
            .any(|p| path == *p || path.strip_prefix(*p).is_some_and(|r| r.starts_with('/')));
        if !on_mcp {
            return forbidden("this token is scoped to MCP tools and is not valid on this route");
        }
    }

    // Rate limit per actor (day-2 governance; disabled unless ATLAS_RATE_LIMIT_RPM > 0).
    if !state.rate.allow(&actor.id) {
        return too_many_requests(&actor.id);
    }

    req.extensions_mut().insert(actor);
    next.run(req).await
}

/// Constant-time byte comparison for the bootstrap shared secret, so a mismatch can't be
/// distinguished by how many leading bytes matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify console username/password. Both comparisons always run (`&`, not `&&`) to limit timing leaks.
pub(crate) fn verify_console_credentials(
    username: &str,
    password: &str,
    expected_user: &str,
    expected_pass: &str,
) -> bool {
    let user_ok = constant_time_eq(username.trim().as_bytes(), expected_user.as_bytes());
    let pass_ok = constant_time_eq(password.as_bytes(), expected_pass.as_bytes());
    user_ok & pass_ok
}

/// Hash a password as a standard Argon2id PHC string (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`)
/// for storage in `console_users`. Argon2 is memory-hard, unlike the plain SHA-256 this replaced —
/// a leaked `console_users` table can no longer be brute-forced offline at GPU/ASIC speed.
pub(crate) fn hash_password(password: &str) -> String {
    use argon2::password_hash::phc::PasswordHash;
    use argon2::password_hash::PasswordHasher;
    use argon2::Argon2;
    // password-hash 0.6 generates the salt itself from the OS RNG (no more explicit SaltString/OsRng);
    // the explicit `PasswordHash` annotation pins which `PasswordHasher<H>` impl is meant.
    let hash: PasswordHash = Argon2::default()
        .hash_password(password.as_bytes())
        .expect("argon2 hashing with default params and OS randomness cannot fail");
    hash.to_string()
}

/// Verify a password against a stored hash. Accepts the current Argon2id PHC-string format
/// (`$argon2...`) and, for backward compatibility with hashes created before this change, the
/// legacy `sha256$<salt>$<digest>` format — no forced password reset on upgrade.
pub(crate) fn verify_password_hash(password: &str, stored: &str) -> bool {
    if stored.starts_with('$') {
        use argon2::password_hash::phc::PasswordHash;
        use argon2::password_hash::PasswordVerifier;
        use argon2::Argon2;
        let Ok(parsed) = PasswordHash::new(stored) else {
            return false;
        };
        return Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
    }
    let Some((algo, rest)) = stored.split_once('$') else {
        return false;
    };
    if algo != "sha256" {
        return false;
    }
    let Some((salt, expected)) = rest.split_once('$') else {
        return false;
    };
    let digest = sha256_hex(&format!("{salt}:{password}"));
    constant_time_eq(digest.as_bytes(), expected.as_bytes())
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(input.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Normalize / validate a privilege role name used for console users + JWTs.
pub(crate) fn normalize_console_role(role: &str) -> Result<&'static str, String> {
    match role.trim().to_ascii_lowercase().as_str() {
        "viewer" => Ok("viewer"),
        "operator" => Ok("operator"),
        "admin" => Ok("admin"),
        other => Err(format!(
            "invalid role '{other}'; expected viewer, operator, or admin"
        )),
    }
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "code": "AUTH_ERROR", "message": msg } })),
    )
        .into_response()
}

fn forbidden(msg: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": "FORBIDDEN", "message": msg } })),
    )
        .into_response()
}

fn too_many_requests(actor: &str) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": { "code": "RATE_LIMITED", "message": format!("rate limit exceeded for '{actor}'") } })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_hash_round_trips() {
        let hash = hash_password("correct horse battery staple");
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password_hash("correct horse battery staple", &hash));
        assert!(!verify_password_hash("wrong password", &hash));
    }

    #[test]
    fn legacy_sha256_hash_still_verifies() {
        // A hash created by the pre-Argon2 hash_password — must keep verifying without forcing
        // every existing console_users row to be reset on upgrade.
        let salt = "fixed-salt-for-test";
        let digest = sha256_hex(&format!("{salt}:hunter2"));
        let legacy = format!("sha256${salt}${digest}");
        assert!(verify_password_hash("hunter2", &legacy));
        assert!(!verify_password_hash("wrong", &legacy));
    }

    #[test]
    fn decode_token_accepts_current_secret() {
        let (token, _, _) = mint_token(
            "new-secret-32-bytes-or-more!!!!",
            "alice",
            "admin",
            "global",
            3600,
        )
        .unwrap();
        let decoded = decode_token(&token, "new-secret-32-bytes-or-more!!!!", None).unwrap();
        assert_eq!(decoded.claims.sub, "alice");
    }

    #[test]
    fn decode_token_falls_back_to_previous_secret_during_rotation() {
        // A token minted before rotation, with the OLD secret...
        let (token, _, _) = mint_token(
            "old-secret-32-bytes-or-more!!!!",
            "bob",
            "operator",
            "global",
            3600,
        )
        .unwrap();
        // ...must still validate against the NEW current secret, as long as the old one is
        // supplied as jwt_secret_previous — this is the whole point of the rotation window.
        let decoded = decode_token(
            &token,
            "new-secret-32-bytes-or-more!!!!",
            Some("old-secret-32-bytes-or-more!!!!"),
        )
        .unwrap();
        assert_eq!(decoded.claims.sub, "bob");
    }

    #[test]
    fn decode_token_rejects_stale_secret_once_rotation_window_closes() {
        let (token, _, _) = mint_token(
            "old-secret-32-bytes-or-more!!!!",
            "carol",
            "viewer",
            "global",
            3600,
        )
        .unwrap();
        // No jwt_secret_previous configured (the operator finished the rotation and removed it) —
        // a token signed with the retired secret must be rejected.
        let result = decode_token(&token, "new-secret-32-bytes-or-more!!!!", None);
        assert!(result.is_err());
    }

    #[test]
    fn decode_token_rejects_garbage_against_both_secrets() {
        let result = decode_token(
            "not-a-jwt-at-all",
            "new-secret-32-bytes-or-more!!!!",
            Some("old-secret-32-bytes-or-more!!!!"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn decode_token_rejects_forged_string_exp() {
        // Regression guard for GHSA-h395-gr6q-cpjc: jsonwebtoken <10.3 treated a claim that
        // failed to parse (e.g. exp sent as a string instead of a number) the same as the claim
        // being entirely absent, silently skipping validate_exp for it — UNLESS the claim was
        // also listed in required_spec_claims, in which case "failed to parse" correctly counts
        // as "not present" and the token is rejected. We rely on exactly that: "exp" is in
        // Validation::new()'s required_spec_claims by default and this code never clears it, so
        // decode_token should already reject a forged never-expiring token — verify that stays
        // true after the 9->10 upgrade rather than trusting the reasoning alone.
        #[derive(serde::Serialize)]
        struct ForgedClaims<'a> {
            sub: &'a str,
            role: &'a str,
            exp: &'a str, // malformed: should be a number
        }
        let secret = "forged-exp-secret-32-bytes-or-more!";
        let forged = encode(
            &Header::default(),
            &ForgedClaims {
                sub: "attacker",
                role: "admin",
                exp: "99999999999", // "never expires", if the bypass worked
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let result = decode_token(&forged, secret, None);
        assert!(
            result.is_err(),
            "a token with a malformed exp claim must be rejected, not silently accepted"
        );
    }
}
