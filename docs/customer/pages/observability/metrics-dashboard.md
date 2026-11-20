<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Metrics

## Purpose

Ceph-native metric samples and OSD utilization averages.

## When to use it

- Operate **Metrics** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/metrics-dashboard`
- Nav: **OBSERVABILITY → Metrics**

## Operate from the console (UX)

1. Open `/metrics-dashboard`.
2. Confirm samples arrive; note OSD average % when shown.
3. Cross-check spikes with Observatory and Ceph.
4. **Empty / fail:** Waiting on samples → backend/Ceph metrics not streaming.
5. **Success:** Sample count > 0 and averages look sane.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Observatory](observatory.md)
- [Ceph](../infrastructure/ceph.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
