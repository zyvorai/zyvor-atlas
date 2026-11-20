<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Observatory

## Purpose

Estate telemetry canvas — capacity lenses and jump to Deck or Alerts.

## When to use it

- Operate **Observatory** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/observatory`
- Nav: **OBSERVABILITY → Observatory (sidebar Observatory)**

## Operate from the console (UX)

1. Open `/observatory` and wait for estate samples.
2. Use lenses / panels for pool and OSD pressure.
3. Jump **Command Deck** or **Alerts** from PageHead actions.
4. **Empty / fail:** No samples → Ceph metrics path cold; check `/metrics-dashboard` and gateway.
5. **Success:** Live estate picture matches Deck health.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Command Deck](../storage/home.md)
- [Alerts](alerts.md)
- [Metrics](metrics-dashboard.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
