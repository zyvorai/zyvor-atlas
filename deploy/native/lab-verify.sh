#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
#
# Live verification of the retention-lock / ransomware / data-reduction features against a real
# multi-process native cluster. Meant to run INSIDE the Dockerfile builder image on the lab host
# (scripts/lab-verify-remote.sh), as plain processes on localhost: it takes no Kubernetes pod slot
# and touches nothing but its own temp directory.
#
# Starts 3 metadata + 3 data nodes (compression and dedup on), the S3 gateway and the Atlas gateway
# (native driver, ransomware auto-protect on), then:
#   1. data reduction: duplicate + compressible objects through S3, byte-exact reads, /v1/reduction
#   2. S3 compliance retention: set, read, extend, refuse shortening, refuse delete and overwrite
#   3. ransomware: a quiet baseline, then a burst of high-entropy overwrites and deletes; the
#      gateway must call it likely_ransomware, auto-protect must lock a snapshot, a manual
#      `protect` must too, and the locked snapshot must refuse deletion
# Prints one PASS/FAIL line per check and exits non-zero if any failed.
# Env: BASELINE_MINUTES (default 8; the assessor needs at least 6 quiet intervals first).
set -uo pipefail

WORK=$(mktemp -d)
TOKEN=lab-verify-token
PIDS=()
FAILS=0
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILS=$((FAILS + 1)); }
check() { # check "name" <command...>
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi
}

BIN=${BIN:-target/debug}
NODE=$BIN/atlas-native-node
S3=$BIN/atlas-native-s3
GW=$BIN/atlas-gateway
CTL=$BIN/atlasctl

echo "$TOKEN" >"$WORK/token"
api() { curl -sS -H "Authorization: Bearer $TOKEN" "$@"; }
AK=labkey
SK=labsecret-0123456789abcdef
S3URL=http://127.0.0.1:19000
s3() { # s3 METHOD path [curl args...]
  local m=$1 p=$2
  shift 2
  curl -sS -o "$WORK/s3.out" -w '%{http_code}' --aws-sigv4 "aws:amz:us-east-1:s3" -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" --user "$AK:$SK" -X "$m" "$S3URL$p" "$@"
}

data_nodes='[{"id":"d1","addr":"127.0.0.1:27101"},{"id":"d2","addr":"127.0.0.1:27102"},{"id":"d3","addr":"127.0.0.1:27103"}]'
for i in 1 2 3; do
  cat >"$WORK/d$i.json" <<EOF
{"node_id":"d$i","data_dir":"$WORK/d$i","http_listen":"127.0.0.1:2720$i",
 "data_node":{"listen":"127.0.0.1:2710$i"}}
EOF
  peers=$(for j in 1 2 3; do [ "$j" = "$i" ] || printf '"m%s":"127.0.0.1:2730%s",' "$j" "$j"; done)
  cat >"$WORK/m$i.json" <<EOF
{"node_id":"m$i","data_dir":"$WORK/m$i","http_listen":"127.0.0.1:2740$i",
 "api_token_file":"$WORK/token",
 "metadata":{"listen":"127.0.0.1:2730$i","peers":{${peers%,}},"data_nodes":$data_nodes,
             "replicas":3,"extent_bytes":4096,"compression":true,"dedup":true,
             "repair_interval_secs":5,"gc_interval_secs":5}}
EOF
done
for n in d1 d2 d3 m1 m2 m3; do
  "$NODE" --config "$WORK/$n.json" 2>"$WORK/$n.log" &
  PIDS+=($!)
done
for i in 1 2 3; do
  for _ in $(seq 1 300); do curl -sf "http://127.0.0.1:2740$i/readyz" >/dev/null && break; sleep 0.1; done
done
leader() {
  for i in 1 2 3; do
    if api "http://127.0.0.1:2740$i/v1/status" 2>/dev/null | grep -q '"role":"leader"'; then echo "$i"; return; fi
  done
}
L=""
for _ in $(seq 1 200); do L=$(leader); [ -n "$L" ] && break; sleep 0.1; done
[ -n "$L" ] || { echo "no leader elected"; exit 2; }
NODEURL=http://127.0.0.1:2740$L
echo "cluster up; leader m$L; every client below uses m$L only (activity windows are per serving node)"

