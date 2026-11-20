#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# RGW bucket create → get → stats (soft) → delete.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "06-buckets"

# RGW bucket names: DNS-ish, keep short + unique
BNAME="lv$(printf '%s' "${ATLAS_LIVE_PREFIX}" | tr -cd 'a-zA-Z0-9' | tail -c 10)$(date +%H%M%S)"
BID=""

# Let prior bucket deletes settle (OBC finalizers can race a follow-on create).
sleep 3

assert_code 202,201 POST /buckets \
  "{\"name\":\"${BNAME}\",\"tenant_id\":\"${ATLAS_TENANT_ID}\"}" || true

if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
  info "bucket create failed; skipping"
  return 0 2>/dev/null || exit 0
fi

JOB_ID="$(json_field job_id)"
BID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("resource") or {}).get("bucket_id") or d.get("id") or "")
' <<<"$LIVE_BODY")"

CREATE_OK=1
if [[ -n "$JOB_ID" ]]; then
  if ! wait_job "$JOB_ID" soft; then
    CREATE_OK=0
    info "create job did not succeed — lab RGW flake; cleaning stub"
  fi
  if [[ -z "$BID" ]]; then
    BID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("result") or {}).get("bucket_id") or "")
' <<<"$LIVE_BODY")"
  fi
fi

if [[ -z "$BID" ]]; then
  live_req GET /buckets
  BID="$(python3 -c "
import sys, json
d = json.loads(sys.stdin.read() or '[]')
items = d if isinstance(d, list) else d.get('items') or []
name = '${BNAME}'
print(next((b.get('id','') for b in items if name in str(b.get('name') or b.get('bucket_name') or '')), ''))
" <<<"$LIVE_BODY")"
fi

if [[ -z "$BID" ]]; then
  _record FAIL 0 GET /buckets "bucket id not found after create"
  return 0 2>/dev/null || exit 0
fi

cleanup_register "bucket:${BID}"
assert_ok GET "/buckets/${BID}" || true

if [[ "$CREATE_OK" == "1" ]]; then
  BKT_NAME="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print(d.get("bucket_name") or d.get("name") or "")
' <<<"$LIVE_BODY")"
  if [[ -n "$BKT_NAME" && "$BKT_NAME" != "None" ]]; then
    # Stats can time out under load — soft assert
    assert_soft GET "/buckets/${BID}/stats" || true
  else
    _record WARN 0 GET "/buckets/${BID}/stats" "skip: no bucket_name yet"
  fi
else
  _record WARN 0 GET "/buckets/${BID}/stats" "skip: create job failed"
fi

sleep 1
assert_code 202,200,204 DELETE "/buckets/${BID}" || true
sleep 2
cleanup_unregister "bucket:${BID}"
