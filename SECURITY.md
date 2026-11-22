<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->

# Security Policy

## Supported versions

Atlas is pre-1.0: only the latest tagged release (currently `v0.5.0`) and `main` receive security
fixes. There is no long-term-support line yet — upgrade to the latest tag to stay covered. See
[CHANGELOG.md](CHANGELOG.md) for what changed between releases.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Preferred: use [GitHub's private vulnerability reporting](https://github.com/zyvorai/atlas/security/advisories/new)
for this repository (Security tab → Report a vulnerability).

Alternative: email **[sales@zyvor.dev](mailto:sales@zyvor.dev)** with a description of the
issue, affected component(s), and reproduction steps if available. We'll acknowledge within a
few business days.

## Supply-chain reporting

Dependency-level vulnerabilities (crates.io / npm advisories) are tracked automatically via
`cargo-deny` (see [`deny.toml`](deny.toml)) and GitHub's Dependabot alerts — no need to report
those separately unless you believe an entry is being missed or misclassified.
