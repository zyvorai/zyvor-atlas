<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Integration notes: `integration/untested-all`

**This branch is UNTESTED. Nothing on it has been built, formatted, linted or run, on any host. It is
not known to compile.** It combines unfinished work (the `wip/untested-*` branches are snapshots)
so that it can be tested later. Do not release or deploy it.

## What is in it

Start: `origin/feat/native-delta-writes` at `cfa2fa05`. Then one `--no-ff` merge each, in this order.
No rebases, so later commits of any branch can be merged on top.

| Order | Merged | Commit | Textual conflicts |
|---:|---|---|---|
| 1 | `wip/untested-disk-failure-recovery` | `e7ed01c6` | `device.rs`, `engine.rs` |
| 2 | `wip/untested-phase2-foldcells` | `45a57f73` | `NATIVE_DELTA_REDESIGN.md` |
| 3 | `wip/untested-small-write-concurrency` | `3e824327` | `engine.rs`, `raft.rs` |
| 4 | `wip/untested-prealloc` | `f8df1d93` | `Cargo.toml`, `atlas-native-node.rs`, `device.rs`, `node.rs`, `sync_guard.rs`, `NATIVE_DELTA_REDESIGN.md` |
| 5 | `wip/untested-node-scoped-tokens` | `c15e9955` | none |
| 6 | `wip/untested-workspace-changes` | `fcebeea6` | `engine.rs` |
| 7 | `wip/untested-s3-conditional-tagging` | `d71fe88c` | `raft.rs` |
| 8 | `wip/untested-fuse-keep-cache` | `f2e58a27` | `engine.rs` |
| 9 | `wip/untested-agent-bench` | `81c06df1` | `deploy/bench-vs-ceph/atlas-entry.sh` |
| 10 | `feat/ui-apple` | `581cdc87` | none |
| 11 | `wip/untested-ui-bucket-settings` | `807989f9` | none |
| 12 | PR #444 (jiff 1.0.3) | `5691c57f` | none |
| 13 | PR #443 (UI dependencies) | `c3869ab3` | none |
| 14 | PR #442 (cargo-deny-action 2.1.4) | `5b17f2d1` | none |
| 15 | PR #441 (cosign-installer 4.1.4) | `8d5b098f` | none |
| 16 | PR #440 (action-semantic-pull-request 6.1.4) | `da3c6012` | none |
| 17 | PR #439 (codeql-action group) | `23db4735` | none |
| 18 | PR #436 (install-action 2.90.8) | `b64d8b46` | none |
| 19 | `origin/main` | `637cd863` | none, no content change |

Rows 1 to 11 are branches under `origin/`. After the follow-up commit that fills new fields in test
literals (below), `origin/main` was merged: it had gained the squash merges of #444, #443, #442,
#441, #440 and #436 while this branch was being assembled. Their content was already here (rows 12
to 16 and 18), so that merge changed no file. At the time of writing `origin/main` has no commit
this branch lacks.

`origin/fix/native-sync-error` (`2f1e1801`) was already fully contained, through
`feat/native-delta-writes`. It was not merged again.

### Pull requests

Included: #444, #443, #442, #441, #440, #436 (also on `main` now) and #439 (still open when this
was written; its head was merged here).

Excluded:

- #437 (rand 0.10.4): its Clippy check fails, and `9c2aba1e` on `main` had just restored rand 0.8
  after a Dependabot bump broke the build.
- #434 (fuser 0.19.0): its Clippy check fails.

CI state of the included pull requests, as reported to this integration and not re-diagnosed here:
all of them fail cargo-deny, as `main` does; #439 also failed one Postgres test
(`mcp_mrtr`, `declining_or_cancelling_executes_nothing_and_consumes_nothing`).

Lockfiles: no lockfile conflicted. Git merged `Cargo.lock` (row 12) by itself and no stale
`jiff 1.0.2` reference remains; `ui/package.json` and `package-lock.json` are those of #443.
Neither was checked with `cargo --locked` or `npm ci`.

