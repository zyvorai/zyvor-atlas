<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Retention locks and ransomware assessment

Atlas native (`bkd_native`) can make data undeletable for a fixed period and can tell you when a
filesystem's activity looks like ransomware. These are the pieces; each says what it does **not**
cover.

## 1. Locked snapshots (block and filesystem)

A snapshot can be retention-locked. Until the lock expires, deleting it fails (HTTP 409) for every
caller, admins included, and the lock can be extended but never shortened or removed.

```sh
atlasctl lock-snapshot snap_native_… --retain-secs 604800     # 7 days
# POST /api/atlas/v1/snapshots/{id}/lock   {"retain_secs": 604800}   (admin, 1 s … 10 years)
# node: POST /v1/snapshots/{id}/lock and POST /v1/fs-snapshots/{id}/lock
```

How it is enforced: the expiry is part of the snapshot in the Raft-replicated catalog.
`DeleteSnapshot` carries the proposing node's clock, so every replica judges the lock at the same
instant, and a lagging replica cannot disagree. Catalogs and log entries from before locking load
as unlocked. Deleting the source volume does not free a locked snapshot's extents.

Not covered: anyone with the raw data-node disks or the ability to stop the whole cluster and edit
its metadata files can still destroy data; this is software retention, not hardware WORM. The node
trusts its own wall clock, so a node whose clock is set far into the future would let a lock lapse
early. Keep NTP healthy.

## 2. S3 object retention

`PutObjectRetention` / `GetObjectRetention` on the native S3 gateway, compliance mode only; see
[`NATIVE_S3.md`](NATIVE_S3.md). It protects an object against the S3 API only. A client with a
FUSE, NFS or SMB mount of the same filesystem can still unlink the file, so combine it with a
locked **filesystem snapshot**, which they cannot touch.

## 3. Ransomware assessment

`POST /api/atlas/v1/ai/ransomware/assess` scores any activity series you send. For native
filesystems Atlas records the series itself: per minute (kept four hours, in memory, on the node
that served the operations) the files modified, renamed and deleted, bytes written, and the share
of sampled written blocks with Shannon entropy above 7.5 bits per byte (encrypted or compressed
data). Only filesystem operations through the native API (FUSE, NFS and SMB gateways) are counted;
S3 writes are not yet.

```sh
GET  /api/atlas/v1/volumes/{id}/ransomware            # operator; read-only verdict
POST /api/atlas/v1/volumes/{id}/ransomware/protect    # admin; optional {"retain_secs": 3600…604800}
```

The verdict is `insufficient_data` (fewer than 7 windows), `normal`, `suspicious` or
`likely_ransomware`, with the signals behind it. It is `likely_ransomware` only when the risk score
is at least 0.6, new high-entropy writes are present **and** there is a behavioural tell (a burst of
modified files, mass renames or mass deletes). A large backup or media ingest is bursty but not
encrypted, so it is not flagged; compressed media alone is not either. The assessment never
executes anything (`can_execute: false`).

`protect` re-assesses on the server and, for `suspicious` or worse, takes a snapshot named
`ransomware-guard-<time>` and locks it (default 24 hours, at most 7 days, because a false positive
pins that capacity). On normal activity it returns 409 and does nothing. It is audited as
`snapshot.ransomware_guard`.

### Automatic protection (opt-in)

`ATLAS_RANSOMWARE_AUTOPROTECT=1` makes the gateway check every native filesystem volume each
`ATLAS_RANSOMWARE_INTERVAL_SECS` (default 60, minimum 10) and run `protect` for
`likely_ransomware` only, at most once per volume every 6 hours. It acts as
`system:ransomware-guard`. Off by default.

## Verification status

Unit and integration tested (engine, Raft catalog, node HTTP, driver fake, gateway, S3 gateway).
Not yet verified on a live native cluster against a real encrypting workload; `STATUS.md` records
what has been run on the lab.
