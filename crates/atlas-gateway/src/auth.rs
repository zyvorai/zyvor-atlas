// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! JWT auth middleware. When `config.auth_required` is false (dev default), requests pass through
//! with an anonymous actor; when true, a valid HS256 Bearer token is required. On the MCP endpoint
//! only, and only when `ATLAS_MCP_IDP_ISSUER` is set, an RS256/ES256 identity-provider access token
//! is also accepted (`idp_token.rs`). An opaque (non-JWT) token on that endpoint is sent to RFC 7662
//! introspection when `ATLAS_MCP_INTROSPECTION_URL` is set (`mcp_introspect.rs`). REST, gRPC and
//! `/metrics` stay Atlas-HS256-only.

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
    /// Resource scope of an agent token (see [`ResourceScope`]). When present the token is scoped
    /// (MCP only) and may name only the volumes, snapshots, workspaces and paths it lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ResourceScope>,
    /// Budget on the sum of `max_bytes` of the workspaces this token's subject holds; unlike `scope`
    /// it does not make the token MCP-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_bytes: Option<i64>,
}

/// Which resources an agent token may name, beyond which tools it may call. Each list holds exact ids
/// or glob patterns where `*` matches any run of characters (`ws_*`, `/data/run1/*`). Deny by default:
/// once a token carries a scope, a tool argument naming a resource of a kind whose list is empty or has
/// no matching entry is refused, and so are tools that enumerate inventory (see `mcp::resource_guard`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceScope {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// The kinds of resource a [`ResourceScope`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Volume,
    Snapshot,
    Workspace,
    Path,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Snapshot => "snapshot",
            Self::Workspace => "workspace",
            Self::Path => "path",
        }
    }
}

/// `*` matches any run of characters (including none and `/`); every other character matches itself.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == b'*')
}

impl ResourceScope {
    /// True when no list has an entry (nothing was requested).
    pub fn is_empty(&self) -> bool {
        self.volumes.is_empty()
            && self.snapshots.is_empty()
            && self.workspaces.is_empty()
            && self.paths.is_empty()
    }

    fn list(&self, kind: ResourceKind) -> &[String] {
        match kind {
            ResourceKind::Volume => &self.volumes,
            ResourceKind::Snapshot => &self.snapshots,
            ResourceKind::Workspace => &self.workspaces,
            ResourceKind::Path => &self.paths,
        }
    }

    /// Whether `name` is allowed for `kind`. A path with a `..` or empty segment, or a NUL, is never
    /// allowed: a pattern like `/data/run1/*` must not be escapable by `/data/run1/../other`.
    pub fn allows(&self, kind: ResourceKind, name: &str) -> bool {
        if name.is_empty() || name.contains('\0') {
            return false;
        }
        if kind == ResourceKind::Path && name.split('/').any(|seg| seg == "..") {
            return false;
        }
        self.list(kind).iter().any(|pat| glob_match(pat, name))
    }
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
    /// See [`Claims::scope`]; `None` = no resource restriction.
    pub scope: Option<ResourceScope>,
    /// See [`Claims::workspace_bytes`].
    pub workspace_bytes: Option<i64>,
    /// The token's `jti` (empty for the bootstrap secret, anonymous callers and legacy tokens), so an
    /// audit row can be traced to the token that made the call.
    pub jti: String,
}

impl Actor {
    fn anonymous() -> Self {
        Self {
            id: "anonymous".into(),
            role: "viewer".into(),
            tenant_id: default_tenant(),
            tools: Vec::new(),
            scope: None,
            workspace_bytes: None,
            jti: String::new(),
        }
    }

