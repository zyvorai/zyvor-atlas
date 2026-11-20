<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Ceph

## Purpose

Day-2 Ceph signals — health rollup, df pools, OSD tree.

## When to use it

- Operate **Ceph** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/ceph`
- Nav: **INFRASTRUCTURE → Ceph (sidebar Ceph)**

## Operate from the console (UX)

1. Open `/ceph`.
2. Read rollup + df pools; jump to Cluster if needed.
3. Investigate degraded/rebuilding before big provisions.
4. **Empty / fail:** No ceph df → driver/credentials; use lab fake driver only for demos.
5. **Success:** HEALTH_OK (or understood warn) with pools listed.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Cluster](cluster.md)
- [Disaster Recovery](dr.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
