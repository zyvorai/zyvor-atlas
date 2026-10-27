<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Dedup savings estimator

"What would dedup save on this pool?" before you turn anything on. Read-only: nothing is enabled,
moved or written. It reads data, so it is admin-only and bounded.

```
GET /api/atlas/v1/pools/{id}/dedup-estimate?max_bytes=&max_secs=&block_size=
atlasctl dedup-estimate <pool-id> [--max-bytes N] [--max-secs N]
```

## As a background job

The `GET` above holds the HTTP request open, which suits a small bounded scan. For a big pool
(`zdb -S` can take hours) queue it instead:

```
POST /api/atlas/v1/pools/{id}/dedup-estimate        body (optional): {"max_bytes":…, "max_secs":…, "block_size":…}
atlasctl dedup-estimate <pool-id> --background
```

It answers `202` with a job id at once. `GET /jobs/{id}` shows `queued`/`running` and, when done, the
same estimate in `result` (plus `pool_id`); `POST /jobs/{id}/cancel` stops it and kills the
`zdb`/`rbd` process. Details:

- It runs on its own task, not the shared job worker, so a multi-hour scan never holds up volume,
  snapshot or DR jobs. One scan runs at a time (`ATLAS_DEDUP_CONCURRENCY`); the rest wait `queued`.
  At most 8 are queued or running at once, beyond which the request gets `409`.
- It is never retried: a failed or timed-out scan is not worth repeating on its own.
- A heartbeat touches the job every minute while the scan runs, so the stale-job reclaim (15 minutes)
  does not start a second copy. There is no percentage: the driver reports only at the end, so
  `progress_percent` stays 0 until it finishes and `result` shows `{"phase":"scanning","elapsed_secs":…}`.
- The job times out at the request's `max_secs` plus a minute.
- Unsupported pool kinds (anything but RBD and zpool) and unknown pools are refused with `400`/`404`
  when you ask, not after waiting in the queue.

Limits are ceilings: `max_bytes` defaults to 1 GiB and is capped at 256 GiB, `max_secs` defaults to
300 and is capped at 3600, `block_size` defaults to 64 KiB (clamped to 4 KiB to 16 MiB).

## How each backend answers

| Backend | Method | What it reads |
|---|---|---|
| ZFS | `zdb_simulated_ddt` | `zdb -S <pool>`: ZFS builds the dedup table it would have had and prints the ratios. Atlas parses the closing `dedup = …, compress = …` line. |
| Ceph (RBD pools) | `block_hash_scan` | Each image's allocated extents from `rbd export-diff pool/image -` (holes are never read), cut into blocks, SHA-256 hashed, distinct blocks LZ4-compressed. |
| Others | none | `400`: the backend does not implement it. |

NFS, Longhorn, macOS and WEKA have no estimator. atlas-native already reports real savings
(`docs/NATIVE_DATA_REDUCTION.md`), so it does not need one.

## Reading the result

| Field | Meaning |
|---|---|
| `dedup_ratio` | logical / distinct. `1.0` means dedup would save nothing. |
| `compression_ratio`, `combined_ratio` | LZ4 on the distinct blocks, and dedup × compression. |
| `estimated_saved_bytes` | Bytes shed. For a block scan, only within the bytes scanned. For ZFS, dedup only, off the space the pool uses now. |
| `coverage` | Fraction of the pool's used space the scan saw. |
| `lower_bound` | `true` when `coverage` < 1 or the scan hit its memory ceiling. |
| `zero_bytes` | All-zero blocks, left out of both ratios: sparse space costs nothing already. |

**A partial scan is a lower bound, not an extrapolation.** A block that repeats between scanned and
unscanned data cannot be seen, so the real saving is at least the figure shown. Raise `max_bytes`
to see more. The scan also keeps one 16-byte key per distinct block, capped at 8 Mi blocks
(about 256 MiB); past that it stops and sets `truncated`.

## Limits

- Fixed-size blocks at the chosen size. A different record size, or content-defined chunking,
  finds different duplicates. VM images that shift by a few bytes between copies will look worse
  than a variable-block dedup would find.
- The Ceph scan reads live images, so it is not crash-consistent, and it competes with client I/O
  for the duration. It reads at most `max_bytes`.
- `zdb -S` reads every block pointer in the pool, which is slow and I/O heavy on a big pool; ZFS
  offers no way to bound it, so Atlas only bounds the wall-clock time (`max_secs`).
- The ZFS saving ignores compression: pool blocks may already be stored compressed, and the
  simulation cannot say whether they are.

## Verification status

Unit and integration tested only: the block scanner (identical, distinct, zero, compressible and
random data, the memory ceiling), the RBD diff-stream parser (writes, zero ranges, budget stops,
malformed streams), the `zdb -S` summary parser, and the gateway route against the fake Ceph and
ZFS drivers.

**Ceph, checked against a real cluster (2026-10-27).** On Rook Ceph 19.2.x (Squid; a single OSD
on an LVM volume over a loop-backed file, so a functional check, not a performance one),
`rbd export-diff pool/image -` of an image head with no snapshot exits 0 and emits the documented
`rbd diff v1` stream (`s` size record, one `w` record, `e` terminator). Fed through the scanner, a
1 MiB image holding 64 KiB blocks A B A C, one zero block and A again gave exactly the expected
327,680 bytes scanned, 65,536 zero, 196,608 distinct (3 blocks) and no compression on random data.

Running it through the deployed real-Ceph gateway (`atlas-gateway-ceph`, bundled Squid `rbd`)
found two bugs the fake driver could not show, both fixed: `rbd du <pool>` reads the pool as an
image in the default pool (it needs `--pool`), and Squid's `rbd du` JSON names images `name`, not
`image`. After the fixes both `GET` and the background `POST` (a `dedup.estimate` job, ending
`succeeded` at 100%) returned an estimate for a pool holding a CSI volume and the test image.
Not covered: pools with many images, large data, concurrent client I/O, and any throughput
figure.

**ZFS is still unverified.** There is no zpool on the lab, so the `zdb -S` sample used by the
parser follows the OpenZFS `zdb(8)` documentation, not a captured run.

The job path is integration tested through the gateway (queued, run, result kept, refused before
queueing). That a running scan does not block other jobs rests on the engine routing it to its own
task, which is unit tested; it has not been exercised with a genuinely slow scan.