# Atlas gateway: fake Ceph, real native driver on the leader, auto-protect every 10 s.
ATLAS_DATABASE_URL="sqlite://$WORK/atlas.db?mode=rwc" ATLAS_BIND_ADDR=127.0.0.1:25110 ATLAS_GRPC_ADDR=127.0.0.1:25111 \
  ATLAS_CEPH_DRIVER_MODE=fake ATLAS_AUTH_REQUIRED=0 ATLAS_JWT_SECRET=lab-verify-secret-at-least-32-bytes!! \
  ATLAS_NATIVE_ENABLE=1 ATLAS_NATIVE_DRIVER_MODE=real ATLAS_NATIVE_ENDPOINTS=$NODEURL \
  ATLAS_NATIVE_TOKEN_FILE=$WORK/token ATLAS_RANSOMWARE_AUTOPROTECT=1 ATLAS_RANSOMWARE_INTERVAL_SECS=10 \
  "$GW" >"$WORK/gw.log" 2>&1 &  # tracing logs to stdout
PIDS+=($!)
GWURL=http://127.0.0.1:25110/api/atlas/v1
for _ in $(seq 1 300); do curl -sf http://127.0.0.1:25110/health >/dev/null && break; sleep 0.2; done
curl -sf http://127.0.0.1:25110/health >/dev/null || { echo "gateway did not start"; tail -20 "$WORK/gw.log"; exit 2; }

# The filesystem is created through the gateway, as an operator would.
"$CTL" --base-url http://127.0.0.1:25110 create-volume labfs --kind filesystem --backend bkd_native >"$WORK/create.out" 2>&1
VOLS=$(curl -sS "$GWURL/volumes")
VOL=$(echo "$VOLS" | python3 -c 'import sys,json;v=[x for x in json.load(sys.stdin) if x["name"]=="labfs"];print(v[0]["id"] if v else "")')
[ -n "$VOL" ] || { echo "filesystem volume not created"; cat "$WORK/create.out"; tail -20 "$WORK/gw.log"; exit 2; }
FSID=${VOL#vol_native_fs_}
echo "filesystem volume $VOL (native id $FSID)"

echo "[{\"bucket\":\"labfs\",\"fs\":\"$FSID\"}]" >"$WORK/buckets.json"
echo "[{\"access_key\":\"$AK\",\"secret_key\":\"$SK\"}]" >"$WORK/creds.json"
"$S3" --endpoint "$NODEURL" --token-file "$WORK/token" --buckets-file "$WORK/buckets.json" \
  --credentials-file "$WORK/creds.json" --listen 127.0.0.1:19000 2>"$WORK/s3.log" &
PIDS+=($!)
for _ in $(seq 1 100); do (echo >/dev/tcp/127.0.0.1/19000) 2>/dev/null && break; sleep 0.1; done

# ---------------------------------------------------------------- 1. data reduction
echo "--- data reduction"
python3 - "$WORK/text.bin" <<'PY'
import sys
line = b"atlas native data reduction check line 0123456789 abcdefghijklmnopqrstuvwxyz\n"
open(sys.argv[1], "wb").write(line * 3500)  # ~266 KiB, very compressible
PY
for n in $(seq 1 12); do
  code=$(s3 PUT "/labfs/dup/copy$n.bin" --data-binary @"$WORK/text.bin")
  [ "$code" = 200 ] || fail "PUT dup/copy$n.bin -> HTTP $code"
done
ok=1
for n in 1 6 12; do
  s3 GET "/labfs/dup/copy$n.bin" >/dev/null
  cmp -s "$WORK/s3.out" "$WORK/text.bin" || ok=0
done
[ $ok = 1 ] && pass "byte-exact read-back of deduplicated, compressed objects" || fail "read-back differs"
sleep 3
RED=$(api "$NODEURL/v1/reduction")
echo "reduction: $RED"
python3 - "$RED" <<'PY' && pass "reduction report: shared and compressed extents, stored < logical" || fail "reduction report does not show savings"
import sys, json
r = json.loads(sys.argv[1])
r = r.get("total", r)
assert r["shared_extents"] > 0, r
assert r["compressed_extents"] > 0, r
assert r["stored_bytes"] < r["logical_bytes"], r
assert r["referenced_bytes"] > r["logical_bytes"], r
PY
GRED=$(curl -sS "$GWURL/native/reduction")
echo "$GRED" | python3 -c 'import sys,json;r=json.load(sys.stdin);assert r["compression_ratio"]>1 and r["sharing_ratio"]>1,r' \
  && pass "gateway /native/reduction ratios above 1" || fail "gateway reduction report: $GRED"

# ---------------------------------------------------------------- 2. S3 compliance retention
echo "--- S3 retention"
code=$(s3 PUT "/labfs/vault/keep.txt" --data-binary "evidence")
[ "$code" = 200 ] && pass "PUT vault/keep.txt" || fail "PUT vault/keep.txt -> $code"
until_ts=$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)
ret_xml() { printf '<Retention xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Mode>COMPLIANCE</Mode><RetainUntilDate>%s</RetainUntilDate></Retention>' "$1"; }
code=$(s3 PUT "/labfs/vault/keep.txt?retention=" --data-binary "$(ret_xml "$until_ts")")
[ "$code" = 200 ] && pass "PutObjectRetention COMPLIANCE +1h" || { fail "PutObjectRetention -> $code: $(cat "$WORK/s3.out")"; }
code=$(s3 GET "/labfs/vault/keep.txt?retention=")
grep -q COMPLIANCE "$WORK/s3.out" && [ "$code" = 200 ] && pass "GetObjectRetention reports COMPLIANCE" || fail "GetObjectRetention -> $code: $(cat "$WORK/s3.out")"
code=$(s3 DELETE "/labfs/vault/keep.txt")
[ "$code" = 403 ] && pass "DELETE of a locked object is refused (403)" || fail "DELETE locked -> $code"
code=$(s3 PUT "/labfs/vault/keep.txt" --data-binary "tampered")
[ "$code" = 403 ] && pass "overwrite of a locked object is refused (403)" || fail "overwrite locked -> $code"
s3 GET "/labfs/vault/keep.txt" >/dev/null
[ "$(cat "$WORK/s3.out")" = "evidence" ] && pass "locked object content unchanged" || fail "locked object changed"
short_ts=$(date -u -d '+10 minutes' +%Y-%m-%dT%H:%M:%SZ)
code=$(s3 PUT "/labfs/vault/keep.txt?retention=" --data-binary "$(ret_xml "$short_ts")")
[ "$code" = 403 ] && ! grep -q SignatureDoesNotMatch "$WORK/s3.out" && pass "shortening retention is refused ($code)" || fail "shortening retention -> $code: $(cat "$WORK/s3.out")"
long_ts=$(date -u -d '+2 hours' +%Y-%m-%dT%H:%M:%SZ)
code=$(s3 PUT "/labfs/vault/keep.txt?retention=" --data-binary "$(ret_xml "$long_ts")")
[ "$code" = 200 ] && pass "extending retention is accepted" || fail "extending retention -> $code"
code=$(s3 PUT "/labfs/free/unlocked.txt" --data-binary x)
code2=$(s3 DELETE "/labfs/free/unlocked.txt")
[ "$code" = 200 ] && [ "$code2" = 204 ] && pass "an unlocked object deletes normally" || fail "unlocked delete -> $code/$code2"

