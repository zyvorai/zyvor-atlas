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

BENCH_ROOT="${BENCH_ROOT:-/data/atlas-bench}"
BENCH_MEM="${BENCH_MEM:-12g}"
BENCH_CPUS="${BENCH_CPUS:-10}"

case "$BENCH_ROOT" in
  /data/atlas-bench | /data/atlas-bench/*) ;;
  *) echo "BENCH_ROOT must be under /data/atlas-bench, got: $BENCH_ROOT" >&2; exit 2 ;;
esac
case "$BENCH_ROOT" in *..*) echo "BENCH_ROOT must not contain '..'" >&2; exit 2 ;; esac

# The host's root disk is nearly full: images and layers go on the benchmark disk too.
BENCH_RUNROOT="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/atlas-bench"
mkdir -p "$BENCH_ROOT/podman" "$BENCH_ROOT/data" "$BENCH_ROOT/results" "$BENCH_ROOT/tmp" "$BENCH_RUNROOT"
export TMPDIR="$BENCH_ROOT/tmp"

# As an array too, for callers that put another command (timeout) in front of podman.
BPODMAN=(podman --root "$BENCH_ROOT/podman" --runroot "$BENCH_RUNROOT")
bpodman() { "${BPODMAN[@]}" "$@"; }

# Resource limits, and the FUSE access both systems need on top, identical for both.
BENCH_LIMIT_ARGS=(--memory "$BENCH_MEM" --memory-swap "$BENCH_MEM" --cpus "$BENCH_CPUS" --pids-limit -1)
BENCH_RUN_ARGS=("${BENCH_LIMIT_ARGS[@]}"
  --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined)

container_name() { echo "atlas-bench-$1"; }
