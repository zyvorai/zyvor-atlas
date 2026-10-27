<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas — Zyvor Storage Control Plane

Atlas is the central storage control plane for the Zyvor product suite. Products (Zeus OS/v9s,
Veyron, Transiva, GuestKit, PacketWolf, Aether, Ragnarok, Machina, HyperSDK) call **Atlas APIs**;
Atlas talks to storage backends through **pluggable drivers**. Ceph is the first driver.

Design authority: `Zyvor_Ceph_Integration_Developer_Implementation_Plan.pdf` (v1.0 engineering draft).

## Current state (slices 1–5 done; verified on real Rook Ceph)
Implemented:
- `atlas-*` cargo workspace, query layer on `sqlx::Any` (`AnyPool`) — SQLite (default, zero-dependency)
  or Postgres (`ATLAS_DATABASE_URL=postgres://...`, HA/multi-replica) selected by URL scheme at
  startup, same SQL/migrations tree logic against both (`migrations/` SQLite,
  `migrations-postgres/` Postgres dialect) — axum 0.8 REST + `tonic` gRPC; async **job engine**.
- **Pluggable backends** behind `StorageDriver`, each with a `fake`/`real` `DriverMode` (fixture-only
  by default, zero external dependency): Ceph (`ceph`/`rbd` CLI), **NFS** (`showmount`/`df`),
  **ZFS** (local `zpool`/`zfs list`) and read-only **Longhorn** — plus a live K8s driver. Real drivers
  never fabricate data; they propagate a real error when the target is unreachable. Discovery worker
  → inventory. **Object storage defaults to Ceph RGW** (`bkd_ceph_lab`, Rook `ObjectBucketClaim`);
  any S3-compatible endpoint (MinIO, Garage, AWS, ...) is usable as a
  bring-your-own backend via the generic `atlas-driver-rgw::S3Target` client — see `docs/RUSTFS.md`
  for the history of Atlas's now-removed first-party RustFS product integration (Helm chart, admin
  proxy, console pages, drive/instance install jobs) and what replaced it. CI gates run on the lab
  host via `scripts/ci-remote.sh` (never locally).
- **Raw disk provisioning** (`docs/DISKS.md`): Disks console page + `GET /zfs/devices`,
  `GET /ceph/nodes/{node}/devices` pickers, `POST /zfs/pools/from-device` (with explicit
  `wipe_existing`) and `POST /ceph/devices`. **ZFS verified live on a real disk** (wiped a stale Ceph
  OSD signature, created a pool). The gateway pod needs `privileged` + hostPath `/dev`, `/run/udev`
  and `/proc/1/mountinfo` for this (`deploy/k8s/atlas-gateway.yaml`); active-pool members, the
  root/boot disk and mounted devices are hard refusals that `wipe_existing` can never override.
  `POST /zfs/pools/{name}/destroy` (Disks page **Destroy…**, typed-name confirm) tears a pool down
  again (verified live: destroyed `tank0`, re-provisioned `sdb` as `tank1`).
- Write path: volumes (PVC + direct RBD), snapshots/clone/restore, CephFS RWX, buckets (Ceph RGW default, BYO S3) + backups
  (`export-diff`→S3, retention, presigned), scheduled snapshots/backups, per-tenant quotas + policies.
- Observability: monitor/alerts + webhook, `/metrics` (Prometheus self), `/metrics/{history,forecast,ceph}`,
  Ceph-native `/ceph/{status,osd-tree,osd-df,df}`, unified `/events`, `/readyz`, OpenTelemetry
  tracing (`docs/TRACING.md`); `deploy/observability/`.
- **AI Ops Advisor** (`docs/AI_ADVISOR.md`): explainable, read-only (`can_execute: false`) risk
  scoring and runbook (`/api/atlas/v1/ai/advisor`, local/auto/LLM modes), correlated incidents
  (`/ai/incidents`, optional LLM-narrated root-cause summary), what-if capacity planning
  (`/ai/what-if`), and median/MAD-based anomaly detection (`/ai/anomalies`) that pauses itself
  rather than scoring on stale/missing telemetry (>15 min old). All four are also exposed as MCP
  tools (`crates/atlas-gateway/src/mcp.rs`, `mcp` feature) for agent-driven ops.
