#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Shared helpers for the Tier-3 live suite (scripts/live/*.sh).
# Sourced by run-all.sh — do not execute directly.
#
# Idempotent: safe to source from every section script; counters persist.
if [[ -n "${_ATLAS_LIVE_LIB:-}" ]]; then
  return 0 2>/dev/null || true
fi
_ATLAS_LIVE_LIB=1

set -euo pipefail

# ---- defaults ---------------------------------------------------------------
: "${ATLAS_BASE_URL:=http://212.8.248.187:30511}"
: "${ATLAS_SSH_HOST:=212.8.248.187}"
: "${ATLAS_SSH_USER:=sus}"
: "${ATLAS_SSH_IDENTITY:=${HOME}/.ssh/id_ed25519_hyper2kvm}"
: "${ATLAS_AUTH_NS:=rook-ceph}"
: "${ATLAS_AUTH_SECRET:=atlas-gateway-auth}"
: "${ATLAS_STORAGE_CLASS:=zyvor-rbd-prod}"
: "${ATLAS_TENANT_ID:=tnt_default}"
: "${ATLAS_CURL_TIMEOUT:=25}"
: "${ATLAS_JOB_TIMEOUT:=90}"
: "${ATLAS_LIVE_PREFIX:=live-${$}-$(date +%s)}"

ATLAS_BASE_URL="${ATLAS_BASE_URL%/}"
API="${ATLAS_BASE_URL}/api/atlas/v1"

PASS_N=0
FAIL_N=0
WARN_N=0
SECTION="${SECTION:-unknown}"

# Resources registered for trap cleanup: "kind:id"
CLEANUP_ITEMS=()

# ---- logging ----------------------------------------------------------------
_c_cyan=$'\033[1;36m'
_c_green=$'\033[1;32m'
_c_red=$'\033[1;31m'
_c_yellow=$'\033[1;33m'
_c_dim=$'\033[2m'
_c_reset=$'\033[0m'

log()  { printf '%s==> %s%s\n' "$_c_cyan" "$*" "$_c_reset"; }
info() { printf '    %s\n' "$*"; }

_record() {
  local status="$1" code="$2" method="$3" path="$4" snip="$5"
  local color="$_c_green"
  case "$status" in
    FAIL) color="$_c_red"; FAIL_N=$((FAIL_N + 1)) ;;
    WARN) color="$_c_yellow"; WARN_N=$((WARN_N + 1)) ;;
    PASS) PASS_N=$((PASS_N + 1)) ;;
  esac
  printf '%s%-4s%s %3s %-6s %s  %s%s%s\n' \
    "$color" "$status" "$_c_reset" "$code" "$method" "$path" "$_c_dim" "$snip" "$_c_reset"
  if [[ -n "${ATLAS_LIVE_LOG:-}" ]]; then
    printf '%-4s %3s %-6s %s  %s\n' "$status" "$code" "$method" "$path" "$snip" >>"$ATLAS_LIVE_LOG"
  fi
}

