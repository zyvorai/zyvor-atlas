<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# atlas-native data reduction: compression and deduplication

Two independent, opt-in switches in the `metadata` section of the node config (Helm:
`node.compression`, `node.dedup`). Both apply to extents written after they are turned on;
existing extents stay as written and always remain readable, whatever the setting is later.

## Compression (`compression: true`)

New extents are LZ4-compressed before they are replicated or erasure-coded, when that saves at
least 12.5%. Extents under 4 KiB, and data that does not shrink (random, encrypted, already
compressed), are stored as they are at the cost of one failed LZ4 attempt.

How it is built: an extent now has a stored size (`payload_len`, serialized as `len` so catalogs
from before this change load unchanged) and an uncompressed size (`raw_len`, 0 = not compressed).
The extent checksum covers the **stored** bytes, so everything below the read path (devices, scrub,
repair, rebuild, tiering to S3, erasure coding) handles the payload as opaque bytes and needed no
change. Only the read path, snapshot export/import, and the FUSE client's direct reader
decompress. Quotas and `used_bytes` still count logical bytes. The file layout reports the stored
size (`payload_len`) so a client can read and decompress directly; a client that predates the
field fails the checksum and falls back to reading through the leader.

## Deduplication (`dedup: true`)

A write whose content is byte-identical to a live extent of the same metadata group references
that extent instead of storing a second copy: volumes, files, snapshots and clones already share
extents by reference count, so this reuses that machinery. Matching is by the SHA-256 of the whole
extent (a grid cell, 4 MiB by default), so only identical, equally aligned extents match; there is
no sub-extent or content-defined chunking.

- **Copy-on-write isolation** is unchanged: overwriting a shared extent writes a new one, and the
  space is returned when the last reference goes.
- **The index is a hint.** It lives in memory on the leader, is rebuilt from the catalog on the
  first write after a restart or leader change (a scan of every extent), and costs roughly 70 bytes
  per extent. Every share is re-validated when it commits: if the extent was freed or changed in
  between, the catalog rejects the commit and the write is redone without sharing.
- **Quotas** still count logical bytes, so a deduplicated write counts against the filesystem's
  quota in full.
- **Cost:** one SHA-256 per extent written, even when nothing matches.
- **Security:** sharing is within one metadata group, across volumes and tenants of that group.
  Whether data of one tenant already exists is observable to another through timing and space
  usage. Do not enable it where tenants must not learn that, and note that a reference makes
  deletion of the data one tenant wrote depend on the others' references.
- **Upgrades:** a shared install is a new field on existing commands. A node on an older build
  would apply it without the liveness check, so turn `dedup` on only after every metadata pod is on
  a build that has it.

## Seeing the savings

`GET /v1/reduction` on a node returns, for the whole node and per metadata group:
`logical_bytes` (each extent once), `referenced_bytes` (once per reference), `stored_bytes` (one
copy of the stored payload), and the counts of extents, compressed extents and shared extents.

- `stored_bytes / logical_bytes` is the compression ratio.
- `referenced_bytes / logical_bytes` is sharing of **every** kind. Snapshots and clones share
  extents too, so it is not deduplication alone; the dedup-only counters are the metrics below.
- `stored_bytes` is one copy: replication multiplies it by the replica count, erasure coding by
  `(k+m)/k`.

Through the gateway: `GET /api/atlas/v1/native/reduction` (operator role) or `atlasctl
native-reduction` returns those counters plus `saved_by_compression_bytes`,
`saved_by_sharing_bytes`, `compression_ratio` (`logical / stored`, above 1 is better) and
`sharing_ratio` (`referenced / logical`). It is cluster-wide, not per volume.

Prometheus: `atlas_native_dedup_hits_total` and `atlas_native_dedup_saved_bytes_total`.

## Limits

No data reduction for Ceph, ZFS or other backends (they do their own). No inline compression of
metadata. The tests check byte exactness and that device usage drops; the only performance numbers
are the single-core primitive costs below. There is no end-to-end write or read measurement and no
run on a cluster.

## What the primitives cost

`cargo run --release -p atlas-native --example reduction_bench` times the per-extent work that
reduction adds, on one thread, in memory, for 4 MiB extents of four kinds of data. It leaves out
replication, erasure coding, devices and the network, so it bounds the CPU cost and says nothing
about throughput through a running cluster.

Measured on 2026-10-26 on one Apple-silicon laptop (12 cores, one used), Rust 1.99.1, release
build, three runs within about 3% of each other:

| Data | Stored size | LZ4 compress MiB/s | LZ4 decompress MiB/s | SHA-256 MiB/s |
|---|---|---|---|---|
| zeros | 0.4% | 29,000 | 111,000 | 3,300 |
| log lines | 24.3% | 1,900 | 9,600 | 3,300 |
| fixed-width records | 79.2% | 1,200 | 4,900 | 3,300 |
| random (kept raw) | 100% | 15,900 (the failed attempt) | not applicable | 3,300 |

Reading it:

- **Compression** costs roughly 1.2 to 1.9 GiB/s of one core on data that shrinks, so one core can
  keep up with about 10 Gbit/s of such writes before replication or erasure coding. The fixed-width
  records still saved 20.8%, which clears the 12.5% threshold; data that saves less is stored raw.
- **Incompressible data** is detected quickly (about 15 GiB/s), so leaving `compression` on for
  encrypted or already-compressed data adds little CPU.
- **Decompression** is several times faster than compression, so reads are cheaper than writes.
- **Dedup** adds one SHA-256 per extent written, about 3.3 GiB/s of one core here, whatever the
  data. Extents are also checksummed once per write for integrity, so enabling dedup roughly
  doubles the hashing work on the write path.
- The hardware has dedicated SHA instructions. A CPU without them will hash more slowly, so measure
  on the target machine before sizing.