- **React console** embedded in the gateway (HTTPS, login, Observatory, Ceph page, Ops Advisor).
  UI identity: **Soundings** bathymetric system — see `docs/ATLAS_UI_CONTRACT.md`
  and `crates/atlas-gateway/ui/src/atlas-soundings.css`.
- `deploy/rook-ceph-lab/` (single-node overlay + day-2 ops scripts), `deploy/k8s/` ceph deployment,
  `deploy/postgres-lab/`, and a Helm chart (`deploy/helm/atlas/`, `database.kind: sqlite|postgres`)
  for production rollout.
- **DataBridge** (`atlas-databridge`): cloud-to-edge DB migration control plane. **Six source engines**
  — Postgres, MySQL, MariaDB (homogeneous → CNPG/Percona) + Oracle, SQL Server (heterogeneous → Postgres
  edge, seeded by Debezium's initial snapshot) + MongoDB (homogeneous document → Percona Server for
  MongoDB, Mongo Kafka sink) on Ceph. Real connectors: Postgres/MySQL/MariaDB default, SQL Server
  (`tiberius`)/Oracle (OCI)/MongoDB behind cargo features; precise CDC lag behind `kafka-lag`. Full
  pipeline (discover→assess→provision→full-load→CDC→validate→cutover→rollback); **Postgres verified
  end-to-end on two live Rook Ceph clusters incl. real Debezium CDC**; **all six source engines'
  real-connector discovery verified live** — MySQL 8.4, MariaDB 11.x, MongoDB 7.0 (replica set),
  SQL Server 2022, Postgres, and Oracle 23ai/26ai Free — on real servers through the deployed k3s gateway
  (binlog ROW / replica-set change streams / `is_cdc_enabled` / supplemental-log-min → `cdc_capable`,
  real db + table/collection names, PKs, counts). The gateway image now bundles the OCI Instant Client
  and builds the `oracle` feature. **Deeper stages: Postgres, MariaDB, and MongoDB are verified through
  real CDC + cutover** on the Rook Ceph lab (Kafka/Debezium + Connect image), and so is MySQL
  (2026-10-04, in-cluster, incl. TIMESTAMP columns and composite keys), as are Oracle (26ai) and
  SQL Server → Postgres (2026-10-04, in-cluster, through cutover) — all six engines now. The
  Connect image runs Debezium 3.7 with the Debezium JDBC sink on every relational edge; an Oracle
  source's `database` is `CDB/PDB`. The Ceph gateway image (`Dockerfile.ceph`) now
  builds with `mongodb`/`sqlserver`/`oracle`/`kafka-lag` features to match the fake/k8s image.
  See `docs/DATABRIDGE.md` + `deploy/databridge/`.

Day-2 operations added (slices, all fake-first tested): control-plane durability (job recovery, graceful
shutdown, self-state backup, deep readyz/livez), alerting maturity (ack/silence + job/CDC/quota rules,
native alerting sinks), cluster-ops & maintenance (OSD ops, backend cordon, worker pause, job
cancellation), governance (token revocation, rate limiting — DB-backed and cross-replica-aware once
on Postgres, still per-pod on SQLite, `docs/HA.md` — optional OIDC/SSO login alongside local
username/password, verified live against a throwaway Dex instance, `deploy/dex-lab/`; optional
Vault-backed secrets resolution at startup, `docs/SECRETS.md`), volume lifecycle (orphan GC,
QoS), DataBridge CDC self-heal, upgrade pre-flight + rollback, k6 load/performance testing
(read-path + write-path), and cross-cluster DR (RBD-mirroring peers/mirrors/failover/resync API +
jobs, **verified live between two Rook clusters**: one-way snapshot mirroring, planned failover
and split-brain recovery on 2026-10-04; two-way (`rx-tx`) mirroring with clean failback on
2026-10-06, where a non-forced promote is refused until `rbd mirror image status` shows this site
replayed the peer's demotion (`GET /dr/mirrors/{id}/status`) — `docs/DR.md`,
`deploy/rook-ceph-dr-lab/`).

