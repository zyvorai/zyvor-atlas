<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Protection Status

## Purpose

Per-volume protection verdict — healthy / degraded / unprotected rollup.

## When to use it

- Operate **Protection Status** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/protection`
- Nav: **DATA PROTECTION → Protection Status**

## Operate from the console (UX)

1. Open `/protection`.
2. Scan worst-case headline (healthy vs unprotected counts).
3. Drill volumes missing snaps/backups → Schedules or Backups.
4. **Empty / fail:** No volumes yet → provision first on Volumes.
5. **Success:** Every critical volume shows a healthy verdict.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Schedules](../storage/schedules.md)
- [Backups](backups.md)
- [Volumes](../storage/volumes.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
