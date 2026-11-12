<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Longhorn backend

Atlas discovers Longhorn v1beta2 `nodes`, `volumes` and `replicas` through the Kubernetes API.
The same driver creates, deletes and expands volumes, creates and deletes snapshots, clones a
snapshot into a new volume, and creates, lists and deletes backups, by writing `volumes`,
`snapshots` and `backups` in `longhorn.io/v1beta2`. Enable it with `ATLAS_LONGHORN_ENABLE=1`
and, if needed, `ATLAS_LONGHORN_NAMESPACE=longhorn-system`. The gateway service account needs
read access to `nodes`, `replicas` and `backupvolumes`, and get/list/watch/create/update/patch/delete
on `volumes`, `snapshots`, `backups` and `backuptargets`. `deploy/k8s/atlas-gateway.yaml` grants
that. The gateway fails startup if the Kubernetes client cannot initialize. Run
`POST /api/atlas/v1/backends/bkd_longhorn/discover` after Longhorn is installed. Failed CRD reads
fail discovery; no demo data is substituted.

The backend shows physical node-disk capacity, volume sizes, PVC identity and Longhorn robustness.
Unknown disk capacity is reported as unknown. Its one synthetic `longhorn` pool aggregates disks.
Replica CRs are reported as OSD rows on that cluster: `host` is the Longhorn node, `device_class`
is `{volume}@{disk}` (or just the volume name when the replica has no disk id). A replica whose
`status.currentState` is `error` or `failed` marks its volume `warn`. Ceph OSD, CephFS and RGW
operations do not apply.

## Native CRD writes

`create_volume` posts a block `Volume`: size as a byte string, 3 replicas, `blockdev` frontend,
`rwo` unless the request asks for `ReadWriteMany` (`rwx`). Optional `longhorn.disk_selector` and
`longhorn.node_selector` become `spec.diskSelector` and `spec.nodeSelector` (Longhorn tags; every
tag must match). Empty selectors are omitted. The native id is `longhorn/<namespace>/<name>`,
which is what discovery reports for a volume that has no PVC. `expand_volume` merge-patches
`spec.size` and refuses a size that is not larger. `delete_volume` deletes the Volume; a missing
object is success. An id that starts with `pvc/` is refused — that volume belongs to the PVC path
below. `create_snapshot` posts a `Snapshot` with `spec.volume`, `spec.createSnapshot: true` and
the `longhornvolume` label. `delete_snapshot` deletes it.

`clone_snapshot` reads the Snapshot and its source Volume, then posts a new Volume with
`spec.dataSource` set to `snap://<volume>/<snapshot>`. The new size defaults to the source size
and cannot be smaller. Access mode, replica count and the source volume's disk and node selectors
are copied. `create_backup` posts a `Backup` whose `spec.snapshotName` is the snapshot and whose
labels are `backup-volume` and `longhornvolume`. `list_backups` reads `backups`; when that list is
empty it reads `backupvolumes` instead. `delete_backup` deletes the Backup CR. A backup only
finishes if the cluster already has a BackupTarget (Longhorn's `default`). `set_backup_target`
creates that CR or merge-patches it. The spec is `backupTargetURL`, optional `credentialSecret`,
and `pollInterval: 300s`. `create_backup` does not create the target.

These calls are unit-tested for the object body and id parsing. They have not been run against
a live Longhorn cluster. Longhorn's admission webhook fills the rest of the Volume spec when
the manager is installed.

`POST /api/atlas/v1/volumes` does not call this path. It still provisions a PVC.

## PVC provisioning

Atlas can create a Longhorn PVC using its Kubernetes job path. Supply both the Atlas backend ID
and a StorageClass whose provisioner is `driver.longhorn.io`:

```json
{
  "tenant_id": "your-tenant-id",
  "name": "app-data",
  "size_bytes": 10737418240,
  "kind": "block",
  "kubernetes": {
    "backend_id": "bkd_longhorn",
    "namespace": "default",
    "storage_class": "longhorn"
  }
}
```

Send this to `POST /api/atlas/v1/volumes` with the usual Atlas authorization. An explicit
Longhorn StorageClass is required: the default policy catalog still resolves to Ceph classes.
Atlas checks the actual StorageClass provisioner and the backend's cordon state before queuing.
`POST /api/atlas/v1/volumes` does not call the native clone, backup or placement path above.
Kubernetes VolumeSnapshot is not implemented by this driver. Validate RWX migration on your
chosen Longhorn storage mode before using it for KubeVirt live migration.
