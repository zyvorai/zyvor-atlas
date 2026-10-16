<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Architecture

Atlas is a storage **control plane**: a stable API in front of pluggable storage backends.
Its job is to let every Zyvor product consume storage by *intent* while Atlas owns the messy
parts — backend drivers, discovery, normalized inventory, ownership mapping, and audit.

## Why a control plane (and not per-product Ceph clients)

Before Atlas, storage logic was scattered: `hyper2kvm` shipped its own ceph-csi StorageClasses,
`machina` shelled out to `rbd`, `v9s` drove raw PVCs, and everyone else passed a bare
`storage_class` string. That duplicates logic and couples every product to Ceph.

Atlas centralizes it:

- **One API** — products never talk to Ceph directly.
- **Pluggable drivers** — Ceph, NFS, ZFS, and Longhorn, each with a fake (fixture,
  zero-dependency default) and real mode where applicable; SAN/cloud backends later, with no
  product changes. Object storage defaults to Ceph RGW; any S3-compatible endpoint is usable as a
  bring-your-own backend via `atlas-driver-rgw` (Atlas dropped its first-party RustFS integration —
  see `docs/RUSTFS.md`).
- **Backend-agnostic model** — a `StorageVolume` may be backed by RBD now and SAN tomorrow.
- **Ownership + audit** — every volume maps to product/resource/tenant/policy/pool/cluster.
- **Explainable AI Ops Advisor** — deterministic risk scoring, incident correlation, what-if
  capacity planning, and anomaly detection layered on the same inventory, read-only
  (`can_execute: false`). The gateway exposes it, plus inventory, I/O risk, DR and job state, as 20
  read-only MCP tools (and one opt-in guarded `create_snapshot`); the `atlas-io` agent serves 6 more at its
  own authenticated `/mcp` — see [MCP.md](MCP.md) and [AI_ADVISOR.md](AI_ADVISOR.md).

## Crate topology

```
atlas-api-types  ─────────────┐ (DTOs: the shared contract)
        ▲                      │
        │                      ▼
atlas-common            atlas-driver-core ──► StorageDriver trait, DriverError, DriverRegistry
(config/error/ids)              ▲   ▲   ▲   ▲
        ▲                       │   │   │   │
        │            ┌──────────┘   │   │   └──────────┐
        │      atlas-driver-ceph    │   │         atlas-driver-k8s
        │      (real + fake)        │   │         (live StorageClass/PVC/PV)
        │            │       atlas-driver-nfs  atlas-driver-zfs
        │            │       (real + fake)     (real + fake)
        │            ▼
        │      atlas-driver-rgw (S3 client for RGW buckets/backups)
        │            ▲
        │      atlas-jobs (async job engine)  atlas-policy (intent → placement)
        │            ▲
        │      atlas-inventory (AnyPool model + audit: SQLite or Postgres)
        │            ▲
        │      atlas-discovery (driver → inventory)   atlas-monitor (alert rules + metrics scrape)
        │            ▲
        └───── atlas-gateway (axum server: state, auth, routes, ai.rs, mcp.rs)  ◄── atlasctl
                     ▲
               atlas-databridge (DB/object migration control plane, layered on the same job engine)
```

Dependency rule: the gateway and discovery worker depend only on the **`StorageDriver` trait**,
never on a concrete backend. Adding a backend = implementing the trait + registering it.

**DataBridge** (`atlas-databridge`) layers a cloud-to-edge DB migration control plane on the same
foundation — it reuses the job engine, inventory (`migrations/0011_databridge.sql`), gateway, and
console, adds source connectors + operator/Debezium CR builders, and a periodic **reconciler** (mirrors
`spawn_scheduler`) that advances long-running pipeline work (edge CR readiness, full-load/validation
Jobs, CDC health). It calls Atlas to provision Ceph-backed storage for the edge databases. See
[DATABRIDGE.md](DATABRIDGE.md).