**Observe-first storage I/O sensor** (`docs/IO_EBPF.md`): `atlas-io` is a separate, optional node
agent (`atlas-io-agent`, own Dockerfile.io/DaemonSet, `deploy/k8s/atlas-io-agent.yaml`) — kept out of
`atlas-gateway` specifically so the main gateway never needs `CAP_BPF`/`CAP_PERFMON`. Fake bio source
by default (zero external dependency, same fake-first pattern as the storage drivers); device-map
attribution, log2-µs histograms, per-cgroup/pid workload accounting, RCA verdicts, and fail-open
leased write-freezes are all live-verified over HTTP/`/metrics` and `atlasctl io`. **Real
block-layer attach** (`bpf` feature: `crates/atlas-io/bpf/atlas_bio.bpf.c`, CO-RE, loaded with
`aya`; built into `Dockerfile.io`) is verified live on kernel 7.0 — real per-device latency and
pid/cgroup attribution; if attach fails, live mode reports the programs missing rather than
fabricating data. The same object measures scheduler queue time (`block_rq_insert`) and loads the
**Atlas Native maps** (`atlas_native_io.bpf.c` ABI): a userspace-filled PID set of the native
binaries, per-(device, op, cgroup) kernel aggregates and a ≥5 ms slow-I/O ring buffer, pinned under
`/sys/fs/bpf/atlas/native` (`/io/native`, `atlasctl io native`) — verified live against a real
six-process native cluster. NFS/ZFS/io_uring programs and the cgroup→volume map are still
contracts only.

**atlas-native filesystems** (`docs/NATIVE_FS.md`): POSIX files and directories in the native
cluster's replicated catalog (inodes, hard links, rename, setattr/truncate, symlinks, special files, extended attributes; per-filesystem extent grid),
file data on the same copy-on-write extents as block volumes, metadata-only snapshots (read-only,
`<fs>@<snap>`) and clones. `atlas-native-mount` (`crates/atlas-native-fuse`, `fuse` feature, Linux)
mounts them through FUSE with leader failover, TTL attribute/name caching, write-back and
read-ahead; close-to-open consistency across mounts. The gateway serves them as `filesystem`
volumes `vol_native_fs_<id>` (create/snapshot/clone/restore/delete via `atlasctl create-volume
--backend bkd_native --kind filesystem`). pjdfstest (12 suites, 8565 tests), git clone + fsck and
snapshot/clone isolation verified over FUSE on the lab. Scaling limit: one Raft group whose
catalog lives fully in memory (~6 KiB/inode on the leader). Commits cost one WAL fsync, Raft
replication is pipelined, and checkpoints write only changed records to `catalog.redb` (redb;
`tracked::Tracked` maps record changed keys; a legacy `catalog.json` migrates on open, one-way).
`tests/metadata_bench.rs` (ignored): a 3-voter group does ~9k creates/s from one proposer and
~12k/s from eight on one host; through FUSE creates are still ~270/s (per-op HTTP round trips).

**Retention locks + ransomware assessment** (`docs/WORM.md`): atlas-native block and filesystem
snapshots can be retention-locked (Raft-enforced, extend-only; `POST /snapshots/{id}/lock`,
`atlasctl lock-snapshot`), the native S3 gateway supports compliance-mode object retention, and
native filesystems record per-minute activity windows (writes, renames, deletes, write entropy) behind
`GET /volumes/{id}/ransomware` and admin `POST …/ransomware/protect` (re-assesses server-side, then
snapshots + locks); `ATLAS_RANSOMWARE_AUTOPROTECT=1` automates it for `likely_ransomware` only.
Unit/integration tested; snapshot locks verified live on the lab native cluster (2026-10-18); S3 retention, activity windows, `likely_ransomware` verdict, auto-protect and protect verified live 2026-10-26 (`deploy/native/lab-verify.sh`, 27/27, synthetic random-byte burst); a real encrypting workload not yet.

