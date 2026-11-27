#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Short check of a running atlas-bench-atlas (atlas-up.sh), not a benchmark: how one 1 MiB direct write
# reaches the data nodes, a 256 MiB 4 KiB O_DIRECT|O_SYNC random write for SECS seconds at queue depth
# 1 and 32, and the data-node, page-cache and disk counters around each. Pass/fail is fio's exit status.
#
# Usage: atlas-smoke.sh   (SECS=30)
# Takes $BENCH_ROOT/bench.lock exclusively, as run.sh does, waiting up to LOCK_WAIT seconds (900);
# LOCK_WAIT=0 runs without it and the result says so.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

SECS="${SECS:-30}"
CT="$(container_name atlas)"
DIR=/mnt/bench/smoke
DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"
inct() { bpodman exec "$CT" "$@" 9>&-; }

# Only the metadata leader answers, so ask all three.
extent="$(inct sh -c 'for p in 7401 7402 7403; do
  curl -sf -H "Authorization: Bearer $(cat /run/atlas/token)" http://127.0.0.1:$p/v1/fs |
    jq -r ".filesystems[] | select(.id == \"bench\") | .extent_bytes"; done' | head -n 1)"
[ -n "$extent" ] || { echo "cannot read the filesystem's extent size from $CT" >&2; exit 1; }
OUT="$BENCH_ROOT/results/atlas-smoke/$(date +%Y%m%d-%H%M%S)-extent-$extent"
mkdir -p "$OUT"

# appends write_ats data_node_bytes disk_writes disk_sectors_written disk_flushes
counters() {
  local dn
  dn="$(inct sh -c 'for p in 7201 7202 7203; do curl -sf http://127.0.0.1:$p/metrics; done' | awk '
    /^atlas_native_data_requests_total.*op="append"/ {a += $NF}
    /^atlas_native_data_requests_total.*op="write_at"/ {w += $NF}
    /^atlas_native_data_written_bytes_total/ {b += $NF}
    END {printf "%.0f %.0f %.0f", a, w, b}')"
  echo "$dn $(awk -v d="$DISK" '$3 == d {print $8, $10, $19 + 0}' /proc/diskstats)"
}

delta() { # <label> <before> <after>
  local a b i
  read -r -a a <<<"$2"
  read -r -a b <<<"$3"
  for i in 0 1 2 3 4 5; do a[i]=$((b[i] - a[i])); done
  echo "$1: data-node appends=${a[0]} write_ats=${a[1]} bytes=${a[2]};" \
    "device $DISK writes=${a[3]} MiB_written=$((a[4] / 2048)) flushes=${a[5]}" | tee -a "$OUT/summary.txt"
}

echo "extent_bytes=$extent disk=$DISK write_cache=$(cat "/sys/block/$DISK/queue/write_cache" 2>/dev/null || echo ?)" |
  tee "$OUT/summary.txt"
inct cat /srv/bench/settings.txt >"$OUT/settings.txt"
inct sh -c "rm -rf $DIR && mkdir -p $DIR"

LOCK_WAIT="${LOCK_WAIT:-900}"
exec 9>"$BENCH_ROOT/bench.lock"
if [ "$LOCK_WAIT" -gt 0 ] && flock -x -w "$LOCK_WAIT" 9; then
  echo "lock=exclusive" | tee -a "$OUT/summary.txt"
else
  echo "lock=none (other benchmark work may overlap)" | tee -a "$OUT/summary.txt"
fi
conditions() {
  echo "$1: loadavg=$(cut -d' ' -f1-3 /proc/loadavg) io_pressure_$(head -1 /proc/pressure/io)" \
    "containers=$(bpodman ps --format '{{.Names}}' 9>&- | tr '\n' ' ')" | tee -a "$OUT/summary.txt"
}
conditions start

before="$(counters)"
inct dd if=/dev/urandom of="$DIR/one" bs=1M count=1 oflag=direct,sync iflag=fullblock status=none
delta "one 1 MiB direct write (3 appends of the whole write = one request per replica)" "$before" "$(counters)"

# fio_one <name> <fio args...>
fio_one() {
  local name=$1 before dirty=0 d pid rc=0
  shift
  before="$(counters)"
  inct fio --name="$name" --ioengine=psync --direct=1 --group_reporting --output-format=json "$@" \
    >"$OUT/$name.json" 2>"$OUT/$name.err" &
  pid=$!
  # The container's own dirty page cache: stays near zero if every write is flushed before it returns.
  while kill -0 "$pid" 2>/dev/null; do
    d="$(inct awk '$1 == "file_dirty" {print $2}' /sys/fs/cgroup/memory.stat 2>/dev/null || echo 0)"
    [ "${d:-0}" -le "$dirty" ] || dirty=$d
    sleep 2
  done
  wait "$pid" || rc=$?
  [ "$rc" -eq 0 ] || { echo "FAILED $name (fio exit $rc)" >&2; cat "$OUT/$name.err" >&2; exit 1; }
  delta "$name" "$before" "$(counters)"
  python3 -I - "$OUT/$name.json" "$name" "$dirty" <<'EOF' | tee -a "$OUT/summary.txt"
import json, sys
text = open(sys.argv[1]).read()
job = json.loads(text[text.index("{"):])["jobs"][0]
w = job["write"]
p = w["clat_ns"].get("percentile", {})
print(f"{sys.argv[2]}: error={job['error']} ios={w['total_ios']} iops={w['iops']:.1f} "
      f"MiB/s={w['bw_bytes'] / 1048576:.2f} runtime_ms={w['runtime']} "
      f"p50_ms={p.get('50.000000', 0) / 1e6:.1f} p99_ms={p.get('99.000000', 0) / 1e6:.1f} "
      f"peak_container_dirty_bytes={sys.argv[3]}")
EOF
}

fio_one layout --filename="$DIR/data" --size=256m --rw=write --bs=1m --end_fsync=1
fio_one randwrite-4k-qd1 --filename="$DIR/data" --size=256m --rw=randwrite --bs=4k --sync=1 \
  --time_based --runtime="$SECS"
fio_one layout32 --directory="$DIR" --numjobs=32 --nrfiles=1 --filesize=8m --size=8m \
  '--filename_format=many.$jobnum' --rw=write --bs=1m --end_fsync=1
fio_one randwrite-4k-qd32 --directory="$DIR" --numjobs=32 --nrfiles=1 --filesize=8m --size=8m \
  '--filename_format=many.$jobnum' --rw=randwrite --bs=4k --sync=1 --time_based --runtime="$SECS"

inct du -sk /srv/bench | tee -a "$OUT/summary.txt"
inct sh -c 'grep -ciE "error|warn" /var/log/atlas/*.log || true' | tee -a "$OUT/summary.txt"
inct rm -rf "$DIR"
conditions end
echo "results in $OUT"