**`atlas-io`** is a separate, optional node agent (`atlas-io-agent`) — deliberately *not* a crate the
gateway depends on, so the control plane never needs `CAP_BPF`/`CAP_PERFMON`. It runs its own HTTP API
(`/io/{health,summary,histograms,workloads,rca,leases,coverage,native,poll}`, `/metrics`) that the
gateway or an operator can poll; fake bio source by default, real CO-RE `block_rq_*` attach behind the
`bpf` feature (latency, queue time, pid/cgroup attribution, kernel-aggregated Atlas Native I/O; if
attach fails, live mode reports missing programs rather than fabricating data). See [IO_EBPF.md](IO_EBPF.md).

## The driver contract

`atlas-driver-core::StorageDriver` (PDF §17.2):

```rust
#[async_trait]
pub trait StorageDriver: Send + Sync {
    fn backend_id(&self) -> &str;
    async fn discover(&self) -> Result<DiscoveryResult, DriverError>;
    async fn health(&self)   -> Result<StorageHealth, DriverError>;
    async fn list_pools(&self)               -> Result<Vec<StoragePool>, DriverError>;
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError>;
    async fn metrics(&self)  -> Result<Vec<MetricSample>, DriverError>;
    // write path (MVP slice 2) — default to Err(DriverError::NotImplemented)
    async fn create_volume / expand_volume / delete_volume / create_snapshot / clone_snapshot / delete_snapshot
}
```

Read methods are live in slice 1. Write methods default to `NotImplemented` so slice 2 is
purely additive.

### Drivers today

| Driver | Crate | What it does | Notes |
|---|---|---|---|
| Real Ceph | `atlas-driver-ceph` | Parses `ceph status`, `ceph df detail`, `ceph osd tree`, `rbd ls -l` into DTOs | Arg-arrays only, `--format json`. `ATLAS_CEPH_DRIVER_MODE=real`. |
| Fake Ceph | `atlas-driver-ceph` | Deterministic fixtures | For local dev/tests/demo. `ATLAS_CEPH_DRIVER_MODE=fake` (default). |
| Real NFS | `atlas-driver-nfs` | `showmount -e` for exports → pools; `/proc/mounts` + `df` for capacity when the export happens to already be locally mounted, `None` otherwise | `ATLAS_NFS_DRIVER_MODE=real`. Never fabricates: an export the server doesn't have is dropped, not invented; unreachable → `DriverError::Unreachable`. Needs `nfs-common`/`showmount` on `PATH`. |
| Fake NFS | `atlas-driver-nfs` | Deterministic fixtures | `ATLAS_NFS_DRIVER_MODE=fake` (default). |
| Real ZFS | `atlas-driver-zfs` | `zpool list -Hp`/`zfs list -Hp` **locally**, on whatever host the gateway process itself runs on | `ATLAS_ZFS_DRIVER_MODE=real`. No remote/SSH support yet — ZFS has no remote query protocol the way `ceph`/`rbd` does. Never fabricates. Needs `zfsutils-linux`. Also the target of `POST /zfs/pools/from-device` (raw-disk provisioning, see `docs/DISKS.md`). |
| Fake ZFS | `atlas-driver-zfs` | Deterministic fixtures | `ATLAS_ZFS_DRIVER_MODE=fake` (default). |
| Ceph RGW (object) | `atlas-driver-rgw` | Generic SigV4 S3 client (`S3Target`) for bucket/object CRUD, presigned URLs | Default backend for `POST /buckets` (`bkd_ceph_lab`, via Rook `ObjectBucketClaim`). Also used for any bring-your-own S3-compatible endpoint (MinIO, Garage, AWS, a customer-run RustFS) — see `docs/RUSTFS.md` for the history of Atlas's now-removed first-party RustFS integration. |
| Longhorn | `atlas-driver-longhorn` | Read-only, over Longhorn's Kubernetes v1beta2 CRDs (`nodes`/`volumes`) | `ATLAS_LONGHORN_ENABLE=1`. No native write path — PVC provisioning goes through the Kubernetes StorageClass path, not this driver. See `docs/LONGHORN.md`. |
| atlas-native | `atlas-driver-native` | `atlas-native-node` HTTP API (`/v1/status`, `/v1/volumes`, snapshots): one cluster, one replicated `native` pool, block volumes; create/expand/delete volumes, create/delete snapshots, clone/restore from a snapshot (copy-on-write); block reads/writes (`/volumes/{id}/data`) | `ATLAS_NATIVE_ENABLE=1` registers `bkd_native`; `ATLAS_NATIVE_DRIVER_MODE=real` + `ATLAS_NATIVE_ENDPOINTS` for a live cluster (fake keeps volumes in memory). The only driver whose write methods the gateway calls: `POST /volumes` with `kubernetes.backend_id: bkd_native`, expand, and the snapshot/clone/restore routes run synchronously through it. See `docs/NATIVE_NODE.md`. |
| Kubernetes | `atlas-driver-k8s` | Lists StorageClasses / PVCs / PVs via `kube-rs`; tags Ceph-backed classes | Always live when a cluster is reachable. Also the mechanism behind `POST /ceph/devices` (patches the `CephCluster` CR for raw-disk-to-OSD provisioning, see `docs/DISKS.md`). |

