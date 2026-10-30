<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Changelog

All notable changes to Atlas will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/). Versions
before `0.2.0` were not tracked here — see `git log` for that history.

## [Unreleased]

### Added

- `atlas-native` feature `rdma` (off by default, Linux): `rdma::device_names()` and a self-connected reliable-connection
  round trip over `libibverbs`, plus `rdma::with_stream`: a `Read`/`Write` byte stream over a reliable-connection queue pair
  between two endpoints (queue-pair endpoints exchanged as JSON over a TCP side channel). Foundation for an RDMA data-node
  transport, which does not exist yet; the data path is still TCP/TLS. Verified on SoftRoCE only.
- `atlas_data`: `DataReadyGate.wait_ready()` and `python3 -m atlas_data gate-wait JOB` block until a job's data is
  staged (Kubernetes init-container use); only "not ready" is retried and a timeout exits non-zero.
- `POST /ceph/nvmeof/namespaces` (admin, `confirm`) and `atlasctl ceph-nvmeof-create`: provision a new NVMe-oF namespace as a
  PVC from an `nvmeof.csi.ceph.com` StorageClass, as the ordinary `volume.create` job. REST and CLI only; not yet run on a
  live Rook cluster.
- Ceph NVMe (`docs/NVME.md`): read-only `GET /ceph/nvme`, `/ceph/osd-objectstore`, `/ceph/sizing`, `/ceph/nvmeof`
  with `atlasctl` commands and MCP tools; Ops Advisor rules for worn NVMe drives and undersized NVMe hosts; device
  class in the Disks picker; `POST /ceph/devices` accepts `db_device`, `db_size_mb`, `wal_size_mb`, `osds_per_device`.
  Not yet run against a live Rook cluster.
- `GET /osds/ok-to-stop?ids=0,1`: whether stopping the given OSDs together keeps every placement group available
  (`ceph osd ok-to-stop`), for checking a node before maintenance. Read-only.
- `GET /ceph/devices`: the drives Ceph tracks with their failure predictions, and a summary of those expected to
  fail within 90 days (`ceph device ls`). Read-only; predictions need Ceph's device-health module.
- `POST /volumes/{id}/import` and the `image.import` job: stream a raw disk image over HTTP(S) into an
  atlas-native volume, skipping all-zero blocks, resuming from recorded progress, and verifying the volume by
  reading it back against a caller-supplied SHA-256. Off until `ATLAS_IMPORT_ALLOWED_HOSTS` lists the hosts it
  may fetch from. Raw images and atlas-native volumes only; see `docs/IMAGE_IMPORT.md`. Imports run beside the
  job worker (`ATLAS_IMPORT_CONCURRENCY`, default 2) so a long import never holds up other jobs.
- `ATLAS_DATABRIDGE_PG_CA_FILE`: an extra PEM bundle of CA certificates to trust for `verify-*` Postgres sources.

### Fixed

- DataBridge Postgres discovery ignored a source's `tls_mode` and always connected in plaintext
  (`sslmode=disable`), even for the default `require`. It now honours `disable`, `prefer`, `require`,
  `verify-ca` and `verify-full`; an unrecognised value is treated as `require`, never plaintext. Credentials are
  now passed as typed connection fields rather than interpolated into a connection string.

## [0.9.0] — 2026-10-19

### Added

- macOS lifecycle on a remote Mac through its agent: `?backend=bkd_macos_<name>` on the `/macos/*` routes, agent
  write endpoints behind `ATLAS_MAC_ALLOW_WRITES=1` (token required), refusals re-run on the Mac. Tested against an
  in-process agent with fake ops and, on a real Mac, against a throwaway disk image.
- macOS local snapshots of the Mac as a whole (`/macos/snapshots`, via `tmutil`; macOS cannot snapshot one volume).
- macOS container resize (`POST /macos/containers/{container}/resize`, agent `resize-container`) and a Mac section on the
  console Disks page (erase, add volume, resize, eject).

### Fixed

- `ATLAS_MAC_ALLOW_DISK_IMAGES=1` (and the new `ATLAS_MAC_ALLOW_WRITES=1`) were rejected by the agent's flag parser;
  they now accept `1`/`true`/`yes`.

