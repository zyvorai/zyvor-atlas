<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Migration Plans

## Purpose

Create and list DataBridge migration plans from registered sources.

## When to use it

- Operate **Migration Plans** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/plans`
- Nav: **DATABRIDGE → Migration Plans**

## Operate from the console (UX)

1. Open `/databridge/plans` after registering a source.
2. **Create** New migration plan → open Plan Detail.
3. Track stages and CDC from the detail page.
4. **Empty / fail:** No plans → register a Cloud Database first.
5. **Success:** Plan row links to `/databridge/plans/:id`.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Cloud Databases](databridge-sources.md)
- [Plan Detail](databridge-plans-id.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