## Request flow (discovery)

```
POST /api/atlas/v1/backends/{id}/discover
        │
        ▼
gateway: resolve driver from DriverRegistry ──► atlas-discovery::run_discovery
        │                                              │
        │                                              ▼
        │                                    driver.discover()  (ceph/rbd CLI or fixtures)
        │                                              │
        │                                              ▼
        │                             atlas-inventory::upsert_discovery (SQLite, one tx)
        │                                   cluster → pools → osds → volumes
        ▼
audit row (storage_audit_logs) + JSON summary back to caller
```

Reads (`/pools`, `/volumes`, `/clusters`, …) come straight from the inventory (SQLite or Postgres,
selected by `ATLAS_DATABASE_URL`'s scheme).
`/storage-classes`, `/kubernetes/pvcs`, `/kubernetes/pvs` are served **live** from the k8s driver.

## Data model (SQLite or Postgres, via `sqlx::Any`)

The query layer runs against `sqlx::AnyPool`: the same `$N`-placeholder SQL text dispatches to
either backend, auto-detected from `ATLAS_DATABASE_URL`'s scheme (`sqlite://` default,
`postgres://` for HA/multi-replica — see [HA.md](HA.md)). Schema is forked into two migration
trees kept in lock-step (`migrations/` SQLite, `migrations-postgres/` Postgres dialect; CI fails
if their highest migration numbers diverge) using a documented type-mapping convention
(`TIMESTAMPTZ`→RFC3339 `TEXT`, `JSONB`→`TEXT` + `json_valid()` CHECK, `BIGSERIAL`→`INTEGER PK
AUTOINCREMENT`/`BIGSERIAL`, `BIGINT`→`INTEGER`). Core tables:

| Table | Purpose |
|---|---|
| `storage_backends` | Registered backends (type, mode, capabilities, secret **reference** only) |
| `storage_clusters` | Discovered clusters (fsid, capacity, health) |
| `storage_pools` | Normalized pools (kind rbd/cephfs/rgw, device class, replica size) |
| `storage_osds` | OSD up/in state, device class, host |
| `storage_volumes` | Zyvor volume abstraction (FK → cluster/pool) |
| `product_bindings` | Ownership: which product/resource/role owns a volume/bucket/share |
| `storage_snapshots` | Snapshot metadata + parent/clone dependency (slice 2) |
| `storage_jobs` | Async job records (slice 2) |
| `storage_policies` | Intent → placement/protection/backup/security (slice 2) |
| `storage_alerts` | Alert records (slice 2) |
| `storage_audit_logs` | Every sensitive action (PDF §14.3) |

Ownership mapping (`product_bindings`) is what lets Atlas answer "which VM owns this RBD image?"
and "which OSDs affect this VM?" — required for RCA, billing, and safe cleanup.

## Security posture

- **Secret references, never secrets** — backends store a reference (Kubernetes Secret / Vault
  path); no keyrings/keys in Atlas tables (PDF §14.1).
