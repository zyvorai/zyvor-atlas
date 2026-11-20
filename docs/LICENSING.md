<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Licensing

Atlas is licensed under the **[Apache License, Version 2.0](../LICENSE)**.

You may use, modify and redistribute it — including in production, in SaaS and managed services, and in
products you sell — under the terms of that license (attribution, a copy of the license, a statement of
changes, and the patent grant/termination terms in §3). There is no runtime license key and no usage
restriction beyond the license itself.

SPDX identifier used in source headers and Cargo/npm metadata: `Apache-2.0` (see
[`LICENSES/Apache-2.0.txt`](../LICENSES/Apache-2.0.txt), [`NOTICE`](../NOTICE)).

Contributions require the [CLA](../CLA.md) and [DCO](../DCO.md) (`git commit -s`). Header checks: `make headers`.

## SQL migrations keep their original headers

The files under `migrations/` and `migrations-postgres/` are licensed under Apache-2.0 like everything
else in this repository, but their header comments are frozen: the migration runner (sqlx) records a
checksum of each applied file, so editing even a comment makes existing databases refuse to start. They
are therefore exempt from the SPDX-identifier check (`scripts/check-license-headers.sh`), and must never be
edited after release.

## Older releases

This repository carries a single license: Apache-2.0. Releases that were published earlier under
different terms keep the terms they were published with — see the licensing notes at the relevant git
tag and [`CHANGELOG.md`](../CHANGELOG.md).

## Bundled third-party software

The gateway images include, under their own licenses: the Oracle Instant Client (Oracle's
redistributable Basic Lite license) for the DataBridge Oracle connector — see [`NOTICE`](../NOTICE) and the
Dockerfiles. Rust and frontend dependency licenses are policed by `cargo deny` (`deny.toml`) and declared in
`package-lock.json`.

**Contact:** [https://zyvor.dev](https://zyvor.dev)
