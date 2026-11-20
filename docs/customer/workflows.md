<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Common workflows

## Provision and snapshot a volume

1. [Command Deck](pages/storage/home.md) — confirm health
2. [Volumes](pages/storage/volumes.md) — **Create volume** from intent/policy
3. [Jobs](pages/observability/jobs.md) — wait for success
4. Volume SlideOver → **Snapshot**, or [Snapshots](pages/storage/snapshots.md) / [Schedules](pages/storage/schedules.md)

## Protect with backups

1. [Buckets](pages/data-protection/buckets.md) — **Create bucket**
2. [Backups](pages/data-protection/backups.md) — **Backup** a volume
3. Confirm under [Protection Status](pages/data-protection/protection.md) and [Jobs](pages/observability/jobs.md)

## Run a DataBridge migration

1. [Cloud Databases](pages/databridge/databridge-sources.md) — **Register** source
2. [Migration Plans](pages/databridge/databridge-plans.md) — **Create** plan
3. [Plan Detail](pages/databridge/databridge-plans-id.md) — stages, CDC start/stop, cutover confirms
4. [Validation](pages/databridge/databridge-validation.md) / [Replication](pages/databridge/databridge-replication.md) / [Edge DB Clusters](pages/databridge/databridge-edge-clusters.md)

## Day-2 Ceph / DR

1. [Backends](pages/infrastructure/backends.md) / [Ceph](pages/infrastructure/ceph.md)
2. [Maintenance](pages/infrastructure/maintenance.md) — pause jobs or cordon before risky work
3. [Disaster Recovery](pages/infrastructure/dr.md) — peers, mirrors, promote/failover (confirm-gated)

## Related

- [Getting Started](getting-started.md)
- [Using the Dashboard](using-the-dashboard.md)
