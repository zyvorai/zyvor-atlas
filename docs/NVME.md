<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# NVMe in Atlas

What Atlas does with NVMe drives under Ceph. Everything here is read-only except the optional
BlueStore layout on `POST /ceph/devices`. Verified by unit tests and the lab CI gates only; none of
it has been run against a live Rook cluster or real NVMe hardware yet.

## Drive health: `GET /ceph/nvme`

Wear, spare, media errors and temperature for each NVMe drive Ceph tracks, from the SMART scrapes
its device-health module holds (`ceph device get-health-metrics`). A drive is listed with
`concerns` when it reports a critical-warning flag, `percentage_used` of 80 or more, any media
error, or available spare at or below its threshold. A drive Ceph has not scraped is reported as
`scraped: false`, never as healthy. At most 64 drives are queried per request.

The Ops Advisor reads the same data (`docs/AI_ADVISOR.md`). CLI: `atlasctl ceph-nvme`; MCP:
`ceph_nvme`.

Assumption not yet checked live: that the scrape JSON is smartctl's, with
`nvme_smart_health_information_log`.

## Device class and DB/WAL placement

`GET /ceph/nodes/{node}/devices` tags each discovered device `class: nvme|ssd|hdd`.
`POST /ceph/devices` can put an OSD's RocksDB/WAL on a separate device. See `docs/DISKS.md`.

## NVMe-oF gateways: `GET /ceph/nvmeof`

Lists Rook `CephNVMeOFGateway` CRs with instance count, pod readiness and findings (a single
gateway has no high availability), plus the StorageClasses of the Ceph-CSI NVMe-oF driver.
`installed: false` means the cluster has no such CRD. Needs a Kubernetes cluster attached. CLI:
`atlasctl ceph-nvmeof`; MCP: `ceph_nvmeof`.

Per Rook's v1.20 documentation (not checked against a running cluster): the CRD arrived in Rook
v1.19, is experimental, and needs Ceph Tentacle (v20) or later. An NVMe namespace is created by a
PVC from an `nvmeof.csi.ceph.com` StorageClass, and deleting that PVC deletes the RBD image and the
namespace. So Atlas does not offer to map an existing RBD volume into a subsystem: Rook has no
supported path for it.

### Provisioning a namespace: `POST /ceph/nvmeof/namespaces`

Creates a new namespace by creating a PVC from one of the listed NVMe-oF StorageClasses. Admin
only, `confirm: true` required, audited. Body: `tenant_id`, `name` (the PVC), `storage_class`,
`size_bytes` (1 to 64 TiB), optional `namespace` (default `default`). A class that is not an
`nvmeof.csi.ceph.com` class is refused. It runs as the ordinary `volume.create` job (tenant quota,
cordon and idempotency apply) with `volumeMode: Block` and `ReadWriteOnce`; the volume then shows in
inventory and `DELETE /volumes/{id}` removes it, which per Rook also removes the RBD image and the
namespace. CLI: `atlasctl ceph-nvmeof-create <name> --storage-class <sc> --size-gib N --yes`. There
is deliberately no MCP tool: like disk operations, this is a human action.

Not verified live: the block mode and access mode are assumptions about the Ceph-CSI NVMe-oF
driver, and Atlas does not yet read back the subsystem NQN or host connection details.

## Console: Fast storage

The console's **Fast storage** page (Infrastructure, operator) shows these reports in one place: NVMe wear, spare
and media errors per drive, hosts against the sizing rule of thumb, the OSD object store and version mix, and the
atlas-io agent's I/O shapes. It is read-only and reads the same endpoints as above plus `GET /io/ai`, which returns
the agent's heuristic shapes (`status: "unavailable"` when `ATLAS_IO_AGENT_URL` is unset or the agent is
unreachable; the page then says the I/O layer was not assessed, never that it is healthy).

## OSD object store and versions: `GET /ceph/osd-objectstore`

What each OSD reports in `ceph osd metadata`: object store and Ceph version, with counts and a
`mixed_versions` flag for rolling upgrades. Keys an OSD does not report are `null` and counted as
`unknown`. Atlas infers nothing from these about the OSD implementation. CLI:
`atlasctl ceph-osd-objectstore`; MCP: `ceph_osd_objectstore`.

## Sizing: `GET /ceph/sizing`

For each host with NVMe drives, compares Kubernetes node capacity with a rule of thumb for
IOPS-optimized NVMe clusters: 10 CPU cores per NVMe device, and 16 GiB of RAM plus 5 GiB per OSD.
These are heuristics from Red Hat sizing guidance quoted to us, not requirements, and not checked
against a primary source. Override them with `ATLAS_SIZING_CORES_PER_NVME`,
`ATLAS_SIZING_RAM_BASE_GIB` and `ATLAS_SIZING_RAM_PER_OSD_GIB`. A host is `ok`, `undersized` (with
the shortfalls listed) or `unknown`: with no Kubernetes cluster attached, or when no node matches
the Ceph host name (tried as-is and with dots turned into dashes), nothing is judged. Capacity is
the node's hardware capacity, not what is left for Ceph. CLI: `atlasctl ceph-sizing`; MCP:
`ceph_sizing`; the Ops Advisor flags undersized hosts.

## Crimson

Crimson is a Seastar-based rewrite of `ceph-osd`. The Red Hat article (January 2021) calls it
unsupported and gives no requirements for control planes, so Atlas does nothing Crimson-specific.
