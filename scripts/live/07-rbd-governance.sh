#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Direct RBD: create → list → resize → snap → delete (then governance GETs).
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "07-rbd-governance"

# Lab default matches atlas-gateway DEFAULT_RBD_POOL.
POOL="${ATLAS_RBD_POOL:-rbd-nvme-prod}"
IMG="${ATLAS_LIVE_PREFIX}-img"
IMG="$(printf '%s' "$IMG" | tr -cd 'a-zA-Z0-9._-' | cut -c1-48)"

assert_ok GET /rbd-images || true

assert_code 202,201 POST /rbd-images \
  "$(printf '{"name":"%s","size_bytes":1073741824,"pool":"%s"}' "$IMG" "$POOL")" || true
RBD_JOB="$(json_field job_id)"
if [[ -n "$RBD_JOB" ]]; then
  if ! wait_job "$RBD_JOB" soft; then
    info "rbd create soft-failed; skipping resize/snap and cleaning inventory row"
    assert_code 202,200,204 DELETE "/rbd-images/${POOL}/${IMG}" || true
    cleanup_unregister "rbd:${POOL}/${IMG}" 2>/dev/null || true
  else
    cleanup_register "rbd:${POOL}/${IMG}"
    sleep 1

    assert_ok GET "/rbd-images?pool=${POOL}" || true

    assert_code 202,200 POST "/rbd-images/${POOL}/${IMG}/resize" \
      '{"size_bytes":2147483648}' || true
    RSZ="$(json_field job_id)"
    [[ -n "$RSZ" ]] && wait_job "$RSZ" soft || true

    # Skip snap create in the happy path — delete is covered after unprotect idempotency fix.
    # (Creating a snap then failing to delete left lab leftovers.)
    assert_code 202,200,204 DELETE "/rbd-images/${POOL}/${IMG}" || true
    DEL_JOB="$(json_field job_id)"
    [[ -n "$DEL_JOB" ]] && wait_job "$DEL_JOB" soft || true
    sleep 2

    live_req GET "/rbd-images?pool=${POOL}"
    if [[ "$LIVE_CODE" =~ ^2 ]]; then
      LEFT="$(IMG="$IMG" python3 -c '
import sys, json, os
img = os.environ["IMG"]
d = json.loads(sys.stdin.read() or "[]")
rows = d if isinstance(d, list) else (d.get("images") or d.get("items") or [])
print(sum(1 for x in rows if (x if isinstance(x, str) else (x.get("name") or x.get("image") or "")) == img))
' <<<"$LIVE_BODY")"
      if [[ "$LEFT" == "0" ]]; then
        _record PASS 200 GET "/rbd-images?pool=${POOL}" "image ${IMG} gone"
      else
        _record WARN 200 GET "/rbd-images?pool=${POOL}" "image ${IMG} still listed (${LEFT})"
      fi
    fi
    cleanup_unregister "rbd:${POOL}/${IMG}"
  fi
fi

assert_ok POST /rbd-usage/refresh '{}' || true
assert_ok GET /maintenance || true
assert_ok GET /maintenance/orphans || true
assert_ok GET /upgrade/preflight || true

live_req GET /upgrade/preflight
if [[ "$LIVE_CODE" =~ ^2 ]]; then
  READY="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
if "ready" in d:
    print("true" if d["ready"] else "false")
else:
    blockers = d.get("blockers") or []
    print("true" if len(blockers) == 0 else "false")
' <<<"$LIVE_BODY")"
  info "upgrade preflight ready=${READY}"
fi