- **Real Ceph credentials stay in-cluster** — the real-mode gateway renders `/etc/ceph` from the
  Rook mon Secret via an initContainer (keyring mode `0600`, process runs as uid `10001`); the
  admin key never leaves the cluster.
- **JWT auth** — cluster manifests set `ATLAS_AUTH_REQUIRED=1` and load `ATLAS_JWT_SECRET` from
  Secret `atlas-gateway-auth`. Local `make run` stays open. The process **refuses to start** if
  auth is required and the JWT secret is the weak shipped default (or &lt; 32 bytes).
- **Bootstrap admin** — optional `ATLAS_BOOTSTRAP_ADMIN_TOKEN` (raw bearer) lets a fresh deploy mint
  lasting service-account JWTs; remove it from the Secret after first use.
- **Audit everything** — discovery, backend registration, and write actions.

## Configuration

All via environment (see `.env.example`), loaded by `atlas-common::Config::from_env()` with
secret-redacting `Debug`:

| Env | Default | Meaning |
|---|---|---|
| `ATLAS_BIND_ADDR` | `127.0.0.1:5110` | Gateway listen address |
| `ATLAS_DATABASE_URL` | `sqlite://atlas.db?mode=rwc` | SQLite URL (WAL + FK on), or `postgres://...` for HA/multi-replica — see [HA.md](HA.md) |
| `ATLAS_CEPH_DRIVER_MODE` | `fake` | `real` (ceph/rbd CLI) or `fake` (fixtures) |
| `ATLAS_NFS_DRIVER_MODE` / `ATLAS_ZFS_DRIVER_MODE` | `fake` | `real` (live `showmount`/`zpool`/`zfs` discovery) or `fake` (fixtures) |
| `ATLAS_S3_CA_CERT` | *(unset)* | Path to a mounted CA cert to additionally trust for any S3-compatible endpoint (private CA / self-signed) — never disables verification, only adds one root |
| `ATLAS_LONGHORN_ENABLE` | `0` | Register the read-only Longhorn backend (see `docs/LONGHORN.md`) |
| `ATLAS_NATIVE_ENABLE`, `ATLAS_NATIVE_DRIVER_MODE`, `ATLAS_NATIVE_ENDPOINTS`, `ATLAS_NATIVE_TOKEN_FILE`, `ATLAS_NATIVE_CA_CERT`, `ATLAS_NATIVE_CLIENT_CERT`/`_KEY`, `ATLAS_NATIVE_TIMEOUT_SECS` | off / `fake` / – / – / – / – / `10` | Register the atlas-native backend `bkd_native` (see `docs/NATIVE_NODE.md`, "Atlas gateway") |
| `ATLAS_HOST_MOUNTINFO_PATH` | *(unset)* | Path to a hostPath-mounted `/proc/1/mountinfo` (e.g. `/host-mountinfo`) so the ZFS root/boot-disk safety check can see the *host's* real mount table from inside a container, not just its own mount namespace — see `docs/DISKS.md` |
| `ATLAS_RATE_LIMIT_RPM` | `600` | Per-actor per-minute request budget |
| `ATLAS_RATE_LIMIT_SYNC_SECS` | `2` | Cross-replica rate-limit counter sync interval (Postgres only) |
| `ATLAS_STATE_BACKUP_SECS` | *(unset)* | Interval for periodic self-state backup to any S3-compatible endpoint (the bucket is created on first use) |
| `ATLAS_KUBECONFIG` | *(unset)* | Explicit kubeconfig; else in-cluster/default |
| `ATLAS_JWT_SECRET` | dev default | HS256 secret (≥32 bytes required when auth is on) |
| `ATLAS_AUTH_REQUIRED` | `0` (local) / `1` (k8s) | Require JWT on `/api` routes |
| `ATLAS_BOOTSTRAP_ADMIN_TOKEN` | *(unset)* | One-shot admin bearer for first mint |
| `ATLAS_AI_BASE_URL` / `ATLAS_AI_MODEL` | *(unset)* | Optional OpenAI-compatible provider for the AI Ops Advisor — see [AI_ADVISOR.md](AI_ADVISOR.md) |
| `RUST_LOG` | `info` | tracing filter |
