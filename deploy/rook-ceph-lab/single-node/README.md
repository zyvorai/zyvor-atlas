<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Single-node Rook Ceph overlay

The top-level `deploy/rook-ceph-lab/` manifests assume a 3-node cluster (replica size 3, host
failure domain). This overlay makes Rook Ceph run on **one node** (k3s dev box): replica size 1,
`failureDomain: osd`, single MDS/mon. Use it for the lab on `212.8.248.187`, not for production.

**Preferred:** from `deploy/rook-ceph-lab/`:

```sh
./up.sh --single-node                 # greenfield
./up.sh --single-node --cluster-only  # Rook operator already installed (hypercluster Helm)
```

That path pins **Ceph Squid `v19.2.3`** (required by Rook ≥1.20), installs the
**`ceph-csi-drivers`** Helm chart with rook-prefixed driver names, and applies the overlay below.

## Manual apply (only if you need to step through)

1. Edit `cluster.yaml`: set `spec.storage.nodes[].name` to your k8s node name (`kubectl get nodes`)
   and the device (e.g. `sdb`). Image must stay Squid — Reef fails Rook 1.20 version check.
2. Apply:

```sh
cd deploy/rook-ceph-lab/single-node
kubectl apply -f cluster.yaml        # CephCluster: 1 mon, device pin
# wait for PHASE=Ready, then ensure CSI drivers exist (see parent up.sh / docs/DEPLOYMENT.md)
kubectl apply -f blockpool-sc.yaml   # RBD pool (size 1) + zyvor-rbd-prod StorageClass (RWO block)
kubectl apply -f cephfs-sc.yaml      # CephFS (size 1) + zyvor-cephfs-shared StorageClass (RWX file)
kubectl apply -f rgw.yaml            # CephObjectStore + zyvor-rgw-bucket StorageClass (S3/OBC)
```

Wait for each to settle:

```sh
kubectl -n rook-ceph get cephcluster            # PHASE Ready; HEALTH_WARN expected on 1 OSD
kubectl -n rook-ceph get driver                 # rook-ceph.rbd… + rook-ceph.cephfs…
kubectl -n rook-ceph get cephfilesystem zyvorfs # PHASE Ready
kubectl -n rook-ceph get cephobjectstore        # PHASE Ready (RGW pod)
kubectl get sc                                  # zyvor-rbd-prod, zyvor-cephfs-shared, zyvor-rgw-bucket
```

## StorageClasses this overlay creates

| StorageClass          | Backend | Access | Used by Atlas policy |
|-----------------------|---------|--------|----------------------|
| `zyvor-rbd-prod`      | RBD     | RWO    | `production`, `database`, `development`, `ai` |
| `zyvor-cephfs-shared` | CephFS  | RWX    | `shared` (ISO libraries, templates, multi-writer product data) |
| `zyvor-rgw-bucket`    | RGW     | S3     | buckets / backups (ObjectBucketClaim) |

`POST /volumes {"policy":"shared", ...}` provisions a **ReadWriteMany** CephFS volume that multiple
pods can mount at once (verified: two pods writing the same volume concurrently). RBD volumes are
RWO — a single writer.
