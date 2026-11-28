#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Entrypoint of atlas-bench-atlas: 3 metadata voters and 3 data nodes (RF 3, default `file` devices)
# under /srv/bench, one filesystem, mounted at /mnt/bench by atlas-native-mount. Creates
# /run/atlas/ready once the mount takes a durable write, and exits non-zero if any process dies.
set -euo pipefail

: "${ATLAS_EXTENT_BYTES:=1048576}" "${ATLAS_REPLICAS:=3}" "${ATLAS_TICK_MS:=200}"
: "${ATLAS_PROPOSAL_TIMEOUT_MS:=30000}" "${ATLAS_REPAIR_INTERVAL_SECS:=86400}" "${ATLAS_GC_INTERVAL_SECS:=60}"
: "${ATLAS_FUSE_THREADS:=32}" "${ATLAS_WRITEBACK_BYTES:=4194304}" "${ATLAS_WRITEBACK_PARALLEL:=8}"
: "${ATLAS_READAHEAD_BYTES:=4194304}" "${ATLAS_MAX_IO_BYTES:=8388608}" "${ATLAS_RETRY_SECS:=60}"
: "${ATLAS_DIRECT_READS:=0}" "${ATLAS_DIRECT_WRITES:=0}" "${ATLAS_START_TIMEOUT:=600}"
# Escape hatches for variants: extra JSON members of the "metadata" config object (e.g.
# '"compression": true'), of the "data_node" object of every data node (ATLAS_DATA_NODE_EXTRA, or
# ATLAS_DATA_EXTRA, which compare.sh sets: e.g. '"preallocate": {"enabled": true}'), or of every
# node's whole configuration, and extra atlas-native-mount arguments (space separated). A node
# refuses names it does not know.
: "${ATLAS_METADATA_EXTRA:=}" "${ATLAS_DATA_EXTRA:=}" "${ATLAS_MOUNT_EXTRA_ARGS:=}"
: "${ATLAS_DATA_NODE_EXTRA:=}" "${ATLAS_NODE_EXTRA:=}"

SRV=/srv/bench
RUN=/run/atlas
LOGS=/var/log/atlas
MNT=/mnt/bench
FS=bench
ENDPOINTS=http://127.0.0.1:7401,http://127.0.0.1:7402,http://127.0.0.1:7403
CLUSTER_EXTENT=4194304
[ "$ATLAS_EXTENT_BYTES" -le "$CLUSTER_EXTENT" ] || CLUSTER_EXTENT=$ATLAS_EXTENT_BYTES

mkdir -p "$RUN" "$LOGS" "$MNT"
rm -f "$RUN/ready"

declare -A PIDS=()

stop_all() {
  rm -f "$RUN/ready"
  if mountpoint -q "$MNT"; then
    timeout 20 fusermount3 -u "$MNT" || umount -l "$MNT" || true
  fi
  local p
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
}

