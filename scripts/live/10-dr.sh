#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# DR control-plane: create peer → preflight → direct-RBD → enable-mirror (soft) → delete all.
# Live rbd mirror dataplane is not asserted (needs a second Ceph cluster).
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "10-dr"

PEER_NAME="${ATLAS_LIVE_PREFIX}-peer"
PEER_ID=""
VOL_ID=""
MIRROR_ID=""
POOL="${ATLAS_RBD_POOL:-rbd-nvme-prod}"
IMG="${ATLAS_LIVE_PREFIX}-drimg"
IMG="$(printf '%s' "$IMG" | tr -cd 'a-zA-Z0-9._-' | cut -c1-48)"

assert_code 201,200 POST /dr/peers \
  "$(printf '{"name":"%s","cluster_fsid":"live-fsid-%s","direction":"rx-tx","secret_ref":"%s-bootstrap"}' \
    "$PEER_NAME" "$$" "$PEER_NAME")" || true
PEER_ID="$(json_field id)"
if [[ -z "$PEER_ID" ]]; then
  _record FAIL 0 POST /dr/peers "no peer id"
  return 0 2>/dev/null || exit 0
fi
cleanup_register "dr-peer:${PEER_ID}"

assert_ok GET /dr/peers || true
assert_ok GET /dr/status || true
assert_ok GET /dr/preflight || true

# Mirror enable requires a direct RBD image (not a CSI PVC volume).
assert_code 202,201 POST /rbd-images \
  "$(printf '{"name":"%s","size_bytes":1073741824,"pool":"%s"}' "$IMG" "$POOL")" || true
JOB_ID="$(json_field job_id)"
VOL_ID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("resource") or {}).get("volume_id") or "")
' <<<"$LIVE_BODY")"
[[ -n "$VOL_ID" ]] && cleanup_register "volume:${VOL_ID}"
cleanup_register "rbd:${POOL}/${IMG}"
[[ -n "$JOB_ID" ]] && wait_job "$JOB_ID" soft || true

if [[ -n "$VOL_ID" && -n "$PEER_ID" ]]; then
  live_req POST "/volumes/${VOL_ID}/mirror?mode=snapshot&peer=${PEER_ID}" '{}'
  if [[ "$LIVE_CODE" =~ ^2 ]]; then
    _record PASS "$LIVE_CODE" POST "/volumes/${VOL_ID}/mirror" "$(_snip "$LIVE_BODY")"
    MJ="$(json_field job_id)"
    [[ -n "$MJ" ]] && wait_job "$MJ" soft || true
  else
    _record WARN "$LIVE_CODE" POST "/volumes/${VOL_ID}/mirror" "dataplane soft: $(_snip "$LIVE_BODY")"
  fi
  sleep 1
  live_req GET /dr/mirrors
  if [[ "$LIVE_CODE" =~ ^2 ]]; then
    _record PASS "$LIVE_CODE" GET /dr/mirrors "$(_snip "$LIVE_BODY")"
    MIRROR_ID="$(VOL_ID="$VOL_ID" python3 -c '
import sys, json, os
vol = os.environ["VOL_ID"]
d = json.loads(sys.stdin.read() or "[]")
if isinstance(d, list):
    for m in d:
        if m.get("volume_id") == vol:
            print(m.get("id") or "")
            break
')"
  fi
  if [[ -n "${MIRROR_ID:-}" ]]; then
    live_req DELETE "/volumes/${VOL_ID}/mirror"
    if [[ "$LIVE_CODE" =~ ^2 ]] || [[ "$LIVE_CODE" == "404" ]]; then
      _record PASS "$LIVE_CODE" DELETE "/volumes/${VOL_ID}/mirror" "disabled"
    else
      _record WARN "$LIVE_CODE" DELETE "/volumes/${VOL_ID}/mirror" "$(_snip "$LIVE_BODY")"
    fi
  fi
fi

assert_code 202,200,204 DELETE "/rbd-images/${POOL}/${IMG}" || true
DEL_JOB="$(json_field job_id)"
[[ -n "$DEL_JOB" ]] && wait_job "$DEL_JOB" soft || true
cleanup_unregister "rbd:${POOL}/${IMG}"
[[ -n "$VOL_ID" ]] && cleanup_unregister "volume:${VOL_ID}"
sleep 1

assert_ok DELETE "/dr/peers/${PEER_ID}" || true
cleanup_unregister "dr-peer:${PEER_ID}"
live_req GET /dr/peers
if [[ "$LIVE_CODE" =~ ^2 ]]; then
  LEFT="$(PEER_ID="$PEER_ID" python3 -c '
import sys, json, os
pid = os.environ["PEER_ID"]
d = json.loads(sys.stdin.read() or "[]")
print(sum(1 for p in d if isinstance(p, dict) and p.get("id") == pid))
' <<<"$LIVE_BODY")"
  if [[ "$LEFT" == "0" ]]; then
    _record PASS 200 GET /dr/peers "peer ${PEER_ID} gone"
  else
    _record FAIL 200 GET /dr/peers "peer ${PEER_ID} still listed"
  fi
fi