# ---------------------------------------------------------------- 3. ransomware
echo "--- ransomware: quiet baseline for ${BASELINE_MINUTES:-8} minutes"
mkdir -p "$WORK/seed"
python3 - "$WORK/seed" <<'PY'
import os, sys
for i in range(60):
    open(f"{sys.argv[1]}/doc{i}.txt", "wb").write(("quarterly report page %d\n" % i).encode() * 40)
PY
for i in $(seq 0 59); do s3 PUT "/labfs/docs/doc$i.txt" --data-binary @"$WORK/seed/doc$i.txt" >/dev/null; done
END=$(( $(date +%s) + ${BASELINE_MINUTES:-8} * 60 ))
n=0
while [ "$(date +%s)" -lt "$END" ]; do
  s3 PUT "/labfs/docs/doc$((n % 60)).txt" --data-binary @"$WORK/seed/doc$((n % 60)).txt" >/dev/null
  n=$((n + 1))
  sleep 15
done
ACT=$(api "$NODEURL/v1/fs/$FSID/activity")
echo "activity windows kept by the serving node: $(echo "$ACT" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["samples"]))')"
VERD=$(curl -sS "$GWURL/volumes/$VOL/ransomware")
echo "baseline verdict: $(echo "$VERD" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["verdict"],d.get("score"))')"
echo "$VERD" | python3 -c 'import sys,json;d=json.load(sys.stdin);assert d["verdict"] in ("normal",) and d["can_execute"] is False,d' \
  && pass "quiet baseline is assessed normal (can_execute false)" || fail "baseline verdict: $VERD"
