<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Jobs

## Purpose

Durable async jobs for every mutation — progress, SSE live updates, failure detail.

## When to use it

- Operate **Jobs** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/jobs`
- Nav: **OBSERVABILITY → Jobs (sidebar Jobs)**

## Operate from the console (UX)

1. Open `/jobs` after any create/backup/migrate/DR action.
2. Select a job for created time, state, and error detail.
3. Use rail Jobs chip for running count without leaving the page.
4. **Empty / fail:** No jobs → you have not mutated yet; failed jobs → fix cause and retry from source page.
5. **Success:** Job reaches succeeded; inventory refreshes.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Volumes](../storage/volumes.md)
- [Activity](activity.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
