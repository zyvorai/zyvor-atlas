#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# teardown.sh — DESTRUCTIVE. Completely uninstall Rook Ceph and wipe its OSD disk.
# Reverses up.sh. DELETES ALL CEPH DATA (RBD volumes, CephFS, RGW buckets).
#
# Follows the official Rook cleanup flow: set the CephCluster cleanupPolicy so Rook's own job
# zaps the disk, delete the CRs, then remove the operator/common/CRDs, namespace, host dir, and
# finally belt-and-suspenders wipe /dev/sdb.
#
#   ./teardown.sh --confirm        # actually do it
#   ./teardown.sh                  # dry-run: print the plan, change nothing
#
set -euo pipefail
NS=rook-ceph
DEV="${CEPH_DEVICE:-sdb}"
HOSTPATH="${ROOK_HOSTPATH:-/var/lib/rook}"
ROOK_VERSION="${ROOK_VERSION:-1.20.2}"
base="https://raw.githubusercontent.com/rook/rook/release-${ROOK_VERSION%.*}/deploy/examples"
say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!! \033[0m %s\n' "$*"; }

if [ "${1:-}" != "--confirm" ]; then
  cat <<EOF
DRY-RUN. This would PERMANENTLY DESTROY the Ceph cluster on this node:
  - uninstall ceph-csi-drivers Helm release (Rook 1.20+)
  - delete CephObjectStore / CephFilesystem / CephBlockPool / CephCluster
  - delete the zyvor-* StorageClasses (any bound PVCs become unusable)
  - delete the rook-ceph operator, common resources, CRDs, and the '$NS' namespace
  - remove $HOSTPATH and zap /dev/$DEV (all Ceph data gone)
Re-run with:  ./teardown.sh --confirm
EOF
  exit 0
fi

warn "DESTROYING Ceph on $(hostname) — device /dev/$DEV — in 5s (Ctrl-C to abort)"; sleep 5

say "0/7 uninstall ceph-csi-drivers chart (Rook 1.20 companion)"
helm uninstall ceph-csi-drivers -n "$NS" 2>/dev/null || true

say "1/7 arm Rook disk-cleanup (zaps OSD disks on cluster delete)"
kubectl -n "$NS" patch cephcluster rook-ceph --type merge \
  -p '{"spec":{"cleanupPolicy":{"confirmation":"yes-really-destroy-data"}}}' || true

say "2/7 delete storage CRs (objectstore, filesystem, blockpool)"
kubectl -n "$NS" delete cephobjectstore --all --ignore-not-found --timeout=120s || true
kubectl -n "$NS" delete cephfilesystem  --all --ignore-not-found --timeout=120s || true
kubectl -n "$NS" delete cephblockpool   --all --ignore-not-found --timeout=120s || true

say "3/7 delete StorageClasses that reference rook"
kubectl delete storageclass zyvor-rbd-prod zyvor-cephfs-shared zyvor-rgw-bucket --ignore-not-found || true

say "4/7 delete the CephCluster (Rook runs its cleanup job to zap /dev/$DEV)"
kubectl -n "$NS" delete cephcluster rook-ceph --ignore-not-found --timeout=300s || true

say "5/7 remove operator, common resources, and CRDs"
kubectl delete -f "$base/operator.yaml" --ignore-not-found || true
kubectl delete -f "$base/common.yaml"   --ignore-not-found || true
kubectl delete -f "$base/crds.yaml"     --ignore-not-found || true

say "6/7 delete the '$NS' namespace (clear stuck finalizers if needed)"
kubectl delete ns "$NS" --ignore-not-found --timeout=120s || {
  warn "namespace stuck — clearing finalizers"
  kubectl get ns "$NS" -o json | jq '.spec.finalizers=[]' | \
    kubectl replace --raw "/api/v1/namespaces/$NS/finalize" -f - || true
}

say "7/7 host cleanup — remove $HOSTPATH and wipe /dev/$DEV"
sudo rm -rf "$HOSTPATH"
# tear down any leftover ceph LVM, then zap the disk
sudo bash -c '
  for m in $(dmsetup ls 2>/dev/null | awk "/ceph/{print \$1}"); do dmsetup remove "$m" || true; done
  sgdisk --zap-all "/dev/'"$DEV"'" || true
  dd if=/dev/zero of="/dev/'"$DEV"'" bs=1M count=200 oflag=direct status=none || true
  blkdiscard "/dev/'"$DEV"'" 2>/dev/null || true
  partprobe "/dev/'"$DEV"'" || true
'
say "done — Ceph removed, /dev/$DEV wiped. Re-install with ./up.sh (+ single-node overlay)."
