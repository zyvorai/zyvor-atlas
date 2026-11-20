<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-driver-k8s

The live Kubernetes driver (read-only in slice 1), built on `kube-rs`.

- `list_storage_classes()` — all StorageClasses; Ceph-backed ones tagged `is_ceph: true`
  (matches `*.rbd.csi.ceph.com` / `*.cephfs.csi.ceph.com`, PDF §7.1).
- `list_pvcs(namespace)` / `list_pvs()` — compact PVC/PV summaries (phase, storage class,
  capacity, access modes, csi driver).
- `try_default()` — builds from the ambient kubeconfig / in-cluster config (honors `KUBECONFIG`).

`create_pvc` and other write operations arrive in slice 2. The gateway serves `/storage-classes`,
`/kubernetes/pvcs`, `/kubernetes/pvs` straight from this driver; if no cluster is reachable those
endpoints return `502`.
