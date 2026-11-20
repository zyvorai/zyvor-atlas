<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Atlas UI — Apple product design contract

Paste reference: apple.com product pages + Mac Terminal for ops output. This section is **policy**.
If they disagree: this file wins on rules; live pixels win for boxes / type / chrome.

## Identity

**Storage is a product. Capacity is a choice. Atlas is the shop.**

| Shell | Base | Accent | Mono | Metaphor |
|---|---|---|---|---|
| **Night** | graphite `#0b0b0f` | Electric blue `#0A84FF` | SF Mono | dark product night |
| **Day** | shop gray `#F5F5F7` | Apple Blue `#0071E3` | SF Mono | light shop |

Cosmic Orange is reserved for the brand mark only — never for CTAs or selected tiles.

Tokens live in `crates/atlas-gateway/ui/src/atlas-soundings.css` + `index.css` (`--at-*`, `--d*`).

## Colour law

- **One interaction colour:** `--at-cyan` (buttons, focus, links, active nav, selected tiles).
- **Depth ramp is state, never interaction** — use `depth(pct)` from `lib/depth.ts`.
- Amber/red (`--at-warn` / `--at-fail`) = cluster health verdicts, not capacity.

## Type law

- **SF system stack** — titles, labels, prose, buttons.
- **SF Mono stack** — numbers, pool/volume names, paths, timestamps, health strings, TerminalPane.

## Shell chrome

- **Top bar (44px) is the only nav surface.** Zyvor mark + Atlas · primary destinations (Overview, Volumes, Observatory, Ceph, DataBridge, Jobs, Alerts) · section overflow menus (Storage extras, Protect, DataBridge extras, Observe extras, Govern, Infra) · right tray (launch pad, search, theme, jobs, alerts, pause, health, account — account includes role, Launch pad, Settings, Auth token, sign out).
- **No vertical icon rail.** Pinned shortcuts (`pinned: true` in `nav/routes.ts`) are not rendered as a separate column — they exist only as single-key shortcuts and the Control Center quick list; the content area runs full width under the top bar.
- **Section overflow:** hover/click dropdown per non-primary section (Storage extras, Protect, DataBridge extras, Observe extras, Govern, Infra) — a real apple.com mega-menu, not a drawer.
- **No mac dock.** Launch pad stays behind ⌘⌥L / account.
- Mobile ≤900px: hamburger drawer with labeled sections (no Suite links).

## Terminal surfaces

Ops dumps (Jobs results/errors, Access tokens, ApiDocs samples, Ceph CRUSH trees) use **TerminalPane**: black `#1e1e1e` canvas, SF Mono, colorful JSON/ANSI-style spans.

## Page archetypes

| Role | Apple analogue | Ship on | Template component |
|---|---|---|---|
| Product hero | Product landing | Overview | `ui/templates/DashboardHero.tsx` |
| Instrument / Ops console | Tech specs / Diagnostics | Observatory, Metrics, Ceph, Maintenance, DR | `ui/templates/DashboardHero.tsx` |
| Catalog index / Timeline | Store browse / Activity feed | Volumes, Snapshots, Backups, Activity, Audit, Alerts, Jobs, Access, … | `ui/templates/ListPage.tsx` |
| Reference (settings) | Preference pane | Settings | `ui/templates/SettingsPage.tsx` |
| Reference (docs) | Spec sheet | ApiDocs | `ui/templates/DocsPage.tsx` |
| Detail | Configurator | PoolDetail, PlanDetail | `ui/templates/DetailPage.tsx` |

Template components are the single source of truth for each archetype's layout — a page and this
table must not drift; update both together.

## Emptiness & motion

- Naked `0` is banned — fill hint + action inside a shop empty box.
- Swipe rails + page enter; respect `prefers-reduced-motion`.
