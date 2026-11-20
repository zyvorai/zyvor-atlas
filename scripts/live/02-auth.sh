#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Auth: 401 without token; mint + revoke; password login; console users CRUD.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "02-auth"

: "${ATLAS_ADMIN_USERNAME:=admin}"
ensure_admin_password

# ---- bearer gate ------------------------------------------------------------
_saved="${ATLAS_TOKEN:-}"
ATLAS_TOKEN=""
assert_code 401 GET /pools || true
ATLAS_TOKEN="$_saved"

assert_ok POST /auth/tokens \
  '{"subject":"live-revoke-me","role":"viewer","ttl_secs":600}' || true
JTI="$(json_field jti)"
if [[ -n "$JTI" ]]; then
  cleanup_register "token:${JTI}"
  assert_ok POST "/auth/tokens/${JTI}/revoke" '{}' || true
fi
assert_ok GET /auth/tokens/revoked || true

# ---- password login (public) ------------------------------------------------
# Bad credentials → 401 (Auth). Empty → 400 (Validation).
assert_code 401 POST /auth/login \
  "{\"username\":\"${ATLAS_ADMIN_USERNAME}\",\"password\":\"wrong-password-xx\"}" || true
assert_code 400 POST /auth/login \
  '{"username":"","password":""}' || true

assert_ok POST /auth/login \
  "{\"username\":\"${ATLAS_ADMIN_USERNAME}\",\"password\":\"${ATLAS_ADMIN_PASSWORD}\",\"ttl_secs\":600}" || true
LOGIN_TOKEN="$(json_field token)"
LOGIN_JTI="$(json_field jti)"
if [[ -n "$LOGIN_JTI" ]]; then
  cleanup_register "token:${LOGIN_JTI}"
fi
if [[ -z "$LOGIN_TOKEN" ]]; then
  _record FAIL 0 POST /auth/login "missing token in login response"
else
  # Login JWT must authorize inventory reads.
  _prev="${ATLAS_TOKEN:-}"
  ATLAS_TOKEN="$LOGIN_TOKEN"
  assert_ok GET /pools || true
  ATLAS_TOKEN="$_prev"
fi

# ---- console users CRUD -----------------------------------------------------
assert_ok GET /auth/users || true

USER_NAME="live-u-${ATLAS_LIVE_PREFIX}"
# Keep username within 64 chars and [A-Za-z0-9._-]
USER_NAME="$(printf '%s' "$USER_NAME" | tr -c 'A-Za-z0-9._-' '-' | cut -c1-64)"
USER_PASS='LiveTest!9'
USER_PASS2='LiveTest!9b'

assert_code 201 POST /auth/users \
  "{\"username\":\"${USER_NAME}\",\"password\":\"${USER_PASS}\",\"role\":\"viewer\"}" || true
if [[ "$LIVE_CODE" =~ ^2 ]]; then
  cleanup_register "user:${USER_NAME}"

  # New user can log in; wrong password cannot.
  assert_code 401 POST /auth/login \
    "{\"username\":\"${USER_NAME}\",\"password\":\"nope-nope\"}" || true
  assert_ok POST /auth/login \
    "{\"username\":\"${USER_NAME}\",\"password\":\"${USER_PASS}\",\"ttl_secs\":300}" || true
  U_JTI="$(json_field jti)"
  [[ -n "$U_JTI" ]] && cleanup_register "token:${U_JTI}"

  # Promote + rotate password.
  assert_ok PUT "/auth/users/${USER_NAME}" \
    '{"role":"operator"}' || true
  assert_ok PUT "/auth/users/${USER_NAME}" \
    "{\"password\":\"${USER_PASS2}\"}" || true

  assert_code 401 POST /auth/login \
    "{\"username\":\"${USER_NAME}\",\"password\":\"${USER_PASS}\"}" || true
  assert_ok POST /auth/login \
    "{\"username\":\"${USER_NAME}\",\"password\":\"${USER_PASS2}\",\"ttl_secs\":300}" || true
  U2_JTI="$(json_field jti)"
  [[ -n "$U2_JTI" ]] && cleanup_register "token:${U2_JTI}"

  # Delete user; subsequent login fails.
  assert_ok DELETE "/auth/users/${USER_NAME}" || true
  cleanup_unregister "user:${USER_NAME}"
  assert_code 401 POST /auth/login \
    "{\"username\":\"${USER_NAME}\",\"password\":\"${USER_PASS2}\"}" || true
fi
