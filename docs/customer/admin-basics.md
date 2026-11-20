<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Admin Basics (Atlas)

## Ports

| Port | Service |
|------|---------|
| **5110** | REST + Storage Center UI (`ATLAS_BIND_ADDR`) |
| **5111** | gRPC (`ATLAS_GRPC_ADDR`; empty disables) |
| **30511** | Common cluster REST/UI NodePort |
| **30512** | Common gRPC NodePort |
| **30543** | Optional HTTPS NodePort |

Always document hosts as `<host>` — never paste lab IPs into customer runbooks.

## Auth

- Lab: `ATLAS_AUTH_REQUIRED=0` (open).
- Production: `ATLAS_AUTH_REQUIRED=1` + JWT secret (`atlas-gateway-auth`); weak defaults rejected at start.
- Mint tokens: `POST /api/atlas/v1/auth/tokens`, or paste into the rail **key** icon.
- Local users: **GOVERNANCE → Access → Create user**.
- CLI: `--token` / `ATLAS_TOKEN`; API base `/api/atlas/v1/...`.
- Unauthenticated: `/health`, `/metrics`.

## Install sketch

```bash
make run          # local gateway + UI
atlasctl health
atlasctl discover
```

Open `http://<host>:5110/`. Cluster: follow product deployment docs / Ceph gateway manifests.

## Operate from the console (admin)

1. Confirm `/health` and Command Deck health chip.
2. **Backends** registered → **Volumes** can provision.
3. Pause / cordon only from **Maintenance** when you mean to quiesce.
4. Mutations return `202` + job id — watch **Jobs** (UI or gRPC `WatchJob`).

## Related

- [Getting Started](getting-started.md)
- [Using the Dashboard](using-the-dashboard.md)
- [Disaster Recovery](pages/infrastructure/dr.md)
