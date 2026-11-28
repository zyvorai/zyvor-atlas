#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Live kernel check of atlas-native-ublk (docs/NATIVE_UBLK.md). Needs root and a loaded ublk_drv;
# it creates a real block device, writes to it, makes an ext4 filesystem on it and mounts it.
# It starts its own throwaway cluster (3 combined nodes on localhost, in a temporary directory),
# SIGKILLs one of its own servers to check recovery, and writes to no block device but the ones
# its own servers create. It also mounts a small tmpfs under the temporary directory.
#
# Usage:
#   sudo ATLAS_UBLK_LIVE=yes deploy/native/ublk-live-check.sh \
#        path/to/atlas-native-node path/to/atlas-native-ublk
#
# It does not load kernel modules: run `modprobe ublk_drv` yourself first.
# Environment:
#   PORT_BASE      first of the localhost ports used (default 27100)
#   TMPDIR         where the cluster keeps its data, a few GiB at most (default /tmp)
#   CHECK_TIMEOUT  seconds after which the whole check is abandoned (default 1800)
#   SKIP_FIO       1 skips the fio steps
# fio is optional: without it the fio steps are skipped, and the result says so. On a spinning
# disk small random writes take seconds each (the cluster rewrites a whole extent for each), so
# the fio steps exceed the server's I/O timeout: point TMPDIR at fast storage or a tmpfs.
set -euo pipefail

NODE_BIN=${1:?usage: ublk-live-check.sh <atlas-native-node> <atlas-native-ublk>}
UBLK_BIN=${2:?usage: ublk-live-check.sh <atlas-native-node> <atlas-native-ublk>}
PORT_BASE=${PORT_BASE:-27100}
CHECK_TIMEOUT=${CHECK_TIMEOUT:-1800}
# Longest any one command on the device may take.
LIMIT=600
# The servers' default --settle-secs, waited out before a killed server's volume is reattached.
SETTLE=15

die() { echo "ublk-live-check: $*" >&2; exit 1; }

[ "${ATLAS_UBLK_LIVE:-}" = yes ] || die "set ATLAS_UBLK_LIVE=yes to create a real block device"
[ "$(uname -s)" = Linux ] || die "Linux only"
[ "$(id -u)" = 0 ] || die "needs root"
[ -c /dev/ublk-control ] || die "/dev/ublk-control is missing; load ublk_drv first"
for bin in "$NODE_BIN" "$UBLK_BIN"; do
  [ -x "$bin" ] || die "$bin not found or not executable"
done
for tool in curl cmp dd blockdev blkdiscard mkfs.ext4 e2fsck mount umount mountpoint sha256sum \
  find timeout readlink stat; do
  command -v "$tool" >/dev/null || die "$tool is not installed"
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/atlas-ublk-check.XXXXXX")
WORK=$(readlink -f "$WORK")
MNT="$WORK/mnt"
PIN="$WORK/pin"
mkdir "$MNT" "$PIN"
TOKEN=ublk-check-token
echo "$TOKEN" >"$WORK/token"
NODE_PIDS=()
UBLK_PID=""
WRITER_PID=""
WATCHDOG_PID=""
DEV=""
# The number of a device one of our servers served and that is not known to be gone: the only
# one teardown may remove.
OUR_ID=""
# Not a round size, so a device of another origin is unlikely to match it.
SIZE=$((268435456 + 4096 * (1 + $$ % 997)))

# Device nodes that existed before this run; none of them is ever opened.
BEFORE=" $(find /dev -maxdepth 1 -name 'ublkb*' 2>/dev/null | tr '\n' ' ') "

# A command on the device or its filesystem, with a limit so a wedged device fails the check.
limit() { timeout --kill-after=10 "$LIMIT" "$@"; }

# Waits up to $2 seconds for process $1 to end.
gone() {
  for _ in $(seq 1 $(($2 * 5))); do
    kill -0 "$1" 2>/dev/null || return 0
    sleep 0.2
  done
  ! kill -0 "$1" 2>/dev/null
}

