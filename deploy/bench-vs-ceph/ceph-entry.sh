#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Entry point of atlas-bench-ceph (runs inside the container only): one mon, one mgr, three BlueStore
# OSDs on regular files, one MDS, one CephFS mounted with ceph-fuse at /mnt/bench. The first start
# builds the cluster under /srv/bench/ceph; later starts reuse it. Any daemon exiting ends the
# container.
set -euo pipefail

[ -f /run/.containerenv ] || { echo "ceph-entry.sh only runs inside the benchmark container" >&2; exit 2; }

D=/srv/bench/ceph
MNT=/mnt/bench
READY=/run/ceph-bench.ready
OSD_GB="${CEPH_OSD_GB:-40}"
PREFILL="${CEPH_PREFILL:-1}"
OSD_IDS=(0 1 2)
PIDS=()
export CEPH_USE_RANDOM_NONCE=1

log() { echo "$(date +%T) $*"; }

# retry <seconds> <command...>: until the command succeeds.
retry() {
  local deadline=$((SECONDS + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      log "timed out waiting for: $*"
      return 1
    fi
    alive || return 1
    sleep 1
  done
}

alive() {
  local p
  for p in "${PIDS[@]}"; do
    kill -0 "$p" 2>/dev/null || { log "a Ceph process exited (pid $p); see $D/log"; return 1; }
  done
}

start() {
  "$@" &
  PIDS+=($!)
}

stop() {
  trap - TERM INT
  log "stopping"
  rm -f "$READY"
  if mountpoint -q "$MNT"; then umount "$MNT" || umount -l "$MNT" || true; fi
  # Reverse start order: client, MDS, OSDs, mgr, mon.
  local i
  for ((i = ${#PIDS[@]} - 1; i >= 0; i--)); do
    kill -TERM "${PIDS[i]}" 2>/dev/null || continue
    timeout 10 tail --pid="${PIDS[i]}" -f /dev/null || kill -KILL "${PIDS[i]}" 2>/dev/null || true
  done
  exit 0
}
trap stop TERM INT

write_conf() {
  cat >"$D/etc/ceph.conf" <<EOF
[global]
fsid = $1
mon_initial_members = a
mon_host = [v2:127.0.0.1:3300/0]
public_network = 127.0.0.0/8
cluster_network = 127.0.0.0/8
ms_bind_msgr1 = false
run_dir = /run/ceph
log_file = $D/log/\$cluster-\$name.log

# Three OSDs on one host: three copies, one OSD may be lost and writes continue.
osd_pool_default_size = 3
osd_pool_default_min_size = 2
osd_crush_chooseleaf_type = 0
# Fixed PG counts, so nothing splits or moves while a benchmark runs.
osd_pool_default_pg_autoscale_mode = off

[mon]
mon_data = $D/mon/\$id
mon_memory_target = 1073741824
auth_allow_insecure_global_id_reclaim = false
# The mon store shares the host disk with other people's data; the default (30% free) is about that
# disk filling up, not about this cluster.
mon_data_avail_warn = 15

[mgr]
mgr_data = $D/mgr/\$id

[mds]
mds_data = $D/mds/\$id
mds_cache_memory_limit = 1073741824

[osd]
osd_data = $D/osd/ceph-\$id
bluestore_block_size = $((OSD_GB * 1073741824))
# 3 x 2.5 GiB inside the 12 GiB container limit that mon, mgr, MDS, ceph-fuse and fio also share.
osd_memory_target = 2684354560
# Otherwise each OSD benchmarks the shared disk at its first start and stores whatever it measured under
# that moment's load. Skipping it gives all three the built-in HDD capacity on every run; client I/O
# has no mClock limit either way.
osd_mclock_skip_benchmark = true
EOF
}

bootstrap_mon() {
  log "creating a new cluster in $D"
  find "$D" -mindepth 1 -delete
  mkdir -p "$D"/{etc,log,mon,mgr/x,mds/a,osd}
  local fsid
  fsid="$(uuidgen)"
  write_conf "$fsid"
  cp "$D/etc/ceph.conf" /etc/ceph/ceph.conf
  ceph-authtool --create-keyring "$D/etc/mon.keyring" --gen-key -n mon. --cap mon 'allow *'
  ceph-authtool --create-keyring "$D/etc/ceph.client.admin.keyring" --gen-key -n client.admin \
    --cap mon 'allow *' --cap osd 'allow *' --cap mds 'allow *' --cap mgr 'allow *'
  ceph-authtool "$D/etc/mon.keyring" --import-keyring "$D/etc/ceph.client.admin.keyring"
  monmaptool --create --fsid "$fsid" --addv a '[v2:127.0.0.1:3300]' --enable-all-features "$D/etc/monmap"
  ceph-mon --mkfs -i a --monmap "$D/etc/monmap" --keyring "$D/etc/mon.keyring"
}

create_osd() {
  local id=$1 dir=$D/osd/ceph-$1 uuid secret got
  uuid="$(uuidgen)"
  secret="$(ceph-authtool --gen-print-key)"
  mkdir -p "$dir"
  got="$(printf '{"cephx_secret": "%s"}' "$secret" | ceph osd new "$uuid" -i -)"
  [ "$got" = "$id" ] || { log "expected osd id $id, got $got"; return 1; }
  ceph-authtool --create-keyring "$dir/keyring" --name "osd.$id" --add-key "$secret" >/dev/null
  if [ "$PREFILL" = 1 ]; then
    # Written zeros, not fallocate: no unwritten-extent conversion in the host filesystem on first
    # write, so the file behaves as closely to a raw device as a file can.
    log "osd.$id: writing $OSD_GB GiB block file"
    dd if=/dev/zero of="$dir/block" bs=16M count=$((OSD_GB * 64)) oflag=direct conv=fsync status=none
  elif [ "$PREFILL" = sparse ]; then
    # On tmpfs: only what is written takes memory.
    truncate -s "${OSD_GB}G" "$dir/block"
  else
    fallocate -l "${OSD_GB}G" "$dir/block"
  fi
  ceph mon getmap -o "$dir/activate.monmap"
  ceph-osd -i "$id" --mkfs --osd-uuid "$uuid" --monmap "$dir/activate.monmap"
}

three_osds_up() { ceph osd stat -f json | jq -e '.num_up_osds == 3 and .num_in_osds == 3'; }
mds_active() { ceph fs status bench -f json | jq -e 'any(.mdsmap[]; .state == "active")'; }

mkdir -p /etc/ceph /run/ceph "$MNT" "$D"
rm -f "$READY"
FRESH=0
if [ ! -f "$D/bootstrapped" ]; then
  FRESH=1
  bootstrap_mon
else
  # Same cluster, but the settings of this version of the script.
  write_conf "$(ceph-conf -c "$D/etc/ceph.conf" --lookup fsid)"
fi
cp "$D/etc/ceph.conf" "$D/etc/ceph.client.admin.keyring" /etc/ceph/

start ceph-mon -f -i a
retry 60 timeout 10 ceph mon stat

if [ "$FRESH" = 1 ]; then
  ceph auth get-or-create mgr.x mon 'allow profile mgr' osd 'allow *' mds 'allow *' -o "$D/mgr/x/keyring"
fi
start ceph-mgr -f -i x

if [ "$FRESH" = 1 ]; then
  for id in "${OSD_IDS[@]}"; do create_osd "$id"; done
fi
for id in "${OSD_IDS[@]}"; do start ceph-osd -f -i "$id"; done
retry 180 three_osds_up

if [ "$FRESH" = 1 ]; then
  ceph osd require-osd-release tentacle
  retry 60 ceph balancer off
  ceph osd pool create cephfs_metadata 32 32 replicated
  ceph osd pool create cephfs_data 128 128 replicated
  ceph fs new bench cephfs_metadata cephfs_data
  ceph fs set bench standby_count_wanted 0
  ceph auth get-or-create mds.a mon 'allow profile mds' mgr 'allow profile mds' mds 'allow *' \
    osd 'allow *' -o "$D/mds/a/keyring"
fi
start ceph-mds -f -i a
retry 120 mds_active

# Multithreaded is the ceph-fuse default. libfuse keeps only 10 idle workers by default and would
# create and destroy threads at queue depth 32; clone_fd gives each worker its own /dev/fuse queue.
start ceph-fuse -f -n client.admin --client_fs bench -o max_idle_threads=64 -o clone_fd "$MNT"
retry 60 mountpoint -q "$MNT"

touch "$D/bootstrapped" "$READY"
log "ready: CephFS mounted at $MNT"
wait -n "${PIDS[@]}" || true
log "a Ceph process exited; see $D/log"
exit 1
