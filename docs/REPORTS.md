<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Operations reports

Four read-only endpoints (operator role) that turn catalog rows into findings. Each response has
`can_execute: false`, a `summary`, and an `assumptions` object naming every threshold; thresholds
are operator heuristics, not backend limits. Nothing is deleted or changed and no backend is
called. Findings are sorted warning before info, then by code and subject, and capped at 1000 (the
summary counts all of them and `findings_truncated` says when the list was cut). Background and
sources: [RESEARCH.md](RESEARCH.md) (round 3). Earlier reports: [PLANNING.md](PLANNING.md).

## Snapshot hygiene

`GET /api/atlas/v1/snapshots/hygiene?stale_days=90&max_per_volume=30&lock_warn_days=7&stuck_minutes=60`

| Code | Severity | Meaning |
| --- | --- | --- |
| `FAILED_SNAPSHOT` | warning | state `error` or `failed` |
| `STUCK_SNAPSHOT` | warning | not `ready` for `stuck_minutes` or more |
| `OVER_RETAINED` | warning | a volume has more than `max_per_volume` snapshots |
| `ORPHAN_SNAPSHOT` | warning | its volume is not in the catalog (defensive: the schema cascades deletes, so this should not occur while foreign keys are enforced) |
| `OLD_SNAPSHOT` | info | `ready`, at least `stale_days` old, not protected and not actively locked |
| `LOCK_EXPIRING` | info | retention lock ends within `lock_warn_days` |
| `LOCK_EXPIRED` | info | retention lock already ended |
| `NO_TIMESTAMP` | info | no parseable creation time; age checks skipped |

An actively locked or protected snapshot is never reported as old. Only the gateway's recorded
`locked_until` is seen; a lock set directly on a native node is not. `busiest_volumes` lists up to 50
volumes by snapshot count with oldest and newest age. Bounds: `stale_days` 1..3650,
`max_per_volume` 1..100000, `lock_warn_days` 1..365, `stuck_minutes` 1..100000; outside is a 400.
`atlasctl snapshot-hygiene [--stale-days N ...]`, `AtlasClient::snapshot_hygiene(query)`.

## Tenant quota headroom

`GET /api/atlas/v1/tenants/quota-headroom?warn_percent=80&critical_percent=95`

Per tenant (from the same live usage the volume-create admission check uses): `status`
(`unlimited`, `ok`, `warning`, `critical`, `exceeded`), `limiting_dimension` (`bytes` or
`volumes`), `percent_used`, and bytes and volumes `used`, `limit`, `percent`, `headroom`.
`exceeded` means usage is above a limit (possible after the limit was lowered); a limit of 0 is
unlimited and has no percent or headroom. Tenants are sorted most used first, unlimited last.
Needs `0 < warn_percent < critical_percent <= 100`. `atlasctl quota-headroom`.

## Snapshot schedule coverage

`GET /api/atlas/v1/schedules/coverage?min_keep=2&overdue_intervals=2`

| Code | Severity | Meaning |
| --- | --- | --- |
| `UNSCHEDULED_VOLUME` | warning | no enabled snapshot or backup schedule (volumes deleting, deleted, failed or in error are skipped) |
| `STALLED_SCHEDULE` | warning | enabled but `next_run_at` is more than `max(overdue_intervals x interval, 5 min)` in the past: the snapshot tick is off or this gateway is not the leader |
| `ORPHAN_SCHEDULE` | warning | targets a volume not in the catalog (defensive, as above) |
| `SCHEDULE_DISABLED` | info | a disabled schedule protects nothing |
| `LOW_RETENTION` | info | `keep` is below `min_keep` (one failed run leaves no restore point) |
| `NO_RETENTION_LIMIT` | info | `keep` is 0, so nothing is ever pruned |

The summary gives `coverage_percent`. This is catalog coverage, not a restore test. Bounds:
`min_keep` 1..10000, `overdue_intervals` 1..1000. `atlasctl schedule-coverage`.

## Ceph health-check explainer

`GET /api/atlas/v1/ceph/health-explain[?codes=OSD_DOWN,PG_DEGRADED]`, or `POST` with
`{"codes": ["OSD_DOWN"]}` or the `checks` object of `ceph health detail -f json` (its keys are the
codes). With no codes the whole table comes back. Each entry has `meaning`, `first_step` (the
command or action the Ceph reference names), a `triage` class (`data_risk`, `degraded`,
`capacity`, `hygiene`, Atlas's own ordering) and a link into the Ceph reference. 22 codes are
covered, all read from the Ceph health-checks page on 2026-11-24; any other code is returned under
`unknown` and never guessed. It is a static reference, not a live diagnosis, and does not call
Ceph. Up to 200 codes, names must be letters, digits and underscores. `atlasctl ceph-health-explain
[--codes A,B | --file health.json]`, `AtlasClient::ceph_health_explain(codes)`.
