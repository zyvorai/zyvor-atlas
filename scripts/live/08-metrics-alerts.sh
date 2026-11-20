#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Metrics series + alert evaluation.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

section "08-metrics-alerts"

assert_ok GET /metrics/summary || true
assert_ok GET /metrics/ceph || true
assert_ok GET /metrics/history || true
assert_ok GET /metrics/forecast || true
assert_ok GET /alerts || true
assert_ok POST /alerts/evaluate '{}' || true
