<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# MCP directory listings

Draft text for MCP directories and awesome-lists. Nothing here has been submitted; the maintainers
decide when and where to post it. Keep claims factual: other storage vendors also ship MCP servers (see
[COMPARISON.md](COMPARISON.md#other-storage-mcp-servers)), so do not describe Atlas as the first.

## One-line description

Atlas: Apache-2.0 storage control plane (Ceph, NFS, ZFS, Longhorn, native filesystem). MCP server with 31
read-only tools (inventory, Ops Advisor, eBPF I/O risk, Ceph, DR, DataBridge, jobs, audit) and two opt-in,
audited, two-phase writes (`create_snapshot`, `ack_alert`); self-hosted, bearer-JWT auth, tenant-scoped.

## Fields most directories ask for

| Field | Value |
|---|---|
| Repository | `https://github.com/zyvorai/zyvor-atlas` |
| Licence | Apache-2.0 |
| Language | Rust |
| Transport | streamable HTTP (`POST /api/atlas/v1/mcp` on the user's own gateway) |
| Auth | `Authorization: Bearer <operator JWT>` (`examples/agent/mint-token.sh`) |
| Hosting | self-hosted only (no public endpoint) |
| Tools | see [MCP.md](MCP.md) |
| Client configs | `examples/agent/claude-mcp.json`, `examples/agent/hermes-config.yaml` |

## awesome-mcp-servers draft

The list's entries are one line each: repository link, a few tag emoji for language, scope and platform,
then a short description. Check the list's current `CONTRIBUTING.md` for the exact tags and the right
category (likely a cloud/infrastructure or "other tools" section) before opening the pull request.

```
- [zyvorai/zyvor-atlas](https://github.com/zyvorai/zyvor-atlas) 🦀 🏠 🐧 – Storage control plane over Ceph, NFS, ZFS and Longhorn: read-only MCP tools for inventory, risk scoring, eBPF I/O risk, DR status and jobs, plus opt-in, audited, two-phase snapshot and alert-ack writes.
```

## Registry notes

The official MCP registry and similar directories describe either a package or a hosted remote URL.
Atlas is self-hosted, so there is no single URL to register. If a directory only accepts hosted
servers, point it at the documentation instead of inventing an endpoint.