# ---- HTTP -------------------------------------------------------------------
# Usage: live_req METHOD PATH [json_body]
# Sets: LIVE_CODE, LIVE_BODY. Path may be absolute (/health) or API-relative (/pools).
live_req() {
  local method="$1" path="$2" body="${3:-}"
  local url
  case "$path" in
    http://*|https://*) url="$path" ;;
    /health|/livez|/readyz|/version|/metrics) url="${ATLAS_BASE_URL}${path}" ;;
    /*) url="${API}${path}" ;;
    *) url="${API}/${path}" ;;
  esac

  local tmp
  tmp="$(mktemp)"
  local args=(-sS -m "$ATLAS_CURL_TIMEOUT" -X "$method"
    -H "Accept: application/json"
    -o "$tmp" -w "%{http_code}")
  if [[ -n "${ATLAS_TOKEN:-}" ]]; then
    args+=(-H "Authorization: Bearer ${ATLAS_TOKEN}")
  fi
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" -d "$body")
  fi

  set +e
  LIVE_CODE="$(curl "${args[@]}" "$url" 2>"${tmp}.err")"
  local curl_rc=$?
  set -e
  if [[ $curl_rc -ne 0 ]]; then
    LIVE_CODE=0
    LIVE_BODY="$(tr '\n' ' ' <"${tmp}.err" 2>/dev/null || echo "curl rc=$curl_rc")"
  else
    LIVE_BODY="$(cat "$tmp" 2>/dev/null || true)"
  fi
  rm -f "$tmp" "${tmp}.err"
}

_snip() {
  printf '%s' "${1:-}" | tr '\n' ' ' | head -c 160
}

# expect 2xx
assert_ok() {
  local method="$1" path="$2" body="${3:-}"
  live_req "$method" "$path" "$body"
  local sn
  sn="$(_snip "$LIVE_BODY")"
  if [[ "$LIVE_CODE" =~ ^2 ]]; then
    _record PASS "$LIVE_CODE" "$method" "$path" "$sn"
    return 0
  fi
  _record FAIL "$LIVE_CODE" "$method" "$path" "$sn"
  return 1
}

# expect specific code (or comma-list like 200,202)
assert_code() {
  local want="$1" method="$2" path="$3" body="${4:-}"
  live_req "$method" "$path" "$body"
  local sn
  sn="$(_snip "$LIVE_BODY")"
  if [[ ",${want}," == *",${LIVE_CODE},"* ]]; then
    _record PASS "$LIVE_CODE" "$method" "$path" "$sn"
    return 0
  fi
  _record FAIL "$LIVE_CODE" "$method" "$path" "want ${want}; ${sn}"
  return 1
}

# soft: 2xx = PASS, timeout/0 = WARN, else FAIL
assert_soft() {
  local method="$1" path="$2" body="${3:-}"
  live_req "$method" "$path" "$body"
  local sn
  sn="$(_snip "$LIVE_BODY")"
  if [[ "$LIVE_CODE" =~ ^2 ]]; then
    _record PASS "$LIVE_CODE" "$method" "$path" "$sn"
    return 0
  fi
  if [[ "$LIVE_CODE" == "0" ]] || [[ "$sn" == *timed\ out* ]] || [[ "$sn" == *Timeout* ]]; then
    _record WARN "$LIVE_CODE" "$method" "$path" "$sn"
    return 0
  fi
  _record FAIL "$LIVE_CODE" "$method" "$path" "$sn"
  return 1
}

# ---- JSON helpers (python3) -------------------------------------------------
json_get() {
  local expr="$1"
  python3 -c "
import sys, json
raw = sys.stdin.read() or 'null'
d = json.loads(raw)
${expr}
" <<<"${LIVE_BODY}"
}

json_field() {
  local field="$1"
  python3 -c "
import sys, json
raw = sys.stdin.read() or ''
try:
    d = json.loads(raw or 'null')
except Exception:
    d = None
if isinstance(d, dict):
    v = d.get('${field}', '')
    if v is None: v = ''
    print(v)
" <<<"${LIVE_BODY}"
}

# ---- jobs -------------------------------------------------------------------
wait_job() {
  local job_id="$1"
  local soft="${2:-}"
  local deadline=$((SECONDS + ATLAS_JOB_TIMEOUT))
  local state=""
  while (( SECONDS < deadline )); do
    live_req GET "/jobs/${job_id}"
    if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
      sleep 2
      continue
    fi
    state="$(json_field state)"
    info "job ${job_id}: ${state}"
    case "$state" in
      succeeded|failed|completed|error) break ;;
    esac
    sleep 2
  done
  if [[ "$state" != "succeeded" && "$state" != "completed" ]]; then
    if [[ "$soft" == "soft" ]]; then
      _record WARN "${LIVE_CODE:-0}" GET "/jobs/${job_id}" "state=${state:-timeout}"
    else
      _record FAIL "${LIVE_CODE:-0}" GET "/jobs/${job_id}" "state=${state:-timeout}"
    fi
    return 1
  fi
  _record PASS "$LIVE_CODE" GET "/jobs/${job_id}" "state=${state}"
  return 0
}

wait_gone() {
  local path="$1"
  local deadline=$((SECONDS + ATLAS_JOB_TIMEOUT))
  while (( SECONDS < deadline )); do
    live_req GET "$path"
    if [[ "$LIVE_CODE" == "404" ]]; then
      _record PASS 404 GET "$path" "gone"
      return 0
    fi
    sleep 2
  done
  _record FAIL "$LIVE_CODE" GET "$path" "still present after delete"
  return 1
}

# ---- cleanup registry -------------------------------------------------------
cleanup_register() {
  CLEANUP_ITEMS+=("$1")
}

cleanup_unregister() {
  local target="$1"
  local out=()
  local item
  for item in "${CLEANUP_ITEMS[@]+"${CLEANUP_ITEMS[@]}"}"; do
    [[ "$item" == "$target" ]] && continue
    out+=("$item")
  done
  CLEANUP_ITEMS=("${out[@]+"${out[@]}"}")
}

live_cleanup() {
  local item kind id
  # reverse order
  local i
  for (( i=${#CLEANUP_ITEMS[@]} - 1; i >= 0; i-- )); do
    item="${CLEANUP_ITEMS[$i]}"
    kind="${item%%:*}"
    id="${item#*:}"
    [[ -z "$id" || "$id" == "$item" ]] && continue
    info "cleanup ${kind} ${id}"
    case "$kind" in
      schedule) live_req DELETE "/schedules/${id}" || true ;;
      snapshot) live_req DELETE "/snapshots/${id}" || true; sleep 2 ;;
      volume)   live_req DELETE "/volumes/${id}?confirm=true" || true; sleep 2 ;;
      bucket)   live_req DELETE "/buckets/${id}" || true; sleep 2 ;;
      rbd)
        # id form: pool/image
        live_req DELETE "/rbd-images/${id}" || true
        sleep 1
        ;;
      db-plan)  live_req DELETE "/databridge/plans/${id}" || true ;;
      db-source) live_req DELETE "/databridge/sources/${id}" || true ;;
      dr-peer)  live_req DELETE "/dr/peers/${id}" || true ;;
      rook-pool) live_req DELETE "/ceph/pools/${id}?force=true" || true; sleep 2 ;;
      token)
        live_req POST "/auth/tokens/${id}/revoke" '{}' || true
        ;;
      user)
        live_req DELETE "/auth/users/${id}" || true
        ;;
    esac
  done
  CLEANUP_ITEMS=()
}

# ---- auth -------------------------------------------------------------------
_ssh() {
  local id_args=()
  if [[ -n "${ATLAS_SSH_IDENTITY}" && -f "${ATLAS_SSH_IDENTITY}" ]]; then
    id_args=(-o IdentitiesOnly=yes -i "${ATLAS_SSH_IDENTITY}")
  fi
  ssh -o BatchMode=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new \
    "${id_args[@]}" "${ATLAS_SSH_USER}@${ATLAS_SSH_HOST}" "$@"
}

fetch_bootstrap_token() {
  _ssh "sudo k3s kubectl -n ${ATLAS_AUTH_NS} get secret ${ATLAS_AUTH_SECRET} \
    -o jsonpath='{.data.bootstrap-admin-token}'" | base64 -d
}

fetch_admin_password() {
  _ssh "sudo k3s kubectl -n ${ATLAS_AUTH_NS} get secret ${ATLAS_AUTH_SECRET} \
    -o jsonpath='{.data.admin-password}'" | base64 -d
}

# Every deployment mints its own admin-password secret — there is no fixed
# default that works everywhere. Mirrors ensure_token's ATLAS_BOOTSTRAP_TOKEN
# fallback: use an explicit override if given, else read it straight from the
# same auth Secret over SSH.
ensure_admin_password() {
  if [[ -n "${ATLAS_ADMIN_PASSWORD:-}" ]]; then
    info "using ATLAS_ADMIN_PASSWORD (explicit)"
    return 0
  fi
  info "fetching admin password via ssh ${ATLAS_SSH_USER}@${ATLAS_SSH_HOST}"
  ATLAS_ADMIN_PASSWORD="$(fetch_admin_password)"
  export ATLAS_ADMIN_PASSWORD
  if [[ -z "${ATLAS_ADMIN_PASSWORD}" ]]; then
    echo "could not obtain admin password; set ATLAS_ADMIN_PASSWORD" >&2
    return 1
  fi
}

mint_admin_token() {
  local boot="$1"
  local prev="${ATLAS_TOKEN:-}"
  ATLAS_TOKEN="$boot"
  live_req POST "/auth/tokens" \
    '{"subject":"live-suite","role":"admin","ttl_secs":7200}'
  ATLAS_TOKEN="$prev"
  if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
    echo "failed to mint admin JWT: HTTP ${LIVE_CODE} $(_snip "$LIVE_BODY")" >&2
    return 1
  fi
  json_field token
}

ensure_token() {
  if [[ -n "${ATLAS_TOKEN:-}" ]]; then
    info "using ATLAS_TOKEN (${#ATLAS_TOKEN} chars)"
    return 0
  fi
  if [[ -n "${ATLAS_BOOTSTRAP_TOKEN:-}" ]]; then
    info "minting JWT from ATLAS_BOOTSTRAP_TOKEN"
    ATLAS_TOKEN="$(mint_admin_token "$ATLAS_BOOTSTRAP_TOKEN")"
    export ATLAS_TOKEN
    return 0
  fi
  info "fetching bootstrap token via ssh ${ATLAS_SSH_USER}@${ATLAS_SSH_HOST}"
  local boot
  boot="$(fetch_bootstrap_token)"
  if [[ -z "$boot" ]]; then
    echo "could not obtain bootstrap token; set ATLAS_TOKEN" >&2
    return 1
  fi
  ATLAS_TOKEN="$(mint_admin_token "$boot")"
  export ATLAS_TOKEN
  info "minted admin JWT (${#ATLAS_TOKEN} chars)"
}

# ---- section banner ---------------------------------------------------------
section() {
  SECTION="$1"
  printf '\n%s========== %s ==========%s\n' "$_c_cyan" "$SECTION" "$_c_reset"
}
