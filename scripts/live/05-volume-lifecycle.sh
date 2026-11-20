#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Volume lifecycle on real Ceph CSI (zyvor-rbd-prod): create→snap→schedule→expand→delete.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "05-volume-lifecycle"

NAME="${ATLAS_LIVE_PREFIX}-vol"
VOL_ID=""
SNAP_ID=""
SCHED_ID=""
JOB_ID=""

# Give discovery a beat to release the SQLite write lock after 04-discover.
sleep 3

VOL_BODY=$(printf '{"name":"%s","size_bytes":1073741824,"storage_class":"%s","tenant_id":"%s","access_mode":"ReadWriteOnce"}' \
  "$NAME" "$ATLAS_STORAGE_CLASS" "$ATLAS_TENANT_ID")

# Returns 0 on success, 2 on retryable SQLite lock, 1 on hard failure.
create_once() {
  local soft="${1:-}"
  assert_code 202,201 POST /volumes "$VOL_BODY" || true
  if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
    return 1
  fi
  JOB_ID="$(json_field job_id)"
  VOL_ID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("resource") or {}).get("volume_id") or d.get("volume_id") or d.get("id") or "")
' <<<"$LIVE_BODY")"
  [[ -n "$VOL_ID" ]] && cleanup_register "volume:${VOL_ID}"
  if [[ -z "$JOB_ID" ]]; then
    return 1
  fi
  if wait_job "$JOB_ID" "$soft"; then
    if [[ -z "$VOL_ID" ]]; then
      VOL_ID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("result") or {}).get("volume_id") or "")
' <<<"$LIVE_BODY")"
      [[ -n "$VOL_ID" ]] && cleanup_register "volume:${VOL_ID}"
    fi
    [[ -n "$VOL_ID" ]] && return 0
    return 1
  fi
  live_req GET "/jobs/${JOB_ID}"
  local err
  err="$(json_field error)"
  info "volume.create failed: ${err:-unknown}"
  if [[ "$err" == *"database is locked"* ]]; then
    return 2
  fi
  return 1
}

rc=0
create_once soft || rc=$?
if [[ $rc -eq 2 ]]; then
  info "retrying volume.create after SQLite lock"
  sleep 5
  rc=0
  create_once || rc=$?
fi

if [[ $rc -ne 0 || -z "$VOL_ID" ]]; then
  _record FAIL 0 GET /volumes "no volume_id after create (skipped lifecycle)"
  return 0 2>/dev/null || exit 0
fi

live_req GET "/volumes/${VOL_ID}"
if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
  _record FAIL "$LIVE_CODE" GET "/volumes/${VOL_ID}" "create finished but volume missing; skipping lifecycle"
  return 0 2>/dev/null || exit 0
fi
_record PASS "$LIVE_CODE" GET "/volumes/${VOL_ID}" "$(_snip "$LIVE_BODY")"
sleep 1

# Snapshot
assert_code 202,201 POST "/volumes/${VOL_ID}/snapshots" \
  "{\"name\":\"snap-${NAME}\"}" || true
SNAP_JOB="$(json_field job_id)"
SNAP_ID="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "{}")
print((d.get("resource") or {}).get("snapshot_id") or d.get("id") or "")
' <<<"$LIVE_BODY")"
[[ -n "$SNAP_ID" ]] && cleanup_register "snapshot:${SNAP_ID}"
if [[ -n "$SNAP_JOB" ]]; then
  wait_job "$SNAP_JOB" || true
fi
sleep 1

# Schedule (interval_secs — not cron)
assert_code 201,200 POST "/volumes/${VOL_ID}/schedule" \
  '{"interval_secs":21600,"kind":"snapshot","keep":2}' || true
SCHED_ID="$(json_field id)"
[[ -n "$SCHED_ID" ]] && cleanup_register "schedule:${SCHED_ID}"
sleep 1

# Expand (new_size_bytes)
assert_code 202,200 POST "/volumes/${VOL_ID}/expand" \
  '{"new_size_bytes":2147483648}' || true
EXP_JOB="$(json_field job_id)"
if [[ -n "$EXP_JOB" ]]; then
  wait_job "$EXP_JOB" || true
fi
sleep 1

# Tear down in order
if [[ -n "$SCHED_ID" ]]; then
  assert_ok DELETE "/schedules/${SCHED_ID}" || true
  cleanup_unregister "schedule:${SCHED_ID}"
fi
if [[ -n "$SNAP_ID" ]]; then
  assert_code 202,200,204 DELETE "/snapshots/${SNAP_ID}" || true
  sleep 2
  cleanup_unregister "snapshot:${SNAP_ID}"
fi
assert_code 202,200,204 DELETE "/volumes/${VOL_ID}?confirm=true" || true
wait_gone "/volumes/${VOL_ID}" || true
cleanup_unregister "volume:${VOL_ID}"
