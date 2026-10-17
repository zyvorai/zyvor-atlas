<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Documentation Index

Start at the top-level [README](../README.md) for the overview and quickstart. This index covers
the **engineering docs** in this folder. Operators and end users of the console should start at
the separate [customer docs](customer/README.md) instead — the two trees are cross-linked, not
duplicated.

## Start here
- **[GETTING_STARTED.md](GETTING_STARTED.md)** — build, run locally (fake driver), `atlasctl`, tests.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — control-plane design, driver model, data model, request flow, config.
- **[API.md](API.md)** — REST v1 reference with real request/response examples.
- **[ATLAS_UI_CONTRACT.md](ATLAS_UI_CONTRACT.md)** — the Soundings UI identity system the React console follows.

## Storage & drivers
- **[DISKS.md](DISKS.md)** — raw disk provisioning: device pickers, `POST /zfs/pools/from-device`, `POST /ceph/devices`.
- **[LONGHORN.md](LONGHORN.md)** — read-only Longhorn driver.
- **[RUSTFS.md](RUSTFS.md)** — history of the now-removed first-party RustFS integration; Ceph RGW is the default object backend today.
- **[IO_EBPF.md](IO_EBPF.md)** — `atlas-io`, the optional observe-first eBPF storage I/O sensor.

## Day-2 operations
- **[DAY2.md](DAY2.md)** — alerts, maintenance, governance, DR day-2 ops.
- **[DR.md](DR.md)** — cross-cluster RBD mirroring, failover runbook, live two-site checklist.
- **[HA.md](HA.md)** — durable job queue, leader lease, Postgres cutover plan.
- **[ALERTING.md](ALERTING.md)** — alert rules, ack/silence, native notification sinks (Slack/PagerDuty/Opsgenie/webhook).
- **[TRACING.md](TRACING.md)** — OpenTelemetry setup and what's instrumented.
- **[SECRETS.md](SECRETS.md)** — optional Vault-backed secrets resolution at startup.
- **[VMWARE_EXIT_DEMO.md](VMWARE_EXIT_DEMO.md)** — 30-minute lab script. Atlas does not import the disk; CDI boots it on the Ceph StorageClass.

## DataBridge (DB & object migration)
- **[DATABRIDGE.md](DATABRIDGE.md)** — cloud-to-edge DB / object migration control plane: six source engines, CDC, cutover.

## AI
- **[AI_ADVISOR.md](AI_ADVISOR.md)** — explainable risk scoring, incident correlation, what-if capacity planning, anomaly detection.
- **[RELEASING.md](RELEASING.md)** — cutting a release: tag, ghcr images, archives, visibility.
- **[MACOS.md](MACOS.md)** — managing a Mac's internal and external disks: discovery, guarded external-disk lifecycle, a fleet of Macs.
- **[MACOS_CLIENT.md](MACOS_CLIENT.md)** — using Atlas storage from a Mac (NFS, SMB, `atlasctl`).
- **[MCP.md](MCP.md)** — the MCP server: tools, auth, the guarded write flow and the threat model.
- **[IO_RISK.md](IO_RISK.md)** — eBPF-backed storage I/O risk levels for the Ops Advisor and agents.
- **[HERMES_AGENT.md](HERMES_AGENT.md)** — the Hermes agent integration.

## Product integration
- **[PRODUCTS.md](PRODUCTS.md)** — `Owner`/gRPC integration conventions; live integrations (Kryton, Zorvia, Fabric, Relay).

## Project
- **[ROADMAP.md](ROADMAP.md)** — what's shipped and what's deferred, slice by slice.
- **[STATUS.md](STATUS.md)** — the short maturity matrix: what's verified on real infrastructure vs. lab-only. When ROADMAP.md and this disagree, STATUS.md wins.
- **[LICENSING.md](LICENSING.md)** — Apache License 2.0; SPDX / CLA / DCO.
- **[../CONTRIBUTING.md](../CONTRIBUTING.md)** — conventions; how to add an endpoint / driver / migration.
- **[../SECURITY.md](../SECURITY.md)** — supported versions, how to report a vulnerability.
- **[../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md)** — community conduct.
- **[../CLA.md](../CLA.md)** · **[../DCO.md](../DCO.md)** · **[../NOTICE](../NOTICE)** — contribution + attribution.

