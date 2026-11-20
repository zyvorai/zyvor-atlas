<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Plan Detail

## Purpose

Single migration plan — stages, CDC controls, cutover, and validation hooks.

## When to use it

- Operate **Plan Detail** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/databridge/plans/:id`
- Nav: **DATABRIDGE → Migration Plans → plan row**

## Operate from the console (UX)

1. Open a plan from `/databridge/plans`.
2. Read engine/CDC badges; start or advance stages per UI.
3. **Stop CDC** / **Restart CDC** when streaming; use confirm for destructive cutover actions.
4. Jump to Validation / Replication / Edge clusters as needed.
5. **Empty / fail:** Stage blocked → fix source connectivity or prior stage job.
6. **Success:** CDC live / cutover complete per badges; Jobs clean.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Validation](databridge-validation.md)
- [Replication](databridge-replication.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
