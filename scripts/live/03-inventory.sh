#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Inventory + observability + DR + DataBridge list GETs.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "03-inventory"

PATHS=(
  /backends
  /backends/summary
  /clusters
  /nodes
  /osds
  /pools
  /volumes
  /volumes.csv
  /rbd-images
  /snapshots
  /schedules
  /buckets
  /backups
  /storage-classes
  /kubernetes/pvcs
  /kubernetes/pvs
  /tenants
  /policies
  /maintenance
  /maintenance/orphans
  /upgrade/preflight
  /dr/peers
  /dr/mirrors
  /dr/status
  /dr/preflight
  /ceph/status
  /ceph/osd-tree
  /ceph/osd-df
  /ceph/df
  /ceph/health-rollup
  /ceph/rook-status
  /ceph/pools
  /ceph/filesystems
  /ceph/object-stores
  /alerts
  /jobs
  /audit
  /audit.csv
  /events
  /chargeback
  /policy-drift
  /auth/tokens/revoked
  /databridge/sources
  /databridge/plans
  /databridge/edge-clusters
  /databridge/cdc-streams
  /databridge/validations
  /databridge/cutovers
)

for p in "${PATHS[@]}"; do
  assert_ok GET "$p" || true
done

# DR preflight should report blockers when no peers (scaffold-only).
live_req GET /dr/preflight
if [[ "$LIVE_CODE" =~ ^2 ]]; then
  if printf '%s' "$LIVE_BODY" | grep -q 'peer'; then
    _record PASS "$LIVE_CODE" GET /dr/preflight "expected peer blocker present"
  else
    _record WARN "$LIVE_CODE" GET /dr/preflight "no peer blocker text (ok if peers registered)"
  fi
fi
