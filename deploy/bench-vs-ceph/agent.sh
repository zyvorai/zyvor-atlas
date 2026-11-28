#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# The agent-workload benchmark of docs/NATIVE_STORAGE_RESEARCH.md, atlas-native against CephFS, with
# the rules of compare.sh: one system at a time under bench.lock, the same container limits, three
# copies, the same disk, FUSE on both sides, rounds in alternating order.
#
# Usage: agent.sh <profile> <tag>     run a profile: all rounds, then the report
#        agent.sh fixtures            generate (or check) the fixtures of the current settings
#        agent.sh report <tag>        judge a tag's results again
#
# Profiles (what each measures, on what):
#   agent      phases A install, B git, C verify (with kill -9 of the client), E fan-out, F discard,
#              G large files, server data on the benchmark disk. CASES="ceph atlas floor"
#              (floor runs E and F only)
#   fork       phase D, fork time against tree size, server data on TMPFS (labelled in the results), a
#              fresh cluster for each size. CASES="ceph atlas floor"            (research bet 2)
#   smallfile  run.sh's smallfile-create-4k job at 1, 8 and 32 writers, on the benchmark disk, with
#              atlas-native's sync metrics. CASES="ceph atlas"                  (research bet 1)
# floor is cp -a --reflink=always on the host's XFS: one copy, no storage system, no container limits.
#
# Settings (environment):
#   ROUNDS=3                 fewer cannot give a verdict
#   CASES                    ceph, atlas, floor and atlas-<name> for variants (below)
#   AGENT_PHASES             of the agent profile, e.g. "A B C" (B needs A; C needs A and B; F needs E)
#   AGENT_METHODS            of D and E: "clone overlay overlay-shared"
#   AGENT_FORK_SIZES="1000 10000 100000"  AGENT_FORK_MODES="quiet busy"  AGENT_FORK_FILE_BYTES=1024
#   AGENT_FANOUT="1 4 16"    AGENT_WRITERS="1 8 32"    AGENT_SMALL_FILES=2000 (in total per job)
#   AGENT_FILES=2000 AGENT_REPO_FILES=1000 AGENT_COMMITS=20 AGENT_SEED=1
#   AGENT_EDITS=20  AGENT_LARGE_FILES=4  AGENT_LARGE_MIB=256
#   AGENT_SETTLE_SECS=10 AGENT_BG_BASE_SECS=10 AGENT_BG_AFTER_SECS=10
#   AGENT_STABLE_SECS=90 AGENT_STABLE_TIMEOUT=900 (phase F)  AGENT_CLONE_TIMEOUT=3600
#   AGENT_CEPH_MAX_CLONES=0  0 leaves mgr/volumes/max_concurrent_clones at Ceph's default
#   AGENT_TMPFS_GB           server data on tmpfs (default 10 for fork, 0 = disk otherwise)
#   BENCH_MEM                container limit (12g; 20g on tmpfs, which counts against it)
#   CEPH_OSD_GB=15           disk runs: size of each OSD block file (written once per cluster)
#   AGENT_SEGMENT_TIMEOUT=28800  seconds one cluster's items may take
#   AGENT_SKIP_BUILD=1       use the atlas images an earlier call built
#   AGENT_FIXTURES=<dir>     default $BENCH_ROOT/fixtures/<name from the settings>
#
# Comparing a feature branch or settings with the base: add a case atlas-<name> and a file
# $AGENT_VARIANTS/<name>.env of KEY=VALUE lines, for example
#   BENCH_ATLAS_SRC=/data/atlas-bench/agents/x/src
#   ATLAS_METADATA_EXTRA="some_setting": true
#   ATLAS_DATA_NODE_EXTRA="name": 1          members of every data node's "data_node" object
#   ATLAS_NODE_EXTRA="name": 1               members of every node's whole configuration
# (values are taken as written: no quotes around the line). ATLAS_* variables set in the calling
# environment apply to every atlas case, the base included.
#
# Results: $BENCH_ROOT/results/<tag>-r<round>-<case>/agent/, the plan and report in
# $BENCH_ROOT/results/<tag>-agent/. Exit status: 0 PASS, 1 FAIL, 3 UNKNOWN (not a pass), 2 usage.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

usage() { sed -n '/^# Usage:/,/^#$/p' "$0" >&2; exit 2; }
lib() { python3 -I "$HERE/agent_lib.py" "$@"; }

