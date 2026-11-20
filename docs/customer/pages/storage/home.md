<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Command Deck

## Purpose

Estate overview — capacity sounding, pool tiles, protection gaps, and quick jumps into volumes, snapshots, and backups.

## When to use it

- Operate **Command Deck** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/`
- Nav: **STORAGE → Command Deck** (or press **H**)

## Operate from the console (UX)

1. Open `/` (Command Deck) after the gateway is reachable.
2. Read the health chip on the rail (HEALTH_OK / DEGRADED / …) and live job/alert counts.
3. Use **New volume** to provision, or follow empty-state links (Create a volume →, Schedule nightly snapshots →, Create first bucket →).
4. Open a pool tile to drill into `/pools/:id` (breadcrumbs: Storage · Pools · …), or jump via Spotlight (⌘K / Ctrl+K).
5. Pause auto-refresh from the rail if you need a stable readout.
6. **Empty / fail:** No capacity → register a backend (INFRASTRUCTURE → Backends) and run discover; auth failures → paste JWT from the key icon.
7. **Success:** Sounding orb + pool/OSD lattice populate; jobs appear when you provision.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Volumes](volumes.md)
- [Jobs](../observability/jobs.md)
- [Ceph](../infrastructure/ceph.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