### Left out

- `origin/exp/native-group-commit`: an experiment that made throughput worse.
- #437 and #434 (above). Other remote branches were not part of this integration.

## Conflicts and how they were resolved

Both sides were kept in every case. Rows refer to the table above.

- **Row 1, disk-failure-recovery**
   - `device.rs`, imports: union (`durable`, `metrics`, `SyncGuard`).
   - `engine.rs`, module list: `combiner`, `delta`, `devices`.
- **Row 2, phase2-foldcells**
   - `NATIVE_DELTA_REDESIGN.md`, status paragraph: combined (phase 1 measured and short of the
     target; phase 2 implemented, no result recorded).
- **Row 3, small-write-concurrency**
   - `raft.rs`, `RaftConfig::new`: `capabilities: CAPABILITIES` (phase 2's set; this branch still
     had `CAP_DELTA` alone) plus `parallel_log_sync`.
   - `engine.rs`, `commit_with`: both added the same `wal.check()?`; only the comment differed.
- **Row 4, prealloc**
   - `Cargo.toml`: both test features.
   - `atlas-native-node.rs`, `main` loop: the SIGTERM seal-and-exit path of preallocation, then
     `restart_reason()` of failure recovery. **The unconditional `fatal_error()` exit was not
     brought back**: it would bypass the bounded restart policy.
   - `device.rs`, `BlockStore`: `probe`, `prealloc_stats` and `seal`; `FailedDevice` kept.
   - `device.rs`, `FileDevice::open_inner`: `durable::drop_cached` before the length is read, then
     tail, shared guard and preparer.
   - `node.rs`, device opening: `open_or_failed(path, open_device(..))`, so an I/O error from
     opening or from starting the preparer gives a `FailedDevice`.
   - `node.rs`, `NativeNode`: `restart_reason`, `raft`, `set_restart_policy` and `seal_devices`.
   - `sync_guard.rs`: `drain` (parallel log sync) and `record`/`fail` (preparer).
   - `NATIVE_DELTA_REDESIGN.md`: preallocation sentence added to the status.
- **Row 6, workspace-changes**
   - `engine.rs`: both re-export blocks.
- **Row 7, s3-conditional-tagging**
   - `raft.rs`: `CAP_DELTA` = 1, `CAP_FOLD_BATCH` = 2, `CAP_RENAME_IF` = 16 (no overlap);
     `CAPABILITIES` is all three and is the `RaftConfig` default.
- **Row 8, fuse-keep-cache**
   - `engine.rs`, module list: `concurrent` and `content_tag`.
- **Row 9, agent-bench**
   - `atlas-entry.sh`: preallocation's `ATLAS_DATA_EXTRA` and the agent bench's
     `ATLAS_DATA_NODE_EXTRA` / `ATLAS_NODE_EXTRA` all kept; `settings.txt` prints both forms.

## Changes made beyond conflict markers (judgement calls)

Scoped-token route table (`node/authz.rs`, `ROUTES`): three routes added by other branches were
missing from it.

| Route | Roles given | Reason |
|---|---|---|
| `GET v1/fs/{fs}/changes` | none (admin and the `api_token_file` token only) | Deny by default; it lists paths, including names the base snapshot had. |
| `POST v1/fs/{fs}/rename-if` | `data`, as `rename` | Same effect as `rename` under extra conditions. Closed, every S3 PUT with a scoped `data` token would fail. |
| `GET v1/fs/{fs}/inodes/{ino}/content-tag` | `read-only`, `data`, as `layout` | A digest of what `layout` and the attributes show; `--keep-cache` mounts use `data` tokens. |

**Consequence to decide:** the chart's scoped `gateway` token (`lifecycle`, `data`) gets 403 on
workspace changes. If the gateway is to list changes with scoped tokens, the route needs a role.
Please review all three rows; they are authorization decisions. The role table in
`NATIVE_NODE.md` was not updated for them.

