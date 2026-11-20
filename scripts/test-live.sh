#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Opt-in Tier-3 live tests against a deployed Ceph-mode Atlas gateway.
#
#   ATLAS_LIVE=1 ./scripts/test-live.sh
#   ATLAS_LIVE=1 ATLAS_BASE_URL=http://212.8.248.187:30511 ATLAS_TOKEN=<jwt> ./scripts/test-live.sh
#
# See scripts/live/README.md for env vars and what the suite mutates.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export ATLAS_LIVE="${ATLAS_LIVE:-1}"
exec bash "${HERE}/live/run-all.sh" "$@"
