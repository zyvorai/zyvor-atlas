<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Cluster

## Purpose

Primary cluster inventory — health, pools, OSDs.

## When to use it

- Operate **Cluster** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/cluster`
- Nav: **INFRASTRUCTURE → Cluster**

## Operate from the console (UX)

1. Open `/cluster`.
2. Read primary name/health and pool/OSD counts.
3. Drill pools toward Ceph / Pool detail.
4. **Empty / fail:** Waiting on inventory → gateway↔Ceph connectivity.
5. **Success:** Health matches Command Deck rollup.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Ceph](ceph.md)
- [Command Deck](../storage/home.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
