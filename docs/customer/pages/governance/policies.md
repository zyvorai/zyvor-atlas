<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Policies

## Purpose

Built-in intent → placement catalog (atlas-policy) used when creating volumes.

## When to use it

- Operate **Policies** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/policies`
- Nav: **GOVERNANCE → Policies**

## Operate from the console (UX)

1. Open `/policies` and review intent names (`production`, `database`, …).
2. Use these intents in **Create volume**.
3. Tenant overrides live under Tenants → Policies.
4. **Empty / fail:** Catalog empty → gateway policy pack missing.
5. **Success:** Intents listed and selectable on create.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Volumes](../storage/volumes.md)
- [Tenants](tenants.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
