#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Runs crates/atlas-native/tests/writeback_errors.rs against real kernel writeback errors: a
# device-mapper device over a loop device over a new sparse file, with an ext4 filesystem on it,
# whose table is changed on request so that writes to chosen blocks fail.
#
# usage: ATLAS_SYNCFIX_REAL_DM=1 writeback-error-lab.sh <work-dir> <test-binary> [test args]
#
#   cargo test -p atlas-native --test writeback_errors --no-run   # prints the binary's path
#
# The devices are printed before anything is written to them.
#
# Both paths must be under /data/atlas-bench. Run as an ordinary user with passwordless sudo:
# sudo is used only for losetup, blockdev, dmsetup, mkfs.ext4, mount and umount of the objects
# this run creates (every call is logged to <work-dir>/dm-sudo.log). The test binary runs as
# the invoking user. Everything is torn down on exit.

set -euo pipefail

NAME=atlas-bench-syncfix
SIZE=1G
ROOT=/data/atlas-bench

die() {
    echo "writeback-error-lab: $*" >&2
    exit 2
}

[ "${ATLAS_SYNCFIX_REAL_DM:-}" = 1 ] || die "set ATLAS_SYNCFIX_REAL_DM=1 to create devices"
[ $# -ge 2 ] || die "usage: $0 <work-dir> <test-binary> [test args]"
[ "$(id -u)" != 0 ] || die "run as an ordinary user, not root"
work=$(realpath -e "$1")
bin=$(realpath -e "$2")
shift 2
for p in "$work" "$bin"; do
    case "$p" in
    "$ROOT"/*) ;;
    *) die "$p is not under $ROOT" ;;
    esac
done
case "$work" in
*[[:space:]]*) die "the work directory must not contain spaces" ;;
esac
[ -d "$work" ] && [ -x "$bin" ] || die "need a directory and an executable"
sudo -n true 2>/dev/null || die "SUDO_UNAVAILABLE: sudo needs a password"

img=$work/dm.img
mnt=$work/dm-mnt
ctl=$work/dm-ctl
log=$work/dm-sudo.log
loop=
created=
mounted=
sectors=
pid=

as_root() {
    echo "sudo -n $*" >>"$log"
    timeout 120 sudo -n "$@"
}

dm_exists() {
    as_root dmsetup info "$NAME" >/dev/null 2>&1
}

: >"$log"
[ ! -e "$img" ] && [ ! -L "$img" ] ||
    die "$img exists: remove it after checking that no loop device uses it"
# A link here would make truncate write, or mount cover, a path outside the work directory.
[ ! -L "$mnt" ] || die "$mnt is a symbolic link"
[ ! -e "$mnt" ] || [ -z "$(ls -A "$mnt")" ] || die "$mnt is not an empty directory"
! dm_exists || die "a device-mapper device named $NAME already exists"
! findmnt -rn "$mnt" >/dev/null || die "$mnt is already a mount point"

cleanup() {
    local status=$?
    set +e
    trap - EXIT INT TERM HUP
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid"
        wait "$pid" 2>/dev/null
    fi
    if [ -n "$created" ]; then
        # Never leave the device suspended: everything below would hang.
        as_root dmsetup resume "$NAME" >/dev/null 2>&1
        [ -n "$sectors" ] && load_table "0 $sectors linear $loop 0"
    fi
    if [ -n "$mounted" ]; then
        as_root umount "$mnt" || { sleep 2; as_root umount "$mnt"; }
    fi
    if [ -n "$created" ]; then
        as_root dmsetup remove --retry "$NAME"
    fi
    if [ -n "$loop" ] && as_root losetup -j "$img" | grep -q "^$loop:"; then
        as_root losetup -d "$loop"
    fi
    left=0
    if dm_exists; then
        echo "LEFT BEHIND: device-mapper device $NAME"
        left=1
    fi
    if [ -n "$(as_root losetup -j "$img")" ]; then
        echo "LEFT BEHIND: a loop device on $img"
        left=1
    fi
    if findmnt -rn "$mnt" >/dev/null; then
        echo "LEFT BEHIND: mount $mnt"
        left=1
    fi
    if [ "$left" = 0 ]; then
        rm -f "$img"
        rmdir "$mnt" 2>/dev/null
    fi
    echo "DM_TEARDOWN_EXIT=$left"
    [ "$status" = 0 ] && status=$left
    exit "$status"
}

# Replaces the device's table. Loading while suspended and resuming at once; no filesystem
# freeze, which would write through the failing table.
load_table() {
    printf '%s\n' "$1" >"$ctl/table"
    as_root dmsetup suspend --nolockfs "$NAME" || return 1
    local ok=0
    as_root dmsetup load "$NAME" "$ctl/table" || ok=1
    as_root dmsetup resume "$NAME" || ok=1
    return $ok
}

# Prints "start count" (in sectors) for the blocks of file $1 that bytes [$2, $2+$3) occupy;
# the whole file without a range.
bad_sectors() {
    local file=$1 first=0 last=999999999
    if [ $# -eq 3 ]; then
        first=$(($2 / 4096))
        last=$((($2 + $3 - 1) / 4096))
    fi
    filefrag -b4096 -v "$file" | awk -v first="$first" -v last="$last" '
        /^ *[0-9]+:/ {
            gsub(/[:.]+/, " ")
            lo = $2; hi = $3; phys = $4
            from = lo > first ? lo : first
            to = hi < last ? hi : last
            if (from <= to) printf "%d %d\n", (phys + from - lo) * 8, (to - from + 1) * 8
        }' | sort -n
}

fail_table() {
    local pos=0 start count table=
    while read -r start count; do
        [ "$start" -ge "$pos" ] || return 1
        [ "$start" -gt "$pos" ] && table+="$pos $((start - pos)) linear $loop $pos"$'\n'
        table+="$start $count error"$'\n'
        pos=$((start + count))
    done <<<"$1"
    [ "$pos" -gt 0 ] || return 1
    [ "$pos" -lt "$sectors" ] && table+="$pos $((sectors - pos)) linear $loop $pos"$'\n'
    printf '%s' "$table"
}

serve() {
    local verb file offset len ranges table
    read -r verb file offset len <"$ctl/request"
    rm -f "$ctl/request"
    echo "request: $verb ${file:-} ${offset:-} ${len:-}" >>"$log"
    case "$verb" in
    heal)
        load_table "0 $sectors linear $loop 0" || return 1
        ;;
    fail | fail-file)
        case "$(realpath -e "$file")" in
        "$mnt"/*) ;;
        *) return 1 ;;
        esac
        if [ "$verb" = fail ]; then
            ranges=$(bad_sectors "$file" "$offset" "$len")
        else
            ranges=$(bad_sectors "$file")
        fi
        [ -n "$ranges" ] || return 1
        table=$(fail_table "$ranges") || return 1
        load_table "$table" || return 1
        ;;
    *) return 1 ;;
    esac
}

rm -rf "$ctl"
mkdir -p "$ctl" "$mnt"
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

truncate -s "$SIZE" "$img"
loop=$(as_root losetup --find --show "$img")
as_root losetup -j "$img" | grep -q "^$loop:" || die "$loop is not attached to $img"
sectors=$(as_root blockdev --getsz "$loop")
as_root dmsetup create "$NAME" --table "0 $sectors linear $loop 0"
created=1
dev=/dev/mapper/$NAME
# The mapping must lead to the loop device just made, and nowhere else.
numbers=$(printf '%d:%d' "0x$(stat -c %t "$loop")" "0x$(stat -c %T "$loop")")
[ "$(as_root dmsetup table "$NAME")" = "0 $sectors linear $numbers 0" ] ||
    die "unexpected table for $NAME"
as_root losetup -j "$img" | grep -q "^$loop:" || die "$loop is not attached to $img"
echo "image: $img"
echo "loop:  $(as_root losetup -j "$img")"
echo "dm:    $dev -> $(readlink -f "$dev"): $(as_root dmsetup table "$NAME")"
as_root mkfs.ext4 -q -E "root_owner=$(id -u):$(id -g),lazy_itable_init=0,lazy_journal_init=0" "$dev"
as_root mount -t ext4 "$dev" "$mnt"
mounted=1
[ "$(findmnt -rno SOURCE "$mnt")" = "$dev" ] || die "$mnt is not $dev"
echo "device $dev on $loop on $img, mounted on $mnt"

data=$mnt/t
mkdir "$data"
ATLAS_WB_DIR=$data ATLAS_WB_CTL=$ctl timeout 900 "$bin" --ignored --test-threads=1 \
    --nocapture --skip verify_after_remount "$@" >"$work/dm-test.log" 2>&1 &
pid=$!
while kill -0 "$pid" 2>/dev/null; do
    if [ -f "$ctl/request" ]; then
        if serve; then
            echo ok >"$ctl/reply.tmp"
        else
            echo failed >"$ctl/reply.tmp"
        fi
        mv "$ctl/reply.tmp" "$ctl/reply"
    fi
    sleep 0.02
done
tests=0
wait "$pid" || tests=$?
cat "$work/dm-test.log"
echo "DM_TEST_EXIT=$tests"

# Check the lost ranges again from a freshly mounted filesystem.
load_table "0 $sectors linear $loop 0"
as_root umount "$mnt"
mounted=
as_root mount -t ext4 "$dev" "$mnt"
mounted=1
verify=0
ATLAS_WB_DIR=$data ATLAS_WB_CTL=$ctl timeout 120 "$bin" --ignored --exact verify_after_remount \
    --nocapture || verify=$?
echo "DM_VERIFY_EXIT=$verify"
[ "$tests" = 0 ] && [ "$verify" = 0 ]
