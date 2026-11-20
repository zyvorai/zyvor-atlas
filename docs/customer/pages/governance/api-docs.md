<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# API Docs

## Purpose

Curated REST + gRPC map for operators and integrators.

## When to use it

- Operate **API Docs** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/api-docs`
- Nav: **GOVERNANCE → API Docs**

## Operate from the console (UX)

1. Open `/api-docs`.
2. Scan Meta, Auth, Inventory, Write path (async jobs), Observability, DR, DataBridge.
3. Prefer `atlasctl` or REST with JWT; mutations return jobs.
4. **Empty / fail:** N/A — static reference; deep examples in product API docs.
5. **Success:** You can name the endpoint for the console action you just took.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Jobs](../observability/jobs.md)
- [Access](access.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