**Data reduction** (`docs/NATIVE_DATA_REDUCTION.md`): atlas-native has opt-in LZ4 compression
(`compression`) and extent deduplication (`dedup`) in the metadata config; `GET /v1/reduction`
reports the savings (also `GET /native/reduction` on the gateway and `atlasctl native-reduction`). The checksum covers the stored payload, so scrub/repair/tiering/EC are
unchanged; the dedup index is only a hint, validated at apply time (a stale share is redone
unshared). Shared extents can sit at different offsets per reference, so consumers must use the
map offset. Unit/integration tested; byte-exact read-back and the savings report verified on a lab multi-process cluster (2026-10-26, favourable synthetic input); no lab throughput/CPU numbers.

**Dedup savings estimator** (`docs/DEDUP_ESTIMATE.md`): read-only "what would dedup save" for Ceph RBD
pools (hashes allocated extents from `rbd export-diff`, bounded by a byte budget, reported as a lower
bound when partial) and ZFS pools (`zdb -S` simulated dedup table), `GET /pools/{id}/dedup-estimate`
and `atlasctl dedup-estimate`, admin-only; `POST` runs it as a `dedup.estimate` background job on its own
task (not the shared worker), with a heartbeat, cancel, and no retries. Unit/integration tested over the fake drivers; a real `rbd export-diff` of an image head
(Rook Ceph 19.2.3, 2026-10-27) parses to the expected figures; not yet run through the gateway on real Ceph or against a real zpool.

**Licensed** under the [Apache License 2.0](LICENSE) (`Apache-2.0`; relicensed from the Zyvor
Production License v1.0 at the maintainer's explicit request — history in
[`docs/LICENSING.md`](docs/LICENSING.md)). Don't weaken or remove license notices,
[`CLA.md`](CLA.md), [`DCO.md`](DCO.md), or [`NOTICE`](NOTICE) without an explicit human
request. There is **no** runtime license-key or trial/JWT gate.

DR also covers journal-mode and pool-mode mirroring and local multi-image consistency groups
(`/volume-groups`, `rbd group` snapshots + rollback job), verified on the two-site lab 2026-10-06.
Journal-mode non-forced promote needs `peer_replayed=1` because a caught-up and a stale copy report
the same status (`docs/DR.md`).

Deferred: cross-site group mirroring (no released Ceph has `rbd mirror group`), per-product
integrations beyond the gRPC surface.

## Layout
- `crates/atlas-common` — config, error, tracing, id helpers.
- `crates/atlas-api-types` — shared serde DTOs.
- `crates/atlas-driver-core` — `StorageDriver` trait + `DriverError` + `DriverRegistry`.
- `crates/atlas-driver-ceph` — `ceph`/`rbd` CLI wrappers + `FakeCephDriver`.
- `crates/atlas-driver-nfs` — `FakeNfsDriver`/`RealNfsDriver` (second backend; exports→pools,
  shares→filesystem volumes).
- `crates/atlas-driver-zfs` — `FakeZfsDriver`/`RealZfsDriver` (third backend; zpools→pools,
  datasets→filesystem volumes) + raw-disk inspection/wipe/`zpool create` (`cmd.rs`).
- `crates/atlas-driver-rgw` — `S3Target`, the generic SigV4 S3 client for the bucket/object write path
  (Ceph RGW, the default, and any bring-your-own S3-compatible endpoint). The first-party
  `atlas-driver-rustfs` crate (RustFS-product-specific discovery, admin proxy, drive/instance install
  jobs) was removed — see `docs/RUSTFS.md`.
