<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Atlas UI design contract

This is policy for the React console in `crates/atlas-gateway/ui`. A page that disagrees with it is
wrong, or this file is updated in the same change.

## Identity

Large, confident headlines; short copy; generous space around big rounded tiles; one blue accent;
graphics that carry real data. It borrows the feel of a modern product page and stays an operations
console: **tables stay dense, legible and fast.**

- System font stack only. No web fonts, no remote stylesheets, no third-party logos, product
  imagery or marketing copy. The Zyvor mark is the only brand image.
- Dark (true black) and light (white and `#f5f5f7`) are equal; neither is an afterthought.

## Where things live (`ui/src`)

| File | Owns |
|---|---|
| `index.css` | Entry. Fixes the cascade layer order and maps Tailwind colours to tokens. Imported first by `main.tsx`. |
| `design/tokens.css` | Every colour, radius, spacing, density and motion value. |
| `design/base.css` | Element defaults, focus ring, reduced motion. |
| `design/controls.css` | Buttons, links, fields, chips, badges, choice tiles. |
| `design/surfaces.css` | Page head, modules, panels, stat tiles, lists, bars, status grids, charts, galleries, empty states, reference rows, terminal pane. |
| `design/table.css` | `.at-tbl` / `.ztable`. |
| `design/overlays.css` | Menus, modal, slide-over, toasts, command palette, launch pad. |
| `design/shell.css` | Global nav, flyouts, local nav, mobile sheet, page canvas. |
| `views/<Page>.css` | Page-specific rules only, wrapped in `@layer components`, imported by that page, with a page prefix (`ov-`, `lg-`). |
| `graphics/` | Reusable graphics components. They read the tokens below. |

Do not add a second override layer on top of these. Change the rule where it lives.

## Tokens

Theme selector: `data-theme="dark" | "light"` on `<html>`.

| Token | Meaning |
|---|---|
| `--a-bg` | Page canvas |
| `--a-surface` | Tile, panel, table |
| `--a-surface-2` | Inner fill, row hover (`--a-surface-3` for tracks and neutral buttons) |
| `--a-ink` / `--a-ink-2` | Primary / secondary text. There is no third text colour. |
| `--a-line` | Hairline (`--a-line-strong` for form-control borders, 3:1) |
| `--a-blue` | The accent: links, focus, selection, graphics. Safe as text. (`--a-blue-fill` is the button fill under white text.) |
| `--a-ok` / `--a-warn` / `--a-fail` | State only, each with a `-wash` background |
| `--a-series-2` | Second chart series. Never interaction. |
| `--a-radius` | Tile radius (`-lg` feature tiles and sheets, `-sm`, `-xs`; `--a-pill`) |
| `--a-font` / `--a-mono` | System sans / system monospace |

The `--at-*`, `--d1`–`--d5` names at the end of `tokens.css` are compatibility aliases for pages not
yet restyled. Do not use them in new work; delete each once nothing reads it.

Canvas code must read colours from computed custom properties, and tokens are therefore plain
values (no `light-dark()`).

## Theme behaviour

`lib/themes.ts`. The preference is **System** (default, follows the OS live), **Light** or **Dark**.
A manual choice persists in `localStorage["atlas.theme"]`; values from the earlier console
(`night`, `day`, `carbon`, `apple-lite`) are migrated. The picker (`ui/ThemeMenu.tsx`) is in the
global nav and on sign-in; Settings offers the same three choices.

## Colour law

- One interaction colour, blue. State colours never mark something clickable.
- Capacity: blue below 75 %, amber from 75 %, red from 90 % (`lib/depth.ts`, classes `dp1`–`dp5`).
- Health: green, amber, red, and a text label or a different shape as well — never colour alone.
- Both themes meet WCAG AA: 4.5:1 text, 3:1 large text, control borders and focus ring.

## Type law

- Page headline `.at-title` 36–60 px; hero headline 44–88 px; module headline 21–28 px; body 14 px;
  tables 13 px.
- Sentence case. No letter-spaced capitals for captions or table headers.
- Monospace for identifiers, paths, timestamps, health strings and terminal output. Numbers in
  tables use tabular figures.

## Shell

- **Global nav** (48 px, translucent): brand, *Overview*, one button per section, then search,
  launch pad, theme, running jobs, alerts, pause, health and account.
- **Flyout**: a section button opens a full-width panel with that section's pages (hover or click;
  Arrow Down moves into it; Escape closes and returns focus). The page behind is dimmed and blurred.
- **Local nav** (desktop, every page except Overview): section name and its sibling pages.
- **Below 1024 px**: menu button opens a full-screen navigation sheet (filter, recents, collapsible
  sections); a jump bar selects any page. Launch pad, pause and running jobs move into the account
  menu.
- Content scrolls under the chrome inside `.at-main-scroll`, centred, 1320 px maximum.
- Navigation is role-filtered from `nav/routes.ts`; single-key shortcuts, `⌘K`/`Ctrl-K`,
  `⌘⌥L`/`Ctrl-Alt-L` and `?` stay.

## Page archetypes

| Role | Ship on | Template component |
|---|---|---|
| Product hero (`hero`) | Overview | `ui/templates/DashboardHero.tsx` |
| Ops console | Observatory, Metrics, Ceph, Maintenance, DR | `ui/templates/DashboardHero.tsx` |
| Index / Timeline | Volumes, Snapshots, Backups, Activity, Audit, Alerts, Jobs, Access, … | `ui/templates/ListPage.tsx` |
| Reference (settings) | Settings | `ui/templates/SettingsPage.tsx` |
| Reference (docs) | ApiDocs | `ui/templates/DocsPage.tsx` |
| Detail | PoolDetail, PlanDetail | `ui/templates/DetailPage.tsx` |

Template components are the single source of truth for each archetype's layout — a page and this
table must not drift; update both together.

Feature tiles belong to Overview and similar summary surfaces. Index pages lead with a headline and
one line of state, then go straight to the table.

## Graphics

- Graphics are first-class: a capacity ring, a throughput chart, one square per OSD.
- **Never invent data to draw one.** No placeholder series, padded cells or demo values. Unknown
  reads as "—"; a chart without enough measurements says so (`ui/ThroughputChart.tsx` leaves out
  the first sample, which has no rate).
- Each has a text equivalent (`aria-label`, legend or list).
- Decorative art is `aria-hidden` and carries no data (sign-in layers).
- Server strings are rendered as text, never with `dangerouslySetInnerHTML`.

## Emptiness and motion

- A bare `0` gets a hint and an action.
- Entrance and scroll reveal are short and subtle (`a-rise`, `.a-reveal`), and everything stops under
  `prefers-reduced-motion`. Do not leave transforms on containers that hold popovers.
