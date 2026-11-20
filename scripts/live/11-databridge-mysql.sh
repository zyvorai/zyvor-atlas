#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Fake MySQL DataBridge: create → discover → plan → stages → delete (create-then-delete hygiene).
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "11-databridge-mysql"

SRC_NAME="${ATLAS_LIVE_PREFIX}-mysql"
PLAN_NAME="${ATLAS_LIVE_PREFIX}-mysql-plan"
SRC_ID=""
PLAN_ID=""

assert_code 201,200 POST /databridge/sources \
  "$(printf '{"name":"%s","kind":"mysql","cloud":"rds","driver_mode":"fake"}' "$SRC_NAME")" || true
SRC_ID="$(json_field id)"
if [[ -z "$SRC_ID" ]]; then
  _record FAIL 0 POST /databridge/sources "no mysql source id"
  return 0 2>/dev/null || exit 0
fi
cleanup_register "db-source:${SRC_ID}"

assert_code 202,200 POST "/databridge/sources/${SRC_ID}/discover" '{}' || true
DISC_JOB="$(json_field job_id)"
[[ -n "$DISC_JOB" ]] && wait_job "$DISC_JOB" soft || true
sleep 1

assert_code 201,200 POST /databridge/plans \
  "$(printf '{"name":"%s","source_id":"%s"}' "$PLAN_NAME" "$SRC_ID")" || true
PLAN_ID="$(json_field id)"
if [[ -z "$PLAN_ID" ]]; then
  assert_ok DELETE "/databridge/sources/${SRC_ID}" || true
  cleanup_unregister "db-source:${SRC_ID}"
  return 0 2>/dev/null || exit 0
fi
cleanup_register "db-plan:${PLAN_ID}"

for stage in assess provision full-load cdc/start validate; do
  ok=0
  for attempt in 1 2; do
    live_req POST "/databridge/plans/${PLAN_ID}/${stage}" '{}'
    if [[ "$LIVE_CODE" =~ ^2 ]]; then
      _record PASS "$LIVE_CODE" POST "/databridge/plans/${PLAN_ID}/${stage}" "$(_snip "$LIVE_BODY")"
      ok=1
      break
    fi
    if [[ "$LIVE_CODE" == "0" ]]; then
      _record WARN "$LIVE_CODE" POST "/databridge/plans/${PLAN_ID}/${stage}" "attempt ${attempt}: $(_snip "$LIVE_BODY")"
      sleep 5
      continue
    fi
    _record WARN "$LIVE_CODE" POST "/databridge/plans/${PLAN_ID}/${stage}" "$(_snip "$LIVE_BODY")"
    break
  done
  if [[ "$ok" -eq 1 ]]; then
    JOB="$(json_field job_id)"
    [[ -n "$JOB" ]] && wait_job "$JOB" soft || true
  fi
  sleep 2
done

assert_ok GET "/databridge/plans/${PLAN_ID}" || true

assert_ok DELETE "/databridge/plans/${PLAN_ID}" || true
cleanup_unregister "db-plan:${PLAN_ID}"
sleep 1
assert_ok DELETE "/databridge/sources/${SRC_ID}" || true
cleanup_unregister "db-source:${SRC_ID}"
wait_gone "/databridge/sources/${SRC_ID}" || true