- `crates/atlas-databridge` — DataBridge: source connectors, assessment, CNPG/Percona CR builders,
  pipeline stages, reconciler (cloud-to-edge DB migration; `migrations/0011`, `/api/atlas/v1/databridge/*`).
- `crates/atlas-io` — optional node sensor (bin `atlas-io-agent`): fake/live bio source, device-map
  attribution, histograms, workload accounting, RCA, fail-open leases; own HTTP API + `/metrics`,
  not embedded in `atlas-gateway` (see `docs/IO_EBPF.md`).
- `crates/atlas-native` — native data plane (served to the gateway by `atlas-driver-native`): replicated
  extent engine, metadata WAL + incremental checkpoints into a redb catalog store (`store`,
  `tracked`), refcounted extents with free-list space reuse,
  and a Raft core for the metadata log (pre-vote, check-quorum, joint-consensus membership changes via
  `POST /v1/members`) with a std-only TCP server
  (`raft_server`), plus networked data nodes (`data_node`, term-fenced writes) so the engine can
  commit through Raft (`MetaBackend::Raft`) with replicas on other hosts. Per-node circuit breaker,
  write failover and scrub/repair (`repair_once`); both transports support optional mutual TLS.
  Data nodes serve one or more devices (`data_node.devices`) with a `file`, `aligned` (`O_DSYNC`,
  4 KiB-aligned) or `io_uring` (`O_DIRECT`, `io-uring` cargo feature, Linux) backend (`raw`,
  `uring`); the engine stripes replicas across a node's devices.
  Bin `atlas-native-node` (`node` module) runs either role with an HTTP health/metrics/volume API
  (`deploy/native/smoke.sh`); `Dockerfile.native` + `deploy/k8s/atlas-native.yaml` (3-pod
  StatefulSet, verified live on the lab k3s incl. leader-pod failover) deployed by
  `scripts/deploy-native-remote.sh <host> [user]`; Helm chart `deploy/helm/atlas-native` (mTLS +
  HTTPS client-cert auth verified live by `deploy/native/helm-live-check.sh`). See `docs/NATIVE_STORAGE.md`, `docs/NATIVE_METADATA.md`,
  `docs/NATIVE_NODE.md`.
- `crates/atlas-driver-native` — `StorageDriver` over the `atlas-native-node` HTTP API (backend
  `bkd_native`, `ATLAS_NATIVE_*`); the one driver whose write methods the gateway calls
  (synchronous volume create/expand/delete, snapshot create/delete, clone/restore — block volumes
  and, as kind `filesystem`, native filesystems) and the only one with a gateway block data path
  (`GET`/`PUT /volumes/{id}/data`). Tested against in-process native nodes.
- `crates/atlas-native-fuse` — `atlas-native-mount` FUSE client for native filesystems (`fuse`
  feature, Linux only; the kernel-independent `ops` layer is tested on any OS against in-process
  nodes). See `docs/NATIVE_FS.md`. Also bin `atlas-native-nfs` (`Dockerfile.native-nfs`, chart
  `nfs.enabled`): NFS gateway supervisor running NFS-Ganesha ≥ 9.14 (`FSAL_VFS`, separate LGPL
  process) over `atlas-native-mount` FUSE mounts; NFSv4.1/4.2 + optional v3/NLM. Verified on the
  lab k3s; see `docs/NATIVE_NFS.md`. Also bin `atlas-native-smb` (`Dockerfile.native-smb`, chart
  `smb.enabled`): SMB gateway supervisor running Samba `smbd` (separate GPL process, Debian
  package) over `--allow-other` mounts; users are Unix accounts (password via `smbpasswd` stdin,
  never argv/logs), oplocks/leases off, single pod. Process supervision shared in `supervise.rs`.
  Verified on the lab k3s; see `docs/NATIVE_SMB.md`. IO500 harness: `Dockerfile.io500` (IO500
  sc26 + Open MPI + `atlas-native-mount`, entry `deploy/io500/io500-run.sh`) and
  `scripts/io500-native-remote.sh`; full suite runs on the lab but no submission (needs NVMe
  hardware; never set `IO500_DROP_CACHES=1` on the shared lab). See `docs/IO500.md`. Also bin
  `atlas-native-replicate` (`replicate.rs`, `Dockerfile.native-replicate`, chart
  `replication.jobs`): stateless async filesystem replication to a replica on another cluster
  (snapshot diff per inode, only changed grid cells; replica state `base`/`local`/`pending` in the
  catalog, `namespace/replica.rs` + `engine/replica.rs`; promote/demote for failover/failback).
  Replica snapshot ids = source ids, so source and target must be separate clusters. Verified
  live between two clusters on the lab k3s; see `docs/NATIVE_REPLICATION.md`. Edge profile:
  `deploy/helm/atlas-native/values-edge.yaml` (single pod, RF 1, `metadata.cache_inodes` /
  `store_cache_bytes` capped, `MALLOC_ARENA_MAX=2`); see `docs/NATIVE_EDGE.md`.
