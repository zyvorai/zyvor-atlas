<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Audit

## Purpose

Compliance trail of state-changing and sensitive actions.

## When to use it

- Operate **Audit** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/audit`
- Nav: **OBSERVABILITY → Audit**

## Operate from the console (UX)

1. Open `/audit`.
2. Filter by actor and action.
3. Verify who created tokens, volumes, or DR failovers.
4. **Empty / fail:** No entries → auth/open mode may omit some events; confirm ATLAS auth mode.
5. **Success:** Expected mutations appear with actor + timestamp.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Access](../governance/access.md)
- [Jobs](jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
