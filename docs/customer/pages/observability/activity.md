<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Activity

## Purpose

Recent operator and system activity stream.

## When to use it

- Operate **Activity** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/activity`
- Nav: **OBSERVABILITY → Activity**

## Operate from the console (UX)

1. Open `/activity`.
2. Scan recent events after provision/protect/migrate actions.
3. Correlate failures with Jobs and Audit.
4. **Empty / fail:** Quiet estate → trigger a volume op and refresh.
5. **Success:** Activity lines match your last mutations.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Jobs](jobs.md)
- [Audit](audit.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