- `crates/atlas-native-s3` — S3 gateway (bin `atlas-native-s3`, `Dockerfile.native-s3`, chart
  `s3.enabled`) on s3s over the native cluster API (no FUSE, unprivileged): bucket = filesystem or
  snapshot, object = file, temp-file + rename PUTs, ETag/headers in xattr `user.atlas.s3`, multipart
  state under `.atlas_s3_uploads/` so replicas scale out. S3 keys bypass POSIX permissions. Verified
  with aws-cli on the lab k3s; see `docs/NATIVE_S3.md`.
- `crates/atlas-native-csi` — CSI driver `native.atlas.zyvor.ai` (bin `atlas-native-csi`,
  `Dockerfile.native-csi`, chart `csi.enabled`): PVCs → filesystems, VolumeSnapshots, clones, per-pod
  `atlas-native-mount` FUSE mounts. `proto/csi.proto` is a wire-compatible subset of the CSI spec
  (no well-known-type imports: Debian's protoc lacks them). Unmounts use `umount2(2)`, not
  `fusermount3 -u` (Ubuntu's AppArmor profile blocks it). Verified on the lab k3s; see `docs/NATIVE_CSI.md`.
- `crates/atlas-driver-macos` — a Mac's APFS containers/volumes and partitions via `diskutil` plists (`ATLAS_MACOS_ENABLE`,
  fake/real, read-only discovery plus guarded external-disk lifecycle (`ops.rs`); pure parsing in `parse.rs` is tested on recorded plists on any OS); see `docs/MACOS.md`.
- `crates/atlas-mac-agent` — per-Mac read-only HTTP agent (bin `atlas-mac-agent`) serving `RemoteMacosDriver`
  (`ATLAS_MACOS_AGENTS` on the gateway); see `docs/MACOS.md`.
- `crates/atlas-driver-weka` — read-only WEKA discovery over its REST API (`ATLAS_WEKA_ENABLE`, fake/real);
 see `docs/COMPARISON.md`.
- `crates/atlas-driver-k8s` — `kube-rs` read-only StorageClass/PVC/PV listing.
- `crates/atlas-inventory` — read/upsert model against `sqlx::AnyPool` (SQLite or Postgres); also
  DB-backed rate-limit counters (`rate_limit.rs`).
- `crates/atlas-discovery` — discovery worker.
- `crates/atlas-gateway` — axum server (bin `atlas-gateway`); `routes/ai.rs` (Ops Advisor/incidents/
  anomalies/what-if), `mcp.rs` (MCP tool exposure, `mcp` feature).
- `crates/atlasctl` — `atlasctl` REST client.
- `migrations/` — SQLite schema; `migrations-postgres/` — the Postgres-dialect equivalent (CI fails
  if the two drift to different highest migration numbers).
- `deploy/rook-ceph-lab/` — lab manifests + `up.sh` (`--single-node`; Rook v1.20.2 + Squid + CSI drivers).
- `deploy/postgres-lab/` — throwaway Postgres for HA query-layer testing/dev.
- `deploy/helm/atlas/` — production Helm chart (`database.kind: sqlite|postgres`, driver modes,
  rate limiting, tracing, secrets).
- `scripts/deploy-remote.sh <host> <user>` — build (`Dockerfile`) + deploy the fake/k8s gateway to `zyvor-system`.
- `scripts/deploy-ceph-gateway-remote.sh <host> [user]` — build (`Dockerfile.ceph`, Squid client) + roll out
  the **real-Ceph** gateway `atlas-gateway-ceph` in `rook-ceph` (NodePort 30511). See `docs/DEPLOYMENT.md`.

## Conventions
- Every source file begins with a Zyvor copyright line plus
  `SPDX-License-Identifier: Apache-2.0`
  (enforced by `make headers` / CI). Don't weaken license notices, [`CLA.md`](CLA.md),
  [`DCO.md`](DCO.md), or [`NOTICE`](NOTICE) without an explicit human request.
- Match the monorepo Rust stack: axum 0.8, `sqlx::Any` (SQLite/Postgres, `$N` placeholders — no
  dialect-specific SQL in app code), `thiserror` 2.0 + `anyhow`, `tracing`.
- Ceph/rbd command wrappers use **arg-arrays only, never string concatenation**.
- **sqlx 0.9 only accepts `&'static str` SQL.** A dynamically built query string must be wrapped in
  `sqlx::AssertSqlSafe(..)` with a one-line comment saying why nothing user-controlled is in it
  (values always go through `$N` binds; the interpolated part is a const, a column list or a generated
  `$1,$2,…` placeholder list). Prefer fixing it in the one helper that builds the string.
- **rustls crypto provider**: `main()` installs `ring` before anything builds a TLS client. reqwest
  0.13's default rustls backend enables `aws-lc-rs` too; with both features on and no explicit install
  rustls panics at startup ("Could not automatically determine the process-level CryptoProvider").
- Workspace dependencies were upgraded to latest majors on 2026-09-28 (kube 4.x, sqlx 0.9,
  tonic/prost 0.14 — codegen is `tonic-prost-build` now — reqwest 0.13, argon2 0.6, …). Not bumped:
  `azure_*` (no compatible release yet) and TypeScript 7 (typescript-eslint caps `<6.1.0`).

## Run locally (no Ceph, no cluster needed)
```
make run            # gateway with fake Ceph driver on 127.0.0.1:5110
cargo run -p atlasctl -- --base-url http://127.0.0.1:5110 health
```
Live K8s path (needs KUBECONFIG): `atlasctl storage-classes`.

## Deploy script gotchas

- **`scripts/deploy-ceph-gateway-remote.sh` no longer restarts `atlas-gateway-ceph`
  unconditionally.** It used to run `kubectl rollout restart` after every `kubectl apply`,
  including no-op re-runs — on Ceph-RBD-backed storage (this Deployment's own PVC) that
  repeatedly interrupts an in-flight CSI mount and corrupts the volume (found live, 2026-08-31,
  chasing a stuck Atlas gateway on a second `../hypercluster`-provisioned host: `../hypercluster`'s
  own `storage apply` calls this script on every re-run, so simply re-running it — even just to
  validate an unrelated fix — kept bouncing a healthy pod and produced a multi-hour Ceph
  `HEALTH_ERR`/`recovery_unfound` cascade). Now it only restarts when `kubectl apply`'s output
  does **not** match `^deployment\.apps/${DEPLOY} unchanged$`. If you touch step 7/7 again, keep
  that gate — see `../zeus-os/CLAUDE.md`'s "unconditional restart on every re-apply" entry for
  the full writeup and two sibling-repo instances of the same bug (`../hypercluster`,
  `../packetwolf`).
