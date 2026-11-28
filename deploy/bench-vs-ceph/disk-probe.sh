#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# What the benchmark disk sustains when every logical write is 4 KiB written to THREE files, each
# write followed by fdatasync: the data part of three replicas on one disk. No system under test, no
# metadata log: an upper bound for the data writes of atlas-native as it stores them today
# (`FileDevice`: buffered pwrite, then fdatasync, one file per data node).
#
# fio cannot make one logical write wait for three parallel file writes, so two models are run, and
# neither is that:
#   fanout  three independent groups, N writers each, one file per group. As many syncs in flight per
#           file as N parallel logical writers would have, but nothing couples the files. The slowest
#           file's rate is reported as the logical rate; take it as optimistic.
#   chain   N writers, each doing write+sync on file a, then b, then c. Coupled, but one after
#           another (atlas-native writes replicas in parallel), so only N syncs are in flight in all.
#           Logical rate = file writes / 3.
# Neither bounds the other: syncs coalesce differently in each, and either one can come out ahead.
# Layouts: append (files start empty and grow; writers take interleaved 4 KiB slots at the end),
# prealloc-seq and prealloc-rand (files written in full beforehand, then overwritten in place).
#
# chain depends on --file_service_type=roundrobin:2. With the default (switch file at every I/O) fio
# aims each fdatasync at a different file from the write before it. Before measuring, every model is
# therefore traced with strace: each thread must alternate pwrite64 and fdatasync on one descriptor.
#
# Usage: disk-probe.sh <tag>          results in $BENCH_ROOT/results/<tag>/disk-probe
#   PROBE_WRITERS="1 8 32 128"  PROBE_ROUNDS=2  PROBE_RUNTIME=25  PROBE_RAMP=5  PROBE_FILE_GB=2
#   PROBE_VERIFY=1      0 skips the strace check (it needs an image with strace, built on first use)
# Takes $BENCH_ROOT/bench.lock exclusively.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

TAG="${1:?usage: disk-probe.sh <tag>}"
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }
WRITERS="${PROBE_WRITERS:-1 8 32 128}"
ROUNDS="${PROBE_ROUNDS:-2}"
RUNTIME="${PROBE_RUNTIME:-25}"
RAMP="${PROBE_RAMP:-5}"
FILE_GB="${PROBE_FILE_GB:-2}"
VERIFY="${PROBE_VERIFY:-1}"
for v in $WRITERS "$ROUNDS" "$RUNTIME" "$FILE_GB"; do
  [[ "$v" =~ ^[1-9][0-9]*$ ]] || { echo "PROBE_* values must be positive integers, got: $v" >&2; exit 2; }
done
[[ "$RAMP" =~ ^[0-9]+$ ]] || { echo "PROBE_RAMP must be an integer" >&2; exit 2; }
[ "$FILE_GB" -le 20 ] || { echo "PROBE_FILE_GB must be at most 20" >&2; exit 2; }
FILE_BYTES=$((FILE_GB * 1024 * 1024 * 1024))

OUT="$BENCH_ROOT/results/$TAG/disk-probe"
DATA="$BENCH_ROOT/data/probe"
CT=atlas-bench-probe
IMAGE=localhost/atlas-bench-atlas:latest
TRACE_IMAGE=localhost/atlas-bench-probe-strace:latest
[ -e "$OUT" ] && { echo "$OUT already exists; pick a new tag" >&2; exit 2; }
bpodman image exists "$IMAGE" || { echo "$IMAGE is missing; build it with atlas-up.sh" >&2; exit 2; }

# One benchmark session at a time (compare.sh holds the same lock), then the disk to ourselves.
exec 8>"$BENCH_ROOT/session.lock"
echo "waiting for the session lock"
flock -x 8
WANTED=0
STARTED=0
cleanup() {
  if [ "$STARTED" = 1 ]; then
    bpodman rm -f --ignore -t 0 "$CT" >/dev/null 8>&- 9>&- || true
    bpodman unshare rm -rf -- "$DATA" 8>&- 9>&- || true
  fi
  [ "$WANTED" = 0 ] || rm -f "$BENCH_ROOT/bench.wanted"
}
trap cleanup EXIT
# Asks builds on this host to hold off.
[ -e "$BENCH_ROOT/bench.wanted" ] || { touch "$BENCH_ROOT/bench.wanted"; WANTED=1; }

if [ "$VERIFY" = 1 ] && ! bpodman image exists "$TRACE_IMAGE"; then
  ctx="$BENCH_ROOT/tmp/probe-strace"
  mkdir -p "$ctx"
  printf '%s\n' "FROM $IMAGE" \
    'RUN apt-get update && apt-get install -y --no-install-recommends strace && rm -rf /var/lib/apt/lists/*' \
    >"$ctx/Containerfile"
  (
    flock -s 9
    bpodman build -q --http-proxy=false -t "$TRACE_IMAGE" -f "$ctx/Containerfile" "$ctx" 8>&- 9>&-
  ) 9>"$BENCH_ROOT/bench.lock"