    /// True for a scoped agent token (see [`Claims::tools`] and [`Claims::scope`]).
    pub fn is_scoped(&self) -> bool {
        !self.tools.is_empty() || self.scope.is_some()
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

/// Map an identity provider's `groups` claim to an Atlas role: `admin` when any group is in
/// `admin_groups`, else `operator` when any is in `operator_groups`, else `viewer` (least privilege,
/// also for a token with no groups at all). One function for the console SSO login
/// (`routes/oidc.rs`) and the IdP access tokens accepted on `/mcp` (`idp_token.rs`), so the two can
/// never disagree about who is an admin.
pub fn role_for_groups(
    groups: &[String],
    admin_groups: &[String],
    operator_groups: &[String],
) -> &'static str {
    if groups.iter().any(|g| admin_groups.contains(g)) {
        "admin"
    } else if groups.iter().any(|g| operator_groups.contains(g)) {
        "operator"
    } else {
        "viewer"
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
    mint_agent_token(
        secret, subject, role, tenant_id, ttl_secs, tools, None, None,
    )
}

/// [`mint_scoped_token`] plus a resource scope and a workspace byte budget (see [`Claims::scope`],
/// [`Claims::workspace_bytes`]). An empty scope is minted as no scope.
#[allow(clippy::too_many_arguments)]
pub fn mint_agent_token(
    secret: &str,
    subject: &str,
    role: &str,
    tenant_id: &str,
    ttl_secs: u64,
    tools: &[String],
    scope: Option<&ResourceScope>,
    workspace_bytes: Option<i64>,
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
        scope: scope.filter(|s| !s.is_empty()).cloned(),
        workspace_bytes: workspace_bytes.filter(|b| *b > 0),
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
        // On the MCP endpoint a 401/403 also carries the RFC 9728 challenge (`WWW-Authenticate`
        // with `resource_metadata` and `scope`) so an MCP client can discover how to authenticate.
        // REST 401s are unchanged. The header map is cloned before any `.await`: a borrow of `req`
        // (its body is not `Sync`) must not live across one. The scope list is built only when a
        // challenge is actually returned.
        let mcp_headers = is_mcp_path(req.uri().path()).then(|| req.headers().clone());
        let Some(token) = token else {
            return unauthorized(
                "missing bearer token",
                mcp_challenge(
                    &state,
                    mcp_headers.as_ref(),
                    crate::oauth_resource::ChallengeKind::None,
                ),
            );
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
                scope: None,
                workspace_bytes: None,
                jti: String::new(),
            }
        } else if let Some(idp) = state
            .mcp_idp
            .as_ref()
            .filter(|_| is_mcp_path(req.uri().path()) && crate::idp_token::is_idp_token(token))
        {
            // An identity-provider access token (RS256/ES256), accepted on the MCP endpoint only and
            // only when `ATLAS_MCP_IDP_ISSUER` is set. Atlas never mints these algorithms, so routing
            // on the header cannot let an Atlas token take this path or the reverse, and every other
            // route (REST, `/metrics`) never reaches this branch. Owned copy: the validator awaits a
            // key fetch, and nothing borrowed from the request may live across that.
            let token = token.to_owned();
            match idp.validate(&token).await {
                Ok(id) => {
                    if !id.jti.is_empty() {
                        match atlas_inventory::tokens::is_revoked(&state.pool, &id.jti).await {
                            Ok(true) => {
                                return unauthorized(
                                    "token has been revoked",
                                    mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                                )
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!("token revocation check failed: {e}"),
                        }
                    }
                    Actor {
                        id: id.actor_id,
                        role: id.role.to_string(),
                        tenant_id: id.tenant_id,
                        tools: id.tools,
                        scope: None,
                        workspace_bytes: None,
                        jti: id.jti,
                    }
                }
                Err(crate::idp_token::IdpError::InsufficientScope) => {
                    return insufficient_scope(mcp_challenge(
                        &state,
                        mcp_headers.as_ref(),
                        crate::oauth_resource::ChallengeKind::InsufficientScope,
                    ));
                }
                Err(e) => {
                    tracing::warn!(reason = %e, "MCP IdP access token refused");
                    return unauthorized(
                        &format!("invalid token: {e}"),
                        mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                    );
                }
            }
        } else if is_mcp_path(req.uri().path())
            && !crate::idp_token::looks_like_jwt(token)
            && state.mcp_introspection.is_some()
        {
            // Opaque access token. JWT-shaped tokens stay on the local validators above and below;
            // this branch never sees them. REST never reaches here.
            let token = token.to_owned();
            let intro = state.mcp_introspection.clone().expect("checked above");
            match intro.introspect(&token).await {
                Ok(id) => {
                    if !id.jti.is_empty() {
                        match atlas_inventory::tokens::is_revoked(&state.pool, &id.jti).await {
                            Ok(true) => {
                                return unauthorized(
                                    "token has been revoked",
                                    mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                                )
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!("token revocation check failed: {e}"),
                        }
                    }
                    Actor {
                        id: id.actor_id,
                        role: id.role.to_string(),
                        tenant_id: id.tenant_id,
                        tools: id.tools,
                        scope: None,
                        workspace_bytes: None,
                        jti: id.jti,
                    }
                }
                Err(crate::mcp_introspect::IntroError::InsufficientScope) => {
                    return insufficient_scope(mcp_challenge(
                        &state,
                        mcp_headers.as_ref(),
                        crate::oauth_resource::ChallengeKind::InsufficientScope,
                    ));
                }
                Err(e) => {
                    tracing::warn!(reason = %e, "MCP opaque access token refused");
                    return unauthorized(
                        &format!("invalid token: {e}"),
                        mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                    );
                }
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
                            Ok(true) => {
                                return unauthorized(
                                    "token has been revoked",
                                    mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                                )
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!("token revocation check failed: {e}"),
                        }
                    }
                    Actor {
                        id: data.claims.sub,
                        role: data.claims.role,
                        tenant_id: data.claims.tenant_id,
                        tools: data.claims.tools,
                        scope: data.claims.scope,
                        workspace_bytes: data.claims.workspace_bytes,
                        jti: data.claims.jti,
                    }
                }
                Err(e) => {
                    return unauthorized(
                        &format!("invalid token: {e}"),
                        mcp_challenge(&state, mcp_headers.as_ref(), crate::oauth_resource::ChallengeKind::InvalidToken),
                    )
                }
            }
        }
    };

