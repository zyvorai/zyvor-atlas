<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlasctl

A thin REST client for the Atlas gateway.

```bash
atlasctl [--base-url URL] [--token JWT] <command>
```

Env: `ATLAS_BASE_URL` (default `http://127.0.0.1:5110`), `ATLAS_TOKEN`. Output is pretty-printed
JSON; non-2xx exits non-zero. Run `atlasctl --help` / `atlasctl <cmd> --help` for flags.

## Commands (grouped)

**Meta / inventory** — `health`, `ready`, `version`, `backends`, `backends-summary`, `discover`,
`clusters`, `pools`, `osds`, `volumes`, `storage-classes`, `metrics`, `alerts`, `ceph-metrics`,
`ceph-status`, `ceph-osd-tree`, `ceph-osd-df`, `ceph-df`, `history`, `forecast`, `self-metrics`,
`policies`, `jobs`, `snapshots`, `tenants`, `audit`, `quota`, `set-quota`, `tenant-policies`,
`set-tenant-policy`, `volume-bindings`, `volume-labels`, `set-volume-label`

**Write path / RBD / object** — `create-volume`, `delete-volume`, `snapshot-volume`,
`clone-snapshot`, `restore-snapshot`, `delete-snapshot`, `create-rbd-image`, `delete-rbd-image`,
`clone-rbd-image`, `resize-rbd-image`, `flatten-rbd-image`, `rbd-images`, `rbd-snaps`,
`rollback-rbd-image`, `refresh-usage`, `create-bucket`, `delete-bucket`, `buckets`, `bucket-stats`,
`bucket-objects`, `backup-volume`, `delete-backup`, `backup-download`, `backups`, `restore-backup`,
`schedule-snapshots`, `schedule-backups`, `schedules`, `delete-schedule`, `issue-token`

**Day-2** — `maintenance`, `set-maintenance`, `cordon-backend`, `uncordon-backend`,
`upgrade-preflight`, `orphans`

**DR** — `dr-peers`, `dr-register-peer`, `dr-preflight`, `dr-status`, `dr-mirrors`, `dr-promote`,
`dr-demote`, `dr-failover`, `dr-set-rpo`, `volume-mirror-enable`, `volume-mirror-disable`

**DataBridge** — `databridge-sources`, `databridge-plans`, `databridge-stage`  
(`assess` · `provision` · `full-load` · `cdc-start` · `cdc-stop` · `cdc-restart` · `validate` ·
`cutover` · `rollback`)

```bash
atlasctl discover
atlasctl dr-preflight
atlasctl databridge-stage PLAN_ID cdc-restart
atlasctl --base-url http://host:30511 pools
```
