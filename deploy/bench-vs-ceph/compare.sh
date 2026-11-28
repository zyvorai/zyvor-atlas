#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# The whole comparison in one session: Ceph, atlas-native as before, atlas-native with delta writes,
# one after another on the same disk, then a Markdown report. Each system is torn down before the next
# starts. Keep the calling session open: without lingering, containers may die when it closes.
# One session at a time on the host ($BENCH_ROOT/session.lock).
#
# Usage: compare.sh <tag>
#   PROFILE=quick|full
#   CASES="ceph atlas atlas-delta"   (also atlas-128k and atlas-delta-128k: 128 KiB cells)
#   ROUNDS=1   with more, all cases in turn, round after round (ceph, atlas-delta, ceph, atlas-delta,
#              ...), result sets <tag>-r<n>-<case>; with 1 they are <tag>-<case>.
#   ATLAS_DELTA_EXTRA='"name": value, ...'   more members of the "delta_writes" object in the delta
#              cases, e.g. '"fold_concurrency": 8'. The node rejects names it does not know.
#   Other run.sh and <system>-up.sh settings pass through.
# With STEADY_SECS set and ceph among the cases, gate.py then judges every atlas case against Ceph
# (<tag>-gate-<case>.txt). Its verdict does not change this script's exit status.
#
# The gate run: ROUNDS=3 CASES="ceph atlas-delta" STEADY_ONLY=1 STEADY_SECS=1200 compare.sh <tag>
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

TAG="${1:?usage: compare.sh <tag>}"
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }
CASES="${CASES:-ceph atlas atlas-delta}"
ROUNDS="${ROUNDS:-1}"
[[ "$ROUNDS" =~ ^[1-9][0-9]?$ ]] || { echo "ROUNDS must be 1..99" >&2; exit 2; }
export PROFILE="${PROFILE:-quick}"
[ "$PROFILE" != quick ] || export CEPH_OSD_GB="${CEPH_OSD_GB:-15}"

result_tag() { # <round> <case>
  if [ "$ROUNDS" -gt 1 ]; then echo "$TAG-r$1-$2"; else echo "$TAG-$2"; fi
}
system_of() { case "$1" in ceph) echo ceph ;; *) echo atlas ;; esac; }

# Everything that can be refused is refused before the first (long) case starts.
delta_cases=0
seen=" "
for case_ in $CASES; do
  case "$case_" in
    ceph | atlas | atlas-128k) ;;
    atlas-delta | atlas-delta-128k) delta_cases=$((delta_cases + 1)) ;;
    *) echo "unknown case: $case_" >&2; exit 2 ;;
  esac
  [[ "$seen" != *" $case_ "* ]] || { echo "case listed twice: $case_" >&2; exit 2; }
  seen="$seen$case_ "
  for round in $(seq 1 "$ROUNDS"); do
    out="$BENCH_ROOT/results/$(result_tag "$round" "$case_")/$(system_of "$case_")"
    [ ! -e "$out" ] || { echo "$out already exists; pick a new tag" >&2; exit 2; }
  done
done
if [ -n "${ATLAS_DELTA_EXTRA:-}" ]; then
  [ "$delta_cases" -gt 0 ] || { echo "ATLAS_DELTA_EXTRA is set but no delta case is in CASES" >&2; exit 2; }
  python3 -I - "$ATLAS_DELTA_EXTRA" <<'PY' || exit 2
import json, sys

def unique(pairs):
    keys = [k for k, _ in pairs]
    if len(keys) != len(set(keys)):
        raise ValueError(f"member given twice (\"enabled\" is set already): {keys}")
    return dict(pairs)

try:
    json.loads('{"enabled": true, %s}' % sys.argv[1], object_pairs_hook=unique)
except ValueError as e:
    sys.exit(f"ATLAS_DELTA_EXTRA must be JSON members such as '\"fold_concurrency\": 8': {e}")
PY
fi

exec 8>"$BENCH_ROOT/session.lock"
echo "waiting for the session lock"
flock -x 8
# Tells other work on this host to hold its builds until the measurements are done.
touch "$BENCH_ROOT/bench.wanted"
CURRENT=""
cleanup() {
  [ -z "$CURRENT" ] || "$HERE/down.sh" "$CURRENT" 8>&- || true
  rm -f "$BENCH_ROOT/bench.wanted"
}
trap cleanup EXIT

reports=()
failed=()
for round in $(seq 1 "$ROUNDS"); do
  for case_ in $CASES; do
    system="$(system_of "$case_")"
    delta='ATLAS_METADATA_EXTRA="delta_writes": {"enabled": true'"${ATLAS_DELTA_EXTRA:+, $ATLAS_DELTA_EXTRA}}"
    # ATLAS_DELTA_EXTRA is dropped where it does not apply, so result sets do not claim it.
    case "$case_" in
      ceph | atlas) extra=(-u ATLAS_DELTA_EXTRA) ;;
      atlas-128k) extra=(-u ATLAS_DELTA_EXTRA ATLAS_EXTENT_BYTES=131072) ;;
      atlas-delta) extra=("$delta") ;;
      atlas-delta-128k) extra=(ATLAS_EXTENT_BYTES=131072 "$delta") ;;
    esac
    name="$case_"
    [ "$ROUNDS" = 1 ] || name="$case_-r$round"
    out_tag="$(result_tag "$round" "$case_")"
    echo "=== $name: start $(date -Is)"
    "$HERE/down.sh" "$system" 8>&-
    CURRENT="$system"
    # 8>&-: containers started here must not keep the session lock.
    if env "${extra[@]}" "$HERE/$system-up.sh" 8>&- &&
      env "${extra[@]}" "BENCH_SESSION=$TAG" "BENCH_ROUND=$round" "$HERE/run.sh" "$system" "$out_tag" 8>&-; then
      echo "=== $name: done $(date -Is)"
    else
      echo "=== $name: FAILED $(date -Is)" >&2
      failed+=("$name")
    fi
    out="$BENCH_ROOT/results/$out_tag/$system"
    [ ! -d "$out" ] || reports+=("$name=$out")
    "$HERE/down.sh" "$system" 8>&-
    CURRENT=""
  done
done

if [ "${#reports[@]}" -gt 0 ]; then
  report="$BENCH_ROOT/results/$TAG.md"
  python3 -I "$HERE/report.py" "${reports[@]}" >"$report"
  cat "$report"
  echo "report: $report"
fi

if [ "${STEADY_SECS:-0}" -gt 0 ] && [[ " $CASES " == *" ceph "* ]]; then
  for case_ in $CASES; do
    [ "$case_" != ceph ] || continue
    ceph_sets=()
    atlas_sets=()
    # Every planned round is listed, so a missing one counts against the verdict.
    for round in $(seq 1 "$ROUNDS"); do
      ceph_sets+=("$BENCH_ROOT/results/$(result_tag "$round" ceph)/ceph")
      atlas_sets+=("$BENCH_ROOT/results/$(result_tag "$round" "$case_")/atlas")
    done
    verdict="$BENCH_ROOT/results/$TAG-gate-$case_.txt"
    rc=0
    python3 -I "$HERE/gate.py" --ceph "${ceph_sets[@]}" --atlas "${atlas_sets[@]}" >"$verdict" || rc=$?
    cat "$verdict"
    echo "gate for $case_: exit $rc ($verdict)"
  done
fi
[ "${#failed[@]}" -eq 0 ] || { echo "failed cases: ${failed[*]}" >&2; exit 1; }
