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

## Crimson

Crimson is a Seastar-based rewrite of `ceph-osd`. The Red Hat article (January 2021) calls it
unsupported and gives no requirements for control planes, so Atlas does nothing Crimson-specific.