## Customer docs
The [customer docs tree](customer/README.md) is a separate, task-oriented guide for operators
using the console: [getting-started](customer/getting-started.md),
[admin-basics](customer/admin-basics.md), [using-the-dashboard](customer/using-the-dashboard.md),
[workflows](customer/workflows.md), plus a per-page reference
([customer/pages/](customer/PAGE_INDEX.md)) and a PDF export index
([customer/pdf/PDF_INDEX.md](customer/pdf/PDF_INDEX.md)). There's also a narrative
[atlas-customer-feature-guide.md](atlas-customer-feature-guide.md) covering the same ground as one
long read.

## Deploy assets
- **[DEPLOYMENT.md](DEPLOYMENT.md)** — end-to-end k3s deploy; version lockstep + pitfalls.
- **[../deploy/rook-ceph-lab/README.md](../deploy/rook-ceph-lab/README.md)** — Rook Ceph + KubeVirt/CDI lab (`up.sh --single-node`).
- `../deploy/rook-ceph-lab/single-node/` — single-OSD overlay for a one-node k3s.
- `../deploy/k8s/` — gateway Deployment/RBAC/Service (fake + real ceph variants) and the `atlas-io-agent` DaemonSet.
- **[../deploy/helm/atlas/README.md](../deploy/helm/atlas/README.md)** — production Helm chart.
- **[../scripts/README.md](../scripts/README.md)** — `deploy-remote.sh` / `deploy-ceph-gateway-remote.sh`.

## Per-crate docs
Each crate with its own `README.md` is linked; the rest are plain workspace members without one yet.

| Crate | Role |
|---|---|
| [`atlas-common`](../crates/atlas-common/README.md) | config, error type, tracing, id helpers |
| [`atlas-api-types`](../crates/atlas-api-types/README.md) | shared serde DTOs (the contract) |
| [`atlas-driver-core`](../crates/atlas-driver-core/README.md) | `StorageDriver` trait + registry |
| [`atlas-driver-ceph`](../crates/atlas-driver-ceph/README.md) | real + fake Ceph drivers |
| `atlas-driver-nfs` | `NfsDriver` — second backend, fixture-only MVP |
| `atlas-driver-zfs` | `ZfsDriver` — third backend, fixture-only MVP + raw-disk provisioning |
| `atlas-driver-rgw` | S3 client for RGW buckets/backups; the default object backend |
| `atlas-driver-longhorn` | read-only Longhorn driver |
| [`atlas-driver-k8s`](../crates/atlas-driver-k8s/README.md) | live Kubernetes driver |
| `atlas-jobs` | async job engine (durable DB queue, single worker) |
| `atlas-policy` | intent → StorageClass + access/volume mode |
| `atlas-monitor` | discovery + alert-rule worker, Ceph mgr metrics scrape |
| [`atlas-inventory`](../crates/atlas-inventory/README.md) | SQLite/Postgres read/upsert model + audit |
| [`atlas-discovery`](../crates/atlas-discovery/README.md) | discovery worker |
| `atlas-databridge` | cloud-to-edge DB + object migration control plane |
| [`atlas-gateway`](../crates/atlas-gateway/README.md) | axum server (bin) |
| [`atlas-io`](../crates/atlas-io/README.md) | optional node sensor (bin `atlas-io-agent`), see [IO_EBPF.md](IO_EBPF.md) |
| [`atlasctl`](../crates/atlasctl/README.md) | REST client |

## Design authority
`Zyvor_Ceph_Integration_Developer_Implementation_Plan.pdf` — section refs (e.g. "PDF §10.2")
throughout the code/docs point back to it.