Test literals: `fault_tests.rs` (disk-failure-recovery) and
`atlas-gateway/tests/workspace_changes.rs` build `NodeConfig`, `DataNodeRole` and `MetadataRole`
field by field and predate `api_tokens_file`, `preallocate`, `delta_writes`, `concurrent_commits`
and `parallel_log_sync`. The missing fields were added with their defaults. These were found by a
text scan, not by a compiler; expect more of this kind.

## Suspected semantic problems (not fixed)

1. `FailedDevice` does not override `byte_granular`, so it answers `false`. A leader's
   `RemoteDevice` caches a data node's answer for good, so after that node restarts with a sound
   device, the leader may keep treating it as non-granular until the leader itself restarts: delta
   writes fall back to ordinary writes and delta repair skips the node. A failed device also
   disables deltas for its whole node (`byte_granular` requires every device). Performance, not
   data safety, as far as read.
2. `NativeEngine::byte_granular` backs off the whole node on any error, while failure recovery
   otherwise backs off only the device that answered with an error.
3. With `concurrent_commits`, holding `write_lock` no longer means that no write is proposed and
   unapplied. `replace_replicas` and locality pinning take the lock and check the applied catalog;
   they then depend on `ReplaceReplica` being rejected at apply time. Failure recovery makes
   replica moves more frequent. The combination was not analysed further.
4. Never exercised together: concurrent commits, parallel log sync, batched folds,
   preallocation, device failure recovery and conditional rename. In particular, failure of a
   parallel log sync against the restart policy, and a preparer sync failure against device
   probing and rebuild.
5. Mixed versions: `FoldCells` and `RenameIf` are gated by capability bits, but once either is in
   a log an older binary cannot replay it. `Catalog.features` lets a later leader propose
   `RenameIf` without hearing every replica again. `FsMeta.incarnation` is not set by older
   builds, so content tags can differ between mixed replicas.
6. The node's HTTP `reason()` has no entry for 412, so a failed conditional rename is sent with
   the reason phrase `Internal Server Error` (the status code is right).
7. Setting the same key in both `ATLAS_DATA_EXTRA` and `ATLAS_DATA_NODE_EXTRA` gives a duplicate
   JSON member, which a node is expected to refuse.
8. Failure-recovery tests were never run with `prealloc-test`, `concurrent-commits-test` or
   `parallel-log-sync-test`; assertions about device lengths or sync order may not hold. Only the
   preallocation branch's own assertion was adjusted for it.

## To run later

1. `cargo fmt --check` (hand-resolved hunks are not formatted), then
   `cargo clippy --workspace --all-targets` and the feature builds (`make native-features`,
   `fuse`, `s3`, `io-uring`, `mcp`).
2. `cargo test --workspace`; then `atlas-native` and its dependants once with each of
   `delta-writes-test`, `concurrent-commits-test`, `parallel-log-sync-test`, `prealloc-test`, and
   with all four.
3. Pay attention to `node::authz` tests (route table), `api_tokens`, `fault_tests`,
   `writeback_errors`, `delta_writes`, `delta_folding`, `concurrent_commits`, `raft_log_sync`,
   `prealloc`, `conditional_rename`, `content_tag`, `changes` and the gateway's
   `workspace_changes` and `mcp_tool_counts`.
4. `scripts/ci-remote.sh` on the lab host, `make headers`, the Postgres tests, cargo-deny.
5. UI: `npm ci`, lint, tests, build in `crates/atlas-gateway/ui`.
6. `helm lint` and `helm template` for both charts, with scoped tokens on and off.
7. Bench harness: `deploy/bench-vs-ceph/test_agent.py` and a smoke run of `compare.sh` and
   `agent.sh`.
8. Live: pjdfstest over FUSE, S3 conditional check (`deploy/native/s3-conditional-check.sh`),
   keep-cache lab script, preallocation probe, device failure and restart behaviour, scoped
   tokens with CSI, S3, NFS, SMB, replication and the gateway, and a rolling upgrade from the
   previous build.
