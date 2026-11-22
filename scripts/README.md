<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Atlas scripts

Reusable automation for testing and deploying Atlas. Run from the repo root.

## License headers

```bash
make headers                              # CI gate — scripts/check-license-headers.sh
python3 scripts/rewrite-spdx-headers.py   # one-shot rewrite of legacy banners → SPDX
```

Every source file needs Zyvor copyright +
`SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0`.
See [`CLA.md`](../CLA.md), [`DCO.md`](../DCO.md), [`NOTICE`](../NOTICE).

## `test-all.sh` — full local gate
One-shot "test everything that needs no external infra, then real DBs if a container runtime is up."

```bash
scripts/test-all.sh                 # clippy + workspace tests + feature compiles + container connector tests
scripts/test-all.sh --no-containers # skip the real-DB step (Tiers 0–1 only)
```

Runs: `cargo clippy -D warnings` → `cargo test --workspace` (unit + gateway integration incl.
`observability.rs` and `databridge_engines.rs`) → optional-feature compile checks
(`sqlserver`/`oracle`/`mongodb`, plus `kafka-lag` when `cmake` is present) → `test-connectors.sh`.
Prints the opt-in follow-ups (Tier 3 real Ceph/k8s write path, Tier 4 UI smoke) at the end.

> Note: `cargo fmt --all --check` is intentionally not part of the gate — the repo's dense one-line
> style predates current stable rustfmt, so CI runs the fmt step `continue-on-error` and clippy/test/
> build are the enforced gates.

## `test-connectors.sh` — real DataBridge source connectors via containers
Spins an ephemeral DB per engine (podman or docker), seeds a `customers`/`orders` schema, exports the
matching `DATABRIDGE_TEST_*` var, runs that engine's env-gated `#[tokio::test]` discovery test, and
tears the container down.

```bash
scripts/test-connectors.sh                  # postgres, mysql, mariadb, mongo, mssql
scripts/test-connectors.sh pg mysql mongo   # a subset
RUNTIME=docker scripts/test-connectors.sh   # force docker
```

The gated tests skip (pass as no-op) when their var is unset, so `cargo test --workspace` stays
infra-free. Engine → var → feature:

| Engine | `DATABRIDGE_TEST_*` (value: `host,port,database,user,password`) | cargo feature |
|---|---|---|
| Postgres | `DATABRIDGE_TEST_PG` (libpq conn string) | *(default)* |
| MySQL | `DATABRIDGE_TEST_MYSQL` | *(default)* |
| MariaDB | `DATABRIDGE_TEST_MARIADB` | *(default)* |
| MongoDB | `DATABRIDGE_TEST_MONGO` | `mongodb` |
| SQL Server | `DATABRIDGE_TEST_MSSQL` | `sqlserver` |

Oracle is compile-only (needs the OCI client): `cargo build -p atlas-databridge --features oracle`.

## `deploy-remote.sh` — deploy the gateway to a remote k3s host
rsync → podman build → import into k3s containerd → `kubectl apply` + **rollout restart** (so a
same-tag `:dev` image actually rolls out) → verify `/health` + `/storage-classes` over NodePort 30510.

```bash
scripts/deploy-remote.sh <host> <user>                  # e.g. 212.8.248.187 sus
scripts/deploy-remote.sh <host> <user> --with-ceph      # also run the Rook Ceph lab (DESTRUCTIVE: consumes a disk)
scripts/deploy-remote.sh <host> <user> --with-k3s-disk  # move the k3s data-dir onto a carved partition
scripts/deploy-remote.sh <host> <user> --rollback       # revert the gateway to its previous revision (rollout undo)
scripts/deploy-remote.sh <host> <user> --force          # deploy even if GET /upgrade/preflight reports blockers
```

Before rolling, it gates on the gateway's `GET /upgrade/preflight` (no HEALTH_ERR cluster / open
critical alerts / in-flight jobs / lagging CDC) — pass `--force` to override, or drain first via
`POST /maintenance {"paused":true}`.

Needs local `ssh`+`rsync` and remote `podman`+`k3s`+`kubectl`. The built image runs **default cargo
features** (real Postgres/MySQL/MariaDB + fake for all); SQL Server/Oracle/MongoDB real connectors and
`kafka-lag` need their features added to the `Dockerfile` build.

