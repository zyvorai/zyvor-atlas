<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Pool Detail

## Purpose

Single pool sounding — volumes and OSD cells for one pool.

## When to use it

- Operate **Pool Detail** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/pools/:id`
- Nav: **Command Deck / Cluster → pool tile**

## Operate from the console (UX)

1. Open `/pools/:id` from a Deck or Cluster pool link.
2. Review fill, volume list, OSD lattice.
3. Jump to Volumes or Command Deck from actions.
4. **Empty / fail:** Pool not found → stale id; return to Deck.
5. **Success:** Pool metrics match Ceph df.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Command Deck](../storage/home.md)
- [Volumes](../storage/volumes.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
