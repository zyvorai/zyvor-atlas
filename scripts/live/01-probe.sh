#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Probe: unauthenticated health surface.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "01-probe"

# Probes (except /metrics when auth is required) work without a bearer token.
_saved="${ATLAS_TOKEN:-}"
ATLAS_TOKEN=""
assert_ok GET /health || true
assert_ok GET /livez || true
assert_ok GET /readyz || true
assert_ok GET /version || true
# Prometheus /metrics is behind the auth layer when ATLAS_AUTH_REQUIRED=1.
assert_code 200,401 GET /metrics || true
ATLAS_TOKEN="$_saved"
# Authenticated scrape must succeed.
assert_ok GET /metrics || true

