#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# What preallocating a data node's files changes, in two stages:
#
#   syncs  The mechanism. prealloc-syncs.py writes 4 KiB blocks, each followed by fdatasync, into
#          files prepared in different ways (not at all, sized, fallocate, zeros written and
#          synced, zeros written only). bpftrace counts the XFS log forces each causes. A layout
#          that still forces the log on most syncs has not saved what preallocation is for. Counts,
#          not speed.
#   rate   The implementation. tests/prealloc_probe.rs drives three FileDevices as three replicas:
#          each logical write is appended to all three at once and waits for the slowest.
#          Preallocation off and on alternate. Reports logical writes a second, latency, the share
#          of appends that found prepared space and, from bpftrace, log forces per file append by
#          the writers and by the preparer. No system under test and no metadata log: like
#          disk-probe.sh, an upper bound for the data writes, not what a cluster will do.
#
# Usage: prealloc-probe.sh <tag>       results in $BENCH_ROOT/results/<tag>/prealloc-probe
#   PROBE_STAGES="syncs rate"  PROBE_SIZES="4096 131072"  PROBE_WRITERS="1 8 32 128"
#   PROBE_ROUNDS=2  PROBE_RUNTIME=25  PROBE_RAMP=5
#   PROBE_AHEAD_BYTES / PROBE_STEP_BYTES   other than the node's defaults (64 MiB, 1 MiB)
#   PROBE_TRACE=1   0: no bpftrace (no sudo needed); the syncs stage is then refused.
# Takes $BENCH_ROOT/session.lock and bench.lock exclusively. Files only under $BENCH_ROOT.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

TAG="${1:?usage: prealloc-probe.sh <tag>}"
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }
STAGES="${PROBE_STAGES:-syncs rate}"
SIZES="${PROBE_SIZES:-4096 131072}"
WRITERS="${PROBE_WRITERS:-1 8 32 128}"
ROUNDS="${PROBE_ROUNDS:-2}"
RUNTIME="${PROBE_RUNTIME:-25}"
RAMP="${PROBE_RAMP:-5}"
TRACE="${PROBE_TRACE:-1}"
for v in $SIZES $WRITERS "$ROUNDS" "$RUNTIME"; do
  [[ "$v" =~ ^[1-9][0-9]*$ ]] || { echo "PROBE_* values must be positive integers, got: $v" >&2; exit 2; }
done
[[ "$RAMP" =~ ^[0-9]+$ ]] || { echo "PROBE_RAMP must be an integer" >&2; exit 2; }
for s in $STAGES; do
  case "$s" in syncs | rate) ;; *) echo "unknown stage: $s" >&2; exit 2 ;; esac
done
if [ "$TRACE" = 1 ]; then
  sudo -n bpftrace --version >/dev/null </dev/null ||
    { echo "bpftrace through sudo -n is needed (or PROBE_TRACE=0)" >&2; exit 2; }
elif [[ " $STAGES " == *" syncs "* ]]; then
  echo "the syncs stage needs the tracer; set PROBE_STAGES=rate with PROBE_TRACE=0" >&2
  exit 2
fi

SRC="$(cd "$HERE/../.." && pwd)"
CACHE="$BENCH_ROOT/cache"
OUT="$BENCH_ROOT/results/$TAG/prealloc-probe"
DATA="$BENCH_ROOT/data/prealloc-probe"
CT=atlas-bench-prealloc-probe
[ ! -e "$OUT" ] || { echo "$OUT already exists; pick a new tag" >&2; exit 2; }
[ ! -e "$DATA" ] || { echo "$DATA is left over from an earlier run; remove it first" >&2; exit 2; }

exec 8>"$BENCH_ROOT/session.lock"
echo "waiting for the session lock"
flock -x 8
WANTED=0
TRACE_PID=""
trace_done() { # a process of this name exiting ends the tracer
  python3 -I -c 'import ctypes; ctypes.CDLL(None).prctl(15, b"fx-done", 0, 0, 0)'
  wait "$TRACE_PID" || true
  TRACE_PID=""
}
cleanup() {
  [ -z "$TRACE_PID" ] || trace_done
  bpodman rm -f --ignore -t 0 "$CT" >/dev/null 8>&- 9>&- || true
  [ ! -e "$DATA" ] || bpodman unshare rm -rf -- "$DATA" 8>&- 9>&- || true
  [ "$WANTED" = 0 ] || rm -f "$BENCH_ROOT/bench.wanted"
}
trap cleanup EXIT
[ -e "$BENCH_ROOT/bench.wanted" ] || { touch "$BENCH_ROOT/bench.wanted"; WANTED=1; }
mkdir -p "$OUT/bin" "$DATA"

