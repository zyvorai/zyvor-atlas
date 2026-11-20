<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Kubernetes

## Purpose

Discovered StorageClasses from the attached cluster.

## When to use it

- Operate **Kubernetes** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/kubernetes`
- Nav: **INFRASTRUCTURE → Kubernetes**

## Operate from the console (UX)

1. Open `/kubernetes`.
2. Verify StorageClasses expected by CSI consumers.
3. **Empty / fail:** No classes → check KUBECONFIG / RBAC for the gateway.
4. **Success:** Classes listed for the cluster Atlas manages.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Cluster](cluster.md)
- [Backends](backends.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