# Stops our server with signal $1 (INT, TERM or HUP). The first stops the device, a second one
# fails requests still waiting for the cluster; SIGKILL is the last resort and fails the check.
stop_daemon() {
  [ -n "$UBLK_PID" ] || return 0
  local sig=${1:-INT} rc=0
  kill "-$sig" "$UBLK_PID" 2>/dev/null
  if ! gone "$UBLK_PID" 30; then
    echo "ublk-live-check: daemon still up 30 s after SIG$sig; signalling again" >&2
    kill "-$sig" "$UBLK_PID" 2>/dev/null
    if ! gone "$UBLK_PID" 30; then
      echo "ublk-live-check: killing daemon $UBLK_PID" >&2
      kill -KILL "$UBLK_PID" 2>/dev/null
      gone "$UBLK_PID" 30 || { echo "ublk-live-check: daemon $UBLK_PID survived SIGKILL" >&2; return 1; }
      rc=1
    fi
  fi
  local status=0
  wait "$UBLK_PID" || status=$?
  echo "daemon exit status after SIG$sig: $status"
  UBLK_PID=""
  : >"$WORK/ublk.pid"
  [ "$rc" = 0 ] && [ "$status" = 0 ]
}

# The device of a server that exited cleanly must be gone.
assert_removed() {
  [ -n "$OUR_ID" ] || die "no device to check"
  [ ! -e "/sys/class/ublk-char/ublkc$OUR_ID" ] || die "ublk device $OUR_ID still exists"
  [ ! -b "/dev/ublkb$OUR_ID" ] || die "/dev/ublkb$OUR_ID still exists"
  OUR_ID=""
}

detach() {
  stop_daemon "${1:-INT}" || die "the daemon did not stop cleanly: $(tail -n 5 "$WORK/ublk.log")"
  assert_removed
}

cleanup() {
  status=$?
  trap - EXIT
  trap '' INT TERM HUP
  set +e
  if [ -n "$WATCHDOG_PID" ]; then kill "$WATCHDOG_PID" 2>/dev/null; fi
  if [ -n "$WRITER_PID" ]; then kill "$WRITER_PID" 2>/dev/null; fi
  for m in "$MNT" "$PIN"; do
    if mountpoint -q "$m"; then
      timeout --kill-after=10 90 umount "$m" || umount -l "$m"
    fi
  done
  stop_daemon INT || status=1
  # Only if our server is gone and left its device behind.
  if [ -n "$OUR_ID" ] && [ -z "$UBLK_PID" ] && [ -e "/sys/class/ublk-char/ublkc$OUR_ID" ]; then
    echo "removing left-over ublk device $OUR_ID" >&2
    timeout --kill-after=10 60 "$UBLK_BIN" --delete-device "$OUR_ID" || status=1
  fi
  for p in "${NODE_PIDS[@]}"; do
    kill "$p" 2>/dev/null
    gone "$p" 20 || kill -KILL "$p" 2>/dev/null
  done
  if [ -n "$WRITER_PID" ]; then gone "$WRITER_PID" 30 || kill -KILL "$WRITER_PID" 2>/dev/null; fi
  if [ "$status" = 0 ]; then
    rm -rf --one-file-system "$WORK"
  else
    echo "ublk-live-check: failed; logs kept in $WORK" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# If the check overruns: make bash exit once its command returns, then kill our server, which
# fails the device's I/O and so releases a command stuck on it.
(
  sleep "$CHECK_TIMEOUT" &
  s=$!
  trap 'kill "$s" 2>/dev/null; exit 0' TERM
  wait "$s"
  echo "ublk-live-check: not finished after $CHECK_TIMEOUT s; giving up" >&2
  kill -TERM $$
  sleep 5
  if [ -s "$WORK/ublk.pid" ]; then kill -KILL "$(cat "$WORK/ublk.pid")"; fi
) &
WATCHDOG_PID=$!

# Refuses to go on unless $DEV is the device this run's live daemon created. Called before
# everything that opens the device for writing.
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
  OUR_ID=$id
}

node_url() { echo "http://127.0.0.1:$((PORT_BASE + 400 + $1))"; }
api() { curl -sS --fail --max-time 60 -H "Authorization: Bearer $TOKEN" "$@"; }

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
stop_node() {
  kill "-${2:-TERM}" "${NODE_PIDS[$1]}" 2>/dev/null || true
  gone "${NODE_PIDS[$1]}" 30 || die "node n$1 did not stop"
  wait "${NODE_PIDS[$1]}" 2>/dev/null || true
}
ready() {
  for _ in $(seq 1 200); do
    curl -sf --max-time 5 "$(node_url "$1")/readyz" >/dev/null && return
    sleep 0.1
  done
  die "node n$1 never became ready"
}

