---
sidebar_position: 1
title: Quickstart
---

<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->

# Quickstart

Run Atlas locally with the fake Ceph driver — no cluster required.

```bash
make run
# Console: http://127.0.0.1:5110
cargo run -p atlasctl -- --base-url http://127.0.0.1:5110 health
```

Default console login is `admin` with the gateway password for your deployment
(`ATLAS_ADMIN_PASSWORD`, or the shipped lab default when auth is open).

## Deploy to k3s

```bash
./scripts/deploy-remote.sh <host> <user>
# NodePort UI: http://<host>:30510
```

See the repo [`README`](https://github.com/zyvorai/zyvor-atlas#readme) and
[Deployment](./deployment.md)
for Rook Ceph and real-Ceph gateway paths.
