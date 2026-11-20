<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Edge DB Clusters

## Purpose

Edge database clusters provisioned as migration targets.

## When to use it

- Operate **Edge DB Clusters** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/edge-clusters`
- Nav: **DATABRIDGE → Edge DB Clusters**

## Operate from the console (UX)

1. Open `/databridge/edge-clusters`.
2. Provision from a migration plan when empty.
3. Confirm cluster health before cutover.
4. **Empty / fail:** No clusters → complete plan provisioning stage first.
5. **Success:** Cluster listed and healthy for the plan.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Migration Plans](databridge-plans.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
