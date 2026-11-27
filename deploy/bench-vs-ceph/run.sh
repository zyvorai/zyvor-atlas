#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
#
# Runs the fio workloads on one system's mount (see common.sh for the contract). One system at a time:
# both share one disk, so this takes $BENCH_ROOT/bench.lock exclusively and refuses to start while the
# other system's container is up.
#
# Usage: run.sh <system> <tag>      system: ceph | atlas; tag names the result set
#   PROFILE=quick|full (default quick)   STEADY_SECS=0 (extra long 4k random-write run)
#   SIZE_MB RAMP RUNTIME REPEATS override the profile.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

SYSTEM="${1:?usage: run.sh <ceph|atlas> <tag>}"
TAG="${2:?usage: run.sh <ceph|atlas> <tag>}"
case "$SYSTEM" in ceph) OTHER=atlas ;; atlas) OTHER=ceph ;; *) echo "unknown system: $SYSTEM" >&2; exit 2 ;; esac
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "tag must be [A-Za-z0-9_-]+" >&2; exit 2; }

case "${PROFILE:-quick}" in
  quick) : "${SIZE_MB:=4096}" "${RAMP:=15}" "${RUNTIME:=60}" "${REPEATS:=1}" ;;
  full) : "${SIZE_MB:=8192}" "${RAMP:=30}" "${RUNTIME:=180}" "${REPEATS:=3}" ;;
  *) echo "PROFILE must be quick or full" >&2; exit 2 ;;
esac
STEADY_SECS="${STEADY_SECS:-0}"
FILES=32
FILE_MB=$((SIZE_MB / FILES))

CT="$(container_name "$SYSTEM")"
OUT="$BENCH_ROOT/results/$TAG/$SYSTEM"
DIR=/mnt/bench/fio

running() { bpodman ps --format '{{.Names}}' | grep -qx "$1"; }
running "$CT" || { echo "$CT is not running; start it with $SYSTEM-up.sh" >&2; exit 2; }
if running "$(container_name "$OTHER")"; then
  echo "$(container_name "$OTHER") is running; stop it first (one system at a time)" >&2
  exit 2
fi
[ -e "$OUT" ] && { echo "$OUT already exists; pick a new tag" >&2; exit 2; }

exec 9>"$BENCH_ROOT/bench.lock"
echo "waiting for the benchmark lock"
flock -x 9
mkdir -p "$OUT"

DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"
inct() { bpodman exec "$CT" "$@"; }
disk_sectors() { awk -v d="$DISK" '$3 == d {print $6, $10}' /proc/diskstats; } # read, written

{
  echo "date=$(date -Is)"
  echo "kernel=$(uname -r)"
  echo "disk=$DISK"
  echo "system=$SYSTEM size_mb=$SIZE_MB ramp=$RAMP runtime=$RUNTIME repeats=$REPEATS files=$FILES"
  echo "mem=$BENCH_MEM cpus=$BENCH_CPUS"
  echo "loadavg=$(cat /proc/loadavg)"
  echo "io_pressure=$(head -1 /proc/pressure/io)"
  echo "fio=$(inct fio --version)"
  echo "commit=$(cat "$HERE/COMMIT" 2>/dev/null || echo unknown)"
  env | grep -E '^ATLAS_(EXTENT_BYTES|DELTA)' || true
} >"$OUT/env.txt"

# Drop the page cache of this system's own server files (never the whole host's).
evict() {
  inct sh -c 'sync -f /srv/bench && find /srv/bench -type f -size +0 -exec sh -c '\''for f; do dd if="$f" iflag=nocache count=0 status=none || true; done'\'' sh {} +'
}

FAILED=0
# fio_run <name> <repeat> <measured-seconds-or-0> <fio args...>
fio_run() {
  local name=$1 rep=$2 secs=$3
  shift 3
  local timing=()
  [ "$secs" -gt 0 ] && timing=(--time_based "--runtime=$secs" "--ramp_time=$RAMP")
  local before after rc=0
  before="$(disk_sectors)"
  local t0
  t0=$(date +%s)
  inct fio --name="$name" --ioengine=psync --direct=1 --group_reporting --randrepeat=1 \
    --output-format=json "${timing[@]}" "$@" >"$OUT/$name.$rep.json" 2>"$OUT/$name.$rep.err" || rc=$?
  after="$(disk_sectors)"
  echo "$name $rep $(($(date +%s) - t0)) $before $after $(awk '{print $1}' /proc/loadavg)" >>"$OUT/disk.txt"
  if [ "$rc" -ne 0 ]; then
    echo "FAILED $name repeat $rep (fio exit $rc): $(tail -3 "$OUT/$name.$rep.err")" >&2
    FAILED=1
  else
    echo "done $name repeat $rep"
  fi
}

