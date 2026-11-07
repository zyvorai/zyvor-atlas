#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Data-path benchmark in a directory: an fio baseline on the filesystem under test, then the Atlas
# engine bench (crates/atlas-native/tests/datapath_bench.rs: 3 in-process data nodes, 3 replicas).
# Writes one JSON document to $OUT. Never touches block devices, never drops caches; it only creates
# files under "$BENCH_DIR/atlas-bench-run" and removes that directory on exit. See docs/NATIVE_PERF.md.
#
#   BENCH_DIR=/data/atlas-bench BENCH_MIB=256 ATLAS_BENCH_BACKEND=all deploy/native/bench.sh
# Env: BENCH_DIR (required), OUT (default $BENCH_DIR/bench.json), BENCH_MIB, BENCH_SMALL_OPS,
#      ATLAS_BENCH_BACKEND (file|aligned|io_uring|all), FIO_SIZE (default = BENCH_MIB MiB),
#      CARGO_FEATURES (e.g. io-uring), BENCH_CARGO (command to run cargo, default `cargo`; e.g.
#      `podman run --rm -v ... builder-image cargo` when cargo only exists in a container).
#      Needs python3; cargo (or BENCH_CARGO); fio is optional.
set -euo pipefail
: "${BENCH_DIR:?set BENCH_DIR to a directory on the filesystem under test}"
[[ -d "$BENCH_DIR" ]] || { echo "BENCH_DIR $BENCH_DIR is not a directory" >&2; exit 1; }
[[ "$BENCH_DIR" != /dev/* && "$BENCH_DIR" != / ]] || { echo "refusing BENCH_DIR=$BENCH_DIR" >&2; exit 1; }
MIB="${BENCH_MIB:-256}"
OUT="${OUT:-$BENCH_DIR/bench.json}"
RUN="${BENCH_DIR%/}/atlas-bench-run"
mkdir -p "$RUN"
LOG="$RUN.log"
trap 'rm -rf -- "${RUN:?}" "${LOG:?}"' EXIT

FIO_JSON=null
if command -v fio >/dev/null 2>&1; then
  echo "==> fio baseline in $RUN" >&2
  # fio is a reference for this filesystem: sequential 8 MiB and random 64 KiB, one job, with
  # O_SYNC writes and O_DIRECT reads (no page cache, no global cache drop). Unlike Atlas it writes
  # one copy, not three.
  FIO_JSON=$(RUN="$RUN" FIO_SIZE="${FIO_SIZE:-${MIB}M}" python3 - <<'PY' || echo null
import json, os, subprocess
run, size = os.environ["RUN"], os.environ["FIO_SIZE"]
def one(name, rw, bs, extra):
    out = subprocess.run(["fio", f"--name={name}", f"--directory={run}", f"--rw={rw}", f"--bs={bs}",
        f"--size={size}", "--ioengine=psync", "--numjobs=1", "--group_reporting",
        "--output-format=json", *extra], capture_output=True, text=True, check=True).stdout
    s = json.loads(out)["jobs"][0]["write" if "write" in rw else "read"]
    pc = s["clat_ns"]["percentile"]
    return {"mib_per_s": s["bw_bytes"] / 1048576, "iops": s["iops"],
            "p50_ms": pc["50.000000"] / 1e6, "p99_ms": pc["99.000000"] / 1e6,
            "p99_9_ms": pc["99.900000"] / 1e6}
r = {}
r["seq_write_8m_osync"] = one("sw", "write", "8m", ["--sync=1"])
r["seq_read_8m_direct"] = one("sr", "read", "8m", ["--direct=1"])
r["rand_write_64k_osync"] = one("rw", "randwrite", "64k", ["--sync=1", "--io_size=13m"])
r["rand_read_64k_direct"] = one("rr", "randread", "64k", ["--direct=1", "--io_size=13m"])
print(json.dumps(r))
PY
)
else
  echo "fio is not installed; skipping the fio baseline (not installing system packages)" >&2
fi

echo "==> Atlas datapath bench (backend: ${ATLAS_BENCH_BACKEND:-file})" >&2
ATLAS_BENCH_DIR="$RUN" ATLAS_BENCH_BACKEND="${ATLAS_BENCH_BACKEND:-file}" BENCH_MIB="$MIB" \
  ${BENCH_CARGO:-cargo} test --release -p atlas-native ${CARGO_FEATURES:+--features "$CARGO_FEATURES"} \
  --test datapath_bench datapath_report -- --ignored --nocapture 2>&1 | tee "$LOG" >&2

FIO_JSON="$FIO_JSON" OUT="$OUT" BENCH_DIR="$BENCH_DIR" LOG="$LOG" python3 - <<'PY'
import datetime, json, os, subprocess
atlas = [json.loads(l.split(" ", 1)[1]) for l in open(os.environ["LOG"]) if l.startswith("BENCH_JSON ")]
df = subprocess.run(["df", "-T", os.environ["BENCH_DIR"]], capture_output=True, text=True).stdout.splitlines()[-1]
doc = {"date": datetime.datetime.now(datetime.timezone.utc).isoformat(), "dir": os.environ["BENCH_DIR"],
       "df": df, "fio": json.loads(os.environ["FIO_JSON"]), "atlas": atlas}
json.dump(doc, open(os.environ["OUT"], "w"), indent=2)
print(json.dumps(doc, indent=2))
PY
