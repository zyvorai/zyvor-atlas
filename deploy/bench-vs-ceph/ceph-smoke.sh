#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Smoke check of a running atlas-bench-ceph, not a benchmark: 30 s of 4k random O_DIRECT+O_SYNC writes
# on 256 MiB, with write counters around it to show that every write reaches all three OSDs and the
# disk: Ceph's own OSD counters, the bytes each ceph-osd process sent to the block layer
# (/proc/<pid>/io), and the host disk. Disk figures are for the whole shared device, so an idle window of
# about the same length is printed next to them.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$HERE/common.sh"

CT="$(container_name ceph)"
OUT="$BENCH_ROOT/results/ceph-smoke"
mkdir -p "$OUT"
exec 9>"$BENCH_ROOT/bench.lock"
echo "waiting for the benchmark lock"
flock -s 9 # never during a run.sh measurement; builds may still overlap, hence the pressure line
trap 'flock -u 9' EXIT
inct() { bpodman exec "$CT" "$@" 9>&-; }

DISK="$(basename "$(findmnt -no SOURCE --target "$BENCH_ROOT")")"
# writes completed, sectors written, flush requests completed
disk() { awk -v d="$DISK" '$3 == d {print $8, $10, $19}' /proc/diskstats; }
perf() {
  inct bash -c 'for i in 0 1 2; do ceph tell osd.$i perf dump; done | jq -s "
    {op_w: (map(.osd.op_w) | add), op_w_bytes: (map(.osd.op_w_in_bytes) | add),
     subop_w: (map(.osd.subop_w) | add),
     deferred: (map(.bluestore | with_entries(select(.key | test(\"deferred\"))) | with_entries(select(.value | type == \"number\"))))}"'
}
# bytes each ceph-osd process has sent to storage, one per line in pid order
osd_bytes() { inct bash -c 'for p in $(pgrep -x ceph-osd); do awk "\$1 == \"write_bytes:\" {print \$2}" /proc/$p/io; done'; }
window() { # <label> <before> <after>
  local b a
  read -ra b <<<"$2"
  read -ra a <<<"$3"
  echo "$1: disk $DISK writes=$((a[0] - b[0])) MiB=$(((a[1] - b[1]) / 2048)) flushes=$((a[2] - b[2]))"
}

FIO=(fio --filename=/mnt/bench/ceph-smoke.dat --size=256m --ioengine=psync --direct=1)
inct "${FIO[@]}" --name=layout --rw=write --bs=1m --end_fsync=1 --output=/dev/null
sleep 15

idle0="$(disk)"
sleep 35
idle1="$(disk)"

perf >"$OUT/perf-before.json"
osd_before="$(osd_bytes)"
before="$(disk)"
inct "${FIO[@]}" --name=smoke --rw=randwrite --bs=4k --sync=1 --time_based --runtime=30 \
  --output-format=json >"$OUT/fio.json"
sleep 5 # let deferred writes reach their final place
after="$(disk)"
osd_after="$(osd_bytes)"
perf >"$OUT/perf-after.json"
inct rm -f /mnt/bench/ceph-smoke.dat

{
  python3 -I - "$OUT/fio.json" <<'EOF'
import json, sys
text = open(sys.argv[1]).read()
w = json.loads(text[text.index("{"):])["jobs"][0]["write"]
p = w["clat_ns"]["percentile"]
print(f"smoke 4k randwrite qd1 direct sync, 256 MiB, 30 s: ios={w['total_ios']} iops={w['iops']:.1f} "
      f"KiB/s={w['bw']} lat_mean_ms={w['lat_ns']['mean'] / 1e6:.2f} "
      f"clat_p50_ms={p['50.000000'] / 1e6:.2f} clat_p99_ms={p['99.000000'] / 1e6:.2f}")
EOF
  echo "host load=$(cut -d' ' -f1-3 /proc/loadavg) $(head -1 /proc/pressure/io)"
  window "idle 35 s" "$idle0" "$idle1"
  window "fio 30 s + 5 s" "$before" "$after"
  paste <(echo "$osd_before") <(echo "$osd_after") |
    awk '{printf "ceph-osd process %d: %.0f KiB written to storage over the fio run\n", NR - 1, ($2 - $1) / 1024}'
  echo "OSD counters (sum of 3 OSDs), delta over the fio run:"
  python3 -I - "$OUT/perf-before.json" "$OUT/perf-after.json" <<'EOF'
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
for k in ("op_w", "op_w_bytes", "subop_w"):
    print(f"  {k}={b[k] - a[k]}")
for k in b["deferred"][0]:
    print(f"  bluestore.{k}={sum(o[k] for o in b['deferred']) - sum(o[k] for o in a['deferred'])}")
EOF
} | tee "$OUT/summary.txt"
