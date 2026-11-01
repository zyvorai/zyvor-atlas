#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
#
# Runs deploy/native/kvcache-smoke.sh against a real throwaway native cluster: plain processes inside the
# Dockerfile builder image on the lab host (scripts/lab-kvcache-remote.sh), with /dev/fuse. It starts the
# same 3 metadata + 3 data node cluster as lab-ransomware.sh, mounts a filesystem through
# `atlas-native-mount`, serves the same filesystem as an S3 bucket, and runs the smoke test over both.
# Pass/fail only; no throughput is measured or claimed.
set -uo pipefail

WORK=$(mktemp -d)
TOKEN=lab-kvcache-token
PIDS=()
MNT=$WORK/mnt
cleanup() {
  fusermount3 -uz "$MNT" 2>/dev/null || true
  sleep 1
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

BIN=${BIN:-target/debug}
NODE=$BIN/atlas-native-node
MOUNT=$BIN/atlas-native-mount
S3=$BIN/atlas-native-s3
GW=$BIN/atlas-gateway
CTL=$BIN/atlasctl
AK=kvkey
SK=kvsecret-0123456789abcdef

echo "$TOKEN" >"$WORK/token"
api() { curl -sS -H "Authorization: Bearer $TOKEN" "$@"; }

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
             "replicas":${RF:-3},"extent_bytes":4194304,
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
L=""
for _ in $(seq 1 200); do
  for i in 1 2 3; do
    api "http://127.0.0.1:2740$i/v1/status" 2>/dev/null | grep -q '"role":"leader"' && L=$i
  done
  [ -n "$L" ] && break
  sleep 0.1
done
[ -n "$L" ] || { echo "no leader elected"; exit 2; }
NODEURL=http://127.0.0.1:2740$L
# Every client gets all three metadata nodes: leadership can move during a long write, and a client
# pinned to the first leader gets 421 not_leader (seen as EIO / HTTP 500) for the rest of the run.
ALLURLS=http://127.0.0.1:27401,http://127.0.0.1:27402,http://127.0.0.1:27403
echo "cluster up; leader m$L"

ATLAS_DATABASE_URL="sqlite://$WORK/atlas.db?mode=rwc" ATLAS_BIND_ADDR=127.0.0.1:25110 ATLAS_GRPC_ADDR=127.0.0.1:25111 \
  ATLAS_CEPH_DRIVER_MODE=fake ATLAS_AUTH_REQUIRED=0 ATLAS_JWT_SECRET=lab-verify-secret-at-least-32-bytes!! \
  ATLAS_NATIVE_ENABLE=1 ATLAS_NATIVE_DRIVER_MODE=real ATLAS_NATIVE_ENDPOINTS=$ALLURLS \
  ATLAS_NATIVE_TOKEN_FILE=$WORK/token "$GW" >"$WORK/gw.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 300); do curl -sf http://127.0.0.1:25110/health >/dev/null && break; sleep 0.2; done
curl -sf http://127.0.0.1:25110/health >/dev/null || { echo "gateway did not start"; tail -20 "$WORK/gw.log"; exit 2; }

"$CTL" --base-url http://127.0.0.1:25110 create-volume kvfs --kind filesystem --backend bkd_native >"$WORK/create.out" 2>&1
VOL=$(curl -sS "http://127.0.0.1:25110/api/atlas/v1/volumes" | python3 -c 'import sys,json;v=[x for x in json.load(sys.stdin) if x["name"]=="kvfs"];print(v[0]["id"] if v else "")')
[ -n "$VOL" ] || { echo "filesystem volume not created"; cat "$WORK/create.out"; exit 2; }
FSID=${VOL#vol_native_fs_}

echo "[{\"bucket\":\"kvfs\",\"fs\":\"$FSID\"}]" >"$WORK/buckets.json"
echo "[{\"access_key\":\"$AK\",\"secret_key\":\"$SK\"}]" >"$WORK/creds.json"
"$S3" --endpoint "$ALLURLS" --token-file "$WORK/token" --buckets-file "$WORK/buckets.json" \
  --credentials-file "$WORK/creds.json" --listen 127.0.0.1:39019 2>"$WORK/s3.log" &
PIDS+=($!)
for _ in $(seq 1 100); do (echo >/dev/tcp/127.0.0.1/39019) 2>/dev/null && break; sleep 0.1; done

mkdir -p "$MNT"
# The exit status is logged: a silent death (137 = killed, 139 = segfault) is otherwise invisible.
( RUST_BACKTRACE=1 RUST_LOG=atlas_native_fuse=debug "$MOUNT" --endpoint "$ALLURLS" --token-file "$WORK/token" --fs "$FSID" "$MNT"; echo "atlas-native-mount exited with status $?" ) >"$WORK/mount.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 100); do mountpoint -q "$MNT" && break; sleep 0.1; done
mountpoint -q "$MNT" || { echo "mount did not come up"; cat "$WORK/mount.log"; exit 2; }
echo "filesystem $VOL mounted at $MNT and served as S3 bucket kvfs"

S3_ENDPOINT=http://127.0.0.1:39019 S3_BUCKET=kvfs S3_ACCESS_KEY=$AK S3_SECRET_KEY=$SK \
  bash deploy/native/kvcache-smoke.sh "$MNT"
RC=$?
if [ "$RC" -ne 0 ]; then
  echo "--- a check failed; logs of the processes under test (last lines)"
  df -h "$WORK" | tail -1
  echo "--- processes (pid state rss_kb cmd)"
  ps -eo pid,stat,rss,args | grep -E 'atlas-native|atlas-gateway' | grep -v grep | cut -c1-120
  echo "--- data dir sizes"
  du -sk "$WORK"/d1 "$WORK"/d2 "$WORK"/d3 "$WORK"/m1 2>/dev/null
  echo "--- container memory"
  cat /sys/fs/cgroup/memory.max /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.events 2>/dev/null
  echo "--- S3 probe (status + body)"
  head -c 2097152 /dev/urandom >"$WORK/probe.bin"
  curl -sS -o "$WORK/probe.body" -w 'PUT %{http_code}\n' --aws-sigv4 "aws:amz:us-east-1:s3" -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
    --user "$AK:$SK" -X PUT --data-binary @"$WORK/probe.bin" "http://127.0.0.1:39019/kvfs/probe" 2>&1
  head -c 400 "$WORK/probe.body"; echo
  echo "--- node status"
  api "$NODEURL/v1/status" | cut -c1-600
  for f in mount s3 gw m1 m2 m3 d1 d2 d3; do
    echo "--- $f.log"
    grep -v "not_found" "$WORK/$f.log" 2>/dev/null | tail -n 25 | cut -c1-240
  done
  mountpoint -q "$MNT" && echo "mount still up" || echo "MOUNT IS GONE"
fi
exit $RC
