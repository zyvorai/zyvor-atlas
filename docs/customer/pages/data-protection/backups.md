<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Backups

## Purpose

Volume backups into object buckets — create, restore, and retire copies.

## When to use it

- Operate **Backups** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/backups`
- Nav: **DATA PROTECTION → Backups**

## Operate from the console (UX)

1. Open `/backups`.
2. **Backup** — select volume + destination bucket; wait for the job.
3. Restore opens a modal (optional new volume name).
4. Bulk-delete selected backups when retiring.
5. **Empty / fail:** Create a bucket first under Buckets; ensure volume exists.
6. **Success:** Backup row with created time; restore job completes.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Buckets](buckets.md)
- [Volumes](../storage/volumes.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
