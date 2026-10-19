<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Deployment

Two things get deployed:
1. the **Atlas gateway** (a container on Kubernetes), and
2. optionally, the **Rook Ceph** storage fabric it manages.

This guide covers a k3s node end-to-end, exactly as verified on `<ephemeral-ip>`.

**Prefer the scripts.** Hand-editing CRs (image tags, mon count, finalizers) is how labs get stuck.
The paths below are the supported ones.

## Prerequisites on the target host

- A running Kubernetes/k3s cluster; `kubectl` works (or `sudo k3s kubectl`).
- `helm` on the machine that runs `up.sh` (required for Ceph CSI on Rook ≥1.20).
- Empty block device for the OSD (destructive to that disk), e.g. `/dev/sdb`.
- Passwordless `sudo` (for `k3s ctr images import`).
- SSH from your workstation: `rsync` + an identity that works on the host
  (`ATLAS_SSH_KEY` / `SSH_KEY`, or `~/.ssh/id_ed25519_hyper2kvm` for the Zyvor lab).

The gateway image is built **on the host** with podman and imported into k3s containerd —
no external registry. `deploy-ceph-gateway-remote.sh` installs podman via apt if missing and
configures `docker.io` as an unqualified-search registry (Ubuntu podman defaults otherwise break
`FROM rust:…` short names).

## Version lockstep (do not drift)

| Component | Pin | Why |
|---|---|---|
| Rook operator / charts | **v1.20.2** (default in `up.sh`) | Current hypercluster Helm default |
| Ceph image | **`quay.io/ceph/ceph:v19.2.3`** (Squid) | Rook ≥1.20 **rejects** Reef (`v18.2.x`) with `minimum version "19.2.0-0 squid"` |
| Gateway client (`Dockerfile.ceph`) | Squid apt (`debian-squid`) | Match cluster major |
| CSI drivers | `ceph-csi-operator/ceph-csi-drivers` Helm chart | Rook 1.20 **does not** ship Driver CRs/SAs; without this chart every PVC stays `Pending` |

Override only together: `ROOK_VERSION=… CEPH_IMAGE=… ./up.sh …`.

## 1. Deploy the gateway (fake Ceph + live k8s driver)

```bash
./scripts/deploy-remote.sh <host> <user>
# e.g. ./scripts/deploy-remote.sh <ephemeral-ip> operator
```

Object storage defaults to Ceph RGW (`bkd_ceph_lab`); no separate object-store server is started by
this script. Any S3-compatible endpoint (MinIO, Garage, AWS, ...) is usable as
a bring-your-own backend — see [`RUSTFS.md`](RUSTFS.md) for the history of Atlas's now-removed
first-party RustFS integration.

NodePort **30510**. Verifies `/health`, `/version`, and `/storage-classes`. Gates automatically on
`GET /upgrade/preflight` before rolling out (pass `--force` to override a blocked pre-flight).

The script builds `localhost/atlas-gateway:dev` on the host (chart `appVersion` is `0.8.0`).
Published images are separate: [`deploy/helm/atlas/values-release.yaml`](../deploy/helm/atlas/values-release.yaml)
pins `ghcr.io/zyvorai/atlas:0.8.0`, and the real-Ceph image is `ghcr.io/zyvorai/atlas-ceph:0.8.0`.
Do not deploy the floating `:latest` tag. What that build is allowed to claim is
[STATUS.md](STATUS.md).

Optional flags: `--with-ceph` (also runs `deploy/rook-ceph-lab/up.sh` on the remote — destructive
to an empty disk, off by default), `--with-k3s-disk` (moves the k3s data-dir onto the disk carved
by `resize-osd.sh`, off by default), `--rollback` (skip build/deploy and `kubectl rollout undo` the
gateway Deployment to its previous ReplicaSet — for a bad upgrade).

## 2. Stand up Rook Ceph (single-node lab)

> **Destructive:** consumes an empty disk (e.g. `/dev/sdb`) as a Ceph OSD.

### Preferred: one script

Edit `deploy/rook-ceph-lab/single-node/cluster.yaml` so `spec.storage.nodes[].name` matches
`kubectl get nodes` and the device name matches `lsblk` (lab: `nldw3-4-32-26` + `sdb`).

```bash
# Full install (operator + cluster + CSI drivers + zyvor-* SCs):
cd deploy/rook-ceph-lab
./up.sh --single-node

# Operator already present (e.g. hypercluster installed Rook via Helm):
./up.sh --single-node --cluster-only
```

