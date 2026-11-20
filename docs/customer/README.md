<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Atlas — Customer Documentation

**Atlas** is the storage control plane — Ceph/RBD volumes, protection, DataBridge migrations, and day-2 ops from Storage Center.

| You want to… | Open |
|--------------|------|
| Install and open the console | [Getting Started](getting-started.md) |
| Learn the shell | [Using the Dashboard](using-the-dashboard.md) |
| Follow a page, step by step | [Page-by-page guides](pages/README.md) |
| Look up any screen by route | [Complete page index](PAGE_INDEX.md) |
| Deploy, auth, ports | [Admin basics](admin-basics.md) |
| Multi-page jobs | [Common workflows](workflows.md) |

## Printable PDFs

```bash
set -a; source scripts/customer-docs/product.env; set +a
node scripts/customer-docs/build-customer-pdfs.mjs
```

Output lands in [`pdf/`](pdf/).

## Product at a glance

```text
  Storage Center  →  http://<host>:5110/     (NodePort often :30511 / HTTPS :30543)
  Health          →  GET /health
  Optional gRPC   →  :5111
  CLI             →  atlasctl …
```

Never publish lab IPs in customer docs — use `<host>`.
