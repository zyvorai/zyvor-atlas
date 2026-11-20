<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Backends

## Purpose

Registered storage backends — discovery and capacity summary.

## When to use it

- Operate **Backends** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/backends`
- Nav: **INFRASTRUCTURE → Backends**

## Operate from the console (UX)

1. Open `/backends`.
2. Confirm at least one backend is registered and healthy.
3. Cordon/uncordon from Maintenance when draining.
4. **Empty / fail:** None registered → install driver/Ceph and discover (`atlasctl discover`).
5. **Success:** Backend row with capacity; Volumes can provision.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Ceph](ceph.md)
- [Maintenance](maintenance.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
