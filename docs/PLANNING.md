<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Planning and readiness reports

Three read-only endpoints (operator role). Each response carries `can_execute: false` and an
`assumptions` object naming every heuristic. They never call a storage backend, start a job, or
change state. Background and sources: [RESEARCH.md](RESEARCH.md).

## Capacity forecast

`GET /api/atlas/v1/capacity/forecast?minutes=10080&warn_days=30&critical_days=7` runs the model
over the stored cluster history (`metrics_history`, 60 to 43200 minutes). `POST` takes your own
series, up to 256 pools and 10000 samples each:

```json
{ "pools": [ { "id": "tank", "capacity_bytes": 1.0e12,
               "samples": [ { "ts": 1790000000, "used_bytes": 4.0e11 } ] } ],
  "warn_days": 30, "critical_days": 7 }
```

Per pool: `status` (`filling`, `not_filling`, `beyond_horizon`, `full`, `insufficient_data`),
`severity` (`ok`, `warning`, `critical`, `unknown`), `days_to_full`, `days_to_80_percent`,
`growth_bytes_per_day`, `r_squared`, `confidence` (`high` needs 10 samples and R squared 0.8;
`low` below 0.5), and `trend` (`accelerating`, `steady`, `decelerating`: slope of the first half of
the samples against the second). Pools are sorted most urgent first. At least 3 samples spanning
one hour are required; growth under 1 MiB per day counts as flat; more than 3650 days is
`beyond_horizon`. NaN, infinite or negative values, non-positive capacity, empty ids and oversized
inputs are 400s. `atlasctl capacity-forecast` and `atlasctl capacity-forecast-file FILE`.

The model is a straight line. Step growth (a big import) fits badly; read `confidence` and `trend`.

## DR readiness

`GET /api/atlas/v1/dr/readiness?target_rpo_secs=3600&drill_max_age_days=90` scores the DR catalog
from 0 to 100 (critical gap 25 points, warning 10, info 2; `ready` 90+, `degraded` 60 to 89,
`not_ready` below) and lists each gap with `code`, `severity`, `subject`, `detail` and
`remediation`. Codes: `NO_PEER`, `NO_MIRRORS`, `MIRROR_NO_PEER`, `MIRROR_ERROR`, `RPO_UNKNOWN`,
`RPO_INVALID`, `RPO_OVER_TARGET`, `RPO_BREACH` (over twice the target), `STATE_STALE`,
`STATE_TIME_UNKNOWN`, `FORCE_PROMOTED`, `NO_DRILL`, `DRILL_STALE`. Missing data is a gap, never a
pass. The RPO is the value last stored in `dr_mirrors.rpo_seconds`; Atlas does not poll it. RTO is
reported as `rto_measured_seconds: null` because Atlas does not time drills. `atlasctl dr-readiness`.

## Scrub plan (dry run)

`POST /api/atlas/v1/maintenance/scrub-plan`:

```json
{ "items": [ { "id": "tank", "kind": "zfs", "size_bytes": 4.0e12,
               "last_scrub_ts": 1789000000, "interval_days": 30, "busy": false } ],
  "max_concurrent": 1, "throughput_mib_s": 100 }
```

Items past their interval (or never scrubbed) are ordered most overdue first and packed into
`max_concurrent` lanes (1 to 16); each gets `start_offset_seconds` and an estimated duration from
size over throughput (minimum 60 s, maximum 30 days). Items not yet due or already `busy` are
listed under `skipped`. Default intervals when `interval_days` is omitted: zfs 30, ceph 7, other 14;
these are heuristics. Up to 1000 items. The plan starts nothing. `atlasctl scrub-plan FILE`.
