<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Getting Started with Atlas

## What you need

| Requirement | Notes |
|-------------|--------|
| Atlas gateway | Default bind **`:5110`** (REST + Storage Center UI) |
| Optional gRPC | **`:5111`** |
| Backend | Ceph via Rook/driver, or lab fake driver |
| Auth | Lab often open (`ATLAS_AUTH_REQUIRED=0`); cluster uses JWT |

## 1. Open Storage Center

- Console: `http://<host>:5110/`
- Cluster NodePort (common): `http://<host>:30511/` (HTTPS may be **`:30543`**)
- Health: `curl http://<host>:5110/health`

## 2. Sign in

| Mode | What you do |
|------|-------------|
| Open (lab) | No token when `ATLAS_AUTH_REQUIRED=0` |
| JWT | Paste bearer token from the top-bar **Account** menu, or `POST /api/atlas/v1/auth/tokens` |
| Local user | **GOVERNANCE → Access** → Create user, then sign in on the login page |
| CLI | `atlasctl --token …` or `ATLAS_TOKEN` |

## 3. Orient yourself (UX)

1. **Command Deck** (`/`) — capacity sounding, pool tiles, protection gaps.
2. Left sidebar: six section groups with **collapsible headers** (chevron) and a **Filter navigation…**
   field at the top; collapse the whole rail to icons from the toggle at the bottom. Top bar:
   Spotlight (**⌘K**), Look & feel (Carbon / Apple Lite), running-jobs indicator, Alerts bell,
   health, and Account (clock, auth token, sign-out). **H** jumps to Command Deck.
3. Sidebar groups: **STORAGE** (always open), **DATA PROTECTION**, **DATABRIDGE**, **OBSERVABILITY**, **GOVERNANCE**, **INFRASTRUCTURE**. Admin/operator-only pages hide for lower roles.
4. Every mutation is a durable job (`202` + id) — watch **Jobs**.

## 4. First workflows

### A. Discover inventory

```bash
atlasctl health
atlasctl discover
atlasctl pools
atlasctl volumes
```

### B. Provision from the console

1. **Volumes** → **Create volume** (policy/intent) → watch **Jobs**.
2. Optional: Snapshot / Schedule from the volume SlideOver.

### C. Protect a volume

**Snapshots** / **Schedules** / **Backups** (needs a **Bucket**) — confirm job completion. Check **Protection Status**.

### D. Check backend health

**Backends** → **Ceph** for day-2 OSD/pool/RGW signals. Investigate warn/crit before large provisions.

## Next steps

- [Using the Dashboard](using-the-dashboard.md)
- [Admin basics](admin-basics.md)
- [Page guides](pages/README.md)
- [Common workflows](workflows.md)