leader() {
  for i in 1 2 3; do
    if api "$(node_url "$i")/v1/status" 2>/dev/null | grep -q '"role":"leader"'; then
      echo "$i"
      return
    fi
  done
}
wait_leader() {
  L=""
  for _ in $(seq 1 100); do L=$(leader); [ -n "$L" ] && break; sleep 0.1; done
  [ -n "$L" ] || die "no leader"
}
start_cluster() {
  for i in 1 2 3; do start_node "$i"; done
  for i in 1 2 3; do ready "$i"; done
  wait_leader
}
stop_cluster() { for i in 1 2 3; do stop_node "$i"; done; }

start_cluster
VOL=ublk-check
api -X POST "$(node_url "$L")/v1/volumes" \
  -d "{\"id\":\"$VOL\",\"name\":\"$VOL\",\"size_bytes\":$SIZE}" >/dev/null

ENDPOINTS="$(node_url 1),$(node_url 2),$(node_url 3)"
start_daemon() {
  : >"$WORK/ublk.out"
  "$UBLK_BIN" --endpoint "$ENDPOINTS" --volume "$VOL" --token-file "$WORK/token" "$@" \
    >"$WORK/ublk.out" 2>>"$WORK/ublk.log" &
  UBLK_PID=$!
  echo "$UBLK_PID" >"$WORK/ublk.pid"
  DEV=""
  for _ in $(seq 1 600); do
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

step() { echo "== [${SECONDS}s] $*"; }

step "attach"
mount -t tmpfs -o size=1m,mode=700 tmpfs "$PIN"
start_daemon
id=${DEV#/dev/ublkb}
q="/sys/block/ublkb$id/queue"
assert_ours
[ "$(blockdev --getsize64 "$DEV")" = "$SIZE" ] || die "size mismatch"
[ "$(cat "$q/logical_block_size")" = 4096 ] || die "logical block size"
[ "$(cat "$q/write_cache")" = "write through" ] || die "write cache: $(cat "$q/write_cache")"
[ "$(cat "$q/discard_max_bytes")" = 0 ] || die "discard should be unsupported"
[ "$(cat "$q/write_zeroes_max_bytes")" -gt 0 ] || die "write zeroes should be supported"
# libublk's run directory, which the server writes to as root.
[ $((8#$(stat -c %a /run/ublksrvd) & 8#22)) = 0 ] || die "other users can write /run/ublksrvd"
for f in physical_block_size minimum_io_size optimal_io_size max_hw_sectors_kb \
  write_zeroes_max_bytes nr_requests; do
  echo "$f: $(cat "$q/$f")"
done

step "the daemon has its own mount namespace, and an unmount here reaches it"
[ "$(readlink "/proc/$UBLK_PID/ns/mnt")" != "$(readlink /proc/self/ns/mnt)" ] ||
  die "the daemon shares our mount namespace"
umount "$PIN"
if grep -qF " $PIN " "/proc/$UBLK_PID/mountinfo"; then
  die "the daemon keeps a filesystem mounted that was unmounted after it started"
fi

step "removing a live device is refused"
if timeout 30 "$UBLK_BIN" --delete-device "$OUR_ID" 2>"$WORK/refused"; then
  die "--delete-device removed a live device"
fi
cat "$WORK/refused"
assert_ours

step "random data, direct I/O, cross-checked through the HTTP API"
dd if=/dev/urandom of="$WORK/random" bs=1M count=32 status=none
assert_ours
limit dd if="$WORK/random" of="$DEV" bs=1M seek=3 oflag=direct conv=notrunc status=none
limit dd if="$DEV" of="$WORK/back" bs=1M skip=3 count=32 iflag=direct status=none
cmp "$WORK/random" "$WORK/back"
wait_leader
api -o "$WORK/api" "$(node_url "$L")/v1/volumes/$VOL/data?offset=$((3 * 1048576))&len=1048576"
cmp -n 1048576 "$WORK/random" "$WORK/api"
# Single blocks, not aligned to extents, and the last block of the device.
dd if="$WORK/random" of="$WORK/block" bs=4096 count=1 status=none
for at in 12345 $((SIZE / 4096 - 1)); do
  assert_ours
  limit dd if="$WORK/block" of="$DEV" bs=4096 seek="$at" oflag=direct conv=notrunc status=none
  limit dd if="$DEV" bs=4096 skip="$at" count=1 iflag=direct status=none | cmp - "$WORK/block"
done
assert_ours
if limit dd if="$WORK/block" of="$DEV" bs=4096 seek=$((SIZE / 4096)) count=1 oflag=direct \
  conv=notrunc status=none 2>/dev/null; then
  die "a write past the end of the device succeeded"
fi

step "write zeroes works, discard is refused"
assert_ours
limit blkdiscard --zeroout --offset $((3 * 1048576)) --length 1048576 "$DEV"
limit dd if="$DEV" bs=1M skip=3 count=1 iflag=direct status=none | cmp - <(head -c 1048576 /dev/zero)
assert_ours
if limit blkdiscard --offset 0 --length 1048576 "$DEV" 2>/dev/null; then die "discard succeeded"; fi

FIO_RAN=no
if [ "${SKIP_FIO:-0}" != 1 ] && command -v fio >/dev/null; then
  FIO_RAN=yes
  # Two jobs: small random writes, and four writers of mixed sizes in their own regions.
  fio_jobs() {
    assert_ours
    limit fio --output="$WORK/fio-4k.$1" --name=small --filename="$DEV" --direct=1 \
      --ioengine=libaio --rw=randwrite --bs=4k --iodepth=16 --size=16M --offset=64M \
      --verify=crc32c --verify_fatal=1 --verify_state_save=0 "${@:2}" ||
      die "fio (4 KiB) failed: $(tail -n 30 "$WORK/fio-4k.$1")"
    assert_ours
    limit fio --output="$WORK/fio-mixed.$1" --name=mixed --filename="$DEV" --direct=1 \
      --ioengine=libaio --rw=randwrite --bsrange=4k-1M --iodepth=8 --numjobs=4 --size=32M \
      --offset=96M --offset_increment=32M --group_reporting \
      --verify=crc32c --verify_fatal=1 --verify_state_save=0 "${@:2}" ||
      die "fio (mixed) failed: $(tail -n 30 "$WORK/fio-mixed.$1")"
    grep -hE '^(small|mixed):|err=| read: | write: |issued rwts' "$WORK/fio-4k.$1" "$WORK/fio-mixed.$1"
  }
  step "fio: write and verify (crc32c)"
  fio_jobs write --do_verify=1
  step "restart the server, fio: verify what was written before"
  detach TERM
  start_daemon
  fio_jobs reread --verify_only=1
  step "fio: the verification notices a damaged block"
  assert_ours
  limit dd if="$WORK/block" of="$DEV" bs=4096 seek=$((64 * 256 + 99)) count=1 oflag=direct \
    conv=notrunc status=none
  assert_ours
  if limit fio --output="$WORK/fio-damaged" --name=small --filename="$DEV" --direct=1 \
    --ioengine=libaio --rw=randwrite --bs=4k --iodepth=16 --size=16M --offset=64M \
    --verify=crc32c --verify_fatal=1 --verify_state_save=0 --verify_only=1 2>"$WORK/err"; then
    die "fio did not notice a damaged block"
  fi
  grep -hm 2 -E 'verify|err=' "$WORK/err" "$WORK/fio-damaged" | head -n 3
else
  echo "fio steps skipped"
fi

step "cluster unreachable: I/O fails after --io-timeout-secs, then works again"
detach HUP
start_daemon --io-timeout-secs 5 --settle-secs 1
stop_cluster
from=$SECONDS
assert_ours
if limit dd if="$DEV" bs=4096 skip=12345 count=1 iflag=direct status=none >/dev/null 2>"$WORK/err"; then
  die "a read succeeded without a cluster"
fi
echo "read failed after $((SECONDS - from)) s: $(head -n 1 "$WORK/err")"
[ $((SECONDS - from)) -le 30 ] || die "the read took too long to fail"
from=$SECONDS
assert_ours
if limit dd if="$WORK/block" of="$DEV" bs=4096 seek=12345 count=1 oflag=direct conv=notrunc \
  status=none 2>"$WORK/err"; then
  die "a write succeeded without a cluster"
fi
echo "write failed after $((SECONDS - from)) s: $(head -n 1 "$WORK/err")"
[ $((SECONDS - from)) -le 30 ] || die "the write took too long to fail"
start_cluster
assert_ours
limit dd if="$WORK/block" of="$DEV" bs=4096 seek=12345 count=1 oflag=direct conv=notrunc status=none
limit dd if="$DEV" bs=4096 skip=12345 count=1 iflag=direct status=none | cmp - "$WORK/block"
detach

step "stop with a request waiting for the cluster: the second signal fails it"
start_daemon --io-timeout-secs 0 --settle-secs 1
stop_cluster
assert_ours
dd if="$WORK/block" of="$DEV" bs=4096 seek=12345 count=1 oflag=direct conv=notrunc status=none \
  2>"$WORK/err" &
WRITER_PID=$!
sleep 3
kill -0 "$WRITER_PID" 2>/dev/null || die "with no I/O timeout the write should be waiting"
kill -INT "$UBLK_PID"
sleep 2
kill -0 "$UBLK_PID" 2>/dev/null || die "the first signal should let the request wait"
kill -0 "$WRITER_PID" 2>/dev/null || die "the first signal failed the request"
ticks() { awk '{sub(/.*\) /, ""); print $12 + $13}' "/proc/$UBLK_PID/stat"; }
t0=$(ticks)
sleep 5
used=$(($(ticks) - t0))
hz=$(getconf CLK_TCK)
echo "daemon CPU while stopping and waiting: $((used * 100 / (5 * hz)))% of one CPU"
[ $((used * 100 / (5 * hz))) -lt 50 ] || die "the daemon spins while it waits to stop"
stop_daemon INT || die "the daemon did not stop cleanly: $(tail -n 5 "$WORK/ublk.log")"
assert_removed
gone "$WRITER_PID" 30 || die "the waiting write never returned"
if wait "$WRITER_PID"; then die "the abandoned write reported success"; fi
echo "waiting write failed: $(head -n 1 "$WORK/err")"
WRITER_PID=""
start_cluster

step "ext4: copy files, fail over the leader during writes, unmount, check"
start_daemon
assert_ours
limit mkfs.ext4 -q -F -E nodiscard "$DEV"
assert_ours
limit mount "$DEV" "$MNT"
limit cp "$WORK/random" "$MNT/before"
limit sync -f "$MNT"
wait_leader
(
  for n in $(seq 1 16); do
    limit dd if="$WORK/random" of="$MNT/during-$n" bs=1M count=8 skip=$((n % 4 * 8)) conv=fsync \
      status=none || exit 1
  done
) &
WRITER_PID=$!
for _ in $(seq 1 600); do [ -e "$MNT/during-3" ] && break; sleep 0.1; done
[ -e "$MNT/during-3" ] || die "the writer made no progress"
wait_leader
failed=$L
stop_node "$failed" KILL
echo "killed leader n$failed"
kill -0 "$WRITER_PID" 2>/dev/null || die "the writer finished before the leader was killed"
wait "$WRITER_PID" || die "writes failed during leader failover"
WRITER_PID=""
start_node "$failed"
ready "$failed"
limit sync -f "$MNT"
(cd "$MNT" && limit sha256sum before during-*) >"$WORK/sums"
# The expected contents, not only what the device returns.
cmp "$WORK/random" "$MNT/before"
for n in $(seq 1 16); do
  dd if="$WORK/random" bs=1M count=8 skip=$((n % 4 * 8)) status=none | cmp - "$MNT/during-$n"
done
limit umount "$MNT"
assert_ours
limit e2fsck -fn "$DEV"

step "detach, reattach, verify"
detach
start_daemon
assert_ours
limit mount -o ro,noload "$DEV" "$MNT"
(cd "$MNT" && limit sha256sum -c --quiet "$WORK/sums")

step "stop the server while its device is mounted, then unmount"
kill -INT "$UBLK_PID"
if gone "$UBLK_PID" 10; then
  echo "the server exited while the filesystem was still mounted"
else
  echo "the server is still up 10 s after SIGINT, with the filesystem mounted"
fi
timeout --kill-after=10 90 umount "$MNT" || die "unmounting a stopped device failed"
gone "$UBLK_PID" 30 || die "the server did not exit after the unmount"
status=0
wait "$UBLK_PID" || status=$?
echo "daemon exit status: $status"
UBLK_PID=""
: >"$WORK/ublk.pid"
[ "$status" = 0 ] || die "the daemon failed to stop: $(tail -n 5 "$WORK/ublk.log")"
assert_removed
start_daemon

step "crash: SIGKILL the server during writes to a mounted filesystem"
assert_ours
limit mount "$DEV" "$MNT"
rm "$MNT"/during-*
limit sync -f "$MNT"
(cd "$MNT" && limit sha256sum before) >"$WORK/sums"
: >"$WORK/acked"
(
  for n in $(seq 1 40); do
    part=$((n % 8 * 4))
    sum=$(dd if="$WORK/random" bs=1M count=4 skip="$part" status=none | sha256sum)
    limit dd if="$WORK/random" of="$MNT/crash-$n" bs=1M count=4 skip="$part" conv=fsync \
      status=none 2>>"$WORK/crash-writer.err" || exit 1
    # Listed only once fsync has returned.
    echo "${sum%% *}  crash-$n" >>"$WORK/acked"
  done
) &
WRITER_PID=$!
for _ in $(seq 1 600); do [ "$(wc -l <"$WORK/acked")" -ge 6 ] && break; sleep 0.1; done
[ "$(wc -l <"$WORK/acked")" -ge 6 ] || die "the writer made no progress"
sleep 0.3
# The mount made here propagates to the server's namespace: it dies holding its own filesystem.
grep -qF " $MNT " "/proc/$UBLK_PID/mountinfo" || die "the server does not see the mount"
kill -0 "$WRITER_PID" 2>/dev/null || die "the writer finished before the server was killed"
kill -KILL "$UBLK_PID"
from=$SECONDS
gone "$UBLK_PID" 30 || die "the killed server did not exit"
wait "$UBLK_PID" 2>/dev/null || true
UBLK_PID=""
: >"$WORK/ublk.pid"
gone "$WRITER_PID" 90 || die "writes hang after the server died"
if wait "$WRITER_PID"; then die "writes succeeded after the server died"; fi
WRITER_PID=""
echo "writer failed $((SECONDS - from)) s after the kill, with $(wc -l <"$WORK/acked") files acknowledged"
if timeout --kill-after=10 60 dd if="/dev/ublkb$OUR_ID" of=/dev/null bs=4096 count=1 \
  iflag=direct status=none 2>/dev/null; then
  die "a read from the dead server's device succeeded"
fi
timeout --kill-after=10 90 umount "$MNT" || die "unmounting after the server died failed"
echo "left behind: $(cd /sys/class/ublk-char && ls -d "ublkc$OUR_ID" 2>/dev/null || echo 'no control node'), \
$([ -b "/dev/ublkb$OUR_ID" ] && echo "/dev/ublkb$OUR_ID" || echo 'no block device')"
[ -e "/sys/class/ublk-char/ublkc$OUR_ID" ] || die "expected the kernel to keep the dead device"
timeout --kill-after=10 60 "$UBLK_BIN" --delete-device "$OUR_ID" || die "removing the dead device failed"
assert_removed
# Writes the dead server had in flight may still be running (docs: late writes).
sleep $((SETTLE + 1))
start_daemon
assert_ours
rc=0
limit e2fsck -p "$DEV" || rc=$?
[ "$rc" -le 1 ] || die "e2fsck -p after the crash: exit status $rc"
assert_ours
limit e2fsck -fn "$DEV"
assert_ours
limit mount -o ro,noload "$DEV" "$MNT"
(cd "$MNT" && limit sha256sum -c --quiet "$WORK/sums")
(cd "$MNT" && limit sha256sum -c --quiet "$WORK/acked")
echo "all $(wc -l <"$WORK/acked") acknowledged files and the earlier one are intact"
limit umount "$MNT"
detach

step "read-only attach refuses writes"
start_daemon --read-only
assert_ours
[ "$(blockdev --getro "$DEV")" = 1 ] || die "device is not read-only"
[ "$(cat "/sys/block/ublkb$OUR_ID/queue/write_zeroes_max_bytes")" = 0 ] || die "write zeroes offered"
assert_ours
if limit dd if="$WORK/block" of="$DEV" bs=4096 count=1 oflag=direct status=none 2>/dev/null; then
  die "a write to a read-only device succeeded"
fi
detach

step "512-byte blocks"
start_daemon --block-size 512 --queues 1 --queue-depth 4 --io-bytes 65536
assert_ours
[ "$(cat "/sys/block/ublkb$OUR_ID/queue/logical_block_size")" = 512 ] || die "logical block size"
limit dd if="$WORK/random" of="$DEV" bs=512 seek=200001 count=1001 oflag=direct conv=notrunc status=none
limit dd if="$DEV" bs=512 skip=200001 count=1001 iflag=direct status=none |
  cmp - <(head -c $((1001 * 512)) "$WORK/random")
detach

echo "ublk-live-check: ok (fio steps run: $FIO_RAN)"
