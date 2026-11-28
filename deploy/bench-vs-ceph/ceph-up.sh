#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Starts atlas-bench-ceph (see common.sh for the contract) and exits 0 only once all three OSDs are up,
# every PG is active+clean, health is OK (or only the warnings in BENIGN), BlueStore runs with its HDD
# settings and the CephFS mount is writable. Safe to re-run. Remove with: down.sh ceph
#
# Usage: ceph-up.sh
#   CEPH_OSD_GB=40     size of each of the three OSD block files (at most 42)
#   CEPH_PREFILL=1     write the block files with zeros first (about 15 min on one HDD); 0 = fallocate;
#                      sparse = truncate, the only choice on tmpfs (BENCH_TMPFS_GB), where it is the
#                      default and BlueStore picks its own device class
#   BENCH_CEPH_IMAGE   image name (default localhost/<prefix>-ceph:<release>)
#   CEPH_UP_TIMEOUT=2700
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

CEPH_RELEASE=v20.2.5
CEPH_OSD_GB="${CEPH_OSD_GB:-40}"
if [ -n "${BENCH_TMPFS_GB:-}" ]; then
  CEPH_PREFILL="${CEPH_PREFILL:-sparse}"
  [ "$CEPH_PREFILL" = sparse ] || { echo "CEPH_PREFILL must be sparse on tmpfs" >&2; exit 2; }
else
  CEPH_PREFILL="${CEPH_PREFILL:-1}"
fi
CEPH_UP_TIMEOUT="${CEPH_UP_TIMEOUT:-2700}"
CT="$(container_name ceph)"
UPSTREAM="quay.io/ceph/ceph:$CEPH_RELEASE"
BASE="localhost/atlas-bench-ceph-base:$CEPH_RELEASE"
IMAGE="${BENCH_CEPH_IMAGE:-localhost/$BENCH_NAME_PREFIX-ceph:$CEPH_RELEASE}"
DATA="$BENCH_ROOT/data/ceph"
# What a running container must have been started with, beyond its image.
RUN_SIG="${BENCH_TMPFS_GB:-}${BENCH_FIXTURES:-}"
# Health checks that do not stop the benchmark. BLUESTORE_SLOW_OP_ALERT reports slow disk operations
# (latency on a loaded HDD, kept for 24 h), not a data or redundancy problem. It is printed if present.
BENIGN='["BLUESTORE_SLOW_OP_ALERT"]'

[[ "$CEPH_OSD_GB" =~ ^[0-9]+$ ]] && [ "$CEPH_OSD_GB" -ge 10 ] && [ "$CEPH_OSD_GB" -le 42 ] ||
  { echo "CEPH_OSD_GB must be 10..42 (three files must stay under 130 GiB)" >&2; exit 2; }

# Setup writes a lot to the shared benchmark disk, so it never overlaps a measurement: run.sh holds this
# lock exclusively, builds and setup share it. 9>&- keeps long-lived podman processes from inheriting it.
exec 9>"$BENCH_ROOT/bench.lock"
echo "waiting for the benchmark lock"
flock -s 9
trap 'flock -u 9' EXIT
pm() { bpodman "$@" 9>&-; }

SRC_HASH="$(cat "$HERE/ceph-Containerfile" "$HERE/ceph-entry.sh" | sha256sum | cut -c1-16)"
if [ "$(pm image inspect -f '{{index .Labels "atlas.bench.src"}}' "$IMAGE" 2>/dev/null || true)" != "$SRC_HASH" ]; then
  if ! pm image exists "$BASE"; then
    pm pull "$UPSTREAM"
    # Keep only atlas-bench-* names in the benchmark's image store.
    pm tag "$UPSTREAM" "$BASE"
    pm untag "$UPSTREAM" "$UPSTREAM"
  fi
  pm build --layers=false --pull=never --build-arg "BASE=$BASE" --label "atlas.bench.src=$SRC_HASH" \
    -t "$IMAGE" -f "$HERE/ceph-Containerfile" "$HERE"
fi

running() { [ "$(pm inspect -f '{{.State.Running}}' "$CT" 2>/dev/null || true)" = true ]; }
label() { pm inspect -f "{{index .Config.Labels \"$1\"}}" "$CT" 2>/dev/null || true; }
current() { [ "$(label atlas.bench.src)" = "$SRC_HASH" ] && [ "$(label atlas.bench.run)" = "$RUN_SIG" ]; }

