#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Shared settings for the atlas-native vs CephFS benchmark. Sourced by the other scripts; runs on the
# Linux benchmark host as an unprivileged user (rootless podman, no host packages).
#
# Contract:
#   <system>-up.sh    starts one detached container, atlas-bench-<system>, holding every daemon, the
#                     FUSE mount at /mnt/bench and fio. Exits 0 only once the mount is usable.
#                     Server data lives in $BENCH_ROOT/data/<system>, mounted at /srv/bench.
#   run.sh <system>   runs the workloads inside that container.
#   down.sh <system>  removes that container and its data.
#   compare.sh <tag>  runs several cases in turn (alternating rounds with ROUNDS); gate.py judges them.
#   disk-probe.sh     what the disk itself sustains for three synced copies (no system under test).
#   agent.sh          the agent-workload profiles (build, fork, small files); see its header.
#
# Overrides, all unset in the fio comparisons above:
#   BENCH_NAME_PREFIX=atlas-bench    container and image names are <prefix>-<system>
#   BENCH_PODMAN_ROOT=$BENCH_ROOT/podman
#   BENCH_TMPFS_GB=N                 server data on a tmpfs of N GiB inside the container's memory
#                                    limit instead of the benchmark disk (say so with the results)
#   BENCH_FIXTURES=<dir>             mounted read-only at /fixtures

BENCH_ROOT="${BENCH_ROOT:-/data/atlas-bench}"
BENCH_MEM="${BENCH_MEM:-12g}"
BENCH_CPUS="${BENCH_CPUS:-10}"

case "$BENCH_ROOT" in
  /data/atlas-bench | /data/atlas-bench/*) ;;
  *) echo "BENCH_ROOT must be under /data/atlas-bench, got: $BENCH_ROOT" >&2; exit 2 ;;
esac
case "$BENCH_ROOT" in *..*) echo "BENCH_ROOT must not contain '..'" >&2; exit 2 ;; esac

BENCH_NAME_PREFIX="${BENCH_NAME_PREFIX:-atlas-bench}"
[[ "$BENCH_NAME_PREFIX" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "BENCH_NAME_PREFIX must be [a-z0-9-]+" >&2; exit 2; }
BENCH_PODMAN_ROOT="${BENCH_PODMAN_ROOT:-$BENCH_ROOT/podman}"
case "$BENCH_PODMAN_ROOT" in
  *..*) echo "BENCH_PODMAN_ROOT must not contain '..'" >&2; exit 2 ;;
  /data/atlas-bench/*) ;;
  *) echo "BENCH_PODMAN_ROOT must be under /data/atlas-bench, got: $BENCH_PODMAN_ROOT" >&2; exit 2 ;;
esac
[[ -z "${BENCH_TMPFS_GB:-}" || "$BENCH_TMPFS_GB" =~ ^[1-9][0-9]?$ ]] ||
  { echo "BENCH_TMPFS_GB must be 1..99" >&2; exit 2; }

# The host's root disk is nearly full: images and layers go on the benchmark disk too.
BENCH_RUNROOT="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/atlas-bench"
mkdir -p "$BENCH_PODMAN_ROOT" "$BENCH_ROOT/data" "$BENCH_ROOT/results" "$BENCH_ROOT/tmp" "$BENCH_RUNROOT"
export TMPDIR="$BENCH_ROOT/tmp"

# As an array too, for callers that put another command (timeout) in front of podman.
BPODMAN=(podman --root "$BENCH_PODMAN_ROOT" --runroot "$BENCH_RUNROOT")
bpodman() { "${BPODMAN[@]}" "$@"; }

# Resource limits, and the FUSE access both systems need on top, identical for both.
BENCH_LIMIT_ARGS=(--memory "$BENCH_MEM" --memory-swap "$BENCH_MEM" --cpus "$BENCH_CPUS" --pids-limit -1)
BENCH_RUN_ARGS=("${BENCH_LIMIT_ARGS[@]}"
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined)

container_name() { echo "$BENCH_NAME_PREFIX-$1"; }

# bench_mounts <system>: sets BENCH_MOUNT_ARGS, the server data at /srv/bench and any fixtures.
bench_mounts() {
  if [ -n "${BENCH_TMPFS_GB:-}" ]; then
    BENCH_MOUNT_ARGS=(--tmpfs "/srv/bench:rw,size=${BENCH_TMPFS_GB}g,mode=0755")
  else
    mkdir -p "$BENCH_ROOT/data/$1"
    BENCH_MOUNT_ARGS=(-v "$BENCH_ROOT/data/$1:/srv/bench")
  fi
  [ -z "${BENCH_FIXTURES:-}" ] || BENCH_MOUNT_ARGS+=(-v "$BENCH_FIXTURES:/fixtures:ro")
}