fi
[ "$VERIFY" = 1 ] && RUN_IMAGE="$TRACE_IMAGE" || RUN_IMAGE="$IMAGE"

exec 9>"$BENCH_ROOT/bench.lock"
echo "waiting for the benchmark lock"
flock -x 9
mkdir -p "$OUT" "$DATA"

DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"
STARTED=1
bpodman rm -f --ignore -t 0 "$CT" >/dev/null 8>&- 9>&-
bpodman run -d --name "$CT" "${BENCH_LIMIT_ARGS[@]}" --network=none --http-proxy=false \
  -v "$DATA:/srv/bench" --entrypoint sleep "$RUN_IMAGE" infinity >/dev/null 8>&- 9>&-
inct() { bpodman exec "$CT" "$@" 8>&- 9>&-; }
# writes completed, sectors written, flushes completed
disk_stats() { awk -v d="$DISK" '$3 == d {print $8, $10, $19 + 0}' /proc/diskstats; }

{
  echo "date=$(date -Is)"
  echo "kernel=$(uname -r)"
  echo "disk=$DISK model=$(cat "/sys/block/$DISK/device/model" 2>/dev/null | tr -s ' ' || echo unknown)"
  echo "rotational=$(cat "/sys/block/$DISK/queue/rotational") scheduler=$(cat "/sys/block/$DISK/queue/scheduler")"
  echo "write_cache=$(cat "/sys/block/$DISK/queue/write_cache") queue_depth=$(cat "/sys/block/$DISK/device/queue_depth" 2>/dev/null || echo unknown)"
  echo "mount=$(findmnt -no FSTYPE,OPTIONS --target "$BENCH_ROOT")"
  echo "mem=$BENCH_MEM cpus=$BENCH_CPUS"
  echo "writers=$WRITERS rounds=$ROUNDS runtime=$RUNTIME ramp=$RAMP file_gb=$FILE_GB"
  echo "fio=$(inct fio --version)"
  echo "commit=$(cat "$HERE/COMMIT" 2>/dev/null || echo unknown)"
  echo "loadavg=$(cat /proc/loadavg)"
  echo "io_pressure=$(head -1 /proc/pressure/io)"
} >"$OUT/env.txt"

# What the rest of the host writes to this disk while we do nothing.
sleep 5
idle0="$(disk_stats)"
sleep 20
echo "idle_20s_before: $idle0 -> $(disk_stats) (writes sectors flushes)" >>"$OUT/env.txt"

COMMON=(--thread --ioengine=psync --direct=0 --fdatasync=1 --bs=4k --fallocate=none --group_reporting)

# The three files of a layout. The preallocated ones are written once and then kept.
dir_of() { case "$1" in append) echo /srv/bench/append ;; *) echo /srv/bench/prealloc ;; esac; }

# fio_args <model> <layout> <writers>: sets ARGS.
fio_args() {
  local model=$1 layout=$2 n=$3 d
  d="$(dir_of "$layout")"
  # Interleaved slots: writer k starts at block k and skips the other writers' blocks.
  local skip=$(((n - 1) * 4096)) usable=$((FILE_BYTES - n * 4096))
  local pattern=()
  case "$layout" in
    append) pattern=("--rw=write:$skip" --offset_increment=4096) ;;
    prealloc-seq) pattern=("--rw=write:$skip" --offset_increment=4096 --allow_file_create=0) ;;
    prealloc-rand) pattern=(--rw=randwrite --allow_file_create=0) ;;
  esac
  # fio rejects a zero skip.
  [ "$skip" -gt 0 ] || pattern[0]="${pattern[0]%:0}"
  [ "$layout" != prealloc-rand ] || usable=$FILE_BYTES
  ARGS=("${COMMON[@]}" "${pattern[@]}" "--numjobs=$n")
  case "$model" in
    fanout)
      ARGS+=("--filesize=$FILE_BYTES" "--size=$usable"
        --name=a "--filename=$d/a"
        --name=b --new_group "--filename=$d/b"
        --name=c --new_group "--filename=$d/c")
      ;;
    chain)
      ARGS+=("--filesize=$FILE_BYTES" "--size=$((usable * 3))" --nrfiles=3 --openfiles=3
        --file_service_type=roundrobin:2
        --name=chain "--filename=$d/a:$d/b:$d/c")
      ;;
  esac
}

