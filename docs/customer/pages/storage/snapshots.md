<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Snapshots

## Purpose

Point-in-time volume snapshots — clone or restore into new volumes.

## When to use it

- Operate **Snapshots** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/snapshots`
- Nav: **STORAGE → Snapshots**

## Operate from the console (UX)

1. Open `/snapshots` to list PIT copies.
2. Create snaps from **Volumes** row → Snapshot (or schedule).
3. Clone or Restore from a snapshot row (confirm modal).
4. Bulk-delete selected snaps when retiring retention.
5. **Empty / fail:** No copies → open Volumes and snapshot a volume first.
6. **Success:** Snapshot row + clone/restore job succeeds.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Volumes](volumes.md)
- [Schedules](schedules.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