if running && current; then
  echo "$CT is already running; checking it"
else
  pm rm -f --ignore -t 60 "$CT" >/dev/null
  bench_mounts ceph
  if [ -z "${BENCH_TMPFS_GB:-}" ] && [ ! -f "$DATA/ceph/bootstrapped" ]; then
    free_gb=$(($(df --output=avail -B1G "$DATA" | tail -1)))
    need_gb=$((CEPH_OSD_GB * 3 + 50))
    [ "$free_gb" -ge "$need_gb" ] || { echo "need $need_gb GiB free under $DATA, have $free_gb" >&2; exit 1; }
  fi
  pm run -d --name "$CT" --hostname atlas-bench-ceph --network none --stop-timeout 60 \
    "${BENCH_RUN_ARGS[@]}" "${BENCH_MOUNT_ARGS[@]}" --label "atlas.bench.run=$RUN_SIG" \
    -e "CEPH_OSD_GB=$CEPH_OSD_GB" -e "CEPH_PREFILL=$CEPH_PREFILL" "$IMAGE" >/dev/null
fi

fail() {
  echo "ceph-up failed: $1" >&2
  pm logs --tail 40 "$CT" >&2 || true
  if running; then pm exec "$CT" timeout 20 ceph -s >&2 || true; fi
  exit 1
}
inct() { pm exec "$CT" "$@"; }
ceph_() { inct timeout 30 ceph "$@"; }

deadline=$((SECONDS + CEPH_UP_TIMEOUT))
until inct test -f /run/ceph-bench.ready 2>/dev/null; do
  running || fail "container exited during start"
  [ "$SECONDS" -lt "$deadline" ] || fail "not ready after ${CEPH_UP_TIMEOUT}s"
  sleep 5
done

healthy() {
  local status health
  status="$(ceph_ status -f json)" || return 1
  health="$(ceph_ health detail -f json)" || return 1
  jq_ "$status" '.osdmap.num_up_osds == 3 and .osdmap.num_in_osds == 3
    and (.pgmap.pgs_by_state | length) == 1 and .pgmap.pgs_by_state[0].state_name == "active+clean"' &&
    jq_ "$health" --argjson ok "$BENIGN" '(.checks | keys) - $ok | length == 0'
}
jq_() {
  local doc=$1
  shift
  printf '%s' "$doc" | pm exec -i "$CT" jq -e "$@" >/dev/null
}

deadline=$((SECONDS + 300))
until healthy; do
  running || fail "container exited"
  [ "$SECONDS" -lt "$deadline" ] || fail "cluster not healthy and clean after 300s"
  sleep 5
done

# BlueStore sits on files: make sure it detected the disk under them as rotational on every OSD, which
# selects the HDD variants of its settings (deferred writes, cache, shard layout).
if [ -z "${BENCH_TMPFS_GB:-}" ]; then
  inct bash -c 'set -o pipefail; ceph osd metadata -f json | jq -e "length == 3 and all(.[];
    .osd_objectstore == \"bluestore\" and .bluestore_bdev_type == \"hdd\" and .rotational == \"1\")"' >/dev/null ||
    fail "an OSD is not BlueStore with HDD settings (ceph osd metadata)"
else
  inct bash -c 'set -o pipefail; ceph osd metadata -f json | jq -e "length == 3 and all(.[];
    .osd_objectstore == \"bluestore\")"' >/dev/null || fail "an OSD is not BlueStore (ceph osd metadata)"
  inct bash -c 'ceph osd metadata -f json |
    jq -r ".[] | \"osd.\(.id) on tmpfs: bdev_type=\(.bluestore_bdev_type) rotational=\(.rotational)\""'
fi

inct bash -c 'set -euo pipefail
  mountpoint -q /mnt/bench
  f=/mnt/bench/.up-check
  head -c 4096 /dev/urandom >/tmp/up-check
  timeout 60 dd if=/tmp/up-check of="$f" bs=4k count=1 oflag=direct,sync status=none
  timeout 60 dd if="$f" bs=4k iflag=direct status=none | cmp - /tmp/up-check
  rm -f "$f" /tmp/up-check' || fail "/mnt/bench is not writable"

ceph_ -s
ceph_ health detail
ceph_ versions
echo "$CT is up: CephFS at /mnt/bench"
