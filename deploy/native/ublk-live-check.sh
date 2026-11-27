#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Live kernel check of atlas-native-ublk (docs/NATIVE_UBLK.md). Needs root and a loaded ublk_drv;
# it creates a real block device, writes to it, makes an ext4 filesystem on it and mounts it.
# It starts its own throwaway cluster (3 combined nodes on localhost, in a temporary directory)
# and touches no block device but the one its own daemon created.
#
# Usage:
#   sudo ATLAS_UBLK_LIVE=yes deploy/native/ublk-live-check.sh \
#        path/to/atlas-native-node path/to/atlas-native-ublk
#
# It does not load kernel modules: run `modprobe ublk_drv` yourself first.
# Environment: PORT_BASE (default 27100).
set -euo pipefail

NODE_BIN=${1:?usage: ublk-live-check.sh <atlas-native-node> <atlas-native-ublk>}
UBLK_BIN=${2:?usage: ublk-live-check.sh <atlas-native-node> <atlas-native-ublk>}
PORT_BASE=${PORT_BASE:-27100}

die() { echo "ublk-live-check: $*" >&2; exit 1; }

[ "${ATLAS_UBLK_LIVE:-}" = yes ] || die "set ATLAS_UBLK_LIVE=yes to create a real block device"
[ "$(uname -s)" = Linux ] || die "Linux only"
[ "$(id -u)" = 0 ] || die "needs root"
[ -c /dev/ublk-control ] || die "/dev/ublk-control is missing; load ublk_drv first"
for bin in "$NODE_BIN" "$UBLK_BIN"; do
  [ -x "$bin" ] || die "$bin not found or not executable"
done
for tool in curl cmp dd blockdev blkdiscard mkfs.ext4 e2fsck mount umount mountpoint sha256sum find; do
  command -v "$tool" >/dev/null || die "$tool is not installed"
done

WORK=$(mktemp -d /tmp/atlas-ublk-check.XXXXXX)
MNT="$WORK/mnt"
mkdir "$MNT"
TOKEN=ublk-check-token
echo "$TOKEN" >"$WORK/token"
NODE_PIDS=()
UBLK_PID=""
DEV=""
# Not a round size, so a device of another origin is unlikely to match it.
SIZE=$((268435456 + 4096 * (1 + $$ % 997)))

# Device nodes that existed before this run; none of them is ever opened.
BEFORE=" $(find /dev -maxdepth 1 -name 'ublkb*' 2>/dev/null | tr '\n' ' ') "

cleanup() {
  status=$?
  set +e
  if mountpoint -q "$MNT"; then umount "$MNT"; fi
  stop_daemon
  for p in "${NODE_PIDS[@]}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  if [ "$status" = 0 ]; then
    rm -rf --one-file-system "$WORK"
  else
    echo "ublk-live-check: failed; logs kept in $WORK" >&2
  fi
}
trap cleanup EXIT

# SIGINT stops the device; a second one fails requests still waiting for the cluster.
stop_daemon() {
  [ -n "$UBLK_PID" ] || return 0
  kill -INT "$UBLK_PID" 2>/dev/null
  for _ in $(seq 1 150); do
    kill -0 "$UBLK_PID" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$UBLK_PID" 2>/dev/null; then
    kill -INT "$UBLK_PID"
    for _ in $(seq 1 150); do
      kill -0 "$UBLK_PID" 2>/dev/null || break
      sleep 0.2
    done
  fi
  local rc=0
  if kill -0 "$UBLK_PID" 2>/dev/null; then
    echo "ublk-live-check: daemon $UBLK_PID did not exit; device ${DEV:-?} may remain" >&2
    rc=1
  else
    wait "$UBLK_PID" || rc=$?
    echo "daemon exit status: $rc"
  fi
  UBLK_PID=""
  return "$rc"
}

