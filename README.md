<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
<div align="center">

# Atlas

**One API for the storage you have. A filesystem for the storage you need.**

[![CI](https://img.shields.io/github/actions/workflow/status/zyvorai/zyvor-atlas/ci.yml?branch=main&style=flat-square&labelColor=1d1d1f&label=CI)](https://github.com/zyvorai/zyvor-atlas/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-0071e3?style=flat-square&labelColor=1d1d1f)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.7.0-0071e3?style=flat-square&labelColor=1d1d1f)](CHANGELOG.md)
[![Rust](https://img.shields.io/badge/Rust-gateway%20%C2%B7%20drivers%20%C2%B7%20native%20fs-dea584?style=flat-square&labelColor=1d1d1f&logo=rust&logoColor=white)](Cargo.toml)
[![Docs](https://img.shields.io/badge/docs-zyvorai.github.io%2Fzyvor--atlas-0071e3?style=flat-square&labelColor=1d1d1f)](https://zyvorai.github.io/zyvor-atlas/)

[![Book a demo](https://img.shields.io/badge/Book_a_demo-0071e3?style=for-the-badge)](https://zyvor.dev/schedule?utm_source=github&utm_medium=atlas&utm_campaign=readme_hero)
[![30-day PoC](https://img.shields.io/badge/30--day_PoC-000000?style=for-the-badge)](https://zyvor.dev/poc?utm_source=github&utm_medium=atlas&utm_campaign=readme_hero)
[![Docs and site](https://img.shields.io/badge/Docs_and_site-2ec4b6?style=for-the-badge)](https://zyvorai.github.io/zyvor-atlas/)

![Atlas - one API for the storage you have, a filesystem for what you need](docs/social/atlas-hero-dark.jpg)

<img src="docs/ux/anim/hero.gif" alt="Atlas Storage Center: overview, volumes, observatory, Ceph, DataBridge, alerts and jobs" width="100%">

**Atlas is two things in one Apache-2.0 platform.** A **control plane** that maps volume, snapshot and bucket intent to Ceph, NFS, ZFS, Longhorn and a Mac's own APFS disks through pluggable drivers. And **Atlas Native**, a distributed filesystem with its own data path: striped and erasure-coded extents, sharded Raft metadata, NFS, SMB and S3 gateways, replication to a second site, and a single-node edge profile.

**4 backends + Native** · **150+ REST endpoints** · **REST · gRPC · SSE** · **Two-way DR, clean failback** · **6 DB engines in DataBridge**

[**Quickstart**](#quickstart) · [**Docs**](https://zyvorai.github.io/zyvor-atlas/) · [**Architecture**](docs/ARCHITECTURE.md) · [**API**](docs/API.md) · [**DR**](docs/DR.md) · [**Status**](docs/STATUS.md) · [**Comparison**](docs/COMPARISON.md)

</div>

---

## Why teams pick Atlas

| When this happens… | Atlas gives you… |
|---|---|
| Every product talks to Ceph pools, NFS exports and ZFS datasets its own way | One intent API (volumes, snapshots, clones, CephFS RWX, S3 buckets) over REST and gRPC |
| You need a scale-out filesystem but not another vendor's appliance | Atlas Native: your own nodes, Raft metadata, erasure coding, NFS/SMB/S3/CSI, under Apache-2.0 |
| A second site exists, but nobody trusts the failback | Two-way RBD mirroring and native replication, with a promote guard and a status endpoint that says when it is safe |
| Edge sites have one small box and no storage team | The Native edge profile: one pod, small caches, replication or snapshot export back to the core |
| Tenants share storage with no guardrails | Per-tenant quotas, rate limiting, OIDC/SSO, Vault-backed secrets and token revocation |
| Databases must move from the cloud to the edge | DataBridge: discovery, full load, CDC and cutover onto edge databases on Ceph |
| You want to try it without a storage cluster | `make run` starts the gateway against a fake driver so you can click through the whole console |

![Capabilities at a glance: Intent, Drivers, Data, Native, Day 2](docs/ux/readme-capabilities.jpg)

---

## One API, every backend

<img src="docs/ux/anim/control-plane.svg" alt="Animated control plane: one intent API mapped to Ceph, NFS, ZFS, Longhorn and Atlas Native drivers" width="100%">

Products call stable Atlas APIs, and the `StorageDriver` trait maps the intent to a backend. Drivers cover Ceph (RBD, CephFS, RGW), NFS, ZFS, read-only Longhorn, macOS disks (APFS containers and volumes, external drives, a fleet of Macs through a read-only agent, guarded erase/add/delete/eject on external disks; [docs](docs/MACOS.md)) and Atlas Native, each with fake and real modes; real drivers never fabricate data. Around it: **DataBridge** cloud-to-edge database migration, **Day 2** operations (alerts, maintenance, upgrade preflight, quotas, audit export), and a read-only **Ops Advisor** that scores risk and plans capacity, also exposed as MCP tools. [Architecture →](docs/ARCHITECTURE.md)

## Agent-ready operations

Atlas speaks the [Model Context Protocol](https://modelcontextprotocol.io) so an AI agent can look at your storage the way an operator would, with the same roles and tenant limits as the REST API. The gateway's MCP endpoint (`/api/atlas/v1/mcp`) exposes 27 **read-only** tools: inventory, the Ops Advisor (risk, incidents, anomalies, what-if), eBPF-backed I/O risk, Ceph status, DR status and preflight, DataBridge, the audit trail, jobs and snapshots. The `atlas-io` node agent serves its own authenticated `/mcp` with six I/O tools. The only writes are opt-in (`ATLAS_MCP_WRITE_ENABLE=1`): `create_snapshot` and `ack_alert` (record that an operator saw an alert), each two-phase with a single-use, five-minute confirmation token bound to the caller and arguments, and audited. Failover, delete, restore, disk wipe and token minting are never exposed. MCP can never trigger a call to an external LLM. [MCP guide →](docs/MCP.md) · [Ops Advisor →](docs/AI_ADVISOR.md) · [I/O risk →](docs/IO_RISK.md) · [Agent pack →](examples/agent)

## Run the filesystem yourself

<img src="docs/ux/anim/native-io.svg" alt="Animated Atlas Native data path: client I/O striped across nodes with Raft metadata and an S3 cold tier" width="100%">

Atlas Native is a distributed filesystem Atlas runs itself: striped, checksummed, copy-on-write extents with direct client reads and `io_uring` on raw devices; the namespace sharded across Raft groups; 3 replicas or Reed-Solomon k+m with a rebuild controller; cold extents tiered to S3; POSIX over FUSE, NFS, SMB, S3 and a Kubernetes CSI driver; quotas, POSIX ACLs and a single-node edge profile. [Native storage →](docs/NATIVE_STORAGE.md)

## Fail over, fail back cleanly

<img src="docs/ux/anim/dr-failover.svg" alt="Animated cross-site DR: replication, failover and a guarded failback" width="100%">

`GET /dr/mirrors/{id}/status` shows each site's live `rbd mirror image status` and whether a promote would be clean. Atlas refuses a non-forced promote until the local `rbd-mirror` has replayed the peer's demotion, and `?force=1` stays available for real disasters. Native clusters get asynchronous replication with read-only replicas, promote and demote. [DR guide →](docs/DR.md) · [Native replication →](docs/NATIVE_REPLICATION.md)

---

## Proof, not promises

Everything below ran on real infrastructure in the Atlas lab. Lab verification is not production support ([STATUS.md](docs/STATUS.md)).

- **Two-way RBD mirroring** between two Rook Ceph clusters, clean failback, unsafe promote refused, checksums matched at every step ([DR.md](docs/DR.md)).
- **Edge profile:** catalog memory flat at about 26 MB from 200k to 400k files, where defaults reach about 196 MB ([NATIVE_EDGE.md](docs/NATIVE_EDGE.md)).
- **Erasure coding:** about 1.5 GiB/s encode and 2 GiB/s two-shard rebuild per core on 4 MiB extents ([NATIVE_STORAGE.md](docs/NATIVE_STORAGE.md)).
- **NFS, SMB, S3 and CSI on Native** verified with real clients and a single-node k3s ([NATIVE_NFS.md](docs/NATIVE_NFS.md)).
- **DataBridge** through CDC and cutover for six source engines ([DATABRIDGE.md](docs/DATABRIDGE.md)).

**Not yet:** NVMe and RDMA benchmark numbers, an IO500 submission, GPUDirect Storage, and cross-site mirroring of RBD consistency groups. See [COMPARISON.md](docs/COMPARISON.md).

---

## Atlas vs Rook alone, and vs WEKA

| | **Atlas** | **Rook alone** |
|---|---|---|
| What it is | Storage control plane with drivers per backend, plus its own filesystem | Kubernetes operator that runs Ceph |
| Interface for products | Intent-based REST, gRPC and SSE APIs | Kubernetes custom resources and CSI PVCs |
| Backends | Ceph, NFS, ZFS, Longhorn, macOS disks, Atlas Native, generic S3 | Ceph |
| Cross-site DR | Two-way RBD mirroring with a guarded failback; native replication | RBD mirroring CRDs |
| Database migration | DataBridge (discovery, full load, CDC, cutover) | Not in scope |
| **Choose Rook alone when** | | You only run Ceph inside Kubernetes and its own dashboard and CRDs are enough |

Compared with a parallel filesystem such as WEKA, including where WEKA is the better choice today: [docs/COMPARISON.md](docs/COMPARISON.md).

## Application recovery drills

Capture coordinated recovery points, restore into new drill volumes, validate application
startup or Native block checksums, measure recovery objectives, and retain cleanup evidence.
The operator tool uses existing Atlas APIs and supports dependency ordering, bounded hooks,
and scheduled drills: [Application recovery guide](docs/APPLICATION_RECOVERY.md).

## AI data services (experimental)

The Python companion ships nine workflows together: shared model chunks, data-ready
job reservations, agent workspace branches, permission-aware discovery, exact-key
KV storage, immutable dataset releases, application recovery checks, per-job cost
estimates and optional Apache Iceberg maintenance. It uses a separate authenticated
API and CLI; external schedulers and serving engines integrate through explicit SDK
adapters. [Guide and limitations](docs/AI_DATA_SERVICES.md).

## The console

<div align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ux/night-00-overview.png">
  <img src="docs/ux/day-00-overview.png" alt="Atlas Storage Center overview" width="820">
</picture>
</div>

<table>
<tr>
<td width="33%"><img src="docs/ux/day-01-volumes.png" alt="Volumes"><br><sub>Volumes</sub></td>
<td width="33%"><img src="docs/ux/day-02-observatory.png" alt="Observatory"><br><sub>Observatory</sub></td>
<td width="33%"><img src="docs/ux/day-04-databridge.png" alt="DataBridge"><br><sub>DataBridge</sub></td>
</tr>
</table>

Full tour: [Gallery](https://zyvorai.github.io/zyvor-atlas/gallery). Screenshots come from a lab deployment.

## Quickstart

```bash
make run          # gateway with a fake driver, no Ceph needed
# Console: http://127.0.0.1:5110
```

Atlas Native on Kubernetes: `helm upgrade --install atlas-native deploy/helm/atlas-native -n atlas-native --create-namespace`, or add `-f deploy/helm/atlas-native/values-edge.yaml` for a one-pod edge site. More: [Getting started](docs/GETTING_STARTED.md) · [Deployment](docs/DEPLOYMENT.md) · [Native FS](docs/NATIVE_FS.md) · [Chart values](deploy/helm/atlas-native/README.md).

## What it is, and is not

Atlas is Apache-2.0 software you can use in production and in commercial products ([licensing](docs/LICENSING.md)). What is verified where, and what is lab-only (including the experimental live eBPF attach), is in [docs/STATUS.md](docs/STATUS.md). History: [CHANGELOG.md](CHANGELOG.md).

## Part of the Zyvor stack

**[Aether](https://github.com/zyvorai/Aether)** runtime portability plane · **[Zorvia](https://github.com/zyvorai/zyvor-zorvia)** KubeVirt VM platform · **[Kryton](https://github.com/zyvorai/zyvor-kryton)** · **[Transiva](https://github.com/zyvorai/zyvor-transiva)** VM migration · [zyvor.dev](https://zyvor.dev)

## Support

The current release is Apache-2.0 (older tags used other licenses, see the [CHANGELOG](CHANGELOG.md)). If it helps you, you can donate any amount you like: <https://rzp.io/rzp/utwdYh3>. A donation is voluntary and supports development and maintenance of the current code. It is not a purchase: it buys no support, SLA, features, priority or license, and it is not tax-deductible (Zyvor AI Labs Private Limited is a company, not a charity). Zyvor Enterprise (supported releases, SLAs, a named contact and enterprise features) is separate: sales@zyvor.dev.

## License and support

Free and open source under the [Apache License 2.0](LICENSE) ([NOTICE](NOTICE)). **Zyvor Enterprise** adds supported releases, deployment and upgrade guidance, priority incident triage, a named technical contact and 24x7 critical intake: [plans](docs/SUBSCRIPTION-MODEL.md) · [pricing](https://zyvor.dev/pricing?utm_source=github&utm_medium=atlas&utm_campaign=readme_license) · [sales@zyvor.dev](mailto:sales@zyvor.dev).

Contributing: [CONTRIBUTING.md](CONTRIBUTING.md), under the [CLA](CLA.md) and [DCO](DCO.md) (`git commit -s`), governed by the [Code of Conduct](CODE_OF_CONDUCT.md). Security issues: [SECURITY.md](SECURITY.md).

<div align="center">

### Put every storage backend behind one API, and run the filesystem yourself

[![Book a demo](https://img.shields.io/badge/Book_a_demo-0071e3?style=for-the-badge)](https://zyvor.dev/schedule?utm_source=github&utm_medium=atlas&utm_campaign=readme_footer)
[![30-day PoC](https://img.shields.io/badge/Start_a_30--day_PoC-000000?style=for-the-badge)](https://zyvor.dev/poc?utm_source=github&utm_medium=atlas&utm_campaign=readme_footer)
[![Pricing](https://img.shields.io/badge/Pricing-1d1d1f?style=for-the-badge)](https://zyvor.dev/pricing?utm_source=github&utm_medium=atlas&utm_campaign=readme_footer)
[![Contact sales](https://img.shields.io/badge/Contact_sales-2ec4b6?style=for-the-badge)](mailto:sales@zyvor.dev?subject=Atlas)
[![Star on GitHub](https://img.shields.io/github/stars/zyvorai/zyvor-atlas?style=for-the-badge&logo=github&label=Star&color=2997ff)](https://github.com/zyvorai/zyvor-atlas)

</div>
