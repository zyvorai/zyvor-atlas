#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Rook CRD integration: read endpoints, then a full pool create -> Ready -> delete lifecycle.
#
# Pool size/failure-domain are env-overridable because they depend on real cluster capacity —
# the default single-node lab only has 1 OSD, so a replica-3 pool can reconcile the CR but never
# reach Ready (min_size unsatisfiable). Point these at whatever this cluster can actually satisfy;
# see docs/DR.md and docs/ROADMAP.md's Rook lifecycle automation entry for the full story.
#   ATLAS_ROOK_POOL_SIZE=1 ATLAS_ROOK_FAILURE_DOMAIN=osd ./scripts/live/run-all.sh
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "12-rook"

# Read endpoints — always safe, no mutation.
assert_ok GET /ceph/rook-status || true
if [[ "$LIVE_CODE" == "200" ]]; then
  HEALTH="$(json_field health 2>/dev/null || true)"
  info "rook cluster health: ${HEALTH:-unknown}"
fi

assert_ok GET /ceph/health-rollup || true
if [[ "$LIVE_CODE" == "200" ]]; then
  SOURCES="$(python3 -c "
import sys, json
d = json.loads(sys.stdin.read() or '{}')
print(','.join(d.get('sources') or []))
" <<<"$LIVE_BODY" 2>/dev/null || true)"
  info "health-rollup sources: ${SOURCES:-unknown}"
fi

POOL_SIZE="${ATLAS_ROOK_POOL_SIZE:-1}"
FAILURE_DOMAIN="${ATLAS_ROOK_FAILURE_DOMAIN:-osd}"
PNAME="lv$(printf '%s' "${ATLAS_LIVE_PREFIX}" | tr -cd 'a-zA-Z0-9' | tail -c 10)$(date +%H%M%S)"
SC_NAME="zyvor-${PNAME}"

assert_code 202,201 POST /ceph/pools \
  "{\"name\":\"${PNAME}\",\"replicated_size\":${POOL_SIZE},\"failure_domain\":\"${FAILURE_DOMAIN}\"}" || true

if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
  info "pool create request failed; skipping rest of 12-rook"
  return 0 2>/dev/null || exit 0
fi

JOB_ID="$(json_field job_id)"
cleanup_register "rook-pool:${PNAME}"

if [[ -n "$JOB_ID" ]]; then
  wait_job "$JOB_ID" soft || true
fi

# The job returning "succeeded" only means the CR/StorageClass apply calls were accepted —
# Rook's own reconcile (and real disk/cluster capacity) decides whether it reaches Ready.
live_req GET /ceph/pools
PHASE="$(python3 -c "
import sys, json
d = json.loads(sys.stdin.read() or '[]')
items = d if isinstance(d, list) else d.get('items') or []
name = '${PNAME}'
print(next((p.get('phase','') for p in items if p.get('name') == name), 'not-listed'))
" <<<"$LIVE_BODY" 2>/dev/null || true)"

if [[ "$PHASE" == "Ready" ]]; then
  _record PASS 200 GET /ceph/pools "pool ${PNAME} reached Ready"
else
  _record WARN 200 GET /ceph/pools "pool ${PNAME} phase=${PHASE:-unknown} (cluster capacity may not satisfy replicated_size=${POOL_SIZE}/failure_domain=${FAILURE_DOMAIN}; see script header)"
fi

sleep 1
assert_code 202,200,204 DELETE "/ceph/pools/${PNAME}?force=true" || true
sleep 2
cleanup_unregister "rook-pool:${PNAME}"