# Refuses to go on unless $DEV is the device this run's live daemon created. Called before
# everything that opens the device.
assert_ours() {
  [ -n "$DEV" ] || die "no device of ours"
  [ -n "$UBLK_PID" ] || die "no daemon of ours"
  [[ "$DEV" =~ ^/dev/ublkb([0-9]+)$ ]] || die "unexpected device path '$DEV'"
  local id=${BASH_REMATCH[1]}
  case "$BEFORE" in *" $DEV "*) die "$DEV existed before this run" ;; esac
  kill -0 "$UBLK_PID" 2>/dev/null || die "our daemon is gone"
  [ -b "$DEV" ] || die "$DEV is not a block device"
  # A ublk control node admits one opener, its server: so ours serves this device.
  find "/proc/$UBLK_PID/fd" -lname "/dev/ublkc$id" 2>/dev/null | grep -q . ||
    die "daemon $UBLK_PID does not hold /dev/ublkc$id"
  [ "$(cat "/sys/block/ublkb$id/size")" = $((SIZE / 512)) ] || die "$DEV has another size"
}

node_url() { echo "http://127.0.0.1:$((PORT_BASE + 400 + $1))"; }
api() { curl -sS --fail -H "Authorization: Bearer $TOKEN" "$@"; }

data_nodes=""
for i in 1 2 3; do
  data_nodes+="{\"id\":\"n$i\",\"addr\":\"127.0.0.1:$((PORT_BASE + 100 + i))\"},"
done
for i in 1 2 3; do
  peers=""
  for j in 1 2 3; do peers+="\"n$j\":\"127.0.0.1:$((PORT_BASE + 300 + j))\","; done
  cat >"$WORK/n$i.json" <<EOF
{"node_id":"n$i","data_dir":"$WORK/n$i","http_listen":"127.0.0.1:$((PORT_BASE + 400 + i))",
 "api_token_file":"$WORK/token",
 "data_node":{"listen":"127.0.0.1:$((PORT_BASE + 100 + i))"},
 "metadata":{"listen":"127.0.0.1:$((PORT_BASE + 300 + i))","peers":{${peers%,}},
             "data_nodes":[${data_nodes%,}],"replicas":2,"extent_bytes":1048576,
             "repair_interval_secs":0,"gc_interval_secs":2}}
EOF
done
start_node() {
  "$NODE_BIN" --config "$WORK/n$1.json" 2>>"$WORK/n$1.log" &
  NODE_PIDS[$1]=$!
}
for i in 1 2 3; do start_node "$i"; done
for i in 1 2 3; do
  for _ in $(seq 1 200); do
    curl -sf "$(node_url "$i")/readyz" >/dev/null && break
    sleep 0.1
  done
  curl -sf "$(node_url "$i")/readyz" >/dev/null || die "node n$i never became ready"
done

leader() {
  for i in 1 2 3; do
    if api "$(node_url "$i")/v1/status" 2>/dev/null | grep -q '"role":"leader"'; then
      echo "$i"
      return
    fi
  done
}
L=""
for _ in $(seq 1 100); do L=$(leader); [ -n "$L" ] && break; sleep 0.1; done
[ -n "$L" ] || die "no leader"
VOL=ublk-check
api -X POST "$(node_url "$L")/v1/volumes" \
  -d "{\"id\":\"$VOL\",\"name\":\"$VOL\",\"size_bytes\":$SIZE}" >/dev/null

ENDPOINTS="$(node_url 1),$(node_url 2),$(node_url 3)"
start_daemon() {
  : >"$WORK/ublk.out"
  "$UBLK_BIN" --endpoint "$ENDPOINTS" --volume "$VOL" --token-file "$WORK/token" "$@" \
    >"$WORK/ublk.out" 2>>"$WORK/ublk.log" &
  UBLK_PID=$!
  DEV=""
  for _ in $(seq 1 300); do
    DEV=$(sed -n 's/^device //p' "$WORK/ublk.out")
    [ -n "$DEV" ] && break
    kill -0 "$UBLK_PID" 2>/dev/null || die "daemon exited: $(tail -n 5 "$WORK/ublk.log")"
    sleep 0.1
  done
  [ -n "$DEV" ] || die "daemon reported no device"
  for _ in $(seq 1 100); do [ -b "$DEV" ] && break; sleep 0.1; done
  assert_ours
  echo "attached $DEV (daemon $UBLK_PID)"
}

step() { echo "== $*"; }

