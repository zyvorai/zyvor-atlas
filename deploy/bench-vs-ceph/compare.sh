#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# The whole comparison in one session: Ceph, atlas-native as before, atlas-native with delta writes,
# one after another on the same disk, then a Markdown report. Each system is torn down before the next
# starts. Keep the calling session open: without lingering, containers may die when it closes.
#
# Usage: compare.sh <tag>
#   PROFILE=quick|full   CASES="ceph atlas atlas-delta"   (plus atlas-128k)
#   Other run.sh and <system>-up.sh settings pass through.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

TAG="${1:?usage: compare.sh <tag>}"
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }
CASES="${CASES:-ceph atlas atlas-delta}"
export PROFILE="${PROFILE:-quick}"
[ "$PROFILE" != quick ] || export CEPH_OSD_GB="${CEPH_OSD_GB:-15}"

# Tells other work on this host to hold its builds until the measurements are done.
touch "$BENCH_ROOT/bench.wanted"
CURRENT=""
cleanup() {
  [ -z "$CURRENT" ] || "$HERE/down.sh" "$CURRENT" || true
  rm -f "$BENCH_ROOT/bench.wanted"
}
trap cleanup EXIT

reports=()
failed=()
for case_ in $CASES; do
  case "$case_" in
    ceph) system=ceph; extra=() ;;
    atlas) system=atlas; extra=() ;;
    atlas-128k) system=atlas; extra=(ATLAS_EXTENT_BYTES=131072) ;;
    atlas-delta) system=atlas; extra=('ATLAS_METADATA_EXTRA="delta_writes": {"enabled": true}') ;;
    *) echo "unknown case: $case_" >&2; exit 2 ;;
  esac
  echo "=== $case_: start $(date -Is)"
  "$HERE/down.sh" "$system"
  CURRENT="$system"
  if env "${extra[@]}" "$HERE/$system-up.sh" &&
    env "${extra[@]}" "$HERE/run.sh" "$system" "$TAG-$case_"; then
    echo "=== $case_: done $(date -Is)"
  else
    echo "=== $case_: FAILED $(date -Is)" >&2
    failed+=("$case_")
  fi
  out="$BENCH_ROOT/results/$TAG-$case_/$system"
  [ ! -d "$out" ] || reports+=("$case_=$out")
  "$HERE/down.sh" "$system"
  CURRENT=""
done

if [ "${#reports[@]}" -gt 0 ]; then
  report="$BENCH_ROOT/results/$TAG.md"
  python3 -I "$HERE/report.py" "${reports[@]}" >"$report"
  cat "$report"
  echo "report: $report"
fi
[ "${#failed[@]}" -eq 0 ] || { echo "failed cases: ${failed[*]}" >&2; exit 1; }
