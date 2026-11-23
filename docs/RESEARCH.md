<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Storage operations research

Scope: what mature storage control planes and distributed stores do for health, capacity, scrub,
replication and DR, what Atlas lacks, and which read-only ideas were built. Marking: a claim with
`[R]` was read from the fetched page named beside it on 2026-11-23; `[U]` means unverified (recalled
or only seen as a search result, not read from a fetched page). Treat every `[U]` as a lead to check,
not as a fact.

## What others do

| Area | Practice | Source |
| --- | --- | --- |
| Ceph health | Named health checks for capacity and data safety: `OSD_NEARFULL`, `POOL_NEAR_FULL`, `PG_NOT_DEEP_SCRUBBED` (relative to `osd_deep_scrub_interval`), `OSD_SCRUB_ERRORS` paired with `PG_DAMAGED`; thresholds are `mon_osd_nearfull_ratio`, `mon_osd_full_ratio`, pool quota warn/crit thresholds. [R] | https://docs.ceph.com/en/latest/rados/operations/health-checks/ |
| Ceph / Rook | Rook exposes mgr modules (pg autoscaler, balancer, crash) and health through CRs. [U] (the Rook mgr page returned 404) | https://rook.io/docs/rook/latest/ |
| Longhorn | Recurring jobs with `retain` (at least 1), `concurrency` (at least 1), named `groups`; a job in the `default` group applies to every volume with no job; count-based or age-based retention. [R] | https://longhorn.io/docs/latest/snapshots-and-backups/scheduling-backups-and-snapshots/ |
| OpenEBS | Replicated engines with per-volume replica rebuild status and snapshot schedules. [U] | https://openebs.io/docs |
| ZFS / TrueNAS | A scrub reads every block and verifies checksums, repairs from redundancy, runs one at a time per pool (not during a resilver), and can be paused/resumed; the manual gives no frequency, systemd ships weekly/monthly per-pool timers. [R] TrueNAS defaults to a scheduled scrub threshold of 35 days. [U] | https://openzfs.github.io/openzfs-docs/man/master/8/zpool-scrub.8.html |
| MinIO | Objects are split over equal erasure sets (2 to 16 drives by default); parity from EC:0 to half the set; read quorum K, write quorum K (K+1 when parity is half); an object with read quorum heals missing shards; production should use EC:3 or higher. [R] Bitrot detection by per-shard hashes on read and a background scanner. [U] | https://docs.min.io/community/minio-object-store/operations/concepts/erasure-coding.html |
| SeaweedFS | Volume servers with replication placement strings (`xyz` copies by rack/DC) and erasure coding for warm volumes. [U] | https://github.com/seaweedfs/seaweedfs/wiki |
| JuiceFS | Metadata engine separate from object store; `juicefs fsck` and `gc` verify object consistency; trash retention. [U] | https://juicefs.com/docs/community/ |
| CSI | The spec's snapshot goals: create and delete a snapshot, provision a volume from a snapshot; reverting in place is out of scope. [R] Clone, expansion and volume-condition (health) capabilities are in the full spec. [U] | https://github.com/container-storage-interface/spec/blob/master/spec.md |
| Capacity forecasting | Linear or Holt-Winters projection of used bytes with a "predict_linear" style time-to-full alert (Prometheus `predict_linear` over 6 to 24 hours). [U] | https://prometheus.io/docs/prometheus/latest/querying/functions/#predict_linear |
| RPO/RTO | Replication lag is the live RPO; RTO is only known by drills. [U] | general practice |
| S3 object lock / lifecycle | Governance and compliance retention, legal hold, lifecycle expiry; Atlas already has compliance retention (`docs/WORM.md`). [U] | https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html |
| SLOs | Error-budget burn rate alerts on availability and latency objectives. [U] | https://sre.google/workbook/alerting-on-slos/ |

## What Atlas had and lacked

Had: cluster-wide `GET /metrics/forecast` (a least-squares days-to-full from `metrics_history`),
an advisor with what-if planning, `GET /dr/status` and `/dr/preflight`, a DR catalog with
`rpo_seconds`, `last_failover_at`, `force_promoted`, native scrub/repair (`repair_once`), WORM locks.

Lacked:

1. Forecast per pool, with a quality signal. The existing forecast is one number for the whole
   cluster and says nothing about how well a line fits or whether growth is speeding up.
2. A DR score. `dr_status` counts mirrors; it does not say what is missing (no peer, stale state,
   RPO over target, no drill ever, a forced promote that needs resync).
3. A scrub/rebalance plan. Operators decide by hand which pool is overdue; there is no ordering,
   concurrency limit or estimate.
4. A joined health summary (backends, DR, alerts) in one view.
5. Not looked at in depth: SLO burn-rate reporting, JuiceFS-style fsck reports, per-tenant RPO.

## Ranked ideas

| Rank | Idea | Why | Decision |
| --- | --- | --- | --- |
| 1 | Capacity forecast per pool with OLS, trend (early vs late slope), confidence (R squared, sample count) and thresholds | Cheap, pure, matches Ceph's near-full checks and Prometheus `predict_linear` practice; no data-path change | Built: `GET/POST /capacity/forecast` |
| 2 | DR readiness score with explicit gaps and honest "RTO not measured" | Turns existing catalog fields into an actionable checklist; drills are what Ceph/storage DR practice says proves RTO | Built: `GET /dr/readiness` |
| 3 | Dry-run scrub schedule: overdue first, lane-limited, duration estimate, skips busy items | Scrub frequency guidance is per-pool and only one scrub per ZFS pool at a time; Ceph reports `PG_NOT_DEEP_SCRUBBED` only after the fact | Built: `POST /maintenance/scrub-plan`; plan only, starts nothing |
| 4 | Storage health summary joining backend health, DR, alerts | Useful, but `doctor`, `ceph/health-rollup` and `dr/status` already cover most inputs; a join adds little before the new reports settle | Not built |
| 5 | SLO burn-rate alerts | Needs a request-level SLI store Atlas does not keep | Rejected for now |
| 6 | Live bitrot verification triggers | Would change the data path or start scrubs; out of scope for read-only work | Rejected |

## Boundaries kept

The three features take caller-supplied samples or read the existing catalog and `metrics_history`.
They never call a driver, start a scrub, or touch erasure coding, Raft, checksums or on-disk
formats. Heuristics (default scrub intervals, penalty weights, the one-hour/three-sample minimum)
are listed in each response's `assumptions` object.