    // A scoped agent token is valid only on the MCP edge (`/mcp`, or the full path when the layer
    // sees it unstripped); everywhere else it is refused, not silently given its role's reach.
    if actor.is_scoped() && !is_mcp_path(req.uri().path()) {
        return forbidden("this token is scoped to MCP tools and is not valid on this route");
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

/// True for the MCP endpoint (`/mcp`, or the full path when the layer sees it unstripped) and
/// anything under it.
fn is_mcp_path(path: &str) -> bool {
    ["/mcp", "/api/atlas/v1/mcp"]
        .iter()
        .any(|p| path == *p || path.strip_prefix(*p).is_some_and(|r| r.starts_with('/')))
}

/// Build the MCP `WWW-Authenticate` value. `headers` is `None` off the MCP endpoint (no header).
/// Called at the point of the response so it does not borrow `state` across an `.await`.
fn mcp_challenge(
    state: &AppState,
    headers: Option<&http::HeaderMap>,
    kind: crate::oauth_resource::ChallengeKind,
) -> Option<http::HeaderValue> {
    let headers = headers?;
    let scope = crate::oauth_resource::bearer_scope(state);
    Some(crate::oauth_resource::challenge(
        state.public_url.as_deref(),
        headers,
        kind,
        &scope,
    ))
}

/// A 401. `www_authenticate` is set only on the MCP endpoint (see `auth_middleware`).
fn unauthorized(msg: &str, www_authenticate: Option<http::HeaderValue>) -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": { "code": "AUTH_ERROR", "message": msg } })),
    )
        .into_response();
    if let Some(v) = www_authenticate {
        resp.headers_mut().insert(http::header::WWW_AUTHENTICATE, v);
    }
    resp
}

fn insufficient_scope(www_authenticate: Option<http::HeaderValue>) -> Response {
    let mut resp = (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": "INSUFFICIENT_SCOPE", "message":
            "token scope does not include an Atlas role or tool" } })),
    )
        .into_response();
    if let Some(v) = www_authenticate {
        resp.headers_mut()
            .insert(http::header::WWW_AUTHENTICATE, v);
    }
    resp
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