## [0.8.0] — 2026-10-19

### Security

- `rustls` 0.23.46 (RUSTSEC-2026-0346: `Debug` for client session values exposed TLS secrets).

### Added

- **Retention locks and ransomware assessment** (`docs/WORM.md`): Raft-enforced, extend-only retention locks on
  atlas-native block and filesystem snapshots (`POST /snapshots/{id}/lock`, `atlasctl lock-snapshot`); compliance-mode
  object retention on the native S3 gateway; per-minute filesystem activity windows behind
  `GET /volumes/{id}/ransomware` and admin `POST …/ransomware/protect`; opt-in `ATLAS_RANSOMWARE_AUTOPROTECT=1`
  for `likely_ransomware` only. Snapshot locks are verified on the lab; activity, protect and S3 retention are
  unit/integration tested only.
- **Native data reduction** (`docs/NATIVE_DATA_REDUCTION.md`): opt-in LZ4 compression and extent deduplication, the
  node's `GET /v1/reduction`, the gateway's `GET /native/reduction` and `atlasctl native-reduction`. Not yet run on
  the lab; no throughput or CPU numbers.
- `atlas-client` crate: typed Rust client for products (`create_volume`, `list_volumes`, `get_job`, `wait_for_job`).
- `GET /io/heatmap` on the I/O sensor: per-minute latency columns per device and operation (fake source only).
- Experimental AI data services companion (`tools/atlas_data`, `docs/AI_DATA_SERVICES.md`): not part of the Rust
  gateway, its own auth and storage.
- Docs site pages for WORM, data reduction, AI data services and IO500.
- `docs/RELEASING.md` (tag, ghcr images, archives, visibility); macOS rows in STATUS, ROADMAP, ARCHITECTURE and COMPARISON.

### Fixed

- The `atlas-native-csi` node test no longer fails on macOS (it reads `/proc` mountinfo, so it runs on Linux only).

## [0.7.0] — 2026-10-17

### Fixed

- Job recovery at start-up no longer fails a job the same process already claimed. It ran on a spawned task and could
  race a job enqueued right after start (the `tests/disks.rs` flake that stopped full workspace runs).

### Added

- `docs/MACOS_CLIENT.md`: using Atlas storage from a Mac (NFS, SMB, `atlasctl`) and why the native FUSE mount
  is not available on macOS yet. The mount commands are documented, not yet run against a live gateway.
- Mac fleet: `atlas-mac-agent` (read-only HTTP agent per Mac, bearer token, TLS required off loopback,
  launchd installer, macOS release archives) and `RemoteMacosDriver`; the gateway registers each agent from
  `ATLAS_MACOS_AGENTS` as its own backend. See `docs/MACOS.md`.
- macOS volume lifecycle (`/macos/*`): erase an external disk, add/delete APFS volumes, eject. Admin only, typed
  confirmation, jobs and audit. Hard refusals (internal disks, the running system, system volumes, disk images,
  odd mounts) are checked in the route and again in the job; fake mode runs them against fixtures. See `docs/MACOS.md`.
- macOS backend (`atlas-driver-macos`, `ATLAS_MACOS_ENABLE`): APFS containers as pools, APFS volumes and other
  filesystem partitions as volumes, capacity/quota/mount and SMART health from `diskutil`; internal and external
  drives, read-only, fake/real modes, rescanned on the monitor interval. See `docs/MACOS.md`.

### Security

