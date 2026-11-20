<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# RBD Images

## Purpose

Raw Ceph RBD images for machina/libvirt and bare VMs (bypassing CSI).

## When to use it

- Operate **RBD Images** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/rbd`
- Nav: **STORAGE → RBD Images**

## Operate from the console (UX)

1. Open `/rbd` and select the target pool (default often `rbd-nvme-prod`).
2. **Create image** with name + size; or Flatten / Clone / Resize / Snapshot from row actions.
3. Clone needs an existing snapshot — create under Snaps first if the hint says so.
4. Rollback is destructive — confirm before reverting to a snap.
5. **Empty / fail:** Wrong pool → switch pool; Ceph not ready → INFRASTRUCTURE → Ceph.
6. **Success:** Image listed in pool; jobs complete for flatten/clone/resize.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Ceph](../infrastructure/ceph.md)
- [Snapshots](snapshots.md)
- [Jobs](../observability/jobs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