# Fresh empty files for every append run; files with every block written for the others.
prepare() {
  local d
  d="$(dir_of "$1")"
  if [ "$1" = append ]; then
    inct sh -c "mkdir -p $d && cd $d && rm -f a b c && : >a && : >b && : >c"
  elif ! inct test -e "$d/filled"; then
    echo "writing three files of ${FILE_GB} GiB in full"
    inct sh -c "mkdir -p $d && cd $d && for f in a b c; do
      fio --name=fill --filename=$d/\$f --rw=write --bs=1m --size=$FILE_BYTES --direct=1 --end_fsync=1 \
        --ioengine=psync >/dev/null || exit 1; done && : >filled"
  fi
  inct sync -f /srv/bench
  sleep 3
}
file_stats() { inct sh -c "cd $(dir_of "$1") && stat -c '$2' a b c"; }

MODELS=(fanout chain)
LAYOUTS=(append prealloc-seq prealloc-rand)

if [ "$VERIFY" = 1 ]; then
  mkdir -p "$OUT/verify"
  for layout in "${LAYOUTS[@]}"; do
    for model in "${MODELS[@]}"; do
      prepare "$layout"
      fio_args "$model" "$layout" 4
      inct strace -f -qq -s 0 -e trace=pwrite64,fdatasync,fsync,ftruncate,fallocate \
        -o /srv/bench/trace.txt fio --time_based --runtime=3 --output-format=json "${ARGS[@]}" \
        >"$OUT/verify/$model.$layout.json"
      inct cat /srv/bench/trace.txt >"$OUT/verify/$model.$layout.trace"
      file_stats "$layout" '%n size=%s blocks=%b*%B' >"$OUT/verify/$model.$layout.files"
    done
  done
  python3 -I - "$OUT/verify" <<'PY' | tee "$OUT/verify.txt"
import re, sys
from collections import defaultdict
from pathlib import Path

call = re.compile(r"^(\d+)\s+(pwrite64|fdatasync|fsync|ftruncate|fallocate)\((\d+)")
bad_total = 0
traces = sorted(Path(sys.argv[1]).glob("*.trace"))
for path in traces:
    threads = defaultdict(list)
    for line in path.read_text(errors="replace").splitlines():
        m = call.match(line)
        if m:
            threads[m[1]].append((m[2], m[3]))
    writes = paired = bad = other = 0
    fds = set()
    for calls in threads.values():
        pending = None  # descriptor of a write not yet synced
        for name, fd in calls:
            if name == "pwrite64":
                bad += pending is not None
                pending = fd
                writes += 1
                fds.add(fd)
            elif name == "fdatasync":
                if pending == fd:
                    paired += 1
                else:
                    bad += 1
                pending = None
            else:
                other += 1
    # A thread may stop between its last write and that write's sync.
    ok = bad == 0 and writes > 0 and writes - paired <= len(threads)
    bad_total += not ok
    print(f"{path.stem}: threads={len(threads)} writes={writes} synced_same_fd={paired} "
          f"mispaired={bad} fsync/ftruncate/fallocate={other} descriptors={len(fds)} {'OK' if ok else 'BAD'}")
if bad_total or len(traces) != 6:
    sys.exit("fio did not sync every write to the file it wrote: results would not mean what they claim")
