#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Tier-3 live suite orchestrator. Hits a deployed Atlas gateway (default:
# http://212.8.248.187:30511). Requires ATLAS_LIVE=1.
#
# Usage:
#   ATLAS_LIVE=1 ./scripts/live/run-all.sh
#   ATLAS_LIVE=1 ATLAS_TOKEN=<jwt> ATLAS_BASE_URL=http://host:30511 ./scripts/live/run-all.sh
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
source "${HERE}/lib.sh"

if [[ "${ATLAS_LIVE:-0}" != "1" ]]; then
  echo "Refusing to run: set ATLAS_LIVE=1 to enable the live suite (mutates remote)." >&2
  echo "  ATLAS_LIVE=1 $0" >&2
  exit 2
fi

ATLAS_LIVE_LOG="${ATLAS_LIVE_LOG:-$(mktemp -t atlas-live.XXXXXX.log)}"
export ATLAS_LIVE_LOG
: >"$ATLAS_LIVE_LOG"

trap 'live_cleanup' EXIT

log "live suite → ${ATLAS_BASE_URL} (prefix=${ATLAS_LIVE_PREFIX})"
log "results log: ${ATLAS_LIVE_LOG}"

# Probe first without auth so a dead NodePort fails fast.
section "boot"
_saved="${ATLAS_TOKEN:-}"
ATLAS_TOKEN=""
live_req GET /health
ATLAS_TOKEN="$_saved"
if [[ "$LIVE_CODE" != "200" ]]; then
  echo "gateway unreachable at ${ATLAS_BASE_URL}/health (HTTP ${LIVE_CODE}): $(_snip "$LIVE_BODY")" >&2
  exit 3
fi
info "health ok"

ensure_token

SECTIONS=(
  01-probe.sh
  02-auth.sh
  03-inventory.sh
  04-discover.sh
  05-volume-lifecycle.sh
  06-buckets.sh
  07-rbd-governance.sh
  08-metrics-alerts.sh
  09-databridge.sh
  10-dr.sh
  11-databridge-mysql.sh
  12-rook.sh
)

for s in "${SECTIONS[@]}"; do
  # Source so PASS/FAIL counters and CLEANUP_ITEMS accumulate.
  # shellcheck disable=SC1090
  source "${HERE}/${s}"
  # SQLite lab: give writers a beat between discover / mutate sections.
  sleep 2
done

live_cleanup
trap - EXIT

printf '\n%s========== SUMMARY ==========%s\n' "$_c_cyan" "$_c_reset"
printf 'PASS=%s FAIL=%s WARN=%s  base=%s\n' "$PASS_N" "$FAIL_N" "$WARN_N" "$ATLAS_BASE_URL"
printf 'log: %s\n' "$ATLAS_LIVE_LOG"

if (( FAIL_N > 0 )); then
  exit 1
fi
exit 0
