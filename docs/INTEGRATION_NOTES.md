<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Integration notes: `integration/untested-all`

This branch combines work that was developed on separate branches. **It has been tested on the lab
host, with the results in [Test results](#test-results) and the exceptions in
[Open findings](#open-findings) and [Not run](#not-run).** It has not been released, deployed,
benchmarked, tried on a cluster of separate hosts or upgraded from an older release. One test was
loosened by a merged branch and one behaviour it covers is still unfixed.

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

Rows 1 to 11 are branches under `origin/`. Rows 1 to 9 and 11 were snapshots of unfinished work;
their finished commits were merged afterwards:

| Order | Merged afterwards | Commit | Textual conflicts |
|---:|---|---|---|
| 20 | `wip/untested-ui-table-fix` (content: `ui/Table.tsx` only) | `1d988d38` | none |
| 21 | `feat/s3-conditional-tagging` | `456e769a` | `NATIVE_S3.md` |
| 22 | `feat/node-scoped-tokens` | `db573d87` | `authz.rs`, `authz/tests.rs`, `api_tokens.rs` (add/add) |
| 23 | `feat/workspace-changes` | `28019c8b` | none |
| 24 | `feat/agent-bench` | `846a848b` | none |
| 25 | `feat/disk-failure-recovery` | `daa2bedf` | none |
| 26 | `feat/disk-failure-recovery` (restart policy default `last`) | `0f07e2a3` | `fault_tests.rs` |
| 27 | `feat/fold-batching` | `668227f7` | none |
| 28 | `feat/fuse-keep-cache` | `0151c25c` | none |
| 29 | `feat/small-write-concurrency` | `b0bf856b` | one comment in `engine.rs` |

`feat/preallocation` (`98aa5d62`) has the same tree as the snapshot in row 4; nothing was merged.
The bucket-settings console work finished on `feat/ui-apple` (`c29f0875`); its content is here
through rows 11 and 20. In the later conflicts, the finished branch's text was taken and this
branch's additions were kept (the three route entries in `authz.rs`; the new config fields in
`fault_tests.rs`).

`origin/main` (`637cd863`) is contained. `origin/fix/native-sync-error` (`2f1e1801`) was already
contained through `feat/native-delta-writes`, which has since gained one merge commit
(`fddba3ce`, row 21's content) that is not merged here.

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

Lockfiles: no lockfile conflicted. `Cargo.lock` was merged by git; `cargo build --locked` of the
gateway and the node and `npm ci` of the console both succeeded.

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
| `GET v1/fs/{fs}/changes` | `read-only`, `data` (limited tokens: their filesystems only) | Decided by the user on 2026-11-28; this branch had first left it admin only. |
| `POST v1/fs/{fs}/rename-if` | `data`, as `rename` | Same effect as `rename` under extra conditions. Closed, every S3 PUT with a scoped `data` token would fail. |
| `GET v1/fs/{fs}/inodes/{ino}/content-tag` | `read-only`, `data`, as `layout` | A digest of what `layout` and the attributes show; `--keep-cache` mounts use `data` tokens. |

The chart's scoped `gateway` token holds `lifecycle` and `data`, so the gateway can list changes
and is not refused with 403. A `lifecycle`-only or `replication` token is. Two limits of the grant
(also in `NATIVE_NODE.md`): a node computes two listings at a time, so a `data` or `read-only` token
can occupy both slots; and a token limited to a clone sees names that existed when the clone was
made even if since removed. The role table in `NATIVE_NODE.md` covers all three routes.

### Fixes made while testing

- Test literals: `fault_tests.rs` and `atlas-gateway/tests/workspace_changes.rs` lacked the config
  fields other branches added (`api_tokens_file`, `preallocate`, `delta_writes`,
  `concurrent_commits`, `parallel_log_sync`); defaults were added. No other compile break between
  branches was found.
- `3eb34590`: `delta_folding.rs` defined `Held` twice and `raft_log_sync.rs` did not borrow-check.
  Both were errors inside the unfinished branches' own files; the finished branches carry the same
  repairs.
- `6ba56598`: clippy (`large_enum_variant` in `engine.rs`, a needless borrow in `concurrent.rs`,
  `type_complexity` in `api_tokens.rs`); the finished branches carry the same repairs.
- `01b1cf6b`: the role of the changes listing (above) and its test.

No test was removed, skipped, ignored or loosened by the integration itself. See the first open
finding for the one loosened by a merged branch.

## Open findings

1. **A snapshot can hold half of a truncate (unfixed; present on `main`).** Truncating into the
   middle of an extent is two commands: the extent without its tail, then the new size. A filesystem
   snapshot taken with delta writes off does not wait for the write lock, so it can hold the old
   size with a zeroed tail, a state the file never had. Reproduced with a throwaway probe test (one
   writer rewriting 8 KiB and truncating to 100 bytes, up to 600 snapshots, local metadata
   backend): bad snapshots **143 of 600 on `main` `637cd863`**, 1 of 600 on `cfa2fa05`, 2 of 600 on
   the combined tree. `concurrent_writers_stay_linearizable_under_snapshots` failed in every run
   of it on this branch until `b0bf856b` (merged in row 29) **loosened the test** to accept that
   state when delta writes are off. The test passing therefore does not mean the behaviour is gone.
   Fixing it (the snapshot taking the write lock, or truncate becoming one command) is a design
   decision and was not made.
2. **`aligned_writes::partial_writes_stay_serialized` fails with the `concurrent-commits-test`
   feature** (alone, with all four test features, and with `io-uring` added). It asserts one
   device write at a time for partial writes, and saw four: with concurrent commits, a write that
   creates a cell's extent runs concurrently by design. The same failure occurs on
   `feat/small-write-concurrency` `b0bf856b` itself. It passes without the feature, which is the
   shipped default. Not changed here: deciding what the test should assert belongs to that feature.
3. `cargo clippy -p atlas-native --features cufile` fails with two
   `not_unsafe_ptr_arg_deref` errors in `cufile.rs`, which is unchanged from `main`; it fails the
   same way on `main` `637cd863`. CI does not build this feature.
4. `FailedDevice` does not override `byte_granular`, so it answers `false`. A leader's
   `RemoteDevice` caches a data node's answer for good, so after that node restarts with a sound
   device, the leader may keep treating it as non-granular until the leader itself restarts: delta
   writes fall back to ordinary writes and delta repair skips the node. A failed device also
   disables deltas for its whole node. Found by reading; not reproduced.
5. `NativeEngine::byte_granular` backs off the whole node on any error, while failure recovery
   otherwise backs off only the device that answered with an error. Found by reading.
6. With `concurrent_commits`, holding `write_lock` no longer means that no write is proposed and
   unapplied. `replace_replicas` and locality pinning take the lock and check the applied catalog;
   they then depend on `ReplaceReplica` being rejected at apply time. Not analysed further.
7. Mixed versions: `FoldCells` and `RenameIf` are gated by capability bits, but once either is in
   a log an older binary cannot replay it. `Catalog.features` lets a later leader propose
   `RenameIf` without hearing every replica again. `FsMeta.incarnation` is not set by older
   builds, so content tags can differ between mixed replicas.
8. The node's HTTP `reason()` has no entry for 412, so a failed conditional rename is sent with
   the reason phrase `Internal Server Error` (the status code is right).
9. Setting the same key in both `ATLAS_DATA_EXTRA` and `ATLAS_DATA_NODE_EXTRA` (bench harness)
   gives a duplicate JSON member, which a node is expected to refuse.

## Test results

All on the lab host (Linux 7.0.0-44-generic, 12 cores, 30 GiB) on 2026-11-28, in rootless
containers: Rust 1.98.1 (`rust-toolchain.toml`), `CARGO_BUILD_JOBS=4`, no incremental build, debug
info and symbols stripped from the dev and test profiles to save disk. Other work ran on the host
during parts of it. Counts are `cargo test` totals (passed / failed / ignored).

**Tree.** `7011cf6e` is the last code commit; the commit after it changes only this file. Checks marked with another commit ran before the later merges;
what changed between is noted.

### Compile, lint, format (at `7011cf6e` unless noted)

| Check | Result |
|---|---|
| `cargo check --workspace --all-targets` (first run, `d5ae3802`) | `CHECK_workspace_EXIT=101`, two test files; 0 after `3eb34590` |
| `cargo check` of each feature (`3eb34590`): `fuse`, `io-uring`, native `s3`, `ublk`, `rdma`, fuse `rdma`, `cufile`, `delta-writes-test`, `concurrent-commits-test`, `parallel-log-sync-test`, `prealloc-test`, all four, gateway `mcp`, atlas-io `mcp` | all `EXIT=0` |
| `cargo check` atlas-io `bpf` (`3eb34590`) | `EXIT=0` once `libbpf-dev` was in the build image |
| DataBridge `sqlserver`, `mongodb`, `azure-blob`, `oracle`, `kafka-lag`, and the first four together (`3eb34590`; the crate is unchanged since) | all `EXIT=0` |
| `cargo clippy --all-targets -- -D warnings`: `CLIPPY_workspace`, `_fuse`, `_io-uring`, `_native-s3`, `_ublk`, `_rdma`, `_fuse-rdma`, `_delta`, `_concurrent`, `_parallelsync`, `_prealloc`, `_allfour`, `_mcp`, `_io-mcp`, `_io-bpf` | all `EXIT=0` |
| `CLIPPY_cufile` | `EXIT=101` (open finding 3) |
| `cargo fmt --all --check` (informational in CI) | `EXIT=1`, 50 files; `cfa2fa05` has 52 and `main` 55; no file drifts here that drifts in neither |

### Rust tests (all at `7011cf6e`)

| Set | Exit | Passed | Failed | Ignored |
|---|---:|---:|---:|---:|
| `TEST_workspace` (`cargo test --workspace`; delta writes off) | 0 | 1292 | 0 | 33 |
| `TEST_delta` (native and fuse, `delta-writes-test`) | 0 | 587 | 0 | 25 |
| `TEST_delta-others` (s3, driver, csi with delta on) | 0 | 40 | 0 | 0 |
| `TEST_fuse` (`--features fuse`) | 0 | 87 | 0 | 4 |
| `TEST_fuse-delta` | 0 | 87 | 0 | 4 |
| `TEST_io-uring` | 0 | 505 | 0 | 21 |
| `TEST_concurrent` (`concurrent-commits-test`) | 101 | 586 | 1 | 25 |
| `TEST_parallelsync` (`parallel-log-sync-test`) | 0 | 587 | 0 | 25 |
| `TEST_prealloc` (`prealloc-test`) | 0 | 587 | 0 | 25 |
| `TEST_allfour` (all four features; native crates, s3, driver, csi) | 101 | 626 | 1 | 25 |
| `TEST_allfour-uring` (all four and `io-uring`) | 101 | 504 | 1 | 21 |
| `TEST_mcp` (gateway `--features mcp`; includes the MCP tool counts 55/61/62/72) | 0 | 367 | 0 | 1 |

The only failure in the three sets with exit 101 is open finding 2.

Earlier runs on this branch, before the finished branches were merged, failed five tests:
`only_an_undecided_batch_keeps_its_space`, `replay_reload_and_snapshot_transfer_reach_the_same_state`
and `batches_wait_until_every_metadata_node_decodes_them` (fixed by row 27),
`keep_cache_follows_a_leader_failover_and_a_restore` (fixed by row 28) and
`concurrent_writers_stay_linearizable_under_snapshots` (open finding 1).

### Repeats for flakiness (`4dcb241a`; the only later code change is one test file)

| Tests | Result |
|---|---|
| Failure recovery: `fault_tests`, `restart`, `node::health`, `engine::devices` (39 tests) | 30 of 30 runs passed |
| `delta_folding` (25 tests, batched folds on clusters) | 30 of 30 runs passed |
| FUSE `ops` keep-cache tests (5) | 30 of 30 runs passed |

### Other CI steps

| Check | Tree | Result |
|---|---|---|
| License headers (1198 files), migrations parity (42), `bash -n` (78 scripts) | `7011cf6e` | `EXIT=0` each |
| Migrations immutability against `main` | `d5ae3802` (no migration changed since) | `EXIT=0` |
| Bench harness `test_agent.py` (69, 1 skipped), `test_gate.py` (37) | `7011cf6e` | `EXIT=0` |
| `test_s3_pattern_probe` (9, 1 skipped), `atlas_recovery` unit tests (31) | `7011cf6e` | `EXIT=0` |
| `atlas_data` tests (120, 1 skipped) and `examples/data/end_to_end.py`, Python 3.12 | `86873a9f` (`tools/` unchanged since) | `EXIT=0` |
| Postgres 16: `TEST_pg_gateway` (`--features mcp`) 367 / 0 / 1; `TEST_pg_inventory` 5 / 0 / 0 | `4dcb241a` | `EXIT=0`. A first attempt failed because the throwaway database's 1.5 GiB tmpfs filled; rerun with 8 GiB. The `mcp_mrtr` failure reported on #439 did not occur |
| `cargo build --workspace`, `cargo build --locked` of gateway and node, `atlas_recovery/tests/live_native.py` | `4dcb241a` | `EXIT=0`, live run `passed` |
| UI: `npm ci`, lint, `tsc --noEmit`, `test:coverage` (51 files, 224 tests), build | `86873a9f` (UI unchanged since) | `EXIT=0` each |
| Headless Chromium check of the bucket settings panel, mocked API, both themes, 1280 and 390 px | same build | `BROWSER_FAILURES=0`; the Lifecycle tab at 390 px overflows by 0 px |

### Real kernel (binaries of `4dcb241a`, debug builds, processes on one host)

| Script | Result |
|---|---|
| `deploy/native/writeback-error-lab.sh` (device-mapper over a loop device on its own file) | `WRITEBACK_LAB_EXIT=0`: 9 tests and the remount check passed; teardown left no device |
| `deploy/native/keep-cache-lab.py` | full run `EXIT=0`, 39 checks; concurrent section alone 10 of 10 runs passed (the one EIO seen on its branch did not recur) |
| `deploy/native/s3-conditional-check.sh`, two S3 gateways, aws-cli 2.42.5 | `passed=43 failed=0` |

## Not run

- `cargo-deny` (reported to fail on `main` too), `npm audit`, the website audit, coverage, the
  Docker image builds and the GitHub workflows themselves.
- Tests under the native `s3` (tiering), `rdma`, `cufile`, `ublk`, atlas-io `bpf` and atlas-io `mcp`
  features: compiled and linted only. DataBridge optional connectors: compiled only.
- `ignored` tests, including the benches and `prealloc_probe`, other than the writeback ones.
- `deploy/native/ublk-live-check.sh`, `smoke.sh`, `lab-verify.sh`, pjdfstest, the NFS and SMB
  gateways, CSI on a cluster, replication between clusters.
- `helm lint` or `helm template` of either chart.
- Any deployment on Kubernetes or separate hosts, a rolling upgrade from an older build, power
  loss, and scoped tokens against a deployed gateway.
- Any benchmark or performance measurement.
