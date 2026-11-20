<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-native Helm chart

Runs `atlas-native-node` ([`docs/NATIVE_NODE.md`](../../../docs/NATIVE_NODE.md)) as a StatefulSet:
every pod is a Raft metadata voter and a data node serving its own PVC. Node ids are pod names,
peers are addressed by the headless Service's stable DNS names, and one generated config is
shared by all pods (`${POD_NAME}` is expanded at startup).

```sh
helm upgrade --install atlas-native deploy/helm/atlas-native -n atlas-native --create-namespace \
  --set image.repository=ghcr.io/zyvorai/atlas-native-node --set image.tag=0.5.0
kubectl -n atlas-native get secret atlas-native-api -o jsonpath='{.data.token}' | base64 -d
```

| Value | Default | Notes |
| --- | --- | --- |
| `replicas` | 3 | Pods = data nodes = Raft members. Scale with the procedure below. |
| `membership.bootstrapReplicas` | 0 | Initial voters (pods `0..N-1`). 0: kept from the existing release, else `replicas` on first install. |
| `node.replicationFactor` | 3 | Copies per extent, at most `replicas`. |
| `node.erasure` | empty | Reed-Solomon scheme such as `4+2` for extents of at least `node.erasureMinBytes`; data + parity must not exceed `replicas`. Empty replicates every extent. |
| `node.extentBytes`, `node.maxRequestBytes`, `node.tickMs`, `node.proposalTimeoutMs`, `node.erasureMinBytes`, `node.repairIntervalSecs`, `node.rebuildDelaySecs`, `node.rebuildBytesPerSec`, `node.scrubBytesPerSec`, `node.gcIntervalSecs` | see `values.yaml` | Map to the node config fields of the same name. |
| `tiering.enabled` | false | Move cold extents to object storage. By default the chart creates an ObjectBucketClaim of `tiering.objectBucketClaim.storageClassName` (Ceph RGW through Rook) and the nodes read its endpoint, bucket and credentials. Set `tiering.endpoint`, `tiering.bucket` and `tiering.existingSecret` (keys `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) to use any other S3-compatible store instead. |
| `tiering.coldAfterSecs`, `tiering.intervalSecs`, `tiering.bytesPerSec`, `tiering.minExtentBytes`, `tiering.prefix`, `tiering.region` | see `values.yaml` | Map to `metadata.tiering` in the node config. |
| `apiToken.existingSecret` | empty | Secret with key `token`; otherwise the chart creates `<fullname>-api` with a random token kept across upgrades (and on uninstall). |
| `tls.enabled` | false | Mutual TLS for Raft and data-node traffic. The certificate must carry every pod name as a DNS SAN. |
| `tls.existingSecret` / `tls.certManager.*` | empty / off | `ca.crt`, `tls.crt`, `tls.key`; or let cert-manager issue `<fullname>-tls` from `issuerRef` with the right SANs. |
| `httpTls.enabled`, `httpTls.existingSecret` | off | HTTPS for the API (`tls.crt`, `tls.key`); probes switch to HTTPS. |
| `httpTls.requireClientCert` | false | Require a client certificate signed by the Secret's `ca.crt` on `/v1/*`; probes and `/metrics` stay open. |
| `persistence.size`, `persistence.storageClass` | 5Gi, default class | Per-pod state volume. |
| `podAntiAffinity` | soft | `soft`, `hard` or `none`. |
| `serviceMonitor.enabled` | false | Prometheus Operator `ServiceMonitor` for `/metrics`. |
| `csi.enabled` | false | CSI driver `native.atlas.zyvor.ai` ([`docs/NATIVE_CSI.md`](../../../docs/NATIVE_CSI.md)): CSIDriver, controller Deployment, node DaemonSet and the StorageClass `csi.storageClass.name`. Not compatible with `httpTls.requireClientCert` yet. |
| `csi.snapshots.enabled` | false | external-snapshotter sidecar and the VolumeSnapshotClass `csi.snapshots.className`; needs the VolumeSnapshot CRDs and snapshot-controller. |
| `csi.node.updateStrategy` | OnDelete | Restarting a node plugin breaks the mounts on that node, so upgrades don't roll it. |
| `csi.storageClass.extentBytes`, `csi.storageClass.mountOptions` | 0, [] | Extent grid of new filesystems; whitelisted `atlas-native-mount` options. |
| `smb.enabled` | false | SMB gateway ([`docs/NATIVE_SMB.md`](../../../docs/NATIVE_SMB.md)): single-replica privileged Deployment running Samba over FUSE mounts of `smb.shares`, and a Service on 445. Needs `smb.users.existingSecret` (key `users.json`) or inline `smb.users.list`. Not compatible with `httpTls.requireClientCert` yet. |
| `smb.encrypt`, `smb.mountArgs` | desired, `[--cache-leases]` | SMB 3 encryption (`desired`, `required`, `off`); `atlas-native-mount` options for every share. |
| `s3.enabled` | false | S3 gateway ([`docs/NATIVE_S3.md`](../../../docs/NATIVE_S3.md)): Deployment of `s3.replicas` unprivileged pods and a Service on `s3.service.port`, serving `s3.buckets` (filesystem or snapshot per bucket). Needs `s3.credentials.existingSecret` (key `credentials.json`) or inline `s3.credentials.keys`. Not compatible with `httpTls.requireClientCert` yet. |
| `s3.tls.existingSecret`, `s3.domains`, `s3.uid`, `s3.gid` | empty, [], 0, 0 | HTTPS for the S3 endpoint; virtual-hosted-style domains; owner of files S3 writes create. |
| `replication.enabled`, `replication.target.*`, `replication.jobs` | false | Asynchronous replication of filesystems to a replica on another cluster ([`docs/NATIVE_REPLICATION.md`](../../../docs/NATIVE_REPLICATION.md)): one unprivileged `atlas-native-replicate` Deployment per job. Not compatible with `httpTls.requireClientCert` yet. |
| `node.cacheInodes`, `node.storeCacheBytes` | 262144, 64 MiB | Memory bounds per metadata group: unchanged inodes kept in memory and the catalog store's page cache (node config `cache_inodes`, `store_cache_bytes`). |
| `node.mallocArenaMax` | 0 | Sets `MALLOC_ARENA_MAX` in the node pods; 0 keeps glibc's default. |

`values-edge.yaml` is a single-node, low-memory profile for edge sites (one pod, one copy per
extent, small caches, a 256 MiB limit): `helm install edge deploy/helm/atlas-native -f
deploy/helm/atlas-native/values-edge.yaml`. See [`docs/NATIVE_EDGE.md`](../../../docs/NATIVE_EDGE.md).

Pods roll only when the rendered config or the pod template changes (`checksum/config`), so an
unchanged `helm upgrade` is a no-op. `deploy/native/helm-live-check.sh` installs the chart with
both TLS layers on a throwaway PKI and checks auth, I/O, leader failover and that no-op upgrade.

## Scaling

New pods join as non-voters (the chart pins `metadata.bootstrap` to the release's original pods), so
scaling is a `helm upgrade` plus a membership change against the leader
(`docs/NATIVE_NODE.md`, "Membership changes"):

```sh
# 3 -> 5: add the pods (all pods roll once because peers/data nodes changed), then promote them.
helm upgrade atlas-native deploy/helm/atlas-native -n atlas-native --reuse-values --set replicas=5
curl -H "Authorization: Bearer $TOKEN" -X POST http://<leader>:7480/v1/members -d '{"voters": {
  "atlas-native-0": "atlas-native-0.atlas-native.atlas-native.svc.cluster.local:7482",
  ... one entry per pod 0..4 ...}}'

# 5 -> 3: demote the highest ordinals first, then remove the pods; repair re-replicates their extents.
curl ... -X POST .../v1/members -d '{"voters": {"atlas-native-0": "...", "atlas-native-1": "...", "atlas-native-2": "..."}}'
helm upgrade atlas-native deploy/helm/atlas-native -n atlas-native --reuse-values --set replicas=3
curl ... -X POST .../v1/repair
```

Remove at most `node.replicationFactor - 1` data nodes before repair has finished, and delete the
removed pods' PVCs afterwards if you do not intend to scale back up.