- Website: `postcss-selector-parser` forced to >= 7.1.6 (Dependabot #26); synced docs pages updated to 0.6.0.

## [0.6.0] — 2026-10-16

### Security

- `hickory-resolver` 0.26.3 (GHSA-5j98-2g5x-46v6, -6w6g-hm98-mhgm, -6f2x-v7q7-m7m5); website lockfile refreshed
  and `tinypool` forced to >= 2.1.2 (prototype-pollution RCE); `POST /volume-groups` takes at most 256 volumes.

### Added

- MCP `ack_alert` (guarded, behind `ATLAS_MCP_WRITE_ENABLE`): same two-phase, single-use, audited flow as
  `create_snapshot`, which now shares one gate with it.
- Optional LLM narratives for `GET /ai/anomalies` and `POST /ai/what-if` via `?mode=auto|llm` (default
  `local`, nothing sent). MCP tools stay local-only. The three narrative paths share one provider helper.
- MCP prompts and resources on the gateway (`triage`, `capacity_review`, `dr_readiness`; `atlas://docs/*`,
  `atlas://skill/ops`) and on the `atlas-io` agent (`io_triage`; I/O docs). Prompt arguments are validated;
  resources are embedded text only. `GET /.well-known/mcp` is a public, static discovery card.
- MCP read tools: `ceph_status`, `ceph_df`, `ceph_osd_tree` (summarised), `databridge_plans`,
  `databridge_cdc_streams`, `list_policies`, `list_audit`. Operator-gated where the REST route has no
  check, tenant-filtered where rows carry a tenant, output capped; audit entries omit request/result bodies.
- `docs/MCP.md` and `examples/agent/` (operator-token mint script, Hermes/Claude client configs,
  ops skill) for MCP-connected agents.
- MCP: opt-in (`ATLAS_MCP_WRITE_ENABLE=1`) guarded `create_snapshot` tool with a two-phase,
  single-use confirmation token and audit trail; no destructive tools (`docs/API.md`).


- `atlas-io-agent`: optional bearer token (`ATLAS_IO_TOKEN`) and, with the `mcp` feature, a read-only
  MCP endpoint at `/mcp`; the gateway sends `ATLAS_IO_AGENT_TOKEN` when fetching `/io/risk`.

- MCP: read-only `io_risk`, `dr_status`, `dr_preflight`, `list_dr_mirrors`, `dr_mirror_status`,
  `list_volume_groups`, `list_snapshots`, `list_jobs`, `get_job` tools (`docs/API.md`).


- Helm: optional Envoy front door via the Gateway API (`envoy.*` values, `Gateway` + `HTTPRoute`,
  HTTP→HTTPS redirect, no request timeout for MCP) and `deploy/envoy-lab/up.sh` (Envoy Gateway v1.9.2);
  `docs/ENVOY.md`.

- Ops Advisor: optional read-only `io_risk` block from `atlas-io-agent` (`ATLAS_IO_AGENT_URL`),
  informational and fail-open (`docs/AI_ADVISOR.md`).

- `atlas-io`: deterministic `risk` scorer (`score`, `RiskLevel`, `CapacitySignal`) combining RCA
  verdicts and queue-cause notes with an optional capacity signal; served at `GET /io/risk` and as `atlas_io_risk_level` (`docs/IO_RISK.md`).
- Application recovery operator tool (`PYTHONPATH=tools python3 -m atlas_recovery`):
  dependency-ordered snapshot capture and restore drills, trusted bounded application hooks,
  Native block-range checksum verification, RPO/RTO objectives, atomic private reports,
  conservative cleanup, latest-snapshot selection, and daily systemd examples.
- Recovery contract scenarios and actual gateway/Native-process smoke checks in CI.

### Changed

- Helm: the Envoy HTTPS listener moves from 443 to `envoy.httpsPort` (default `7443`); the HTTP→HTTPS
  redirect names the port. Set `envoy.httpsPort=443` to keep the old behaviour.
- `deploy/k8s/atlas-io-agent-live.yaml`: the live block-layer BPF variant of the agent DaemonSet
  (BPF/PERFMON, tracefs), matching what the lab runs; `atlas-io-agent.yaml` stays fake by default.
- I/O RCA distinguishes scheduler queue pressure from issue-to-complete service latency,
  handles insufficient or inconsistent queue evidence, and restricts hottest-issuer attribution
  to the known volume while preserving the existing response schema.

### Fixed

- MCP `list_snapshots` returned every tenant's snapshots to any operator; it is now limited to the caller's
  tenant unless admin.
- MCP: remote clients were rejected by the transport's loopback-only `Host` check; it is now
  lifted when auth is required, with `ATLAS_MCP_ALLOWED_HOSTS` to override.

### Documentation

- AI and agents are now visible: README section, website homepage (feature band, block, comparison row,
  footer link), `llms.txt`, and `docs/MCP_LISTING.md` (directory listing drafts, nothing submitted). Stale
  "advisory only / no write tools" wording corrected in ROADMAP, STATUS, API and ARCHITECTURE; COMPARISON
  gains the MCP row and a neutral note on other storage MCP servers.
- The website now copies `docs/ux/anim` into `static/anim`, which the homepage artwork already referenced
  but the repo never supplied.

## [0.5.0] — 2026-10-09

Documentation: a rewritten README with animated graphics, an animated landing page, and the full docs set published on the website.

### Added

- **Observe-first storage I/O sensor** (`crates/atlas-io`, binary `atlas-io-agent`):
  in-process bio pipeline (log2-µs histograms, cgroup/pid attribution, device→volume
  map, deterministic RCA, time-limited write-freeze leases that fail open). Default
  `ATLAS_IO_MODE=fake` so CI never needs `CAP_BPF`. Contract C sources live in
  `crates/atlas-io/bpf/`. `atlasctl io health|summary|histograms|workloads|rca|coverage|leases`.
  Optional DaemonSet: `deploy/k8s/atlas-io-agent.yaml`. See [docs/IO_EBPF.md](docs/IO_EBPF.md).
- **Live eBPF attach for `atlas-io`** (`bpf` feature, aya/CO-RE): block-layer tracepoints with
  queue time and error codes, plus the Atlas Native eBPF maps (per-process stats and a slow-I/O ring
  buffer), `GET /io/native`, `atlas_io_native_*` metrics and `atlasctl io native`. NFS, ZFS,
  io_uring and per-cgroup programs are still reported missing. Verified on a lab host (kernel 7.0).
- **Journal-mode and pool-mode RBD mirroring**: `mode=journal` enables and
  `GET`/`PUT /dr/pools/{pool}/mirroring`. A non-forced journal-mode promote needs
  `peer_replayed=1`, backed by `journal_peers_replayed` on the demoted site. Verified on the
  two-site Rook lab (`docs/DR.md`).
- **RBD consistency groups** (`/volume-groups`): crash-consistent group snapshots and a
  confirm-gated rollback job; migration `0035`. Local to one cluster; no released Ceph mirrors
  `rbd group`s across sites.

### Removed
- First-party **RustFS** product integration: `atlas-driver-rustfs`, Storage → RustFS console,
  `/rustfs/*` admin proxy, drive/instance jobs, vendored Helm subchart, and `deploy/rustfs-lab`.
  Object storage now defaults to Ceph RGW (`bkd_ceph_lab`). Bring-your-own S3 remains via
  `atlas-driver-rgw`. See `docs/RUSTFS.md`.
- Remaining RustFS leftovers: the ignored `ATLAS_RUSTFS_*` configuration and its startup warning,
  the `POST /backends/{id}/selftest` route (it could only ever answer "unsupported"), the
  `RUSTFS_ACCESS_KEY`/`RUSTFS_SECRET_KEY` credential-alias shim, the dead `/rustfs/*` console hooks and
  types, and the `ATLAS_RUSTFS_*` block in `.env.example`. Kept on purpose so existing data still
  loads: the `BackendType::Rustfs` value and the retired `bucket.*.rustfs` / `rustfs.*` job variants
  (they are rejected with a clear error), plus the rejection of the `bkd_rustfs_lab` backend id.

### Fixed
- `POST /dr/mirrors/{id}/promote?force=1` was rejected with a 400 even though the docs and the
  guard's error message say to use it. `force` and `peer_replayed` now accept `0`, `1`, `false`
  and `true`.
- DataBridge docs and the console's engine table still called MySQL CDC-only and SQL Server and
  Oracle discovery-only; all six engines were verified through cutover on 2026-10-04.
- The Helm chart's `s3.caSecretName` (trust a private CA for a bring-your-own S3 endpoint) set
  `ATLAS_S3_CA_CERT`, but the code only read the legacy `ATLAS_RUSTFS_CA_CERT`, so the setting had no
  effect. The code now reads `ATLAS_S3_CA_CERT`. **If you set `ATLAS_RUSTFS_CA_CERT` directly, rename it.**

### Changed
- Relicensed from the Zyvor Production License v1.0 to the **Apache License, Version 2.0**
  ([LICENSE](LICENSE), SPDX `Apache-2.0`): use, modification and redistribution — including production,
  SaaS and managed services — are permitted under the license's terms; there is no longer a commercial
  license requirement. Source headers, Cargo/npm metadata, `NOTICE`, `docs/LICENSING.md`, the README and the
  website were updated; `LICENSES/Apache-2.0.txt` replaces the old license text. Releases already published
  under an earlier license keep the terms they were published with (see `docs/LICENSING.md`). The CLA/DCO now
  refer to Apache-2.0; the CLA's grant wording should be reviewed by counsel.
- README restyled as a landing page (blue/white hero with light/dark variants, capability tiles, Day/Night console overview).

## [0.4.0] — 2026-09-21

### Upgrade and migration notes

- No database migration is required from 0.3.0 to 0.4.0. Starting a 0.4.0 gateway applies the same `migrations/` or `migrations-postgres/` tree it already applied on `main`; this release does not add a schema revision of its own.
- Artifacts tagged `v0.3.0` stay under AGPL-3.0 plus the Atlas Commercial License. Artifacts tagged `v0.4.0` use `LicenseRef-Zyvor-Production-1.0`. The current [`LICENSE`](LICENSE) is the Apache License 2.0; the terms of older tags are those published at the tag.
- Helm lab installs keep the `localhost/atlas-gateway:dev` default. Published images are `ghcr.io/zyvorai/atlas:0.4.0` and `ghcr.io/zyvorai/atlas-ceph:0.4.0`, selected with [`deploy/helm/atlas/values-release.yaml`](deploy/helm/atlas/values-release.yaml). Do not deploy the floating `:latest` tag. A lab `scripts/deploy-remote.sh` roll-out of this tree reported `GET /version` as `0.4.0` (`/health`, discovery, and live StorageClasses also succeeded). That image is the local `:dev` import, not the GHCR tag, which is published only when `v0.4.0` is pushed.

### Changed — Zyvor Production License v1.0

- Replaced the AGPL-3.0 + Atlas Commercial License dual license with the [Zyvor Production License v1.0](LICENSE). Non-production use is free. Production, SaaS, managed services, OEM, and redistribution require a separate paid commercial license. Published SKU prices are withdrawn; commercial terms are issued separately ([https://zyvor.dev](https://zyvor.dev)). Source headers use `LicenseRef-Zyvor-Production-1.0`. There is still no runtime license key. The v0.3.0 AGPL text is [`LICENSES/AGPL-3.0-only-v0.3.0.txt`](https://github.com/zyvorai/atlas/blob/v0.3.0/LICENSES/AGPL-3.0-only-v0.3.0.txt) (archived license texts were later removed from `main`, see [`docs/LICENSING.md`](docs/LICENSING.md)); the v0.3.0 commercial summary is [`LICENSES/LicenseRef-Atlas-Commercial-v0.3.0.md`](https://github.com/zyvorai/atlas/blob/v0.3.0/LICENSES/LicenseRef-Atlas-Commercial-v0.3.0.md).

### Added — operator Make targets

- `make help` lists targets. `make status` runs `atlasctl health`. `make deploy-remote H=<host> U=sus` deploys the gateway; `make deploy-ceph` deploys the real-Ceph gateway. `make ci` is unchanged.

### Added — NFS/ZFS real driver mode

- Added `RealNfsDriver`/`RealZfsDriver` alongside the existing fixture-only drivers, selected per
  backend via `ATLAS_NFS_DRIVER_MODE`/`ATLAS_ZFS_DRIVER_MODE` (`fake`, unchanged default, or
  `real`) — mirrors the existing `ATLAS_CEPH_DRIVER_MODE` Fake/Real split.
- Real NFS runs `showmount -e` to confirm the server is reachable and get its real export list; a
  configured export the server doesn't have is dropped, never fabricated. Per-export capacity comes
  from `df` when the export is already locally mounted, `None` otherwise.
- Real ZFS runs `zpool list -Hp`/`zfs list -Hp` locally on the gateway's own host (no remote/SSH
  support yet); a zpool that isn't actually locally importable is dropped, never fabricated.
- Both real drivers propagate a real error, never a silent fake-data fallback, when the target is
  unreachable. Helm chart gained `nfs.driverMode`/`zfs.driverMode` (default `fake`).

### Added — Cross-replica rate limiting

- `RateLimiter::allow()` stays synchronous and database-free (still safe to call from the gRPC
  path's `tonic::Interceptor`); cluster awareness now comes from a separate periodic background
  task (`ATLAS_RATE_LIMIT_SYNC_SECS`, default 2s) that syncs each replica's per-window count
  through a new `rate_limit_counters` table. An actor already over budget cluster-wide is denied on
  the next local check, even if that replica's own count hasn't hit the limit yet — eventually
  consistent within one sync interval. No effect on a single-replica SQLite deployment.

### Added — Atlas Ops Advisor

- Added `POST /api/atlas/v1/ai/advisor`: explainable risk scoring and prioritized, read-only
  runbooks from Atlas capacity, forecasts, recovery telemetry, alerts, and recent job failures.
- Added an optional OpenAI-compatible summary provider with HTTPS enforcement, bounded context and
  output, prompt-injection resistance, deterministic fallback, operator RBAC, and audit logging.
- Added local/provider mode documentation and unit plus REST/RBAC regression coverage.
- Added a Storage Center **Ops Advisor** page with question presets, explicit local/auto/LLM
  controls, risk visualization, evidence cards, and a prioritized non-executing runbook.
- Added `GET /ai/incidents` to correlate related alerts and recent failures into explainable
  incident families with bounded confidence and visible source signals.
- Added `POST /ai/what-if` plus console controls to compare baseline risk with capacity, growth,
  alert-resolution, and recovery-completion scenarios without changing live inventory.
- Added `GET /ai/anomalies` with model-free median/MAD detection for capacity, I/O, job, and alert
  surges; the console exposes explainable scores and adjustable sensitivity.
- `GET /ai/anomalies` now reports `telemetry_status` (`fresh`/`stale`/`unavailable`) and
  `latest_sample_age_minutes`, pausing detection with a warning when the latest sample is over
  15 minutes old or missing/invalid — an old spike is never presented as a current incident.
- Added `list_incidents`, `detect_anomalies`, and `what_if_capacity` MCP tools alongside
  `ops_advisor`, sharing one code path with their REST counterparts so the MCP edge can't drift;
  `GET /ai/incidents` also gained an opt-in `?mode=` narrative (local by default) reusing the
  advisor's existing OpenAI-compatible provider.

## [0.3.0] — 2026-09-14

### Changed — Dual license (AGPL-3.0 + ACL); remove trial JWT gate

- Open-source under [AGPL-3.0](https://github.com/zyvorai/atlas/blob/v0.3.0/LICENSE)
  ; commercial track via
  [Atlas Commercial License (ACL)](https://github.com/zyvorai/atlas/blob/v0.3.0/COMMERCIAL_LICENSE.md).
  The license notes that applied to this tag are [docs/LICENSING.md at v0.3.0](https://github.com/zyvorai/atlas/blob/v0.3.0/docs/LICENSING.md), not the current license document.
- Removed Ed25519 JWT trial stack: `atlas-license`, `atlas-license-tool`, gateway
  `license_middleware` / `GET /license/status`, `LicenseBanner`, deploy Secret wiring, and
  `ATLAS_LICENSE_ENFORCE`. AGPL self-host is ungated (same model as Aurora).
- Per-file `SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Atlas-Commercial` headers;
  [`LICENSES/` at v0.3.0](https://github.com/zyvorai/atlas/tree/v0.3.0/LICENSES), [`NOTICE`](https://github.com/zyvorai/atlas/blob/v0.3.0/NOTICE), [`CLA.md`](https://github.com/zyvorai/atlas/blob/v0.3.0/CLA.md), [`DCO.md`](https://github.com/zyvorai/atlas/blob/v0.3.0/DCO.md);
  `make headers` / CI license-header lint.

### Added — Relay product ownership

- Document `relay` as an Atlas product consumer in `docs/PRODUCTS.md` (`owner.product=relay`,
  `resource_type=database`, `role=data_disk`). Relay keeps its Postgres ledger; Atlas provisions
  the data PVC and optional RGW backups. Implementation lives in the Relay repo
  (`docs/ATLAS_STORAGE.md`, `scripts/atlas-provision-relay-storage.sh`).

### Added — Apple.com-style console redesign

- Redesigned the React console (side rail, top nav, all 32 routes across 5 page templates) to a
  light/dark Apple product-page aesthetic, layered on top of the existing "Soundings"
  bathymetric UI identity. Verified live against the real-Ceph gateway.

### Added — GitHub-native CI/CD and supply-chain hardening

- Dependabot (cargo/npm ×2/github-actions/docker), CODEOWNERS, issue/PR templates, `SECURITY.md`.
- CodeQL (JavaScript/TypeScript + Rust), `dependency-review` on PRs, OSSF Scorecard, PR-title
  and PR-labeler checks.
- `cargo-llvm-cov` / `vitest --coverage` as CI-artifact-only coverage (no external service).
- `rust-toolchain.toml` + `.nvmrc` pin the Rust and Node versions CI already resolves to.
- Every GitHub Action pinned to a commit SHA; every Docker base image pinned to a `sha256`
  digest; `npm install -g npm@<exact>` in both Dockerfiles.
- `docker-publish.yml` (GHCR push + keyless cosign signing + CycloneDX SBOM + Trivy scan) and
  `release.yml` (CHANGELOG-driven GitHub Releases + cross-platform `atlasctl` binaries + UI dist
  bundle), both gated on `v*` tags.
- Branch protection on `main`: required status checks, admin bypass preserved.

### Fixed — Security hardening

- Legacy `sha256$...` password hashes now upgrade to Argon2 transparently on successful login.
- Audit-log export refuses plaintext HTTP (loopback exempted for tests) — compliance-sensitive
  data no longer leaves the process over an unencrypted connection even if misconfigured.
- `cargo-deny` was silently scoped to `--all-features` by its GitHub Action's own default,
  defeating `deny.toml`'s documented default-features-only intent; corrected.
- Dependency bumps for known advisories surfaced once the above was fixed: `chacha20`, `h2`,
  `rustls` (RUSTSEC-2026-0285, published mid-development — TLS 1.3 handshake messages accepted
  across encryption level boundaries).

### Changed — README, docs site, social card

- Rewrote `README.md`: tech-stack badges, architecture diagram, "Why Atlas" comparison, Star
  History chart, new hero social card (`docs/social/atlas-share-card.svg`/`.png`, hand-authored
  from the project's own design tokens).
- Removed `Co-Authored-By` trailers from the full git history.

## [0.2.0] — 2026-08-25

### Added — Trial/licensing (Ed25519-signed JWT)

- Same design as sibling Zyvor products Veyron (`trial.rs`) and Aurora
  (`gtm_api.middleware.license`): a signed token carries who it was issued to and when it
  expires, verified against an embedded public key — no server-side clock, so deleting local
  state can't extend a trial. New `atlas-license` crate (verify/status, product tag
  `atlas-trial`) and `atlas-license-tool` (sales-only `keygen`/`issue` CLI).
- Gateway wiring: `license_middleware` gates the bearer-protected API with `402` once a trial
  expires; `GET /license/status` stays reachable alongside `/auth/login` and `/auth/oidc/*`
  so an expired install can still show *why*. `LicenseBanner.tsx` in the console UI.
  `Config::license_enforce` defaults to enforced (matching Aurora); local dev
  (`make run`/`make run-databridge`) and both `deploy/k8s/atlas-gateway*.yaml` manifests
  explicitly opt out until a real customer token exists. See `docs/LICENSING.md`.
- `Config::validate_for_start()` now warns (non-fatally) at boot if enforcement is on with no
  locatable token, instead of silently 402ing every protected route with no signal.

### Fixed — Cross-tenant write vulnerability across volumes, buckets, and direct RBD

A real, exploitable pre-existing gap: the tenant-scoping refactor (`require_tenant`/
`tenant_scope` in `auth.rs`) was applied to read handlers but never propagated to write
handlers. A tenant-scoped operator could create a volume or RBD image attributed to (and
quota-charged against) a *different* tenant, or expand/snapshot/clone/restore/resize/migrate/
flatten any tenant's volume or RBD image by id, or delete/prune/download any tenant's bucket,
backup, or bucket object — including minting a presigned S3 download URL for another tenant's
backup data. Fixed at every affected call site in `volumes.rs`, `object_store.rs`, and
`rbd.rs`; centralized in `object_store.rs`'s shared `bucket_s3_target()` helper so every object
operation is covered by one check. Admin-only operations are unchanged (admin is deliberately
global everywhere else in this codebase). New regression test
`operator_cannot_write_across_tenants` in `tenant_isolation.rs`.

### Added — UI lint, component tests, and CI gates

- `eslint.config.js` (flat config) for the gateway console — previously had none. CI now runs
  `npm run lint` + `npm run test` + `npm audit --audit-level=high` (previously only
  `npm run build`); the audit gate found and fixed 2 pre-existing high-severity advisories
  (nanoid, react-router) via a non-breaking patch bump.
- First component tests (`@testing-library/react` + jsdom) — `LicenseBanner.test.tsx`, guarding
  the licensed/expired/days-remaining states after that component shipped with zero tests and
  one real bug (see below) caught only by manual live verification.

### Fixed — License status couldn't distinguish "expired" from "never had a token"

Caught during live verification of the trial/licensing feature above, before it shipped:
`GET /license/status` reported an expired-but-authentically-signed token identically to no
token being present at all (`trial_expired: false` either way), because `jsonwebtoken`'s
built-in `validate_exp` rejected the token before its claims could be inspected. Since
`LicenseBanner.tsx`'s render logic branches on `trial_expired`, this would have silently kept
the "your trial has ended" message from ever appearing. Fixed by deferring expiry checking to
`atlas-license::status()` (which has the real `exp` claim to work from) instead of
`jsonwebtoken`'s decode-time rejection; regression-tested on both the Rust and TypeScript sides.

### Fixed — Real MySQL DataBridge CDC, unrunnable end-to-end before this pass

Live-verified `discover → full-load → cdc/start → validate` against a real Percona edge and a
real Kafka/Strimzi/Debezium stack for the first time; the code path had never been run against
live infra. Found and fixed 8 real bugs: `mysql_operator.rs`'s PXC CR builder never set
`allowUnsafeConfigurations` (blocks a single-node edge from ever reporting `ready`); `loader.rs`'s
`mysqldump` pipeline was missing `CREATE DATABASE` (target db doesn't exist yet), `--no-tablespaces`
(source creds lack `PROCESS`), `--skip-add-locks` (PXC's `pxc_strict_mode` rejects `LOCK TABLES`),
and `--set-gtid-purged=OFF` (conflicts with the edge's own GTID set); `streaming.rs`'s
`connect_spec()` used Strimzi's ~90s-to-kill default probe, too tight for a Debezium+JDBC Connect
image to boot; `streaming.rs`'s Debezium source config was missing `time.precision.mode: connect`;
`deploy/databridge/connect/Dockerfile` pinned Debezium 3.0.8.Final against a Kafka 4.3.0 base
image whose client library removed a method 3.0.8's schema-history recovery depends on
(`NoSuchMethodError`) — bumped to 3.3.0.Final. See `docs/DATABRIDGE.md` for two further
non-code operational findings surfaced getting a row to actually land on the edge: a failed
sink message permanently poisons its Kafka consumer offset (a task restart alone never skips
past it), and Debezium unconditionally encodes MySQL `TIMESTAMP` columns (not `DATETIME`) as
ISO-8601 strings the JDBC sink can't bind — a genuine open gap for any real schema using
`TIMESTAMP`, not yet fixed.