step "attach"
start_daemon
id=${DEV#/dev/ublkb}
q="/sys/block/ublkb$id/queue"
assert_ours
[ "$(blockdev --getsize64 "$DEV")" = "$SIZE" ] || die "size mismatch"
[ "$(cat "$q/logical_block_size")" = 4096 ] || die "logical block size"
[ "$(cat "$q/write_cache")" = "write through" ] || die "write cache: $(cat "$q/write_cache")"
[ "$(cat "$q/discard_max_bytes")" = 0 ] || die "discard should be unsupported"
echo "write_zeroes_max_bytes: $(cat "$q/write_zeroes_max_bytes")"

step "random data, direct I/O, cross-checked through the HTTP API"
dd if=/dev/urandom of="$WORK/random" bs=1M count=32 status=none
assert_ours
dd if="$WORK/random" of="$DEV" bs=1M seek=3 oflag=direct conv=notrunc status=none
assert_ours
dd if="$DEV" of="$WORK/back" bs=1M skip=3 count=32 iflag=direct status=none
cmp "$WORK/random" "$WORK/back"
L=$(leader)
api -o "$WORK/api" "$(node_url "$L")/v1/volumes/$VOL/data?offset=$((3 * 1048576))&len=1048576"
cmp -n 1048576 "$WORK/random" "$WORK/api"
# Single blocks, not aligned to extents.
dd if="$WORK/random" of="$WORK/block" bs=4096 count=1 status=none
assert_ours
dd if="$WORK/block" of="$DEV" bs=4096 seek=12345 oflag=direct conv=notrunc status=none
assert_ours
dd if="$DEV" bs=4096 skip=12345 count=1 iflag=direct status=none | cmp - "$WORK/block"

step "write zeroes works, discard is refused"
assert_ours
blkdiscard --zeroout --offset $((3 * 1048576)) --length 1048576 "$DEV"
assert_ours
dd if="$DEV" bs=1M skip=3 count=1 iflag=direct status=none | cmp - <(head -c 1048576 /dev/zero)
assert_ours
if blkdiscard --offset 0 --length 1048576 "$DEV" 2>/dev/null; then die "discard succeeded"; fi

if command -v fio >/dev/null; then
  step "fio write + verify"
  assert_ours
  fio --name=verify --filename="$DEV" --direct=1 --ioengine=libaio --rw=randwrite --bs=4k \
    --iodepth=16 --numjobs=1 --size=16M --offset=64M --verify=crc32c --do_verify=1 \
    --verify_fatal=1 --output="$WORK/fio.log" || die "fio failed: see $WORK/fio.log"
else
  echo "fio not installed; skipped"
fi

step "ext4: copy files, fail over the leader during writes, unmount, check"
assert_ours
mkfs.ext4 -q -E nodiscard "$DEV"
assert_ours
mount "$DEV" "$MNT"
cp "$WORK/random" "$MNT/before"
L=$(leader)
(for n in $(seq 1 8); do cp "$WORK/random" "$MNT/during-$n" && sync "$MNT/during-$n"; done) &
writer=$!
sleep 1
kill "${NODE_PIDS[$L]}"
echo "killed leader n$L"
wait "$writer" || die "writes failed during leader failover"
start_node "$L"
sync
sha256sum "$MNT"/before "$MNT"/during-* >"$WORK/sums"
umount "$MNT"
assert_ours
e2fsck -fn "$DEV"

step "detach, reattach, verify"
stop_daemon
[ ! -b "$DEV" ] || die "$DEV still exists after the daemon exited"
start_daemon
assert_ours
mount -o ro,noload "$DEV" "$MNT"
sha256sum -c --quiet "$WORK/sums"
umount "$MNT"
stop_daemon

step "read-only attach refuses writes"
start_daemon --read-only
assert_ours
[ "$(blockdev --getro "$DEV")" = 1 ] || die "device is not read-only"
assert_ours
if dd if="$WORK/block" of="$DEV" bs=4096 count=1 oflag=direct status=none 2>/dev/null; then
  die "a write to a read-only device succeeded"
fi
stop_daemon

echo "ublk-live-check: ok"
