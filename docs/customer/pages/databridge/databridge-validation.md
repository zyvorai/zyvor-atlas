<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Validation

## Purpose

Validation runs and per-table results for migrated data.

## When to use it

- Operate **Validation** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/validation`
- Nav: **DATABRIDGE → Validation**

## Operate from the console (UX)

1. Open `/databridge/validation` after plan stages that emit checks.
2. Open a run → per-table SlideOver.
3. Fix failures before cutover.
4. **Empty / fail:** No runs → trigger validation from Plan Detail / workflow.
5. **Success:** Tables pass; ready for cutover.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Plan Detail](databridge-plans-id.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