## `deploy-ceph-gateway-remote.sh` — real-Ceph gateway on a remote k3s host

Deploys `atlas-gateway-ceph` into `rook-ceph` (NodePort **30511**). Builds `Dockerfile.ceph`
(Squid `ceph-common`) on the remote with podman.

```bash
scripts/deploy-ceph-gateway-remote.sh <host> <user>   # e.g. 212.8.248.187 sus
ATLAS_SSH_KEY=~/.ssh/id_ed25519_hyper2kvm scripts/deploy-ceph-gateway-remote.sh <host> <user>
```

Idempotent helpers the script owns (do not do these by hand first):
- SSH identity: `ATLAS_SSH_KEY` / `SSH_KEY` / `~/.ssh/id_ed25519_hyper2kvm`
- install **podman** via apt if missing + set `unqualified-search-registries = ["docker.io"]`
- import `localhost/atlas-gateway:ceph` into k3s (`imagePullPolicy: Never`)
- ensure ClusterRole, `atlas-tls`, `atlas-gateway-auth` (JWT + bootstrap admin token)

**Prereq:** Rook Ceph Ready **and** CSI drivers installed (`zyvor-rbd-prod` must bind). See
`deploy/rook-ceph-lab/up.sh --single-node` and [docs/DEPLOYMENT.md](../docs/DEPLOYMENT.md).

## `io500-native-remote.sh` — IO500 on an atlas-native release

Builds `Dockerfile.io500` on a remote k3s host, creates a scratch filesystem in an existing
`atlas-native` Helm release, runs IO500 in one privileged pod against it, copies the results to
`./io500-results/<timestamp>/` and deletes the pod and the filesystem.

```bash
scripts/io500-native-remote.sh <host> <user> --namespace atlas-native --np 16
```

Defaults are IO500's (stonewall 300 s, full-size phases); see [docs/IO500.md](../docs/IO500.md).

## `demo/record-atlas-demo.mjs` + `demo/upload-atlas-demo.py` — client demo reel
Playwright drives a real Chromium session against a live gateway (login → Command Deck → Volumes →
DataBridge → Observatory → Ceph), burns in a caption/subtitle bar per scene, and `recordVideo` +
ffmpeg produce a 1440×900 (web) and 1920×1080 mp4. `upload-atlas-demo.py` pushes the result to
YouTube via the Data API v3 (reuses the OAuth token convention from the other Zyvor product demos).

```bash
node scripts/demo/record-atlas-demo.mjs http://<gateway-host>:<port>
python3 scripts/demo/upload-atlas-demo.py atlas-storage-center-demo-1080p.mp4 --token /path/to/token.json
```

Needs `playwright` (+ Chromium) resolvable from `scripts/demo/` and `ffmpeg` on PATH for the recorder;
`google-auth-oauthlib` + `google-api-python-client` for the uploader.

## `demo/record-atlas-workflow-demo.mjs` + `demo/upload-atlas-workflow-demo.py` — hands-on workflow demo
Same recorder pipeline, but it actually drives the write path instead of just touring pages: create
a volume with an intent-based policy → open its detail drawer → snapshot it → expand it live to
40 GiB → cut to `/jobs` and wait for both async jobs to genuinely reconcile (the fake driver
simulates realistic Ceph resize/snapshot latency) → confirm the resize back on the Volumes list.
Expect ~2 minutes end to end — this is real job completion time, not scripted delay.

```bash
node scripts/demo/record-atlas-workflow-demo.mjs http://<gateway-host>:<port>
python3 scripts/demo/upload-atlas-workflow-demo.py atlas-create-volume-workflow-1080p.mp4 --token /path/to/token.json
```

Creates and deletes a real `wow-demo-<id>` volume against whatever gateway you point it at — clean
up manually (`DELETE /api/atlas/v1/volumes/<id>?force=true`) if the script errors out mid-run.

## Related (under `deploy/`)
- `deploy/rook-ceph-lab/up.sh` — Rook **v1.20.2** + Ceph **Squid v19.2.3** + `ceph-csi-drivers` +
  `zyvor-*` StorageClasses. Use `--single-node` (and `--cluster-only` if the operator is already up).
- `deploy/databridge/up.sh` — installs the edge operators (CloudNativePG / Percona / Strimzi) for real migrations.
- `deploy/observability/up.sh` — observability stack.
