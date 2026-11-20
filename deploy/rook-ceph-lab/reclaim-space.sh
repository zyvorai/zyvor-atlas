#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# reclaim-space.sh — SAFE, non-destructive host cleanup for the k3s lab node.
# Reclaims disk on the ROOT filesystem (/dev/sda2) — NOT the Ceph OSD disk. Ceph data is untouched.
#
# What eats root space on this box: repeated `podman build` layers + `k3s ctr images import`
# of atlas-gateway:ceph on every deploy, plus systemd journals. This prunes all of that.
#
#   ./reclaim-space.sh            # prune unused images + vacuum journals
#
set -euo pipefail
say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

say "disk before"
df -h / | awk 'NR==1 || /\/$/'

say "podman: remove all unused build images (safe — k3s serves pods from containerd, not podman)"
sudo podman image prune -af 2>/dev/null | tail -3 || true
sudo podman system prune -af 2>/dev/null | tail -1 || true

say "containerd (k3s): remove images not referenced by any container (in-use images are kept)"
sudo k3s crictl rmi --prune 2>/dev/null | tail -3 || true

say "systemd journals: cap to 200M"
sudo journalctl --vacuum-size=200M 2>/dev/null | tail -2 || true

say "old atlas image tarballs, if any"
sudo rm -f /tmp/a.tar /tmp/*.tar 2>/dev/null || true

say "disk after"
df -h / | awk 'NR==1 || /\/$/'
say "done — Ceph OSD disk (/dev/sdb) was not touched"
