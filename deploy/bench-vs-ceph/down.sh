#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Removes one system's benchmark container and its server data. Results, images and build caches stay.
#
# Usage: down.sh <ceph|atlas>
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

SYSTEM="${1:?usage: down.sh <ceph|atlas>}"
case "$SYSTEM" in ceph | atlas) ;; *) echo "unknown system: $SYSTEM" >&2; exit 2 ;; esac

bpodman rm -f --ignore -t 30 "$(container_name "$SYSTEM")"
# Files written inside the user namespace may belong to subordinate uids.
bpodman unshare rm -rf -- "$BENCH_ROOT/data/$SYSTEM"
echo "removed $(container_name "$SYSTEM") and $BENCH_ROOT/data/$SYSTEM"