PROFILE_NAME="${1:-}"
case "$PROFILE_NAME" in
  agent | fork | smallfile) [ $# -eq 2 ] || usage ;;
  fixtures) [ $# -eq 1 ] || usage ;;
  report)
    [ $# -eq 2 ] && [[ "$2" =~ ^[A-Za-z0-9_-]+$ ]] || usage
    dir="$BENCH_ROOT/results/$2-agent"
    rc=0
    python3 -I "$HERE/agent_report.py" "$dir/plan.json" >"$dir/report.md" || rc=$?
    cat "$dir/report.md"
    echo "report: $dir/report.md"
    exit "$rc"
    ;;
  *) usage ;;
esac

# The fixtures of a plan: generated once, checked every time.
fixtures() { # <plan.json>
  local args
  mapfile -t args < <(lib fixtures "$1")
  [ "${#args[@]}" -gt 1 ] || { echo "cannot read the fixture settings" >&2; exit 2; }
  FIXTURES="${AGENT_FIXTURES:-$BENCH_ROOT/fixtures/${args[0]}}"
  case "$FIXTURES" in
    *..* | *:*) echo "AGENT_FIXTURES must not contain '..' or ':'" >&2; exit 2 ;;
    /data/atlas-bench/*) ;;
    *) echo "the fixtures must be under /data/atlas-bench, got: $FIXTURES" >&2; exit 2 ;;
  esac
  python3 -I "$HERE/agent_fixtures.py" ensure "$FIXTURES" "${args[@]:1}"
}

if [ "$PROFILE_NAME" = fixtures ]; then
  plan="$(mktemp "$BENCH_ROOT/tmp/agent-plan.XXXXXX")"
  trap 'rm -f "$plan"' EXIT
  lib plan fork fixtures "$plan"
  fixtures "$plan"
  echo "fixtures: $FIXTURES"
  exit 0
fi

TAG="$2"
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }
PLANDIR="$BENCH_ROOT/results/$TAG-agent"
SEGMENT_TIMEOUT="${AGENT_SEGMENT_TIMEOUT:-28800}"
FLOOR="$BENCH_ROOT/data/agent-floor"
[ ! -e "$PLANDIR" ] || { echo "$PLANDIR already exists; pick a new tag" >&2; exit 2; }
plan_tmp="$(mktemp "$BENCH_ROOT/tmp/agent-plan.XXXXXX")"
trap 'rm -f "$plan_tmp"' EXIT
# Everything that can be refused is refused before the first (long) step.
lib plan "$PROFILE_NAME" "$TAG" "$plan_tmp"
TMPFS_GB="$(lib get "$plan_tmp" tmpfs_gb)"
MEM="$(lib get "$plan_tmp" mem)"
runs="$(lib runs "$plan_tmp")"
while read -r _ _ _ dir; do
  [ ! -e "$BENCH_ROOT/results/$dir/agent" ] || { echo "$BENCH_ROOT/results/$dir/agent exists; pick a new tag" >&2; exit 2; }
done <<<"$runs"
if [ "$TMPFS_GB" != 0 ]; then
  [[ "$MEM" =~ ^([1-9][0-9]*)g$ ]] || { echo "BENCH_MEM must be <n>g on tmpfs" >&2; exit 2; }
  mem_gb="${BASH_REMATCH[1]}"
  [ "$mem_gb" -gt "$TMPFS_GB" ] || { echo "BENCH_MEM=$MEM must exceed the tmpfs of $TMPFS_GB GiB" >&2; exit 2; }
fi
fixtures "$plan_tmp"
FIXTURE_HEAD="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["head"])' "$FIXTURES/fixtures.json")"

exec 8>"$BENCH_ROOT/session.lock"
echo "waiting for the session lock"
flock -x 8
WANTED=0
CURRENT=""
cleanup() {
  [ -z "$CURRENT" ] || "$HERE/down.sh" "$CURRENT" 8>&- 9>&- || true
  [ ! -d "$FLOOR" ] || rm -rf -- "$FLOOR"
  [ "$WANTED" = 0 ] || rm -f "$BENCH_ROOT/bench.wanted"
  rm -f "$plan_tmp"
}
trap cleanup EXIT
for system in ceph atlas; do
  if bpodman ps --format '{{.Names}}' 8>&- | grep -qx "$(container_name "$system")"; then
    echo "$(container_name "$system") is running; stop it first (down.sh $system)" >&2
    exit 2
  fi
done
# Tells other work on this host to hold its builds until the measurements are done.
[ -e "$BENCH_ROOT/bench.wanted" ] || { touch "$BENCH_ROOT/bench.wanted"; WANTED=1; }
mkdir -p "$PLANDIR"
cp "$plan_tmp" "$PLANDIR/plan.json"
PLAN="$PLANDIR/plan.json"
DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"

# case_env <case>: sets CASE_ENV, the case's own settings as env arguments.
case_env() { mapfile -t CASE_ENV < <(lib env "$PLAN" "$1"); }
atlas_image() { echo "localhost/$BENCH_NAME_PREFIX-atlas:agent-$1"; }

# One atlas image per case, all built before anything is measured.
for case_ in $(awk '$3 == "atlas" && !seen[$2]++ {print $2}' <<<"$runs"); do
  [ "${AGENT_SKIP_BUILD:-0}" != 1 ] || continue
  case_env "$case_"
  echo "=== building the image of $case_"
  env "${CASE_ENV[@]}" BENCH_ATLAS_BUILD_ONLY=1 "BENCH_ATLAS_IMAGE=$(atlas_image "$case_")" ATLAS_SKIP_BUILD=0 \
    "$HERE/atlas-up.sh" 8>&-
done

commit_of() { # <source dir>
  local c=""
  [ ! -f "$1/deploy/bench-vs-ceph/COMMIT" ] || c="$(cat "$1/deploy/bench-vs-ceph/COMMIT")"
  if [ -z "$c" ] && c="$(git -C "$1" rev-parse HEAD 2>/dev/null)"; then
    [ -z "$(git -C "$1" status --porcelain 2>/dev/null)" ] || c="$c-dirty"
  fi
  echo "${c:-unknown}"
}
host_conditions() { # <suffix>
  echo "loadavg$1=$(cat /proc/loadavg)"
  echo "io_pressure$1=$(tr '\n' ';' </proc/pressure/io)"
}

# start_cluster <system> <case>
start_cluster() {
  local system=$1 case_=$2 env_=(BENCH_FIXTURES="$FIXTURES" BENCH_MEM="$MEM")
  if [ "$TMPFS_GB" != 0 ]; then
    local free_gb
    free_gb="$(awk '$1 == "MemAvailable:" {print int($2 / 1048576)}' /proc/meminfo)"
    [ "$free_gb" -ge "${MEM%g}" ] ||
      { echo "only $free_gb GiB of memory available, the container limit is $MEM" >&2; return 1; }
    env_+=(BENCH_TMPFS_GB="$TMPFS_GB")
  fi
  if [ "$system" = ceph ]; then
    if [ "$TMPFS_GB" != 0 ]; then
      env_+=(CEPH_OSD_GB="${CEPH_OSD_GB:-10}" CEPH_PREFILL=sparse)
    else
      env_+=(CEPH_OSD_GB="${CEPH_OSD_GB:-15}")
    fi
    env "${env_[@]}" "$HERE/ceph-up.sh" 8>&-
  else
    case_env "$case_"
    env "${CASE_ENV[@]}" "${env_[@]}" ATLAS_SKIP_BUILD=1 "BENCH_ATLAS_IMAGE=$(atlas_image "$case_")" \
      "$HERE/atlas-up.sh" 8>&-
  fi
}

# in_container <container> <config> <out> <segment>: the runner, under the benchmark lock.
in_container() {
  local ct=$1 config=$2 out=$3 segment=$4 rc=0
  bpodman exec "$ct" mkdir -p /tmp/agent 9>&-
  local f
  for f in agent_lib.py agent_run.py; do bpodman cp "$HERE/$f" "$ct:/tmp/agent/$f" 9>&-; done
  bpodman cp "$config" "$ct:/tmp/agent/config.json" 9>&-
  timeout "$SEGMENT_TIMEOUT" "${BPODMAN[@]}" exec "$ct" python3 -I /tmp/agent/agent_run.py /tmp/agent/config.json \
    9>&- >"$out/$segment.jsonl" 2> >(tee "$out/$segment.log" >&2 9>&-) || rc=$?
  return "$rc"
}

failed=()
while read -r round case_ system dir <&7; do
  out="$BENCH_ROOT/results/$dir/agent"
  mkdir -p "$out"
  # The tree this case's binaries are built from (this one for ceph and floor).
  src="$HERE/../.."
  if [ "$system" = atlas ]; then
    src="${BENCH_ATLAS_SRC:-$src}"
    case_env "$case_"
    for kv in "${CASE_ENV[@]}"; do
      case "$kv" in BENCH_ATLAS_SRC=*) src="${kv#*=}" ;; esac
    done
  fi
  {
    echo "date=$(date -Is)"
    echo "kernel=$(uname -r)"
    echo "disk=$DISK"
    echo "system=$system"
    echo "case=$case_"
    echo "session=$TAG"
    echo "round=$round"
    echo "commit=$(commit_of "$src")"
    echo "harness_sha256=$(cat "$HERE"/agent*.py "$HERE"/agent.sh "$HERE"/*-entry.sh | sha256sum | cut -d' ' -f1)"
    echo "tmpfs_gb=$([ "$system" = floor ] && echo 0 || echo "$TMPFS_GB")"
    echo "mem=$([ "$system" = floor ] && echo host || echo "$MEM")"
    echo "cpus=$([ "$system" = floor ] && echo host || echo "$BENCH_CPUS")"
    echo "fixtures_head=$FIXTURE_HEAD"
    env | grep -E '^(ATLAS|CEPH|AGENT)_' | sort || true
    host_conditions ""
  } >"$out/env.txt"

  while read -r segment phases sizes <&6; do
    echo "=== round $round $case_ $segment: start $(date -Is)"
    status=ok
    config="$out/$segment.config.json"
    if [ "$system" = floor ]; then
      lib config "$PLAN" floor "$round" "$phases" "$sizes" "$FIXTURES" "$DISK" "$FLOOR" >"$config"
      rm -rf -- "$FLOOR"
      (
        flock -x 9
        timeout "$SEGMENT_TIMEOUT" python3 -I "$HERE/agent_run.py" "$config" 9>&- \
          >"$out/$segment.jsonl" 2> >(tee "$out/$segment.log" >&2 9>&-)
      ) 8>&- 9>"$BENCH_ROOT/bench.lock" || status="runner-exit-$?"
      rm -rf -- "$FLOOR"
    else
      ct="$(container_name "$system")"
      lib config "$PLAN" "$system" "$round" "$phases" "$sizes" /fixtures "$DISK" /unused >"$config"
      CURRENT="$system"
      "$HERE/down.sh" "$system" 8>&-
      if start_cluster "$system" "$case_" >"$out/$segment.up.log" 2>&1; then
        if [ "$system" = atlas ]; then
          grep -q '^node_binary=' "$out/env.txt" || {
            echo "node_binary=$(bpodman exec "$ct" sha256sum /usr/local/bin/atlas-native-node | cut -d' ' -f1)"
          } >>"$out/env.txt"
        fi
        echo "image_$segment=$(bpodman inspect -f '{{.Image}}' "$ct")" >>"$out/env.txt"
        (
          flock -x 9
          in_container "$ct" "$config" "$out" "$segment"
        ) 8>&- 9>"$BENCH_ROOT/bench.lock" || status="runner-exit-$?"
        if [ "$system" = atlas ]; then
          bpodman exec "$ct" cat /srv/bench/settings.txt >"$out/settings-$segment.txt" || true
        else
          bpodman exec "$ct" timeout 60 ceph config dump >"$out/settings-$segment.txt" || true
        fi
        if [ "$status" != ok ]; then
          bpodman logs --tail 200 "$ct" >"$out/$segment.container.log" 2>&1 || true
          bpodman cp "$ct:/tmp/agent-logs" "$out/$segment.mount-logs" 2>/dev/null || true
          [ "$system" != atlas ] || bpodman cp "$ct:/var/log/atlas" "$out/$segment.node-logs" 2>/dev/null || true
        fi
      else
        status=up-failed
        tail -n 40 "$out/$segment.up.log" >&2
        bpodman logs --tail 200 "$ct" >"$out/$segment.container.log" 2>&1 || true
      fi
      "$HERE/down.sh" "$system" 8>&-
      CURRENT=""
    fi
    echo "segment_$segment=$status" >>"$out/env.txt"
    if [ "$status" = ok ]; then
      echo "=== round $round $case_ $segment: done $(date -Is)"
    else
      echo "=== round $round $case_ $segment: FAILED ($status) $(date -Is)" >&2
      failed+=("r$round/$case_/$segment")
    fi
  done 6< <(lib segments "$PLAN" "$system")
  {
    echo "end=$(date -Is)"
    host_conditions "_end"
  } >>"$out/env.txt"
done 7<<<"$runs"

rc=0
python3 -I "$HERE/agent_report.py" "$PLAN" >"$PLANDIR/report.md" || rc=$?
cat "$PLANDIR/report.md"
echo "report: $PLANDIR/report.md"
[ "${#failed[@]}" -eq 0 ] || echo "segments that did not succeed: ${failed[*]}" >&2
exit "$rc"
