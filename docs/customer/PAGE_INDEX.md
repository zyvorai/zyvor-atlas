<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Atlas — Complete page index

Every primary navigable dashboard route.

_Generated: 2026-09-01 · 32 routes_

Regenerate: `node scripts/customer-docs/generate-page-index.mjs`

## STORAGE

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Command Deck | `/` | Estate overview — capacity sounding, pool tiles, protection gaps, and quick jumps into volumes, snapshots, and backups. | [Open](pages/storage/home.md) |
| Volumes | `/volumes` | Intent-backed volume inventory — create, expand, snapshot, schedule, and delete across backends. | [Open](pages/storage/volumes.md) |
| RBD Images | `/rbd` | Raw Ceph RBD images for machina/libvirt and bare VMs (bypassing CSI). | [Open](pages/storage/rbd.md) |
| Snapshots | `/snapshots` | Point-in-time volume snapshots — clone or restore into new volumes. | [Open](pages/storage/snapshots.md) |
| Schedules | `/schedules` | Automate periodic snapshots or backups for a volume. | [Open](pages/storage/schedules.md) |

## DATA PROTECTION

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Backups | `/backups` | Volume backups into object buckets — create, restore, and retire copies. | [Open](pages/data-protection/backups.md) |
| Buckets | `/buckets` | Object gateway buckets for exports and backup destinations. | [Open](pages/data-protection/buckets.md) |
| Protection Status | `/protection` | Per-volume protection verdict — healthy / degraded / unprotected rollup. | [Open](pages/data-protection/protection.md) |

## OBSERVABILITY

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Observatory | `/observatory` | Estate telemetry canvas — capacity lenses and jump to Deck or Alerts. | [Open](pages/observability/observatory.md) |
| Activity | `/activity` | Recent operator and system activity stream. | [Open](pages/observability/activity.md) |
| Alerts | `/alerts` | Open alert ledger — silence or resolve before capacity work. | [Open](pages/observability/alerts.md) |
| Metrics | `/metrics-dashboard` | Ceph-native metric samples and OSD utilization averages. | [Open](pages/observability/metrics-dashboard.md) |
| Jobs | `/jobs` | Durable async jobs for every mutation — progress, SSE live updates, failure detail. | [Open](pages/observability/jobs.md) |
| Audit | `/audit` | Compliance trail of state-changing and sensitive actions. | [Open](pages/observability/audit.md) |

## GOVERNANCE

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Tenants | `/tenants` | Tenant index — quotas and policy overrides. | [Open](pages/governance/tenants.md) |
| Access | `/access` | Local users for Storage Center sign-in — create and delete accounts. | [Open](pages/governance/access.md) |
| Policies | `/policies` | Built-in intent → placement catalog (atlas-policy) used when creating volumes. | [Open](pages/governance/policies.md) |
| Settings | `/settings` | Console settings — theme and session preferences for Storage Center. | [Open](pages/governance/settings.md) |
| API Docs | `/api-docs` | Curated REST + gRPC map for operators and integrators. | [Open](pages/governance/api-docs.md) |

## DATABRIDGE

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Cloud Databases | `/databridge/sources` | Register external / cloud database sources for DataBridge migrations. | [Open](pages/databridge/databridge-sources.md) |
| Migration Plans | `/databridge/plans` | Create and list DataBridge migration plans from registered sources. | [Open](pages/databridge/databridge-plans.md) |
| Plan Detail | `/databridge/plans/:id` | Single migration plan — stages, CDC controls, cutover, and validation hooks. | [Open](pages/databridge/databridge-plans-id.md) |
| Edge DB Clusters | `/databridge/edge-clusters` | Edge database clusters provisioned as migration targets. | [Open](pages/databridge/databridge-edge-clusters.md) |
| Replication | `/databridge/replication` | CDC replication streams for active migration plans. | [Open](pages/databridge/databridge-replication.md) |
| Validation | `/databridge/validation` | Validation runs and per-table results for migrated data. | [Open](pages/databridge/databridge-validation.md) |

## INFRASTRUCTURE

| Page | Route | Purpose | Guide |
|------|-------|---------|-------|
| Backends | `/backends` | Registered storage backends — discovery and capacity summary. | [Open](pages/infrastructure/backends.md) |
| Kubernetes | `/kubernetes` | Discovered StorageClasses from the attached cluster. | [Open](pages/infrastructure/kubernetes.md) |
| Cluster | `/cluster` | Primary cluster inventory — health, pools, OSDs. | [Open](pages/infrastructure/cluster.md) |
| Ceph | `/ceph` | Day-2 Ceph signals — health rollup, df pools, OSD tree. | [Open](pages/infrastructure/ceph.md) |
| Pool Detail | `/pools/:id` | Single pool sounding — volumes and OSD cells for one pool. | [Open](pages/infrastructure/pools-id.md) |
| Maintenance | `/maintenance` | Pause the job engine, cordon backends, and clean orphan backups. | [Open](pages/infrastructure/maintenance.md) |
| Disaster Recovery | `/dr` | Cross-cluster RBD mirroring — peers, mirrors, promote/demote/failover. | [Open](pages/infrastructure/dr.md) |

## Related

- [Customer docs home](README.md)
- [Page-by-page guides](pages/README.md)
