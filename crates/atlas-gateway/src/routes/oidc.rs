// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! OIDC/SSO login. A second way to *obtain* an Atlas session JWT (alongside `POST /auth/login`'s
//! local username/password) — a successful OIDC round-trip mints the exact same HS256 JWT
//! `mint_token` always has, so `auth_middleware`/`require_role`/token revocation are all
//! untouched downstream. Disabled (routes still mounted but return `503`) unless
//! `ATLAS_OIDC_ISSUER_URL`/`_CLIENT_ID`/`_REDIRECT_URL` are all set and discovery succeeded at
//! startup (`AppState.oidc` is `None`).

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Redirect},
    Json,
};
use openidconnect::core::CoreResponseType;
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, CsrfToken, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    Scope,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use crate::state::AppState;

/// `GET /auth/oidc/status` — unauthenticated, cheap check so the console only shows a "Sign in
/// with SSO" button when the feature is actually configured.
pub(crate) async fn oidc_status(State(s): State<AppState>) -> Json<Value> {
    Json(json!({ "enabled": s.oidc.is_some() }))
}

/// `GET /auth/oidc/login` — full-page redirect to the identity provider's authorization endpoint
/// (must be a real browser navigation, not a fetch — the IdP may need to set its own cookies /
/// can't run inside an iframe).
pub(crate) async fn oidc_login(State(s): State<AppState>) -> AppResult<impl IntoResponse> {
    let oidc = s
        .oidc
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("OIDC login is not configured".into()))?;

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_token, nonce) = oidc
        .client
        .authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("groups".into()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    oidc.stash(
        csrf_token.secret().clone(),
        nonce.secret().clone(),
        pkce_verifier.secret().clone(),
    );
    Ok(Redirect::to(auth_url.as_str()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct OidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /auth/oidc/callback` — the identity provider redirects here with `?code=&state=` (or
/// `?error=` if the user cancelled / the IdP rejected the request). Exchanges the code, verifies
/// the ID token (signature, issuer, audience, nonce, expiry — all handled by
/// `openidconnect`'s `claims()` call), maps the `groups` claim to an Atlas role, mints a normal
/// Atlas JWT, and redirects the browser back into the SPA with the token as a query param for
/// the frontend to pick up and strip (see `ui/src/main.tsx`).
pub(crate) async fn oidc_callback(
    State(s): State<AppState>,
    Query(q): Query<OidcCallbackQuery>,
) -> AppResult<impl IntoResponse> {
    let oidc = s
        .oidc
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("OIDC login is not configured".into()))?;

    if let Some(err) = q.error {
        return Err(AppError::Auth(format!(
            "identity provider returned an error: {err} ({})",
            q.error_description.unwrap_or_default()
        )));
    }
    let code = q
        .code
        .ok_or_else(|| AppError::Validation("missing code".into()))?;
    let csrf_state = q
        .state
        .ok_or_else(|| AppError::Validation("missing state".into()))?;

    let pending = oidc
        .take(&csrf_state)
        .ok_or_else(|| AppError::Auth("unknown or expired login attempt".into()))?;

    let token_response = oidc
        .client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| AppError::Auth(format!("failed to build token request: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
        .request_async(&oidc.http)
        .await
        .map_err(|e| AppError::Auth(format!("token exchange failed: {e}")))?;

    let id_token = token_response
        .extra_fields()
        .id_token()
        .ok_or_else(|| AppError::Auth("identity provider did not return an ID token".into()))?;
    let nonce = Nonce::new(pending.nonce);
    let claims = id_token
        .claims(&oidc.client.id_token_verifier(), &nonce)
        .map_err(|e| AppError::Auth(format!("ID token verification failed: {e}")))?;
    let subject = claims.subject().as_str().to_string();

    // `groups` is a common but non-standard claim (Dex, Keycloak, Okta, ... all emit it under
    // this name). Read it from the already-signature-verified token's raw JSON payload rather
    // than fighting openidconnect's generic `AdditionalClaims` type machinery for one extra
    // field — `claims` above already proved this exact JWT is authentic.
    let groups = id_token_groups(id_token);

    let oidc_cfg = s
        .config
        .oidc
        .as_ref()
        .expect("AppState.oidc is Some only when Config.oidc is Some");
    let role =
        crate::auth::role_for_groups(&groups, &oidc_cfg.admin_groups, &oidc_cfg.operator_groups);
    // Tenant scoping (see auth::tenant_scope/require_tenant): read from the configured claim
    // (e.g. a bank's IdP might emit "tenant" or "department") when set, else "global" — admin
    // role bypasses this filter entirely regardless of what's resolved here.
    let tenant_id = oidc_cfg
        .tenant_claim
        .as_deref()
        .and_then(|claim| id_token_string_claim(id_token, claim))
        .unwrap_or_else(|| "global".to_string());

    let (token, exp, jti) =
        crate::auth::mint_token(&s.config.jwt_secret, &subject, role, &tenant_id, 86_400)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        None,
        &subject,
        "auth.oidc_login",
        "console",
        &subject,
        "ok",
        Some(json!({ "role": role, "tenant_id": tenant_id, "groups": groups, "jti": jti, "exp": exp })),
        None,
    )
    .await;

    // Query param, not a URL fragment: this is a same-origin SPA reload (the gateway itself
    // serves the console), so the token reaching the server here is fine, and a fragment would
    // never even be visible to this handler in the first place. `main.tsx` strips it from the
    // URL immediately after reading it.
    Ok(Redirect::to(&format!(
        "/?atlas_token={token}&atlas_role={role}"
    )))
}

fn id_token_groups(id_token: &openidconnect::core::CoreIdToken) -> Vec<String> {
    id_token_raw_payload(id_token)
        .and_then(|p| p.get("groups").cloned())
        .and_then(|g| g.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Read an arbitrary string claim (e.g. a bank IdP's `"tenant"` or `"department"` claim) from the
/// already-signature-verified token's raw JSON payload — same rationale as `id_token_groups`.
fn id_token_string_claim(
    id_token: &openidconnect::core::CoreIdToken,
    claim: &str,
) -> Option<String> {
    id_token_raw_payload(id_token)?
        .get(claim)?
        .as_str()
        .map(String::from)
}

fn id_token_raw_payload(id_token: &openidconnect::core::CoreIdToken) -> Option<serde_json::Value> {
    use base64::Engine;
    let Ok(serde_json::Value::String(compact)) = serde_json::to_value(id_token) else {
        return None;
    };
    let payload_b64 = compact.split('.').nth(1)?;
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&payload_bytes).ok()
}
