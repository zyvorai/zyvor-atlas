<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Buckets

## Purpose

Object gateway buckets for exports and backup destinations.

## When to use it

- Operate **Buckets** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/buckets`
- Nav: **DATA PROTECTION → Buckets**

## Operate from the console (UX)

1. Open `/buckets`.
2. **Create bucket** — name + backend/RGW target.
3. Open a bucket to list/upload objects when the panel is wired.
4. **Empty / fail:** No RGW → check Ceph/RGW on Backends/Ceph pages.
5. **Success:** Bucket listed; usable as Backup destination.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Backups](backups.md)
- [Ceph](../infrastructure/ceph.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