SNAPS_BEFORE=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')
SNAP_IDS_BEFORE=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;print(" ".join(x["id"] for x in json.load(sys.stdin)))')

echo "--- ransomware: encrypt-and-delete burst"
for i in $(seq 0 59); do
  head -c 8192 /dev/urandom >"$WORK/enc.bin"
  s3 PUT "/labfs/docs/doc$i.txt.locked" --data-binary @"$WORK/enc.bin" >/dev/null &
  p1=$!
  s3 PUT "/labfs/docs/doc$i.txt" --data-binary @"$WORK/enc.bin" >/dev/null &
  p2=$!
  # Not a bare `wait`: that also waits for the node/gateway daemons started above and never returns.
  wait "$p1" "$p2"
  s3 DELETE "/labfs/docs/doc$i.txt" >/dev/null
done
ACT=$(api "$NODEURL/v1/fs/$FSID/activity")
python3 - "$ACT" <<'PY' && pass "activity window shows high-entropy writes and deletes" || fail "activity window lacks the burst"
import sys, json
s = json.loads(sys.argv[1])["samples"]
# The burst can straddle two one-minute windows and the earlier reduction writes sit in another one,
# so judge the windows together rather than picking the busiest by bytes.
hot = [x for x in s if x["high_entropy_ratio"] > 0.5]
print("high-entropy windows:", hot)
assert hot, s
assert sum(x["files_deleted"] for x in s) >= 20, s
PY
VERD=$(curl -sS "$GWURL/volumes/$VOL/ransomware")
echo "burst verdict: $(echo "$VERD" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["verdict"],d.get("score"))')"
echo "$VERD" | python3 -c 'import sys,json;d=json.load(sys.stdin);assert d["verdict"]=="likely_ransomware" and d["can_execute"] is False,d' \
  && pass "burst is assessed likely_ransomware (read-only verdict)" || fail "burst verdict: $VERD"

echo "--- auto-protect (every 10 s)"
AUTO=""
for _ in $(seq 1 18); do
  AUTO=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')
  [ "$AUTO" -gt "$SNAPS_BEFORE" ] && break
  sleep 5
done
[ "$AUTO" -gt "$SNAPS_BEFORE" ] && pass "auto-protect took a snapshot ($SNAPS_BEFORE -> $AUTO)" || fail "auto-protect took no snapshot"
grep -q "locked snapshot taken" "$WORK/gw.log" && pass "gateway logged the auto-protect lock" || fail "no auto-protect log line"
# The snapshot auto-protect added, not "the last in the list" (the API promises no ordering).
AUTOSNAP=$(curl -sS "$GWURL/snapshots" | python3 -c 'import sys,json;old=set(sys.argv[1].split());print(next(x["id"] for x in json.load(sys.stdin) if x["id"] not in old))' "$SNAP_IDS_BEFORE")
code=$(curl -sS -o "$WORK/del.out" -w '%{http_code}' -X DELETE "$GWURL/snapshots/$AUTOSNAP")
[ "$code" = 409 ] && pass "the auto-protect snapshot refuses deletion (409)" || fail "delete locked snapshot -> $code $(cat "$WORK/del.out")"

echo "--- manual protect"
code=$(curl -sS -o "$WORK/prot.out" -w '%{http_code}' -X POST "$GWURL/volumes/$VOL/ransomware/protect")
[ "$code" = 201 ] && pass "manual protect re-assessed and locked a snapshot" || fail "manual protect -> $code $(cat "$WORK/prot.out")"
PSNAP=$(python3 -c 'import sys,json;print(json.load(open(sys.argv[1])).get("snapshot_id",""))' "$WORK/prot.out")
code=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE "$GWURL/snapshots/$PSNAP")
[ "$code" = 409 ] && pass "the protect snapshot refuses deletion (409)" || fail "delete protect snapshot -> $code"
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$GWURL/snapshots/$PSNAP/lock" -H 'content-type: application/json' -d '{"retain_secs":120}')
[ "$code" = 409 ] && pass "shortening the lock is refused (409)" || fail "shorten lock -> $code"

echo
if [ "$FAILS" = 0 ]; then echo "ALL CHECKS PASSED"; else echo "$FAILS CHECK(S) FAILED"; echo "--- gateway log tail"; tail -15 "$WORK/gw.log"; echo "--- s3 log tail"; tail -10 "$WORK/s3.log"; fi
exit "$FAILS"
