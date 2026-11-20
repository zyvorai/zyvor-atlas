<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Page-by-page guides

Each guide follows: Purpose → When to use it → How to get there → Operate from the console (UX) → Related pages.

Every route is also listed in the [complete page index](../PAGE_INDEX.md).

## Data Protection

| Page | What it covers |
|------|----------------|
| [Backups](data-protection/backups.md) | Volume backups into object buckets — create, restore, and retire copies. |
| [Buckets](data-protection/buckets.md) | Object gateway buckets for exports and backup destinations. |
| [Protection Status](data-protection/protection.md) | Per-volume protection verdict — healthy / degraded / unprotected rollup. |

## Databridge

| Page | What it covers |
|------|----------------|
| [Edge DB Clusters](databridge/databridge-edge-clusters.md) | Edge database clusters provisioned as migration targets. |
| [Plan Detail](databridge/databridge-plans-id.md) | Single migration plan — stages, CDC controls, cutover, and validation hooks. |
| [Migration Plans](databridge/databridge-plans.md) | Create and list DataBridge migration plans from registered sources. |
| [Replication](databridge/databridge-replication.md) | CDC replication streams for active migration plans. |
| [Cloud Databases](databridge/databridge-sources.md) | Register external / cloud database sources for DataBridge migrations. |
| [Validation](databridge/databridge-validation.md) | Validation runs and per-table results for migrated data. |

## Governance

| Page | What it covers |
|------|----------------|
| [Access](governance/access.md) | Local users for Storage Center sign-in — create and delete accounts. |
| [API Docs](governance/api-docs.md) | Curated REST + gRPC map for operators and integrators. |
| [Policies](governance/policies.md) | Built-in intent → placement catalog (atlas-policy) used when creating volumes. |
| [Settings](governance/settings.md) | Console settings — theme and session preferences for Storage Center. |
| [Tenants](governance/tenants.md) | Tenant index — quotas and policy overrides. |

## Infrastructure

| Page | What it covers |
|------|----------------|
| [Backends](infrastructure/backends.md) | Registered storage backends — discovery and capacity summary. |
| [Ceph](infrastructure/ceph.md) | Day-2 Ceph signals — health rollup, df pools, OSD tree. |
| [Cluster](infrastructure/cluster.md) | Primary cluster inventory — health, pools, OSDs. |
| [Disaster Recovery](infrastructure/dr.md) | Cross-cluster RBD mirroring — peers, mirrors, promote/demote/failover. |
| [Kubernetes](infrastructure/kubernetes.md) | Discovered StorageClasses from the attached cluster. |
| [Maintenance](infrastructure/maintenance.md) | Pause the job engine, cordon backends, and clean orphan backups. |
| [Pool Detail](infrastructure/pools-id.md) | Single pool sounding — volumes and OSD cells for one pool. |

## Observability

| Page | What it covers |
|------|----------------|
| [Activity](observability/activity.md) | Recent operator and system activity stream. |
| [Alerts](observability/alerts.md) | Open alert ledger — silence or resolve before capacity work. |
| [Audit](observability/audit.md) | Compliance trail of state-changing and sensitive actions. |
| [Jobs](observability/jobs.md) | Durable async jobs for every mutation — progress, SSE live updates, failure detail. |
| [Metrics](observability/metrics-dashboard.md) | Ceph-native metric samples and OSD utilization averages. |
| [Observatory](observability/observatory.md) | Estate telemetry canvas — capacity lenses and jump to Deck or Alerts. |

## Storage

| Page | What it covers |
|------|----------------|
| [Command Deck](storage/home.md) | Estate overview — capacity sounding, pool tiles, protection gaps, and quick jumps into volumes, snapshots, and backups. |
| [RBD Images](storage/rbd.md) | Raw Ceph RBD images for machina/libvirt and bare VMs (bypassing CSI). |
| [Schedules](storage/schedules.md) | Automate periodic snapshots or backups for a volume. |
| [Snapshots](storage/snapshots.md) | Point-in-time volume snapshots — clone or restore into new volumes. |
| [Volumes](storage/volumes.md) | Intent-backed volume inventory — create, expand, snapshot, schedule, and delete across backends. |

---

32 guides. Regenerate: `node scripts/customer-docs/generate-guide-index.mjs`.
