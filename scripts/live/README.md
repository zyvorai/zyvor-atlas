<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Live remote test suite (Tier 3)

Exercises a **deployed** Atlas gateway over HTTP — default
`http://212.8.248.187:30511` (`atlas-gateway-ceph` NodePort). This is not the
fake-driver `cargo test` path; it mutates the remote cluster and cleans up.

## Run

```bash
ATLAS_LIVE=1 ./scripts/test-live.sh

# or explicitly:
ATLAS_LIVE=1 ATLAS_BASE_URL=http://212.8.248.187:30511 ATLAS_TOKEN=<jwt> \
  ./scripts/live/run-all.sh
```

`scripts/test-all.sh` invokes this when `ATLAS_LIVE=1` is set in the environment.

Without `ATLAS_LIVE=1`, `run-all.sh` exits 2 and does nothing.

## Auth

| Env | Purpose |
|-----|---------|
| `ATLAS_TOKEN` | Prefer a minted admin JWT |
| `ATLAS_BOOTSTRAP_TOKEN` | Bootstrap admin token; suite mints a JWT |
| *(neither)* | SSH to the lab host and read `atlas-gateway-auth` / `bootstrap-admin-token`, then mint |
| `ATLAS_ADMIN_PASSWORD` | Password for `02-auth`'s password-login check. Every deployment mints its own — if unset, the suite reads it straight from the same `atlas-gateway-auth` Secret (key `admin-password`) over SSH, same mechanism as the bootstrap token above. There is no fixed default that works across deployments. |

SSH defaults: `ATLAS_SSH_USER=sus`, `ATLAS_SSH_HOST=212.8.248.187`,
`ATLAS_SSH_IDENTITY=$HOME/.ssh/id_ed25519_hyper2kvm`, secret in
`ATLAS_AUTH_NS=rook-ceph`.

## What it covers

| Section | Checks |
|---------|--------|
| `01-probe` | `/health` `/livez` `/readyz` `/version`; `/metrics` accepts 401 without token and 200 with token |
| `02-auth` | 401 without token; mint + revoke; `POST /auth/login` (bootstrap admin); console users CRUD + login |
| `03-inventory` | ~45 GET inventory / Ceph / DR / DataBridge / audit paths |
| `04-discover` | `POST …/discover` on Ceph only (set `ATLAS_LIVE_DISCOVER_ALL=1` for nfs/zfs) |
| `05-volume-lifecycle` | create → snap → schedule → expand → **delete** on `zyvor-rbd-prod` |
| `06-buckets` | create → get → stats (WARN on timeout) → **delete** |
| `07-rbd-governance` | RBD **create → resize → snap → delete**, usage refresh, maintenance, upgrade preflight |
| `08-metrics-alerts` | `/metrics/{summary,ceph,history,forecast}`, alerts evaluate |
| `09-databridge` | fake Postgres source → discover → plan → assess…validate → **delete plan → delete source** |
| `10-dr` | DR peer create → preflight → direct-RBD mirror attempt (soft) → **delete image → delete peer** |
| `11-databridge-mysql` | fake MySQL source → plan stages → **delete plan → delete source** |
| `12-rook` | `/ceph/{rook-status,health-rollup,pools}` reads → pool **create → (soft) verify Ready → delete**. Pool size/failure-domain are env-overridable (`ATLAS_ROOK_POOL_SIZE`, `ATLAS_ROOK_FAILURE_DOMAIN`, default `1`/`osd` for a single-OSD lab) — the create/delete calls always run, but "Ready" is a WARN not a FAIL since it depends on real cluster capacity, not just the code path |

Writes use prefix `live-<pid>-<ts>-*` and register a `trap` cleanup so leftovers
are deleted even on failure.

## Design notes

- Sections run **sequentially** (SQLite lock contention under concurrent discover
  + writes has knocked the NodePort offline in the lab). Volume create retries once
  on `database is locked`; the gateway busy-timeout is 30s.
- Storage class defaults to `zyvor-rbd-prod` (`ATLAS_STORAGE_CLASS`).
- Bucket `/stats` timeouts count as **WARN**, not FAIL.
- DR promote / OSD out-in are **not** exercised (scaffold / destructive).

## Env reference

| Variable | Default |
|----------|---------|
| `ATLAS_LIVE` | required `1` |
| `ATLAS_BASE_URL` | `http://212.8.248.187:30511` |
| `ATLAS_TOKEN` | *(minted if unset)* |
| `ATLAS_STORAGE_CLASS` | `zyvor-rbd-prod` |
| `ATLAS_TENANT_ID` | `tnt_default` |
| `ATLAS_CURL_TIMEOUT` | `25` |
| `ATLAS_JOB_TIMEOUT` | `90` |
| `ATLAS_LIVE_DISCOVER_ALL` | `0` |
| `ATLAS_LIVE_LOG` | temp file path |
| `ATLAS_ADMIN_USERNAME` | `admin` (password-login check) |
| `ATLAS_ADMIN_PASSWORD` | *(fetched from `atlas-gateway-auth` Secret if unset — see Auth above)* |