RUST_IMAGE="$(awk '$1 == "FROM" && / AS builder$/ {print $2}' "$SRC/Dockerfile.native")"
CHANNEL="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$SRC/rust-toolchain.toml")"
[ -n "$RUST_IMAGE" ] && [ -n "$CHANNEL" ] || { echo "cannot read the builder image or toolchain" >&2; exit 1; }
if [[ " $STAGES " == *" rate "* ]]; then
  mkdir -p "$CACHE/cargo-registry" "$CACHE/cargo-git" "$CACHE/rustup" "$CACHE/target"
  (
    flock -s 9
    find "$SRC/crates" "$SRC/Cargo.toml" "$SRC/Cargo.lock" -type f -exec touch {} +
    bpodman run --rm --replace --name atlas-bench-prealloc-build --http-proxy=false \
      --cpus "$BENCH_CPUS" --memory "$BENCH_MEM" --pids-limit -1 \
      -v "$SRC:/build:ro" -w /build -v "$OUT/bin:/out" \
      -v "$CACHE/cargo-registry:/usr/local/cargo/registry" -v "$CACHE/cargo-git:/usr/local/cargo/git" \
      -v "$CACHE/rustup:/cache/rustup" -v "$CACHE/target:/cache/target" \
      -e RUSTUP_HOME=/cache/rustup -e CARGO_TARGET_DIR=/cache/target -e "CHANNEL=$CHANNEL" \
      "$RUST_IMAGE" bash -euo pipefail -c '
        rustup toolchain install "$CHANNEL" --profile minimal --no-self-update
        cargo test --release --locked -p atlas-native --test prealloc_probe --no-run --message-format=json \
          >/tmp/build.json
        bin=$(grep -o "\"executable\":\"[^\"]*/prealloc_probe-[^\"]*\"" /tmp/build.json | tail -1 | cut -d\" -f4)
        cp "$bin" /out/prealloc_probe' 8>&- 9>&-
  ) 9>"$BENCH_ROOT/bench.lock"
fi

exec 9>"$BENCH_ROOT/bench.lock"
echo "waiting for the benchmark lock"
flock -x 9

DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"
{
  echo "date=$(date -Is)"
  echo "kernel=$(uname -r)"
  echo "disk=$DISK write_cache=$(cat "/sys/block/$DISK/queue/write_cache" 2>/dev/null || echo unknown)"
  echo "mount=$(findmnt -no FSTYPE,OPTIONS --target "$BENCH_ROOT")"
  echo "mem=$BENCH_MEM cpus=$BENCH_CPUS"
  echo "stages=$STAGES sizes=$SIZES writers=$WRITERS rounds=$ROUNDS runtime=$RUNTIME ramp=$RAMP trace=$TRACE"
  echo "ahead_bytes=${PROBE_AHEAD_BYTES:-default} step_bytes=${PROBE_STEP_BYTES:-default}"
  echo "commit=$(cat "$HERE/COMMIT" 2>/dev/null || git -C "$SRC" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "loadavg=$(cat /proc/loadavg)"
  echo "io_pressure=$(head -1 /proc/pressure/io)"
} >"$OUT/env.txt"

# xfs_log_force_seq is what an fsync calls, in the calling thread, when the inode has metadata
# the log does not hold yet. Names: fx-* the syncs stage, pw-* the probe's writers, atlas-prealloc
# the preparer.
PROGRAM='
kprobe:xfs_log_force_seq /strncmp(comm, "fx-", 3) == 0 || strncmp(comm, "pw-", 3) == 0 ||
  comm == "atlas-prealloc"/ { @force[comm] = count(); }
tracepoint:sched:sched_process_exit /comm == "fx-done"/ { exit(); }
interval:s:7200 { exit(); }
BEGIN { printf("ready\n"); }
'
trace_start() { # <output file>
  [ "$TRACE" = 1 ] || return 0
  sudo -n bpftrace -B none -e "$PROGRAM" </dev/null >"$1" 2>&1 8>&- 9>&- &
  TRACE_PID=$!
  for _ in $(seq 1 600); do
    ! grep -q '^ready' "$1" || return 0
    kill -0 "$TRACE_PID" 2>/dev/null || break
    sleep 0.1
  done
  echo "bpftrace did not start:" >&2
  cat "$1" >&2
  exit 1
}
trace_stop() { [ -z "$TRACE_PID" ] || trace_done; }

if [[ " $STAGES " == *" syncs "* ]]; then
  trace_start "$OUT/syncs.trace"
  python3 -I "$HERE/prealloc-syncs.py" "$DATA/syncs" >"$OUT/syncs.txt"
  trace_stop
fi

if [[ " $STAGES " == *" rate "* ]]; then
  settings=()
  for v in PROBE_AHEAD_BYTES PROBE_STEP_BYTES; do
    [ -z "${!v:-}" ] || settings+=(-e "$v=${!v}")
  done
  for size in $SIZES; do
    sync -f "$DATA"
    trace_start "$OUT/rate-$size.trace"
    bpodman run --rm --replace --name "$CT" "${BENCH_LIMIT_ARGS[@]}" --network=none --http-proxy=false \
      -v "$DATA:/srv/bench" -v "$OUT/bin:/probe:ro" \
      -e PROBE_DIR=/srv/bench/rate -e "PROBE_BYTES=$size" -e "PROBE_WRITERS=$WRITERS" \
      -e "PROBE_ROUNDS=$ROUNDS" -e "PROBE_RUNTIME=$RUNTIME" -e "PROBE_RAMP=$RAMP" "${settings[@]}" \
      "$RUST_IMAGE" /probe/prealloc_probe --ignored --nocapture prealloc_rate_probe 8>&- 9>&- |
      tee "$OUT/rate-$size.txt"
    trace_stop
  done
fi

{
  echo "loadavg_end=$(cat /proc/loadavg)"
  echo "io_pressure_end=$(head -1 /proc/pressure/io)"
  echo "end=$(date -Is)"
} >>"$OUT/env.txt"

python3 -I - "$OUT" "$TRACE" <<'PY' | tee "$OUT/summary.txt"
import re, statistics, sys
from collections import defaultdict
from pathlib import Path

out, traced = Path(sys.argv[1]), sys.argv[2] == "1"

def forces(path):
    return {m[1]: int(m[2]) for m in re.finditer(r"^@force\[([^\]]+)\]: (\d+)", path.read_text(), re.M)}

if (out / "syncs.txt").exists():
    seen = forces(out / "syncs.trace")
    print("syncs: 4 KiB write + fdatasync into a file prepared each way")
    print(f"{'layout':<14}{'syncs':>7}{'log forces':>12}")
    for m in re.finditer(r"^SYNCS (\S+) (\d+)", (out / "syncs.txt").read_text(), re.M):
        print(f"{m[1]:<14}{m[2]:>7}{seen.get('fx-' + m[1], 0):>12}")
    if not any(k.startswith("fx-") for k in seen):
        sys.exit("no log force was seen at all, not even for appends: the tracer did not measure")

for path in sorted(out.glob("rate-*.txt"), key=lambda p: int(p.stem.split("-")[1])):
    cases, appends = defaultdict(list), defaultdict(int)
    for line in path.read_text().splitlines():
        if not line.startswith("PROBE prealloc="):
            continue
        f = dict(kv.split("=", 1) for kv in line.split()[1:])
        cases[(int(f["writers"]), f["prealloc"])].append(f)
        appends[f["prealloc"]] += int(f["appends"])
    if not cases:
        sys.exit(f"{path.name}: no results")
    print(f"\nrate, {path.stem.split('-')[1]} bytes: logical writes/s (three files each), median of rounds")
    print(f"{'writers':>8}{'prealloc':>10}{'logical/s':>11}{'range':>13}{'p50 ms':>9}{'p99 ms':>9}"
          f"{'prepared':>10}{'waits':>7}{'failures':>9}{'runs':>6}")
    for key in sorted(cases):
        r = cases[key]
        med = lambda k: statistics.median(float(x[k]) for x in r)
        total = lambda k: sum(int(x[k]) for x in r)
        lo, hi = min(float(x["logical_per_s"]) for x in r), max(float(x["logical_per_s"]) for x in r)
        known = total("prepared") + total("unprepared")
        share = f"{100 * total('prepared') / known:.1f}%" if known else "-"
        print(f"{key[0]:>8}{key[1]:>10}{med('logical_per_s'):>11.1f}{f'{lo:.0f}-{hi:.0f}':>13}"
              f"{med('p50_ms'):>9.1f}{med('p99_ms'):>9.1f}{share:>10}{total('waits'):>7}"
              f"{total('failures'):>9}{len(r):>6}")
    if traced:
        seen = forces(path.with_suffix(".trace"))
        for setting in ("off", "on"):
            if appends[setting]:
                n = seen.get(f"pw-{setting}", 0)
                print(f"log forces by writers, prealloc {setting}: {n} for {appends[setting]} file appends "
                      f"({n / appends[setting]:.3f} each)")
        if appends["on"]:
            n = seen.get("atlas-prealloc", 0)
            print(f"log forces by the preparer: {n} ({n / appends['on']:.3f} per file append with it on)")
PY
echo "results in $OUT"