PY
  rm -f "$OUT"/verify/*.trace
fi

# Rounds go through every case once each, so a slow spell on the shared host does not fall on one case.
for round in $(seq 1 "$ROUNDS"); do
  for layout in "${LAYOUTS[@]}"; do
    for n in $WRITERS; do
      for model in "${MODELS[@]}"; do
        name="$model.$layout.n$n.r$round"
        prepare "$layout"
        fio_args "$model" "$layout" "$n"
        load="$(awk '{print $1}' /proc/loadavg)"
        before="$(disk_stats)"
        t0=$(date +%s.%N)
        rc=0
        inct fio --time_based "--runtime=$RUNTIME" "--ramp_time=$RAMP" --output-format=json "${ARGS[@]}" \
          >"$OUT/$name.json" 2>"$OUT/$name.err" || rc=$?
        t1=$(date +%s.%N)
        after="$(disk_stats)"
        sizes="$(file_stats "$layout" %s | tr '\n' ' ')"
        echo "$name rc=$rc t0=$t0 t1=$t1 load1=$load before=$before after=$after sizes=$sizes" >>"$OUT/runs.txt"
        [ "$rc" = 0 ] || echo "$rc" >"$OUT/$name.failed"
        echo "$(date +%T) done $name rc=$rc"
      done
    done
  done
done

{
  echo "loadavg_end=$(cat /proc/loadavg)"
  echo "io_pressure_end=$(head -1 /proc/pressure/io)"
} >>"$OUT/env.txt"
sleep 15
idle0="$(disk_stats)"
sleep 20
echo "idle_20s_after: $idle0 -> $(disk_stats) (writes sectors flushes)" >>"$OUT/env.txt"
echo "end=$(date -Is)" >>"$OUT/env.txt"

python3 -I - "$OUT" <<'PY' | tee "$OUT/summary.txt"
import json, re, statistics, sys
from collections import defaultdict
from pathlib import Path

out = Path(sys.argv[1])
runs = {}
for line in (out / "runs.txt").read_text().splitlines():
    name = line.split()[0]
    f = re.search(r"t0=(\S+) t1=(\S+) load1=(\S+) before=(\d+) (\d+) (\d+) after=(\d+) (\d+) (\d+) sizes=(.*)", line)
    t0, t1, load = float(f[1]), float(f[2]), float(f[3])
    runs[name] = {"load": load, "dev_iops": (int(f[7]) - int(f[4])) / (t1 - t0),
                  "dev_mib": (int(f[8]) - int(f[5])) * 512 / 2**20 / (t1 - t0),
                  "sizes": [int(s) for s in f[10].split()]}
file_bytes = int(re.search(r"file_gb=(\d+)", (out / "env.txt").read_text())[1]) << 30

cases = defaultdict(list)
failed = 0
for name, host in runs.items():
    model, layout, n, _ = name.split(".")
    try:
        if (out / f"{name}.failed").exists():
            raise ValueError("fio failed")
        text = (out / f"{name}.json").read_text()
        jobs = json.loads(text[text.index("{"):])["jobs"]
        if len(jobs) != (3 if model == "fanout" else 1) or any(j["error"] for j in jobs):
            raise ValueError("unexpected jobs or errors")
        rates, p50, p99 = [], [], []
        writes = syncs = 0
        for j in jobs:
            w, s = j["write"], j["sync"]
            if not w["total_ios"]:
                raise ValueError("a group wrote nothing")
            writes += w["total_ios"]
            syncs += s["lat_ns"]["N"]  # fio leaves sync.total_ios at 0
            rates.append(w["iops"])
            p50.append(s["lat_ns"]["percentile"]["50.000000"] / 1e6)
            p99.append(s["lat_ns"]["percentile"]["99.000000"] / 1e6)
        # Boundaries of the ramp and of the run can split a write from its sync, once per thread.
        threads = int(n[1:]) * len(jobs)
        if abs(writes - syncs) > 2 * threads:
            raise ValueError(f"{writes} writes but {syncs} syncs")
        # An appending file must have grown with the writes, not been sized to the full range at once;
        # a preallocated one must have kept its size.
        sizes = host["sizes"]
        if layout == "append" and not all(0 < s < file_bytes for s in sizes):
            raise ValueError(f"appending files have unexpected sizes {sizes}")
        if layout != "append" and sizes != [file_bytes] * 3:
            raise ValueError(f"preallocated files changed size: {sizes}")
        logical = min(rates) if model == "fanout" else rates[0] / 3
        cases[(layout, int(n[1:]), model)].append(
            dict(logical=logical, rates=rates, p50=max(p50), p99=max(p99), writes=writes, syncs=syncs, **host))
    except (OSError, ValueError, KeyError, IndexError) as e:
        failed += 1
        print(f"FAILED {name}: {e}")

print("logical/s: fanout = slowest of the three files (uncoupled); chain = file writes / 3 (one after another).")
print("sync ms: fdatasync latency, worst file. dev: whole-disk writes, everything on the host included.")
print("load1: before each run; it counts the blocked threads of the probe's earlier runs.")
print(f"{'layout':<14}{'writers':>8}  {'model':<7}{'logical/s':>10}{'range':>13}{'file writes/s':>20}"
      f"{'sync p50':>10}{'p99':>8}{'dev w/s':>9}{'dev MiB/s':>10}{'load1':>7}{'runs':>5}")
for key in sorted(cases):
    r = cases[key]
    med = lambda k: statistics.median(x[k] for x in r)
    lo, hi = min(x["logical"] for x in r), max(x["logical"] for x in r)
    if key[2] == "fanout":
        files = "/".join(f"{statistics.median(x['rates'][i] for x in r):.0f}" for i in range(3))
    else:
        files = f"{statistics.median(x['rates'][0] for x in r):.0f} total"
    print(f"{key[0]:<14}{key[1]:>8}  {key[2]:<7}{med('logical'):>10.1f}{f'{lo:.0f}-{hi:.0f}':>13}{files:>20}"
          f"{med('p50'):>10.1f}{med('p99'):>8.1f}{med('dev_iops'):>9.0f}{med('dev_mib'):>10.2f}"
          f"{med('load'):>7.1f}{len(r):>5}")
writes = sum(x["writes"] for r in cases.values() for x in r)
syncs = sum(x["syncs"] for r in cases.values() for x in r)
print(f"measured file writes={writes} fdatasyncs={syncs} (fio's own counts, ramps excluded)")
sys.exit(1 if failed else 0)
PY
echo "results in $OUT"
