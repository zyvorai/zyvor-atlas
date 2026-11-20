<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Disaster Recovery

## Purpose

Cross-cluster RBD mirroring — peers, mirrors, promote/demote/failover.

## When to use it

- Operate **Disaster Recovery** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/dr`
- Nav: **INFRASTRUCTURE → Disaster Recovery**

## Operate from the console (UX)

1. Open `/dr` and read peers/mirrors + control-plane/dataplane readiness.
2. Add peer; enable mirroring on images; Promote / Demote / Failover with confirms.
3. Before failing back, demote the other site and wait until the mirror's status reports
   `promote_ready`. Atlas refuses a normal promote until then. Force promote only for disasters
   and split-brain recovery.
4. **Empty / fail:** Dataplane unverified → need second Ceph site; peers=0 → add peer first.
5. **Success:** Mirrors healthy; failover job succeeds when rehearsed.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Ceph](ceph.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
