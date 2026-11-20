<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Schedules

## Purpose

Automate periodic snapshots or backups for a volume.

## When to use it

- Operate **Schedules** when your job matches this page
- Prefer **Command Deck** (`/`) if you are unsure where to start
- Confirm gateway auth and that a storage driver is registered if inventories look empty

## How to get there

- Route: `/schedules`
- Nav: **STORAGE → Schedules**

## Operate from the console (UX)

1. Open `/schedules`.
2. Click **Schedule** — choose volume, cadence, retention, snapshot vs backup.
3. Confirm the schedule row appears; verify later runs under Snapshots / Backups / Jobs.
4. **Empty / fail:** Need at least one volume and (for backups) a bucket.
5. **Success:** Schedule listed; first fire shows as a job.

Use `http://<host>:5110/` for Storage Center (cluster NodePort often `:30511`, HTTPS `:30543`). Health: `GET /health`. Mutations return durable jobs — watch **Jobs**. Never publish lab IPs in customer docs.

## Related pages

- [Volumes](volumes.md)
- [Backups](../data-protection/backups.md)
- [Getting Started](../../getting-started.md)
- [Page index](../../PAGE_INDEX.md)