`up.sh` will:
1. apply the CephCluster (Squid image, 1 mon),
2. wait until phase **Ready** (two consecutive polls),
3. install **`ceph-csi-drivers`** with rook-prefixed names
   (`rook-ceph.rbd.csi.ceph.com` / `rook-ceph.cephfs.csi.ceph.com`),
4. apply single-node RBD / CephFS / RGW StorageClasses + snapshot class.

### Via hypercluster (platform path)

```bash
# from the hypercluster repo, with cluster.conf pointed at the host + SSH_KEY set:
./hypercluster -c cluster.conf storage apply --backend rook-ceph \
  --confirm-destroy <ip>:/dev/sdb
```

That path also pins Squid, sets mon count from node count, and installs `ceph-csi-drivers`.
Atlas StorageClasses (`zyvor-*`) still come from Atlas `up.sh --single-node --cluster-only`
(or the single-node YAML overlay) if you want the full Atlas SC set.

### Verify Ceph + CSI

```bash
kubectl -n rook-ceph get cephcluster          # PHASE Ready (HEALTH_WARN is OK on 1 OSD)
kubectl -n rook-ceph get driver               # rook-ceph.rbd… and rook-ceph.cephfs…
kubectl get csidriver | grep rook-ceph
kubectl get sc | grep zyvor
kubectl -n rook-ceph get pods | grep 'rbd.csi\|cephfs.csi'   # ctrlplugin + nodeplugin Running
```

If PVCs stay `Pending` with `Waiting for … rook-ceph.rbd.csi.ceph.com`, the CSI drivers chart
was skipped — re-run the CSI step from `up.sh` or:

```bash
helm repo add ceph-csi-operator https://ceph.github.io/ceph-csi-operator
helm upgrade --install ceph-csi-drivers ceph-csi-operator/ceph-csi-drivers -n rook-ceph \
  -f https://raw.githubusercontent.com/rook/rook/v1.20.2/deploy/charts/ceph-csi-drivers/values.yaml
```

Smoke-test RBD:

```bash
kubectl apply -f - <<'YAML'
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: atlas-rbd-smoke, namespace: default }
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: zyvor-rbd-prod
  resources: { requests: { storage: 2Gi } }
YAML
kubectl -n default get pvc atlas-rbd-smoke      # → Bound
```

## 3. Run Atlas in real Ceph mode

```bash
./scripts/deploy-ceph-gateway-remote.sh <host> <user>
# e.g. ./scripts/deploy-ceph-gateway-remote.sh <ephemeral-ip> operator
```

What the script does (idempotent):
1. rsync repo (uses `ATLAS_SSH_KEY` / `SSH_KEY` / lab hyper2kvm key),
2. ensure **podman** + docker.io short-name config,
3. `podman build -f Dockerfile.ceph` (Squid `ceph-common`),
4. import `localhost/atlas-gateway:ceph` into k3s,
5. ensure ClusterRole, `atlas-tls`, `atlas-gateway-auth`,
6. apply `deploy/k8s/atlas-gateway-ceph.yaml` + rollout (NodePort **30511**).

The Deployment PVC uses **`zyvor-rbd-prod`** — CSI must already be Ready or the pod stays Pending.

Cluster deploys **require JWT auth** (`ATLAS_AUTH_REQUIRED=1`). Mint a lasting admin JWT from the
bootstrap token, then remove the bootstrap key:

```bash
B=http://<host>:30511
BOOT=$(kubectl -n rook-ceph get secret atlas-gateway-auth -o jsonpath='{.data.bootstrap-admin-token}' | base64 -d)
H=(-H "Authorization: Bearer $BOOT")
curl -sS "${H[@]}" -H 'Content-Type: application/json' \
  -d '{"subject":"ops","role":"admin","ttl_secs":86400}' \
  $B/api/atlas/v1/auth/tokens
# then patch-remove bootstrap-admin-token and rollout restart (see scripts output)
```

Verify:

```bash
curl -s "${H[@]}" $B/health
curl -s "${H[@]}" $B/readyz          # ceph_driver.mode=real
curl -s "${H[@]}" -X POST $B/api/atlas/v1/backends/bkd_ceph_lab/discover
curl -s "${H[@]}" $B/api/atlas/v1/storage-classes | jq
curl -s "${H[@]}" "$B/api/atlas/v1/volumes?kind=block" | jq
```

## Auth bootstrap (cluster)

