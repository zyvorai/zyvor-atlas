<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Alerts

## Purpose

Open alert ledger — silence or resolve before capacity work.

## When to use it

- Operate **Alerts** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/alerts`
- Nav: **OBSERVABILITY → Alerts**, or the **top-bar bell** popover (same destination)

## Operate from the console (UX)

1. Open `/alerts` (or top-bar bell → jump).
2. Filter by state; read critical vs open counts in PageHead.
3. **Silence** (1h) or **Resolve** with confirm.
4. **Empty / fail:** Filter too tight → clear filter; webhook paging may still need silence.
5. **Success:** Criticals cleared or deliberately silenced.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Jobs](jobs.md)
- [Ceph](../infrastructure/ceph.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
