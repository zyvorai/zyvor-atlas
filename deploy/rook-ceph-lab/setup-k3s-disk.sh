#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# setup-k3s-disk.sh — carve /dev/sdb2 from the free tail of the OSD disk and relocate the
# k3s data-dir (containerd image store + local-path PV data + datastore) onto it.
#
# Pairs with resize-osd.sh: that caps Ceph to /dev/sdb1 (default 400 GiB) and leaves the rest
# of /dev/sdb (~531 GiB on the 931 GiB lab disk) FREE. Without this, k3s keeps piling its
# containerd images + local-path volumes onto the small root FS (/dev/sda2) until it fills.
# This script turns that free tail into /dev/sdb2, formats it, and moves /var/lib/rancher there
# so the k3s storage load lands on the big disk.
#
# It creates ONLY a new partition in unallocated space and never touches /dev/sdb1 — the Ceph
# OSD and all Ceph data are left intact. It is therefore safe to run on a live Ceph cluster.
#
# DESTRUCTIVE to /dev/sdb2 only (it is created + formatted). k3s is stopped during the data
# move and restarted afterwards; the pre-move copy is kept at /var/lib/rancher.pre-sdb2 until
# you delete it.
#
#   ./setup-k3s-disk.sh --confirm      # create /dev/sdb2, move k3s data-dir onto it
#   ./setup-k3s-disk.sh                # dry-run: print the plan, change nothing
#
# Env overrides:
#   CEPH_DEVICE=sdb        the shared OSD disk (partition 2 is carved from its free tail)
#   RANCHER_DIR=/var/lib/rancher   the k3s data-dir root that gets relocated
#   K3S_SERVICE=k3s        systemd unit to stop/start around the move (k3s or k3s-agent)
#   FS=ext4                filesystem to lay down on the new partition
#
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEV="${CEPH_DEVICE:-sdb}"
PART="${DEV}2"
RANCHER_DIR="${RANCHER_DIR:-/var/lib/rancher}"
K3S_SERVICE="${K3S_SERVICE:-k3s}"
FS="${FS:-ext4}"
STAGE="/mnt/k3s-newdisk"
BACKUP="${RANCHER_DIR}.pre-sdb2"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!! \033[0m %s\n' "$*"; }

if [ "${1:-}" != "--confirm" ]; then
  cat <<EOF
DRY-RUN. This would put the k3s storage load on the big disk (nothing changes yet):
  1. create /dev/$PART from the FREE tail of /dev/$DEV (after the ${DEV}1 Ceph partition),
     using all remaining space; /dev/${DEV}1 and the Ceph OSD are left untouched
  2. mkfs.$FS /dev/$PART  (label: k3s-data)
  3. systemctl stop $K3S_SERVICE            # release open files under $RANCHER_DIR
  4. rsync $RANCHER_DIR/ -> /dev/$PART, then mount /dev/$PART at $RANCHER_DIR (fstab, by UUID)
     (old copy kept at $BACKUP; delete it once you've verified)
  5. systemctl start $K3S_SERVICE  and verify the node + free space on $RANCHER_DIR
Re-run with:  ./setup-k3s-disk.sh --confirm
EOF
  exit 0
fi

require() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
require sgdisk; require partprobe; require rsync; require blkid; require lsblk

[ -b "/dev/$DEV" ]   || { echo "no such disk: /dev/$DEV"; exit 1; }
[ -b "/dev/${DEV}1" ] || { echo "/dev/${DEV}1 not found — run ./resize-osd.sh first to carve the Ceph partition and free the rest of /dev/$DEV"; exit 1; }

if [ -b "/dev/$PART" ]; then
  warn "/dev/$PART already exists — skipping partition creation; will (re)move k3s data onto it"
else
  say "1/5 create /dev/$PART from the free tail of /dev/$DEV (Ceph /dev/${DEV}1 untouched)"
  sudo bash -c '
    set -e
    # partition 2 = next aligned free sector after existing partitions .. end of disk.
    sgdisk -n 2:0:0 -t 2:8300 -c 2:k3s-data "/dev/'"$DEV"'"
    partprobe "/dev/'"$DEV"'"
    sleep 2
    lsblk -o NAME,SIZE,TYPE,MOUNTPOINT "/dev/'"$DEV"'"
  '
  [ -b "/dev/$PART" ] || { echo "kernel did not surface /dev/$PART — check 'sgdisk -p /dev/$DEV' (is there free space?)"; exit 1; }

  say "2/5 mkfs.$FS on /dev/$PART"
  sudo "mkfs.$FS" -F -L k3s-data "/dev/$PART"
fi

if mountpoint -q "$RANCHER_DIR" 2>/dev/null; then
  warn "$RANCHER_DIR is already a mountpoint — assuming k3s data already lives on a dedicated FS; nothing to move"
  exit 0
fi

UUID="$(sudo blkid -s UUID -o value "/dev/$PART")"
[ -n "$UUID" ] || { echo "could not read UUID of /dev/$PART"; exit 1; }

say "3/5 stop $K3S_SERVICE to release open files under $RANCHER_DIR"
if ! sudo systemctl stop "$K3S_SERVICE" 2>/dev/null; then
  if [ -d "$RANCHER_DIR" ]; then
    echo "could not stop $K3S_SERVICE and $RANCHER_DIR exists — refusing to rsync/move a directory a live service may still be writing to" >&2
    exit 1
  fi
  warn "could not stop $K3S_SERVICE (not installed yet?) — $RANCHER_DIR doesn't exist yet, continuing"
fi

say "4/5 move $RANCHER_DIR onto /dev/$PART and mount it there (UUID=$UUID)"
sudo bash -c '
  set -e
  DEV_PART="/dev/'"$PART"'"; RD="'"$RANCHER_DIR"'"; ST="'"$STAGE"'"; BK="'"$BACKUP"'"; UU="'"$UUID"'"; FSTYPE="'"$FS"'"
  mkdir -p "$ST"
  mount "$DEV_PART" "$ST"
  if [ -d "$RD" ]; then
    rsync -aHAX --numeric-ids "$RD"/ "$ST"/
  else
    mkdir -p "$RD"
  fi
  umount "$ST"; rmdir "$ST" 2>/dev/null || true
  # keep the old tree as a backup, then mount the new FS at the data-dir path
  if [ -d "$RD" ] && [ ! -e "$BK" ]; then mv "$RD" "$BK"; fi
  mkdir -p "$RD"
  # persist the mount (idempotent: drop any stale line for this path first)
  sed -i "\| $RD |d" /etc/fstab
  printf "UUID=%s %s %s defaults,noatime 0 2\n" "$UU" "$RD" "$FSTYPE" >> /etc/fstab
  systemctl daemon-reload 2>/dev/null || true
  mount "$RD"
  echo "mounted:"; findmnt "$RD" || true
'

say "5/5 restart $K3S_SERVICE and verify"
sudo systemctl start "$K3S_SERVICE" 2>/dev/null || warn "could not start $K3S_SERVICE — start it manually once you've confirmed the mount"
echo
df -h "$RANCHER_DIR" || true
echo
if command -v kubectl >/dev/null 2>&1; then
  kubectl get nodes -o wide 2>/dev/null || warn "kubectl not ready yet — give k3s a moment"
fi
cat <<EOF

Done. The k3s data-dir now lives on /dev/$PART (the free tail of /dev/$DEV); Ceph on /dev/${DEV}1
is untouched. The pre-move copy is at $BACKUP — once 'df -h $RANCHER_DIR' and 'kubectl get nodes'
look right, reclaim it with:
  sudo rm -rf $BACKUP
EOF
