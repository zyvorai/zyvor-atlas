<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Replication

## Purpose

CDC replication streams for active migration plans.

## When to use it

- Operate **Replication** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/replication`
- Nav: **DATABRIDGE → Replication**

## Operate from the console (UX)

1. Open `/databridge/replication`.
2. Confirm streams match plans with CDC started.
3. Restart CDC from Plan Detail if a stream stalls.
4. **Empty / fail:** No streams → Start CDC on a plan.
5. **Success:** Stream lag/state acceptable for cutover.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Plan Detail](databridge-plans-id.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
