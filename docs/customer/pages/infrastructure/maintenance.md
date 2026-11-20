<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Maintenance

## Purpose

Pause the job engine, cordon backends, and clean orphan backups.

## When to use it

- Operate **Maintenance** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/maintenance`
- Nav: **INFRASTRUCTURE → Maintenance**

## Operate from the console (UX)

1. Open `/maintenance`.
2. Pause job engine only when you intend to quiesce (new jobs queue).
3. Cordon a backend to reject new provisioning.
4. Delete orphan backups carefully.
5. **Empty / fail:** Pause stuck → check API auth; cordon blocked → role.
6. **Success:** Desired pause/cordon state reflected; resume when done.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Backends](backends.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
