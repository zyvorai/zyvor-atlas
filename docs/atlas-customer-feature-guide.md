<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Atlas — Feature Guide

> **The central storage control plane for the Zyvor product suite.**

Atlas lets products ask for intent — "give me production block storage" — instead of wrestling with pool internals, then maps that intent to a real backend through pluggable drivers. It gives you block, file, and object storage from a single gateway, with async provisioning, snapshots, backups, replication, per-tenant governance, and a live console on top. Ceph is the first driver (RBD, CephFS, RGW/S3), with NFS, ZFS, and Longhorn backends alongside it and DataBridge adding cloud-to-edge database and object mobility.

**4** Storage backends (Ceph · NFS · ZFS · Longhorn) · **6** Database engines migratable via DataBridge · **80+** REST endpoints across the control plane · **3** Access surfaces — REST · gRPC · SSE

This is the customer-facing onboarding guide — how to access the product, your first workflows, and how to use every feature. A print-ready PDF of the same content sits alongside this file.

## Contents

0. [Getting started — access & first workflows](#getting-started)
1. [Control Plane & Architecture](#1-control-plane-architecture)
2. [Block, File & Object Storage](#2-block-file-object-storage)
3. [Data Protection](#3-data-protection)
4. [Multi-Backend & Ceph Operations](#4-multi-backend-ceph-operations)
5. [Observability & Metrics](#5-observability-metrics)
6. [Alerting & Day-2 Ops](#6-alerting-day-2-ops)
7. [Governance & Multi-Tenancy](#7-governance-multi-tenancy)
8. [DataBridge — Database Mobility](#8-databridge-—-database-mobility)
9. [DataBridge — Object Migration](#9-databridge-—-object-migration)
10. [Disaster Recovery](#10-disaster-recovery)
11. [Storage Center Console](#11-storage-center-console)

## Getting started

**How to access it**

- **Web:** "Storage Center" React console served from the gateway binary at `http://127.0.0.1:5110/` (local `make run`), or `http://:30511/` on a Ceph-mode NodePort deploy.
- **CLI:** `atlasctl` headless REST client — `cargo run -p atlasctl -- ` (health, ready, discover, pools, volumes, snapshots, backups, buckets, tenants, tokens, ceph-status …); global flags `--base-url` (`ATLAS_BASE_URL`) and `--token` (`ATLAS_TOKEN`).
- **API:** REST base `/api/atlas/v1/...` (all JSON); a `tonic` gRPC edge (`atlas.v1.AtlasStorage`) on `ATLAS_GRPC_ADDR` (`:5111`, NodePort 30512) with server-streaming `WatchJob`; async writes return `202 + job_id`, streamable over SSE at `/api/atlas/v1/jobs/{id}/watch`.
- **Login:** Auth is off by default (dev; actor is `anonymous`). Set `ATLAS_AUTH_REQUIRED=1` to require an HS256 JWT (`Authorization: Bearer `); mint one via `POST /api/atlas/v1/auth/tokens`.
- **Needs:** A Ceph (RBD/CephFS/RGW), NFS, ZFS, or Longhorn backend. For evaluation you need no cluster at all — run the fake driver with `ATLAS_CEPH_DRIVER_MODE=fake` (this is what `make run` does).

**Your first workflows**

- **Run the gateway fake-first (no cluster)**
  1. `make run` — starts the gateway with the fake Ceph driver on `127.0.0.1:5110`, runs SQLite migrations, and does one discovery pass.
  1. `curl -s localhost:5110/health` returns `{ "status": "ok" }`.
  1. Open `http://127.0.0.1:5110/` for the Storage Center console (inventory, capacity, jobs, alerts, tenants).
- **Populate & query inventory**
  1. `curl -s -X POST localhost:5110/api/atlas/v1/backends/bkd_ceph_lab/discover` (or `atlasctl discover`) normalizes backend state into the SQLite inventory.
  1. `curl -s localhost:5110/api/atlas/v1/pools | jq` and `.../volumes` — expected fake inventory: 3 pools, 2 volumes, 6 OSDs.
  1. In the console: Storage Center → Inventory / Pools / Volumes.
- **Register an NFS or ZFS backend**
  1. `POST /api/atlas/v1/backends` with `{ "name": "nas-1", "backend_type": "nfs", ... }` — Atlas instantiates the driver and discovers it immediately (not just a catalog row).
  1. Confirm with `GET /api/atlas/v1/backends`; filter pools by `GET /api/atlas/v1/pools?backend=`.
  1. **Honest status**: NFS and ZFS are architecture-proof drivers today — they exist to show a
     non-Ceph backend flows through the same discovery → inventory → REST/gRPC/UI surface as Ceph,
     not to talk to a real NFS/ZFS host yet. Registration always succeeds and always reports the
     same deterministic capacity fixture (8 TB / 30% used) regardless of what server/exports you
     give it — it never actually runs `showmount`/`df` or `zpool list`/`zfs list` against it. Real
     capacity/volume discovery for these two backends is not implemented; only Ceph is backed by
     the real driver. **Object buckets** default to Ceph RGW (via `ObjectBucketClaim`); bucket
     create/delete and object PUT/GET/DELETE/list/presigned-URLs are real, signed S3 calls against
     it. **Longhorn** has a real, signed read path (node
     disk capacity, volume health via its Kubernetes CRDs) but no native write path of its own —
     PVC provisioning for it goes through the existing Kubernetes StorageClass path, not this
     driver.
- **Provision a volume by intent**
  1. `POST /api/atlas/v1/volumes` with `{ "tenant_id": "...", "name": "billing-db-root", "size_bytes": 3221225472, "kind": "block", "policy": "database", "owner": {...} }` → `202 + job_id`.
  1. Poll `GET /api/atlas/v1/jobs/{id}` (or watch SSE at `/api/atlas/v1/jobs/{id}/watch`) until `succeeded`.
  1. CLI equivalent: `atlasctl create-volume billing-db-root --size-gib 3 --policy database --namespace default`.
- **Protect a volume (snapshot → backup → schedule)**
  1. Snapshot now: `POST /api/atlas/v1/volumes/{id}/snapshots`.
  1. Provision a bucket then back up off-cluster: `POST /api/atlas/v1/buckets` → `POST /api/atlas/v1/backup-jobs` with `{ "volume_id": "...", "bucket_id": "...", "mode": "data", "keep": 7 }`.
  1. Automate it: `POST /api/atlas/v1/volumes/{id}/schedule` with `{ "interval_secs": 3600, "keep": 24 }` (or `kind: "backup"`).
- **DataBridge migration, fake-first**
  1. `make run-databridge` runs the whole discover-to-cutover pipeline with no cloud or Kubernetes creds.
  1. Or by API: `POST /databridge/sources` → `.../discover` → `POST /databridge/plans` → `.../assess` → `.../provision` → `.../full-load` → `.../cdc/start` → `.../validate` → `.../cutover` (admin, guarded).

## 1. Control Plane & Architecture

_An intent-driven gateway that decouples every product from the storage underneath it._

- **Intent-Based Provisioning** — Products request an intent class ("production block storage") and Atlas resolves it to a concrete backend, pool, and placement. — _Callers stay decoupled from Ceph or any future backend — no pool internals leak into product code._
  - **How:** REST `POST /api/atlas/v1/volumes` with a `policy` intent (e.g. `"database"`), which `atlas-policy` resolves to a StorageClass; browse the catalog at `GET /api/atlas/v1/policies`. Console: Storage Center → Volumes → Create.
- **Pluggable Storage Drivers** — A single StorageDriver trait fronts every backend: real Ceph and a real read-only Kubernetes driver, plus NFS/ZFS drivers proving the architecture scales past Ceph (fixture data, not live yet — see §4), and a fake driver for local runs. — _Add a backend once and every product gets it through the same stable API._
  - **How:** The driver is chosen by `backend_type` at registration — `POST /api/atlas/v1/backends { "backend_type": "ceph|nfs|zfs|kubernetes" }`; run local with `ATLAS_CEPH_DRIVER_MODE=fake`. Console: Storage Center → Backends.
- **Atlas Gateway** — An axum server that centralizes auth, audit, and the API surface, and embeds the Storage Center console in the binary. — _One deployable front door for storage across the Zyvor product suite._
  - **How:** Start it with `make run`; it serves the console at `http://127.0.0.1:5110/` and REST at `/api/atlas/v1/*` from one binary. Health: `curl localhost:5110/health`.
- **REST + gRPC Surfaces** — The same control plane is reachable over REST and a tonic gRPC edge with streaming WatchJob for product integrations. — _Human tools use REST; product runtimes get typed, streaming gRPC._
  - **How:** REST at `/api/atlas/v1/*`; gRPC `atlas.v1.AtlasStorage` on `:5111` — `grpcurl -plaintext :5111 list`, `grpcurl -plaintext :5111 atlas.v1.AtlasStorage/ListPools`.
- **Inventory & Discovery** — A discovery worker normalizes backend state into a SQLite inventory of pools, volumes, and ownership bindings. — _A consistent, queryable picture of every asset regardless of which backend holds it._
  - **How:** REST `POST /api/atlas/v1/backends/{id}/discover` (or `atlasctl discover`), then read `GET /api/atlas/v1/pools` · `/volumes` · `/clusters`. Console: Storage Center → Inventory.
- **Durable, Self-Healing State** — Restarts recover the job engine (interrupted jobs fail safe, queued jobs re-enqueue), with graceful SIGTERM drain and deep readiness probes. — _The control plane stays trustworthy across pod restarts and rollouts._
  - **How:** Automatic on boot; verify with deep readiness `curl localhost:5110/readyz` (`atlasctl ready`) vs liveness `GET /livez`. Wire k8s readinessProbe→`/readyz`, livenessProbe→`/livez`.

> Core principle: products request intent, not pool internals. Atlas maps intent to a backend and owns inventory, ownership, and audit.

## 2. Block, File & Object Storage

_Provision and manage RBD block, CephFS file, and RGW/S3 object storage from one API._

- **Block Volumes** — Create, get, expand, and delete Ceph-backed block volumes (PVCs) as async jobs that return a job id immediately. — _Fast, non-blocking volume lifecycle with a durable audit of every change._
  - **How:** REST `POST /api/atlas/v1/volumes` (`kind: "block"`) → `202 + job_id`; `GET /volumes/{id}`, `DELETE /volumes/{id}`. CLI: `atlasctl create-volume NAME --size-gib 5 --policy database`. Console: Storage Center → Volumes → Create.
- **CephFS File Shares (RWX)** — Provision shared read-write-many file storage on CephFS for workloads that need concurrent access. — _Multi-writer file storage without standing up a separate NAS._
  - **How:** REST `POST /api/atlas/v1/volumes` with `"policy": "shared"` (recommended — always resolves to CephFS/RWX correctly) or an explicit placement (`kubernetes.storage_class: "zyvor-cephfs-shared"`, `access_mode: ReadWriteMany`, `kind: "filesystem"` — set `kind` explicitly on this path, since it isn't inferred from the storage class). Console: Storage Center → Volumes → Create (File).
- **Object Buckets** — Create S3 buckets on Ceph RGW (the default backend, via `ObjectBucketClaim`), with presigned upload/download URLs and per-bucket quotas/stats; any other S3-compatible endpoint (MinIO, Garage, AWS, a customer-run RustFS) is usable as a bring-your-own backend. — _Self-service object storage, portable across backends._
  - **How:** REST `POST /api/atlas/v1/buckets` with `{ "name": "...", "max_objects": ..., "max_size": "2G" }`; list `GET /buckets`; usage/quota via `GET /buckets/{id}/stats`. CLI: `atlasctl create-bucket NAME`. Console: Storage Center → Buckets. `name` must be a valid Kubernetes/S3-style name, 3-63 characters (lowercase, `-`/`.`, no uppercase or underscores) — an invalid name is rejected immediately rather than failing after the fact.
- **Direct RBD Image Ops** — Provision, clone, resize, flatten, snapshot, and roll back RBD images directly, with per-image usage tracking. — _Full low-level control when you need to bypass the PVC abstraction — the only bypass-CSI path in Atlas, for non-Kubernetes consumers like machina/libvirt._
  - **How:** REST `POST /api/atlas/v1/rbd-images {name, size_bytes, pool?}` to create, `.../clone {name, snap?}` for a golden-image copy, `.../resize`, `.../flatten`, `.../snapshots`, `.../rollback`, `DELETE /rbd-images/{pool}/{image}`. Same cordon + quota admission as `POST /volumes`. CLI: `atlasctl create-rbd-image`. Console: Storage Center → Ceph → RBD Images.
  - **Mounting it on the consuming host:** Atlas records the volume; attaching it is a standard
    Ceph client operation on the host itself, with a working `ceph.conf` + keyring for the
    cluster: `sudo rbd map <pool>/<image>` (→ `/dev/rbd0`), `sudo mkfs.ext4 /dev/rbd0` (first use
    only), `sudo mount /dev/rbd0 /mnt/<name>`; reverse with `sudo umount` + `sudo rbd unmap`. For
    libvirt/QEMU VM disks (the machina path), the image is normally attached directly as the VM's
    block device via `rbd map` + a `<disk type="block">` domain entry (or librbd in QEMU directly)
    — the guest OS owns the filesystem, so no host-level `mkfs`/`mount` is needed. To mount a
    `shared` CephFS volume from a non-Kubernetes host (it's CSI-mounted automatically inside
    Kubernetes), use the kernel client: `sudo mount -t ceph mon1,mon2,mon3:/volumes/csi/<subvolume>
    /mnt/shared -o name=client.<id>,secretfile=/etc/ceph/client.<id>.secret`.
- **Safe Resize (Grow & Shrink)** — Expand volumes freely; shrink is opt-in behind an explicit allow_shrink flag as a data-loss guard. — _Reclaim over-provisioned space without accidentally destroying data._
  - **How:** Grow: `POST /api/atlas/v1/volumes/{id}/expand { "new_size_bytes": ... }`. Shrink: `POST /api/atlas/v1/rbd-images/{pool}/{image}/resize { "size_bytes": ..., "allow_shrink": true }`.
- **Kubernetes Storage Inventory** — Read-only listing of StorageClasses, PVCs, and PVs live from the cluster via kube-rs. — _See exactly how Atlas storage surfaces inside Kubernetes._
  - **How:** REST `GET /api/atlas/v1/storage-classes` · `/kubernetes/pvcs` · `/kubernetes/pvs` (needs a reachable cluster; `502` otherwise). CLI: `atlasctl storage-classes`.

## 3. Data Protection

_Snapshots, clones, and off-cluster backups — scheduled, verified, and retention-managed._

- **Snapshots** — Take point-in-time volume snapshots, then clone or restore from them behind a safe-delete guard. — _Instant rollback points without copying whole volumes._
  - **How:** REST `POST /api/atlas/v1/volumes/{id}/snapshots`; list `GET /snapshots`; `DELETE /snapshots/{id}` (409 if cloned from, `?force=true` overrides). CLI: `atlasctl snapshot-volume VOLUME_ID`.
- **Clone & Restore** — Spin a new volume from a snapshot or restore a volume in place from any snapshot. — _Branch environments from a known-good state in seconds._
  - **How:** REST `POST /api/atlas/v1/snapshots/{id}/clone` (new independent volume) or `.../restore` (point-in-time copy). CLI: `atlasctl clone-snapshot SNAPSHOT_ID --name NAME`.
- **Off-Cluster Backups** — Stream RBD export-diff to RGW/S3 as verified multipart uploads, with restore-from-data and presigned downloads. — _Durable, verifiable backups that survive loss of the source pool._
  - **How:** REST `POST /api/atlas/v1/backup-jobs { "volume_id": ..., "bucket_id": ..., "mode": "data" }`; restore via `POST /restore-jobs`; presigned download `GET /backups/{id}/download?what=data`. CLI: `atlasctl backup-volume VOLUME_ID --bucket-id BUCKET_ID`.
- **Scheduled Snapshots & Backups** — Register recurring snapshot and backup schedules per volume, driven by background workers. — _Set-and-forget protection instead of manual, forgettable runs._
  - **How:** REST `POST /api/atlas/v1/volumes/{id}/schedule` with `{ "interval_secs": 3600, "keep": 24 }` (or `{ "kind": "backup", "bucket_id": ..., "mode": "data" }`); list `GET /schedules`.
- **Retention Policies** — Keep-N plus max-age retention automatically prunes old backups (and object versions). — _Bounded storage cost without hand-pruning stale copies._
  - **How:** Set `keep` (keep-N) and `max_age_secs` on `POST /api/atlas/v1/backup-jobs` or on a schedule; defaults come from `ATLAS_BACKUP_KEEP` / `ATLAS_BACKUP_MAX_AGE_SECS`. Pruning runs as delete jobs after each backup.
- **Orphan Backup GC** — Surface backups whose source volume is gone so they can be cleaned up deliberately. — _No silent buildup of orphaned data you're still paying to store._
  - **How:** REST `GET /api/atlas/v1/maintenance/orphans` lists dangling backups; clean each with `DELETE /api/atlas/v1/backups/{id}`.

## 4. Multi-Backend & Ceph Operations

_Run Ceph, NFS, and ZFS side by side — and operate Ceph natively when it's the driver._

- **Three Storage Backends** — Ceph, NFS, and ZFS all live behind the same StorageDriver, with filters, per-backend gauges, and a backends summary. — _Mix backends under one control plane and one API._
  - **How:** REST `GET /api/atlas/v1/backends`; filter inventory with `GET /api/atlas/v1/pools?backend=` and `/volumes?backend=`. Console: Storage Center → Backends. Only the Ceph driver talks to real infrastructure today — NFS and ZFS are architecture-proof drivers that always report a fixed capacity fixture (see below), not real capacity/volumes from your server.
- **Dynamic Backend Registration** — Register an NFS or ZFS backend over the API and Atlas instantiates the driver and discovers it immediately. — _Onboard new storage without redeploying the gateway._
  - **How:** REST `POST /api/atlas/v1/backends { "backend_type": "nfs"|"zfs", "server": ..., "targets": [...] }` — instantiates the driver and discovers on the spot. Console: Storage Center → Backends → Add. The NFS/ZFS drivers don't yet run real `showmount`/`df` or `zpool list`/`zfs list` against the target — registration always succeeds and always returns the same deterministic 8 TB / 30%-used fixture regardless of the server address given, so treat this as a proof of the pluggable-driver architecture rather than production-ready NFS/ZFS support.
- **Backend Cordon & Drain** — Cordon a backend to reject new provisioning (503) while you drain and service it, then uncordon. — _Take storage offline for maintenance without breaking callers._
  - **How:** REST `POST /api/atlas/v1/backends/{id}/cordon` then `.../uncordon` (creates against a cordoned backend return `503`). Console: Storage Center → Backends → Cordon.
- **Ceph-Native Introspection** — Read ceph status, osd-tree, osd-df, and df directly through dedicated endpoints. — _Diagnose the real cluster without leaving the control plane._
  - **How:** REST `GET /api/atlas/v1/ceph/status` · `/ceph/osd-tree` · `/ceph/osd-df` · `/ceph/df`. CLI: `atlasctl ceph-status | ceph-osd-tree | ceph-osd-df | ceph-df`.
- **OSD Management** — Mark OSDs in or out and reweight them via ceph osd commands from the API. — _Rebalance and service OSDs through the same audited surface._
  - **How:** REST `POST /api/atlas/v1/osds/{osd_id}/out` · `/in` · `/reweight?weight=0.8` → `202 + job_id` (real Ceph).
- **QoS & Pool Migration** — Set per-image IOPS/BPS QoS limits and migrate images between pools with rbd migration. — _Tune performance and rebalance placement without downtime._
  - **How:** REST `POST /api/atlas/v1/rbd-images/{pool}/{image}/qos?iops=&bps=` (0 clears a cap) and `.../migrate?dest_pool=` → `202 + job_id` (real Ceph).

> OSD, QoS, and pool-migration operations act on a real Ceph cluster — they are exercised fake-first in CI and fully verified against live Rook Ceph.

## 5. Observability & Metrics

_Live capacity, health, forecasts, and a cinematic console view of the whole estate._

- **Prometheus Metrics** — Self-metrics at /metrics plus Ceph metrics and per-backend gauges for scraping. — _Drop Atlas straight into your existing monitoring stack._
  - **How:** REST `GET /metrics` (Prometheus text exposition) and `GET /api/atlas/v1/metrics/ceph[?prefix=ceph_osd]`. CLI: `atlasctl self-metrics` / `atlasctl ceph-metrics`.
- **Persisted Time-Series History** — Capacity and usage history is persisted and served at /metrics/history for trend analysis. — _See where storage has been, not just where it is now._
  - **How:** REST `GET /api/atlas/v1/metrics/history`. CLI: `atlasctl history --minutes 60`.
- **Capacity Forecast** — A days-to-full forecast projects when pools and backends will run out of headroom. — _Buy or reclaim capacity before you hit the wall._
  - **How:** REST `GET /api/atlas/v1/metrics/forecast`. CLI: `atlasctl forecast --minutes 1440`.
- **Observatory** — Six live canvas visualizations render the cluster, pools, jobs, and flows in real time in the console. — _An at-a-glance operations view that reads like mission control._
  - **How:** Console only: Storage Center → Observatory (live canvas views fed by the metrics and `/events` streams).
- **Unified Activity Feed** — A single /events stream merges activity across every backend and subsystem. — _One timeline for everything happening in storage._
  - **How:** REST `GET /api/atlas/v1/events` (SSE stream). Console: Storage Center → Activity.
- **Grafana Bundle** — A ready-to-apply Prometheus + Grafana observability bundle ships in deploy/observability. — _Stand up dashboards in minutes instead of building them from scratch._
  - **How:** Apply the shipped bundle: `kubectl apply -f deploy/observability` (Prometheus scrapes `/metrics`; import the Grafana dashboards).

## 6. Alerting & Day-2 Ops

_Atlas is operable, not just observe-and-provision — with alerts, maintenance, and safe upgrades._

- **Rule-Based Alerting** — Rules cover cluster health, pool near-full, OSD down/latency, capacity forecast, recovery, failing jobs, CDC errors, and tenant-quota thresholds. — _Catch storage problems by policy, not by luck._
  - **How:** REST `GET /api/atlas/v1/alerts[?state=open]`; run rules on demand with `POST /api/atlas/v1/alerts/evaluate` (also every `ATLAS_MONITOR_INTERVAL_SECS`). Console: Storage Center → Alerts.
- **Alert Lifecycle** — Acknowledge, silence for a window, or resolve alerts, with a single webhook sink for downstream notification. — _Route real signal to your team and mute the noise._
  - **How:** REST `POST /api/atlas/v1/alerts/{id}/ack` · `/silence[?secs=3600]` · `/resolve`; webhook sink via `ATLAS_ALERT_WEBHOOK_URL`.
- **Maintenance Pause** — Pause the job worker so new work holds in queue until you resume — cluster-wide freeze for maintenance. — _Do risky work with nothing new landing mid-flight._
  - **How:** REST `POST /api/atlas/v1/maintenance { "paused": true }` (resume with `false`); read state with `GET /api/atlas/v1/maintenance`.
- **Cancel a Wedged Job** — Free a job stuck on a slow or unresponsive backend call instead of waiting out its timeout. — _One stuck operation on one volume never has to freeze provisioning for everyone else._
  - **How:** REST `POST /api/atlas/v1/jobs/{id}/cancel` (admin) — kills any live `ceph`/`rbd` process the job is waiting on and marks it failed; `409` if it already finished. Bucket quota/stats calls (`radosgw-admin`) are separately time-bounded and no longer need this.
- **Upgrade Pre-Flight** — A preflight check blocks upgrades on HEALTH_ERR clusters, open critical alerts, in-flight jobs, or lagging CDC. — _Never ship an upgrade into an unhealthy cluster._
  - **How:** REST `GET /api/atlas/v1/upgrade/preflight` → `{ ready, checks, blockers }`; `scripts/deploy-remote.sh` gates on it automatically.
- **Gated Rollout & Rollback** — deploy-remote.sh auto-gates on pre-flight and supports rollout undo, with a --force override. — _Ship and un-ship with guardrails baked into the script._
  - **How:** `scripts/deploy-remote.sh  ` (auto-gates on pre-flight + rollout-restarts); `scripts/deploy-remote.sh   --rollback` for `kubectl rollout undo`; `--force` to override.
- **Control-Plane State Backup** — Optional VACUUM INTO snapshots of the control-plane DB stream to S3/RGW on a schedule with keep-N retention. — _Recover the control plane itself, not just the data it manages._
  - **How:** Enable via env: `ATLAS_STATE_BACKUP_SECS` + `ATLAS_STATE_BACKUP_ENDPOINT`/`_BUCKET`/`_ACCESS_KEY`/`_SECRET_KEY` (`_KEEP` default 24). Off by default.

## 7. Governance & Multi-Tenancy

_Tenant isolation, quotas, policy, audit, and cost attribution — built into the control plane._

- **Multi-Tenancy** — Per-tenant quotas and policy overrides isolate consumers of shared storage. — _Give each team its own guardrails on one physical cluster._
  - **How:** REST `GET|PUT /api/atlas/v1/tenants/{id}/quota` and `GET /tenants/{id}/policies` · `PUT|DELETE /tenants/{id}/policies/{intent}`. Console: Storage Center → Tenants.
- **Tenant Quotas** — Set and read capacity quotas per tenant, with alerts as usage approaches 80% and 95%. — _Prevent one tenant from starving everyone else._
  - **How:** REST `PUT /api/atlas/v1/tenants/{id}/quota { "max_bytes": ..., "max_volumes": ... }` (admin); `GET` returns limits + live usage. A create that would exceed a limit is rejected `409` before enqueue.
- **Placement Policies & Drift** — Intent-to-placement policies govern where storage lands, and a policy-drift report flags volumes that no longer match. — _Keep placement compliant with intent over time, not just at creation._
  - **How:** REST `GET /api/atlas/v1/policies` (catalog), `PUT /tenants/{id}/policies/{intent}` (per-tenant override), and `GET /api/atlas/v1/policy-drift` (volumes off their policy).
- **Service-Account JWTs & RBAC** — Issue role-scoped JWTs and revoke leaked tokens before their TTL, enforced by RBAC across REST and gRPC. — _Grant least-privilege access and kill compromised tokens instantly._
  - **How:** REST `POST /api/atlas/v1/auth/tokens { "subject": ..., "role": "operator", "ttl_secs": 3600 }` (admin); revoke with `POST /auth/tokens/{jti}/revoke`, list `GET /auth/tokens/revoked`.
- **Audit Log** — Every mutating action is recorded, exportable as CSV, with configurable retention-day pruning. — _A defensible trail of who changed what, when._
  - **How:** REST `GET /api/atlas/v1/audit.csv` (SIEM export); retention via `ATLAS_AUDIT_RETENTION_DAYS` (0 = keep forever).
- **Chargeback** — Attribute cost per tenant from a configurable USD-per-GiB-month rate. — _Turn shared storage into a billable, accountable service._
  - **How:** REST `GET /api/atlas/v1/chargeback` (per-tenant usage + optional cost); set the rate with `ATLAS_CHARGEBACK_USD_PER_GIB_MONTH`.

> Governance controls include a per-minute rate limiter (429 on breach) so a runaway caller can't overwhelm the gateway.

## 8. DataBridge — Database Mobility

_Migrate managed cloud databases to open, self-managed engines on Ceph at the edge._

| Source engine | Edge target | Migration type | Real connector |
|---|---|---|---|
| PostgreSQL | CloudNativePG | homogeneous | default (tokio-postgres) |
| MySQL / MariaDB | Percona XtraDB | homogeneous | default (sqlx) |
| Oracle | CloudNativePG (Postgres) | heterogeneous | feature: oracle (OCI) |
| SQL Server | CloudNativePG (Postgres) | heterogeneous | feature: sqlserver |
| MongoDB | Percona Server for MongoDB | homogeneous (document) | feature: mongodb |

- **Discover & Assess** — Introspect a live source database (tables, PKs, sizes, WAL/CDC capability) and score edge-readiness. — _Know exactly what you're migrating — and its risks — before you start._
  - **How:** REST `POST /api/atlas/v1/databridge/sources` (register) → `.../sources/{id}/discover`; create a plan and score it with `POST /databridge/plans/{id}/assess` (each returns a job).
- **Edge Provisioning on Ceph** — Provision CloudNativePG, Percona XtraDB, or Percona Server for MongoDB with data and WAL on Ceph RBD. — _Land your database on open engines and your own storage, off the cloud bill._
  - **How:** REST `POST /api/atlas/v1/databridge/plans/{id}/provision` → `202 + job_id`; read the result at `GET /databridge/edge-clusters`.
- **Full-Load + CDC Replication** — Dump-and-restore full load, then Debezium streams ongoing changes (WAL/binlog/redo/oplog) via Kafka to the edge. — _Migrate with the source still live — no big-bang downtime._
  - **How:** REST `POST /api/atlas/v1/databridge/plans/{id}/full-load`, then `.../cdc/start` (`.../cdc/stop` to halt); watch lag via `GET /databridge/cdc-streams`.
- **Guarded Cutover & Rollback** — Cutover is admin-guarded on validation-passed and low CDC lag, with rollback allowed inside a defined window. — _Switch over safely and back out if something looks wrong._
  - **How:** REST `POST /api/atlas/v1/databridge/plans/{id}/validate` → `.../cutover` (admin, guarded on validation + low CDC lag) → `.../rollback` (admin, within the rollback window).
- **Six Source Engines** — PostgreSQL, MySQL, MariaDB, Oracle, SQL Server, and MongoDB — homogeneous and heterogeneous (to Postgres) paths. — _One migration control plane spanning your whole database estate._
  - **How:** REST `POST /api/atlas/v1/databridge/sources { "kind": "postgres"|"mysql"|..., "driver_mode": "fake"|"real" }`; Oracle/SQL Server/MongoDB connectors are behind cargo features (`oracle`/`sqlserver`/`mongodb`).
- **CDC Self-Heal** — A stalled stream is re-established via cdc/restart, and the reconciler auto-restarts an unhealthy stream up to 3× before alerting. — _Replication recovers itself instead of quietly falling behind._
  - **How:** REST `POST /api/atlas/v1/databridge/plans/{id}/cdc/restart` re-establishes a stalled stream; the reconciler also auto-restarts up to 3× before raising a `CDC replication error` alert.

## 9. DataBridge — Object Migration

_Move AI datasets, model weights, and RAG documents from cloud object stores into Ceph RGW._

- **Cloud-to-Ceph Object Copy** — Copy objects from a cloud store into a Ceph RGW bucket so products consume from a local S3 endpoint. — _Pull AI-scale datasets on-prem and off the cloud egress meter._
  - **How:** REST `POST /api/atlas/v1/databridge/object` (create the migration) then `POST /databridge/object/{id}/start` → `202 + job_id`; watch via `/jobs/{id}/watch`.
- **Incremental, Verified Copy** — List source, diff against destination by key+size, stream each changed object recording sha256, then verify every key landed. — _Trustworthy syncs that only move what changed and prove they arrived._
  - **How:** Automatic within the copy job; poll `GET /api/atlas/v1/databridge/object/{id}` for `objects_{total,done}` / `bytes_{total,done}` / `verified`.
- **Multi-Cloud Sources** — AWS S3, GCS (S3-interop), and any S3-compatible store work today; Azure Blob has a native feature-gated connector. — _One mover for the object stores you actually use._
  - **How:** Set the source endpoint/type in the `POST /api/atlas/v1/databridge/object` body; Azure Blob is behind the `azure-blob` cargo feature.
- **Concurrent Streaming** — Objects stream and copy concurrently with configurable part size and concurrency for AI-scale datasets. — _Move terabytes fast instead of one slow object at a time._
  - **How:** Tune part size and concurrency in the migration config on `POST /api/atlas/v1/databridge/object`; the copy runs as an async job.
- **Secret-Ref Credentials** — Credentials are referenced from a Kubernetes Secret, resolved in-process at run time, and never stored or logged. — _Migrate data without spilling access keys into the control plane._
  - **How:** Pass a Kubernetes Secret reference (not raw keys) in the migration body; Atlas resolves it in-process at run time and never persists or logs the keys.

## 10. Disaster Recovery

_Cross-cluster RBD mirroring and failover scaffolding for a second Ceph site._

- **Cluster Peering** — Register and remove peer Ceph clusters as DR targets through the control plane. — _Catalog your DR relationships in one place._
  - **How:** REST `POST /api/atlas/v1/dr/peers` (bootstrap token via a k8s Secret ref) · `GET /dr/peers`.
- **Volume Mirroring** — Enable or disable rbd mirror on a volume and list all mirrors with their DR status. — _Keep critical volumes replicated to a second site._
  - **How:** REST `POST /api/atlas/v1/volumes/{id}/mirror?mode=snapshot&peer=` · `DELETE .../mirror`; list `GET /dr/mirrors` and posture `GET /dr/status`.
- **Promote & Demote (Failover)** — Promote a mirror at the DR site or demote the primary to orchestrate failover. — _Flip sites when the primary goes down._
  - **How:** REST `POST /api/atlas/v1/dr/mirrors/{id}/promote` (secondary → primary) · `/demote` (the reverse).

> Honest status: the DR control-plane catalog, API, and jobs are complete, but the underlying rbd mirror operations are not yet verified against a live second Ceph cluster — treat DR as scaffolding pending a two-site validation.

## 11. Storage Center Console

_A Zeus OS-style React console embedded in the gateway, wiring every capability to a UI._

- **Embedded React Console** — A React/Vite/Tailwind SPA served from the gateway binary over HTTPS with a branded login. — _Nothing extra to deploy — the UI ships inside Atlas._
  - **How:** Browse to `http://127.0.0.1:5110/` (local) or `http://:30511/` (Ceph-mode NodePort) — the SPA is served from the same gateway binary.
- **Full-Capability Coverage** — Inventory, capacity/health, job progress via SSE, alerts, metrics, tenants, and every write action are wired into the UI. — _Drive the whole control plane without touching the CLI._
  - **How:** Storage Center's persistent sidebar — six section groups (Storage, Data Protection, DataBridge, Observability, Governance, Infrastructure) with collapsible headers, **Filter navigation…**, role-aware entries, and **Recent** / **Suite** links — each page backed by the matching `/api/atlas/v1/*` endpoints.
- **Live Job Progress (SSE)** — Async jobs stream progress to the console over server-sent events as they run. — _Watch provisioning, backups, and migrations advance in real time._
  - **How:** Console Jobs view subscribes to `GET /api/atlas/v1/jobs/{id}/watch` (`text/event-stream`); the gRPC parity is `WatchJob`.
- **Themeable Design** — Two Apple shop shells: **Carbon** (black canvas + Apple Blue, dark, default) and **Apple Lite** (#F5F5F7 + Apple Blue, light). Elevated boxes, SF system type, selection tiles, and swipe rails. — _A console that looks like a product shop, in whichever lighting your ops team prefers._
  - **How:** Switch themes (Carbon / Apple Lite) from the top-bar Look & feel icon or **Settings → Appearance**.
- **atlasctl CLI** — A headless REST client covering health, discovery, volumes, snapshots, backups, buckets, RBD, tenants, tokens, and more. — _Script and automate everything the console can do._
  - **How:** `cargo run -p atlasctl -- ` (e.g. `atlasctl health`, `atlasctl volumes`, `atlasctl create-volume ...`); global flags `--base-url` (`ATLAS_BASE_URL`) and `--token` (`ATLAS_TOKEN`).

## Getting started

1. **Run the gateway (no Ceph, no cluster)** — make run starts the gateway with the fake Ceph driver on 127.0.0.1:5110 — no real storage required.
2. **Populate inventory** — cargo run -p atlasctl -- discover pulls a normalized inventory from the driver; then pools, volumes, and health are live.
3. **Open the Storage Center** — Browse to http://127.0.0.1:5110/ for the embedded console — inventory, capacity, jobs, alerts, tenants, and write actions.
4. **Try DataBridge fake-first** — make run-databridge runs the whole discover-to-cutover pipeline with no cloud or Kubernetes creds needed.
5. **Go real on k3s + Rook Ceph** — Use deploy/rook-ceph-lab and scripts/deploy-remote.sh to stand up real Ceph and deploy the gateway in Ceph mode.

> **Good to know:** Atlas is at slices 1–5 plus DataBridge, verified end-to-end on real k3s + Rook Ceph clusters. Some capabilities require real infrastructure or optional build features: cross-cluster DR (rbd mirror) is verified between two lab Rook clusters, including two-way mirroring with a guarded clean failback, but each deployment must drill its own sites before setting `ATLAS_DR_DATAPLANE_VERIFIED`; true multi-replica HA requires Postgres (single-replica SQLite is the default); Oracle, SQL Server, MongoDB, precise CDC lag, and Azure Blob connectors are behind cargo features that link native dependencies; CDC and cutover are verified live against Kafka/Debezium on the lab for all six source engines (Postgres, MySQL incl. `TIMESTAMP` columns, MariaDB, MongoDB, and SQL Server and Oracle into Postgres).

---
_Atlas is developed by ZyvorAI Labs. Licensed under the Zyvor Production Licence v2.0. See [LICENSING.md](LICENSING.md). Contact **https://zyvor.dev**._
