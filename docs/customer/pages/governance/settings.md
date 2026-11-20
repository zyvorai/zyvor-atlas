<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Settings

## Purpose

Console settings — theme and session preferences for Storage Center.

## When to use it

- Operate **Settings** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/settings`
- Nav: **GOVERNANCE → Settings (sidebar Settings)**

## Operate from the console (UX)

1. Open `/settings`.
2. Adjust Look & feel from the top-bar theme menu (Carbon, Apple Lite) if not on this page.
3. Confirm token/session via the Account menu (top-bar key icon).
4. **Empty / fail:** Changes not sticking → local storage blocked; re-auth.
5. **Success:** Theme/session match operator preference.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Access](access.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
