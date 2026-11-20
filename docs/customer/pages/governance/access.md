<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Access

## Purpose

Local users for Storage Center sign-in — create and delete accounts.

## When to use it

- Operate **Access** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/access`
- Nav: **GOVERNANCE → Access**

## Operate from the console (UX)

1. Open `/access`.
2. **Create user** with username/password; they sign in on the login page.
3. Delete users from the table when retiring access.
4. Paste service JWT via rail key icon for API automation.
5. **Empty / fail:** No users → create one; login fails → check ATLAS_AUTH_REQUIRED and token.
6. **Success:** New user can sign in; table lists accounts.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Settings](settings.md)
- [API Docs](api-docs.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