# One job rotating over all files (queue depth 1), or one job per file (queue depth FILES). Both use the
# same files, and no two jobs share an inode, so neither system is held back by a per-file lock.
ONE=(--directory="$DIR" --numjobs=1 --nrfiles="$FILES" "--filesize=${FILE_MB}m" "--size=$((FILE_MB * FILES))m"
  '--filename_format=data.$filenum' --openfiles="$FILES")
MANY=(--directory="$DIR" --numjobs="$FILES" --nrfiles=1 "--filesize=${FILE_MB}m" "--size=${FILE_MB}m"
  '--filename_format=data.$jobnum')

inct mkdir -p "$DIR"
echo "laying out $FILES files of ${FILE_MB} MiB"
fio_run layout 1 0 "${ONE[@]}" --rw=write --bs=1m --file_service_type=sequential --end_fsync=1
[ "$FAILED" -eq 0 ] || { echo "layout failed; nothing else would be meaningful" >&2; exit 1; }

# measure <name> <fio args...>: REPEATS runs, reads start with a cold server cache.
measure() {
  local name=$1 rep
  shift
  for rep in $(seq 1 "$REPEATS"); do
    case "$name" in *read*) evict ;; esac
    fio_run "$name" "$rep" "$RUNTIME" "$@"
  done
}

measure seqread-1m-fresh "${ONE[@]}" --rw=read --bs=1m --file_service_type=sequential
measure randread-4k-qd32-fresh "${MANY[@]}" --rw=randread --bs=4k
for bs in 4k 64k; do
  measure "randwrite-$bs-qd1" "${ONE[@]}" --rw=randwrite --bs="$bs" --sync=1 --file_service_type=random
  measure "randwrite-$bs-qd32" "${MANY[@]}" --rw=randwrite --bs="$bs" --sync=1
done
measure randrw70-4k-qd32 "${MANY[@]}" --rw=randrw --rwmixread=70 --bs=4k --sync=1
measure seqwrite-1m "${ONE[@]}" --rw=write --bs=1m --sync=1 --file_service_type=sequential
if [ "$STEADY_SECS" -gt 0 ]; then
  fio_run steady-randwrite-4k-qd32 1 "$STEADY_SECS" "${MANY[@]}" --rw=randwrite --bs=4k --sync=1 \
    --log_avg_msec=10000 --write_iops_log=/srv/bench/steady
  inct sh -c 'cat /srv/bench/steady_iops*.log' >"$OUT/steady_iops.log" || true
fi
measure randread-4k-qd32-after "${MANY[@]}" --rw=randread --bs=4k
measure seqread-1m-after "${ONE[@]}" --rw=read --bs=1m --file_service_type=sequential

for rep in $(seq 1 "$REPEATS"); do
  inct mkdir -p "$DIR/small.$rep"
  fio_run smallfile-create-4k "$rep" 0 --directory="$DIR/small.$rep" --numjobs=8 --nrfiles=250 --filesize=4k \
    --rw=write --bs=4k --create_on_open=1 --openfiles=1 --file_service_type=sequential --fsync_on_close=1
done

# Data integrity: random 4k writes, then read back and check every block.
fio_run verify 1 0 --directory="$DIR" --filename=verify.dat --size=128m --rw=randwrite --bs=4k --sync=1 \
  --numjobs=1 --verify=crc32c --verify_fatal=1 --do_verify=1

inct du -sk /srv/bench >"$OUT/space.txt" || true
{
  echo "end=$(date -Is)"
  echo "loadavg_end=$(cat /proc/loadavg)"
} >>"$OUT/env.txt"
echo "results in $OUT"
exit "$FAILED"
