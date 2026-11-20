<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Cloud Databases

## Purpose

Register external / cloud database sources for DataBridge migrations.

## When to use it

- Operate **Cloud Databases** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/sources`
- Nav: **DATABRIDGE → Cloud Databases**

## Operate from the console (UX)

1. Open `/databridge/sources`.
2. **Register** a source database (connection + engine).
3. Open a source → schema SlideOver (tables).
4. Then create a Migration Plan.
5. **Empty / fail:** Connection refused → network/creds; schema empty → privileges.
6. **Success:** Source listed with reachable schema.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Migration Plans](databridge-plans.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
