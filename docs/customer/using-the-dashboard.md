<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Using the Dashboard

Storage Center is the Atlas operator console on the gateway (`http://<host>:5110/`).

## Shell chrome

Navigation lives in a persistent **left sidebar**, grouped by six sections. Each section
(except **Storage**) can be collapsed with the chevron on its header; the section containing
your current page stays open. Use **Filter navigation…** at the top of the sidebar to narrow
the list. Collapse the whole sidebar to an icon-only rail from the toggle at its base (state
remembered per browser). Below ~900px the sidebar hides and the hamburger opens the same
nav in a drawer — no duplicate shortcut list.

**Role-aware nav:** Viewer, Operator, and Admin sessions see different sidebar entries (governance
and destructive ops require higher roles). Direct URLs to gated pages show an access message.

The top bar carries everything else:

| Control | What it does |
|---------|----------------|
| Sidebar | STORAGE · DATA PROTECTION · DATABRIDGE · OBSERVABILITY · GOVERNANCE · INFRASTRUCTURE — filter, section collapse, icon rail |
| Brand | Zyvor mark + **Atlas** — click to jump to **Command Deck** |
| Spotlight | **⌘K** / **Ctrl+K** — pools, volumes, modules (role-filtered) |
| Look & feel | Carbon (dark shop, default) or Apple Lite (light shop) |
| Jobs spinner | Shows while jobs are running → `/jobs` |
| Alerts bell | Open alert count → `/alerts` (also under OBSERVABILITY → Alerts) |
| Pause | Freeze auto-refresh |
| Health pill | Ceph/Atlas rollup (HEALTH_OK, DEGRADED, …) |
| Account menu | Clock, paste/clear JWT, sign out |

**Keyboard:** **H** jumps to Command Deck. **?** opens shortcut help.

**Detail pages** (pool drill-down, migration plan detail) show breadcrumbs above the page title.

## Page grammar

Most screens share: **PageHead** (eyebrow + title + live state + ≤2 actions) → table/canvas → SlideOver or FormModal for mutations.

Empty states name the next action (Create volume, Create bucket, Register source, …) — follow those CTAs rather than shrugging.

## Operate tips

1. After any **Create** / **Backup** / **Failover**, open **Jobs** until `succeeded`.
2. Prefer console actions for day-2; use `atlasctl` for scripts and CI.
3. DataBridge: register **Cloud Databases** → **Migration Plans** → Plan Detail (CDC / cutover) → **Validation**.
4. Never paste lab IPs into runbooks — use `<host>`.

## Related

- [Getting Started](getting-started.md)
- [Page index](PAGE_INDEX.md)
- [Workflows](workflows.md)