fail() {
  echo "atlas-entry: $*" >&2
  local f
  for f in "$LOGS"/*.log; do
    [ -e "$f" ] || continue
    echo "--- $(basename "$f")" >&2
    tail -n 15 "$f" | cut -c1-300 >&2
  done
  stop_all
  exit 1
}
trap 'stop_all; exit 0' TERM INT

(umask 077 && head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$RUN/token")
AUTH="Authorization: Bearer $(cat "$RUN/token")"
api() { curl -sS --max-time 40 -H "$AUTH" "$@"; }

GRANT=""
if [ "$ATLAS_DIRECT_WRITES" = 1 ]; then
  (umask 077 && head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$RUN/grant")
  GRANT="\"grant_key_file\": \"$RUN/grant\","
fi

DATA_NODES='[{"id":"d1","addr":"127.0.0.1:7101"},{"id":"d2","addr":"127.0.0.1:7102"},{"id":"d3","addr":"127.0.0.1:7103"}]'
PEERS='{"m1":"127.0.0.1:7301","m2":"127.0.0.1:7302","m3":"127.0.0.1:7303"}'
for i in 1 2 3; do
  cat >"$RUN/d$i.json" <<EOF
{"node_id": "d$i", "data_dir": "$SRV/d$i", "http_listen": "127.0.0.1:720$i",
 "api_token_file": "$RUN/token", $GRANT
 "data_node": {"listen": "127.0.0.1:710$i"${ATLAS_DATA_NODE_EXTRA:+, $ATLAS_DATA_NODE_EXTRA}${ATLAS_DATA_EXTRA:+, $ATLAS_DATA_EXTRA}}${ATLAS_NODE_EXTRA:+, $ATLAS_NODE_EXTRA}}
EOF
  cat >"$RUN/m$i.json" <<EOF
{"node_id": "m$i", "data_dir": "$SRV/m$i", "http_listen": "127.0.0.1:740$i",
 "api_token_file": "$RUN/token", $GRANT
 "metadata": {"listen": "127.0.0.1:730$i", "peers": $PEERS, "data_nodes": $DATA_NODES,
   "replicas": $ATLAS_REPLICAS, "extent_bytes": $CLUSTER_EXTENT, "tick_ms": $ATLAS_TICK_MS,
   "proposal_timeout_ms": $ATLAS_PROPOSAL_TIMEOUT_MS,
   "repair_interval_secs": $ATLAS_REPAIR_INTERVAL_SECS,
   "gc_interval_secs": $ATLAS_GC_INTERVAL_SECS${ATLAS_METADATA_EXTRA:+, $ATLAS_METADATA_EXTRA}}${ATLAS_NODE_EXTRA:+, $ATLAS_NODE_EXTRA}}
EOF
done

for n in d1 d2 d3 m1 m2 m3; do
  atlas-native-node --config "$RUN/$n.json" >>"$LOGS/$n.log" 2>&1 &
  PIDS[$n]=$!
done

alive() {
  local n
  for n in "${!PIDS[@]}"; do
    kill -0 "${PIDS[$n]}" 2>/dev/null || fail "$n exited during startup"
  done
}

# One deadline for the whole startup: on a saturated disk the first elections can take minutes.
DEADLINE=$((SECONDS + ATLAS_START_TIMEOUT))
for port in 7201 7202 7203 7401 7402 7403; do
  until curl -sf --max-time 2 "http://127.0.0.1:$port/readyz" >/dev/null; do
    [ "$SECONDS" -lt "$DEADLINE" ] || fail "node on port $port not ready after ${ATLAS_START_TIMEOUT}s"
    alive
    sleep 0.2
  done
done

LEADER=""
while [ "$SECONDS" -lt "$DEADLINE" ]; do
  for i in 1 2 3; do
    if api "http://127.0.0.1:740$i/v1/status" 2>/dev/null | grep -q '"role":"leader"'; then LEADER=$i; fi
  done
  [ -n "$LEADER" ] && break
  alive
  sleep 0.2
done
[ -n "$LEADER" ] || fail "no metadata leader elected"
URL=http://127.0.0.1:740$LEADER
echo "cluster up; leader m$LEADER"

fs_extent() { api "$URL/v1/fs" | jq -r --arg id "$FS" '.filesystems[] | select(.id == $id) | .extent_bytes'; }
HAVE="$(fs_extent)"
if [ -z "$HAVE" ]; then
  code=""
  for _ in $(seq 1 30); do
    code="$(api -o "$RUN/create.json" -w '%{http_code}' -H 'Content-Type: application/json' \
      -d "{\"id\": \"$FS\", \"name\": \"$FS\", \"extent_bytes\": $ATLAS_EXTENT_BYTES}" "$URL/v1/fs")" || code=""
    [ "$code" = 201 ] && break
    sleep 1
  done
  [ "$code" = 201 ] || fail "filesystem create failed: $code $(cat "$RUN/create.json" 2>/dev/null)"
  HAVE="$(fs_extent)"
fi
[ "$HAVE" = "$ATLAS_EXTENT_BYTES" ] ||
  fail "filesystem $FS has extent_bytes=$HAVE, wanted $ATLAS_EXTENT_BYTES (remove it with down.sh atlas first)"
echo "filesystem $FS: extent_bytes=$HAVE"

MOUNT_ARGS=(--endpoint "$ENDPOINTS" --token-file "$RUN/token" --fs "$FS"
  --fuse-threads "$ATLAS_FUSE_THREADS" --writeback-bytes "$ATLAS_WRITEBACK_BYTES"
  --writeback-parallel "$ATLAS_WRITEBACK_PARALLEL" --readahead-bytes "$ATLAS_READAHEAD_BYTES"
  --max-io-bytes "$ATLAS_MAX_IO_BYTES" --retry-secs "$ATLAS_RETRY_SECS")
[ "$ATLAS_DIRECT_READS" = 1 ] && MOUNT_ARGS+=(--direct-reads)
[ "$ATLAS_DIRECT_WRITES" = 1 ] && MOUNT_ARGS+=(--direct-writes)
read -r -a EXTRA <<<"$ATLAS_MOUNT_EXTRA_ARGS"
atlas-native-mount "${MOUNT_ARGS[@]}" "${EXTRA[@]}" "$MNT" >>"$LOGS/mount.log" 2>&1 &
PIDS[mount]=$!
for _ in $(seq 1 300); do
  mountpoint -q "$MNT" && break
  alive
  sleep 0.1
done
mountpoint -q "$MNT" || fail "mount did not come up"

head -c 4096 /dev/urandom >"$RUN/probe"
dd if="$RUN/probe" of="$MNT/.atlas-ready-probe" bs=4096 count=1 oflag=direct,sync status=none ||
  fail "probe write failed"
cmp "$RUN/probe" "$MNT/.atlas-ready-probe" || fail "probe read back differs"
rm "$MNT/.atlas-ready-probe"

{
  echo "extent_bytes=$HAVE (verified through GET /v1/fs)"
  echo "mount_args=${MOUNT_ARGS[*]:6} ${EXTRA[*]}"
  jq -c '.metadata | del(.peers, .data_nodes, .listen)' "$RUN/m1.json"
  echo "data_node=$(jq -c '.data_node | del(.listen)' "$RUN/d1.json")"
  [ -z "$ATLAS_DATA_NODE_EXTRA" ] || echo "data_node_extra=$ATLAS_DATA_NODE_EXTRA"
  [ -z "$ATLAS_NODE_EXTRA" ] || echo "node_extra=$ATLAS_NODE_EXTRA"
  atlas-native-mount --version
  fio --version
} >"$SRV/settings.txt"
cat "$SRV/settings.txt"
touch "$RUN/ready"
echo "ready: $MNT"

set +e
wait -n
rc=$?
dead=""
for n in "${!PIDS[@]}"; do kill -0 "${PIDS[$n]}" 2>/dev/null || dead="$dead $n"; done
fail "process exited (status $rc):$dead"