```bash
curl -sS -H "Authorization: Bearer $BOOT" -H 'Content-Type: application/json' \
  -d '{"subject":"ops","role":"admin","ttl_secs":86400}' \
  $B/api/atlas/v1/auth/tokens
kubectl -n rook-ceph patch secret atlas-gateway-auth --type=json \
  -p='[{"op":"remove","path":"/data/bootstrap-admin-token"}]'
kubectl -n rook-ceph rollout restart deploy/atlas-gateway-ceph
```

Local `make run` stays open (`ATLAS_AUTH_REQUIRED` unset). The gateway **refuses to start** if auth
is required and `ATLAS_JWT_SECRET` is the weak shipped default.

## Durable database

The gateway's SQLite DB is backed by a **ReadWriteOnce PVC** (not `emptyDir`), so inventory/jobs
survive pod restarts. The Deployments use `strategy: Recreate` because RWO requires the old pod to
release the volume before the new one mounts it.

- `deploy/k8s/atlas-gateway.yaml` → PVC `atlas-gateway-data` on the **cluster default** StorageClass
  (deploys even without Ceph, e.g. k3s `local-path`).
- `deploy/k8s/atlas-gateway-ceph.yaml` → PVC `atlas-gateway-ceph-data` on **`zyvor-rbd-prod`**
  (dogfoods the platform's own Ceph).

For multi-replica HA, switch `ATLAS_DATABASE_URL` to Postgres once the sqlx port lands (see
[HA.md](HA.md)). Until then a `postgres://` URL is rejected at startup.

## Day-2 durability & health

- **Job recovery on boot** — interrupted `running` jobs fail-safe; `queued` jobs re-enqueue.
- **Graceful shutdown** — REST + gRPC drain on `SIGTERM` (`terminationGracePeriodSeconds: 30`).
- **Liveness vs readiness** — `/livez` vs `/readyz` (DB + backend driver + worker heartbeats).
- **Self-state backup** — optional SQLite → S3/RGW via `ATLAS_STATE_BACKUP_*` (off by default).
- **Gated rollout + rollback** — `deploy-remote.sh`/`deploy-ceph-gateway-remote.sh` check
  `GET /upgrade/preflight` before rolling out (no HEALTH_ERR cluster / open critical alerts /
  in-flight jobs / lagging CDC), and support `--rollback` (`kubectl rollout undo`) for a bad upgrade.
- **Wedged job escape hatch** — `POST /jobs/{id}/cancel` frees the single-worker queue from a job
  stuck inside a hung `ceph`/`rbd` call instead of waiting out `ATLAS_JOB_TIMEOUT_SECS`. See
  [DAY2.md](DAY2.md).

## Image name note

`podman save` preserves the `localhost/` prefix, so k3s imports
`localhost/atlas-gateway:ceph`. The Deployment uses that name with `imagePullPolicy: Never`.

## Air-gapped builds (Oracle Instant Client)

```bash
podman build --build-arg ORACLE_IC_URL=https://mirror.corp/instantclient-basiclite-linux.x64-21.13…zip \
  -t atlas-gateway:dev .
```

## Pitfalls (lab lessons)

| Symptom | Cause | Fix |
|---|---|---|
| CephCluster: `minimum version "19.2.0-0 squid"` | Reef image under Rook 1.20 | Use `CEPH_IMAGE=quay.io/ceph/ceph:v19.2.3` / updated manifests |
| Stuck `Detecting Ceph version`, no mon/osd | Image pull / stuck job, or deleting CR blocked by pool finalizer | Pre-pull image; clear stuck `CephBlockPool` finalizers; re-run `up.sh` |
| PVC Pending forever, no CSIDriver | Missing `ceph-csi-drivers` chart (Rook 1.20) | Install chart with rook-prefixed driver names |
| Gateway pod Pending unbound PVC | Same as above | Fix CSI, then rollout restart |
| `podman: command not found` / short-name resolve error | Fresh Ubuntu host | Use `deploy-ceph-gateway-remote.sh` (installs + registries.conf) |
| SSH auth fails to lab host | Wrong key | `ATLAS_SSH_KEY=~/.ssh/id_ed25519_hyper2kvm` |
| `HEALTH_WARN` on single-node | Expected (replication / crush) | Not a blocker if PHASE=Ready and OSD up |

## Teardown

```bash
kubectl delete -f deploy/k8s/atlas-gateway-ceph.yaml --ignore-not-found
kubectl delete -f deploy/k8s/atlas-gateway.yaml --ignore-not-found
kubectl -n default delete pvc atlas-rbd-smoke --ignore-not-found
# Full Ceph wipe (destructive):
cd deploy/rook-ceph-lab && ./teardown.sh --confirm
```
