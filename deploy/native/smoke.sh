#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Starts 3 metadata + 3 data atlas-native-node processes on localhost from generated configs,
# exercises the HTTP API with curl, kills the metadata leader and checks the survivors keep serving.
# Usage: deploy/native/smoke.sh [path/to/atlas-native-node]
set -euo pipefail

BIN=${1:-target/debug/atlas-native-node}
WORK=$(mktemp -d)
TOKEN=smoke-token
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "$TOKEN" >"$WORK/token"
api() { curl -sS -H "Authorization: Bearer $TOKEN" "$@"; }

data_nodes='[{"id":"d1","addr":"127.0.0.1:17101"},{"id":"d2","addr":"127.0.0.1:17102"},{"id":"d3","addr":"127.0.0.1:17103"}]'
for i in 1 2 3; do
  cat >"$WORK/d$i.json" <<EOF
{"node_id":"d$i","data_dir":"$WORK/d$i","http_listen":"127.0.0.1:1720$i",
 "data_node":{"listen":"127.0.0.1:1710$i"}}
EOF
  peers=$(for j in 1 2 3; do [ "$j" = "$i" ] || printf '"m%s":"127.0.0.1:1730%s",' "$j" "$j"; done)
  cat >"$WORK/m$i.json" <<EOF
{"node_id":"m$i","data_dir":"$WORK/m$i","http_listen":"127.0.0.1:1740$i",
 "api_token_file":"$WORK/token",
 "metadata":{"listen":"127.0.0.1:1730$i","peers":{${peers%,}},"data_nodes":$data_nodes,
             "replicas":3,"extent_bytes":4096,"repair_interval_secs":2,"gc_interval_secs":2}}
EOF
done
for n in d1 d2 d3 m1 m2 m3; do
  "$BIN" --config "$WORK/$n.json" 2>"$WORK/$n.log" &
  PIDS+=($!)
done

wait_ready() {
  for _ in $(seq 1 200); do
    if curl -sf "http://127.0.0.1:$1/readyz" >/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "port $1 never became ready" >&2
  return 1
}
for i in 1 2 3; do wait_ready "1740$i"; done

leader() {
  for i in 1 2 3; do
    if api "http://127.0.0.1:1740$i/v1/status" 2>/dev/null | grep -q '"role":"leader"'; then
      echo "$i"
      return
    fi
  done
}
L=""
for _ in $(seq 1 100); do L=$(leader); [ -n "$L" ] && break; sleep 0.1; done
echo "leader: m$L"

VOL=$(api -X POST "http://127.0.0.1:1740$L/v1/volumes" -d '{"name":"smoke","size_bytes":8192}' |
  sed -E 's/.*"id":"([^"]+)".*/\1/')
head -c 4096 /dev/urandom >"$WORK/block"
api -X PUT --data-binary @"$WORK/block" "http://127.0.0.1:1740$L/v1/volumes/$VOL/data?offset=0"
api -o "$WORK/back" "http://127.0.0.1:1740$L/v1/volumes/$VOL/data?offset=0&len=4096"
cmp "$WORK/block" "$WORK/back"
echo "write/read through leader: ok"

kill "${PIDS[$((L + 2))]}"
echo "killed leader m$L"
NEW=""
for _ in $(seq 1 100); do
  NEW=$(leader)
  [ -n "$NEW" ] && [ "$NEW" != "$L" ] && break
  sleep 0.1
done
[ -n "$NEW" ] && [ "$NEW" != "$L" ] || { echo "no new leader" >&2; exit 1; }
echo "new leader: m$NEW"
api -o "$WORK/back2" "http://127.0.0.1:1740$NEW/v1/volumes/$VOL/data?offset=0&len=4096"
cmp "$WORK/block" "$WORK/back2"
api -X PUT --data-binary @"$WORK/block" "http://127.0.0.1:1740$NEW/v1/volumes/$VOL/data?offset=4096"
echo "read + write after failover: ok"

curl -sf "http://127.0.0.1:1740$NEW/metrics" | grep -E '^atlas_native_(raft_role\{.*leader"\}|volumes|repair_runs_total)'
curl -sf "http://127.0.0.1:17201/metrics" | grep -E '^atlas_native_data_(fence|requests_total)'
echo "smoke: ok"
