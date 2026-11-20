<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Tenants

## Purpose

Tenant index — quotas and policy overrides.

## When to use it

- Operate **Tenants** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/tenants`
- Nav: **GOVERNANCE → Tenants**

## Operate from the console (UX)

1. Open `/tenants`.
2. Open a tenant → set Quota (Save) or review Policies slide-over.
3. Overrides fall back to built-in catalog when empty.
4. **Empty / fail:** No tenants → create via API/admin path your deploy uses; quotas block create if exceeded.
5. **Success:** Quota saved; volume creates respect limits.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Policies](policies.md)
- [Access](access.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
