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
metadata.

Lab, 2026-10-26 (`deploy/native/lab-verify.sh`, six-process cluster, compression and dedup on):
12 copies of a ~266 KiB, very compressible text object read back byte-exact, and the node reported
269,500 logical bytes, 3,234,000 referenced (sharing 12.0x) and 10,020 stored (compression 26.9x);
the gateway's `/native/reduction` agreed. That input is deliberately favourable, so the ratios show
the mechanism works, not what real data will save. A CPU/throughput measurement on the lab is not
recorded; the tests check byte exactness and that device usage drops, not performance.
