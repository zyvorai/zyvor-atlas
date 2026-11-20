#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Discover: Ceph only by default (serialized). Set ATLAS_LIVE_DISCOVER_ALL=1 for nfs/zfs too.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "04-discover"

live_req GET /backends
if [[ ! "$LIVE_CODE" =~ ^2 ]]; then
  _record FAIL "$LIVE_CODE" GET /backends "$(_snip "$LIVE_BODY")"
  return 0 2>/dev/null || exit 0
fi

IDS="$(python3 -c '
import sys, json
d = json.loads(sys.stdin.read() or "[]")
items = d if isinstance(d, list) else d.get("items") or d.get("backends") or []
for b in items:
    i = b.get("id") or ""
    t = (b.get("backend_type") or b.get("type") or "").lower()
    print(f"{i}\t{t}")
' <<<"$LIVE_BODY")"

while IFS=$'\t' read -r id typ; do
  [[ -z "$id" ]] && continue
  if [[ "$typ" == "ceph" ]] || [[ "$id" == *ceph* ]]; then
    assert_ok POST "/backends/${id}/discover" || true
    sleep 1
  elif [[ "${ATLAS_LIVE_DISCOVER_ALL:-0}" == "1" ]]; then
    assert_ok POST "/backends/${id}/discover" || true
    sleep 1
  else
    info "skip discover ${id} (${typ}) — set ATLAS_LIVE_DISCOVER_ALL=1 to include"
  fi
done <<<"$IDS"
