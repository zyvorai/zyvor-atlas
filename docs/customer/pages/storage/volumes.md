<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Volumes

## Purpose

Intent-backed volume inventory — create, expand, snapshot, schedule, and delete across backends.

## When to use it

- Operate **Volumes** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/volumes`
- Nav: **STORAGE → Volumes (sidebar Volumes)**

## Operate from the console (UX)

1. Open `/volumes` and wait for the inventory table.
2. Click **Create volume** — pick name, size, policy/intent, tenant; submit starts a durable job.
3. Use **Refresh** / **Export CSV**; select rows for bulk delete when needed.
4. Open a row → SlideOver: Snapshot, Expand, Create schedule, or delete.
5. Watch **Jobs** (`/jobs`) until create/expand/snapshot succeed.
6. **Empty / fail:** No volumes → Create volume; empty after create → check Jobs and backend registration.
7. **Success:** Volume appears with state/size; mutations return `202` + job id.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Snapshots](snapshots.md)
- [Schedules](schedules.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
