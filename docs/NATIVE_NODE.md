<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native-node

`atlas-native-node --config <file.json>` runs one native storage node (`crates/atlas-native`,
`node` module). A node runs a **data node** (serves one or more local devices to the cluster), a
**metadata replica** (Raft + `NativeEngine`), or both, and always serves an HTTP endpoint for
health, metrics, status, block volumes and POSIX filesystems. See `docs/NATIVE_METADATA.md` for the
storage design behind it, `docs/NATIVE_FS.md` for filesystems and the `atlas-native-mount` FUSE
client, and "Atlas gateway" below for how the gateway drives it.

## Roles and topology

- **Data nodes** serve `data_dir/data/nvme0.data` by default, or the devices listed in
  `data_node.devices` (see "Devices" below). Their `node_id` is the id the metadata replicas use
  for them (and their TLS server name).
- **Metadata replicas** form one Raft group (3 or 5 voters), or several sharing the same voters
  (`metadata.groups`), and all list the same `data_nodes`.
  Only the leader accepts mutations. Block data reads (`GET .../data` on a volume or snapshot) are
  answered by any replica after a read barrier, so they include every write completed before the
  request; a replica that cannot reach a leader answers 503. `?stale=1` skips the barrier and
  returns whatever the replica has applied, which can be arbitrarily old (a restarted replica starts
  from its last checkpoint). The volume list (`GET /v1/volumes`) has no barrier. A leader may skip
  the quorum round under a read lease (`docs/NATIVE_METADATA.md`, "Leader-lease reads"), which
  relies on its monotonic clock not running much slow.

A typical small cluster runs metadata and data roles on the same three hosts, or three metadata
nodes plus N data nodes. `replicas` must not exceed the number of data nodes; with one spare data
node, writes keep working and repair can re-replicate when a data node dies.

## Config

Unknown fields are rejected. A combined node (`m1` running both roles):

```json
{
  "node_id": "m1",
  "data_dir": "/var/lib/atlas-native",
  "http_listen": "127.0.0.1:7480",
  "api_token_file": "/etc/atlas-native/token",
  "tls": {
    "ca": "/etc/atlas-native/ca.pem",
    "cert": "/etc/atlas-native/m1.pem",
    "key": "/etc/atlas-native/m1-key.pem"
  },
  "data_node": { "listen": "10.0.0.1:7481" },
  "metadata": {
    "listen": "10.0.0.1:7482",
    "peers": { "m1": "10.0.0.1:7482", "m2": "10.0.0.2:7482", "m3": "10.0.0.3:7482" },
    "data_nodes": [
      { "id": "m1", "addr": "10.0.0.1:7481", "rack": "r1" },
      { "id": "m2", "addr": "10.0.0.2:7481", "rack": "r2" },
      { "id": "m3", "addr": "10.0.0.3:7481", "rack": "r3" }
    ],
    "replicas": 3,
    "extent_bytes": 4194304,
    "repair_interval_secs": 300,
    "gc_interval_secs": 60
  }
}
```

`${NAME}` anywhere in the file is replaced with the environment variable `NAME` before parsing; an
unset variable or an unterminated `${` is a config error. `metadata.peers` may list every voter
including this node (its own entry is ignored), so one file can be shared by all members. Peer and
data-node addresses are `host:port` and resolved on every connect, so DNS names that move to a new IP
(a rescheduled pod) keep working.

| Field | Default | Notes |
| --- | --- | --- |
| `api_token_file` | none | The administrator's bearer token: with it, every `/v1/*` request needs it (or a scoped token), and its holder may call everything. With neither this nor `api_tokens_file`, `/v1/*` is open: keep `http_listen` on localhost or a private network. |
| `api_tokens_file` | none | JSON list of named tokens limited to roles and to filesystems and volumes; see [API tokens](#api-tokens). |
| `http_tls` | none | `{"cert", "key", "client_ca"?}`: serve the HTTP API over TLS. With `client_ca`, every `/v1/*` request must also present a client certificate signed by it (401 otherwise; a certificate from another CA fails the handshake), while `/healthz`, `/readyz` and `/metrics` stay open to probes and scrapers. |
| `tls` | none | Mutual TLS for the Raft and data-node transports (not the HTTP endpoint). Node ids must then be DNS names present as SANs on each node's certificate. |
| `max_request_bytes` | 64 MiB | Larger HTTP bodies and read lengths get 413. |
| `metadata.bootstrap` | every `peers` entry | Initial Raft voters, used until the first membership change commits. A node not listed starts as a non-voter (it never campaigns) and waits to be added through `POST /v1/members`. Every id needs a `peers` entry. |
| `metadata.replicas` | 3 | Between 1 and the number of `data_nodes`. |
| `metadata.erasure` | unset | Reed-Solomon scheme `k+m` (e.g. `4+2`, `8+3`; k up to 32, m up to 8) for new extents; unset keeps replicating. Needs at least k+m data nodes; see [Erasure coding](#erasure-coding). |
| `metadata.erasure_min_bytes` | 64 KiB | Smaller extents (file tails, small files) are still replicated. |
| `metadata.concurrent_commits` | off | Writes of new or completely replaced extents commit concurrently ([Concurrent commits](NATIVE_METADATA.md#concurrent-commits-opt-in)). Acts on a group's leader; set it on every metadata node. Not measured. |
| `metadata.dedup` | `false` | Store identical extents once; see [`NATIVE_DATA_REDUCTION.md`](NATIVE_DATA_REDUCTION.md). |
| `metadata.compression` | `false` | LZ4-compress new extents that shrink by at least 12.5%; see [`NATIVE_DATA_REDUCTION.md`](NATIVE_DATA_REDUCTION.md). |
| `metadata.extent_bytes` | 4 MiB | Writes are split into extents of this size. |
| `metadata.delta_writes` | off | Store small partial overwrites as deltas folded later (`{"enabled": true, ...}`); read [`NATIVE_DELTA_WRITES.md`](NATIVE_DELTA_WRITES.md), including its upgrade rules, before enabling. |
| `metadata.tick_ms` | 50 | Raft tick; elections take 10–20 ticks. |
| `metadata.proposal_timeout_ms` | 5000 | Bounds leader readiness, each proposal and each data-node I/O. |
| `metadata.parallel_log_sync` | off | Sync the Raft leader's log on its own thread, in parallel with replication ([Parallel leader log sync](NATIVE_METADATA.md#parallel-leader-log-sync-opt-in)). Per node. Not measured. |
| `metadata.repair_interval_secs` | 300 | Time between the starts of two scrub passes of the leader's rebuild controller; 0 disables the controller (scrub and rebuild). See [Rebuild and scrub](#rebuild-and-scrub). |
| `metadata.rebuild_delay_secs` | 60 | How long a data node must fail every request before its replicas and shards are rebuilt on other nodes. |
| `metadata.rebuild_bytes_per_sec` | 256 MiB | Rebuild traffic cap (bytes read plus written per second, per group); 0 is unlimited. |
| `metadata.scrub_bytes_per_sec` | 64 MiB | Scrub traffic cap, likewise. |
| `metadata.gc_interval_secs` | 60 | Leader-only GC loop; 0 disables. |
| `metadata.cache_inodes` | 262144 | Unchanged inodes each group keeps in memory (at least 64); the rest are read from its catalog store when needed. See [`NATIVE_EDGE.md`](NATIVE_EDGE.md) for what it costs. |
| `metadata.store_cache_bytes` | 67108864 | Page cache of each group's catalog store (at least 1 MiB). |
| `metadata.tiering` | unset | Move cold extents to object storage; see [Tiering](#tiering). |
| `metadata.groups` | 1 | Raft groups the namespace is sharded across (1–64), all on `metadata.listen`; see [Metadata groups](#metadata-groups). Every metadata node must use the same value. It can be raised later but never lowered. |
| `data_nodes[].host` / `rack` / `zone` | `id` / `id` / empty | Failure domains for placement; replicas always land on distinct hosts. |
| `data_nodes[].free_bytes` | 1 TiB | Placement capacity hint. |
| `data_nodes[].devices` | 1 | How many devices that data node serves; new replicas are striped across them round-robin. |
| `data_node.devices` | `[]` | `[{"path", "backend"?}]`, served in index order. Empty: one `file` device at `data_dir/data/nvme0.data`. |
| `data_node.preallocate` | off | `{"enabled", "ahead_bytes"?, "step_bytes"?}`: zero `file` devices ahead of their appends (see "Preallocation" below). |

### Devices

A data node can serve several devices, for example one per NVMe drive:

```json
"data_node": {
  "listen": "10.0.0.1:7481",
  "devices": [
    { "path": "/dev/nvme0n1", "backend": "io_uring" },
    { "path": "/dev/nvme1n1", "backend": "io_uring" }
  ]
}
```

The metadata replicas must then list `"devices": 2` for that data node. A replica records which
device it is on, so device order must not change once data is written.

| `backend` | Access | Notes |
| --- | --- | --- |
| `file` (default) | Buffered I/O on a regular file, `fsync` per write | What every data node used before devices were configurable. Appends only reserve their offset under a lock, so concurrent writes and their syncs overlap. |
| `aligned` | `pread`/`pwrite` with `O_DSYNC` on 4 KiB-aligned buffers | Regular file or block device. Appends start on a 4 KiB boundary; unaligned overwrites read-modify-write the edge blocks. |
| `io_uring` | `O_DIRECT` + `O_DSYNC` through `io_uring`, bypassing the page cache | Linux only, and only in builds with the `atlas-native/io-uring` feature (otherwise the node refuses to start). Same layout as `aligned`. |

A block device has no file length to say how much of it is in use, so `aligned` and `io_uring`
persist a high-water mark in `data_dir/data/dev<index>/<name>.hwm`, raised 256 MiB at a time
before space past it is handed out. Losing that file on a block device loses track of the data on
it; on a regular file the file length is used when the mark is missing. Switching an existing
`file` device to `aligned` or `io_uring` keeps its data: its extents need not be aligned, and new
appends start at the next 4 KiB boundary.

Upgrade order: upgrade the data nodes first, then raise `data_nodes[].devices` on the metadata
replicas. A client asking for device 1 or above first asks the data node how many devices it
serves; an older data node doesn't understand the question, so the request fails instead of
silently landing on device 0. Requests for device 0 are unchanged on the wire.

### Failed data syncs

A data node answers a write with success only once its data is durable. When that fails (the
`fdatasync` of the `file` backend, or a write of the `aligned` or `io_uring` backend):

- the device refuses every later write until the node's process is restarted. Linux reports a
  writeback error to one caller only and then calls the pages clean, so a later success would prove
  nothing;
- writes in progress at the same time are not acknowledged either. Each write waits until every
  sync that began before its own sync returned has returned, and fails if any of them failed:
  one of those may be the caller that was told about this write's data. Syncs still run
  concurrently; the cost is that a write's answer can wait for a slower, earlier sync;
- reads go on, including everything written before the failure;
- the node logs `atlas-native: device <path> failed, ...` once, reports
  `atlas_native_data_device_failed{node,device}` = 1 on `/metrics`, lists the device under
  `data_node.failed_devices` in `/v1/status`, and asks to be restarted (below).

A device that cannot be opened at startup because of an I/O error (a dead disk, a filesystem
remounted read-only, missing permissions) does not stop the node: it takes its position as a failed
device, so that the other devices and the metadata role run. Other errors, such as a backend this
build lacks, still stop the node from starting.

#### What the cluster does meanwhile

The engine on the metadata leader treats the devices of a node separately. A write that a device
refuses goes to another device of the same node, within the same write; only when no device takes
it, or the node cannot be reached, does it go to another node. What a write needs does not change:
the full replica (or shard) count, each on a different node. A node with a single device is backed
off as a whole, as before.

The leader learns of a failed device from a refused write or from the rebuild controller's probe,
and checks again before offering it another write, so a restarted node is used again without any
step by hand. `GET /v1/status` (`data_nodes[].failed_devices`) and
`atlas_native_node_failed_devices{node}` show what it knows.

A device that has stayed failed for `rebuild_delay_secs` has its replicas and shards moved by the
rebuild controller to devices that accept writes, on the same node or another one that holds no
other part of the extent. `POST /v1/repair` does so at once. The old copy is dropped only by the
same commit that records its replacement. With nowhere to go (single-device nodes and no spare
node) the data stays where it is, still readable, counted in `degraded_extents`.

#### Restarting, and its limit

Only reopening the file ends the failed state. While a restart is wanted, `/healthz` answers 503
(`restart required: ...`) and `atlas-native-node` exits non-zero within about a second, so that
Kubernetes, systemd or another supervisor starts it again. **Run the node under a supervisor that
restarts it**; without one it stays down. A stopped metadata replica (a failed log, vote or
checkpoint write) is handled the same way.

On a disk that is bad for good a restart cures nothing, so each component (`device<i>`, `metadata`)
gets at most 3 restarts an hour. They are counted in `data_dir/restarts.json`, written before the
restart is asked for. Past the limit, or when the count cannot be read or written, the process
**stays up**: `/healthz` answers 200, the component stays failed, reads and the healthy devices go
on, and `atlas_native_failed_without_restart{node,component}` = 1 says that someone has to act.
`/readyz` answers 503 if the metadata replica is stopped or no data device accepts writes. A count
that could not be written is tried again every 10 s.

| Environment | Default | Meaning |
| --- | --- | --- |
| `ATLAS_NATIVE_FAILURE_RESTARTS` | 3 | Restarts per component per window; 0 asks for none. |
| `ATLAS_NATIVE_FAILURE_WINDOW_SECS` | 3600 | The window. |
| `ATLAS_NATIVE_DEVICE_FAILURE_RESTART` | `any` | When a failed data device asks for a restart: `any` (at once), `last` (only when no device of the node accepts writes), `never`. |

`any` gets a device back soonest, but a restart also takes the node's healthy devices and its
metadata replica away for as long as it takes. With as many data nodes as `replicas`, writes fail
for that long. On a node with several devices, `last` keeps writes going on the healthy ones and
leaves the failed one to the rebuild controller and an operator.

Related metrics: `atlas_native_restart_required{node}`, `atlas_native_metadata_failed{node}`,
`atlas_native_failure_restarts{node,component}` (restarts counted in the window, which survives
the restart) and `atlas_native_failed_data_devices{node,group}` (devices the leader is moving data
off). `/v1/status` has `failures` and `restarts_counted`.

To recover a node that no longer asks for restarts: fix or replace the disk, delete
`data_dir/restarts.json` to restore its allowance (or wait out the window), and restart the process.

#### What a restart reads back

A restart of the process does not clear the page cache: after a failed sync Linux can go on serving
the new bytes from memory although the disk never got them. Only a reboot discards every such page.
What the node does about it:

| State | On open |
| --- | --- |
| Data devices (`file`, `aligned`) | Asks Linux to drop the file's cached pages (`posix_fadvise`), so reads come from the disk. `io_uring` uses `O_DIRECT`. Nothing refers to the ranges of unacknowledged writes in any case: a write is committed only after its sync, and every sync it could depend on, succeeded. |
| Metadata log | Read as the cache has it and written to a new, synced file. A record whose sync failed can therefore come back and take effect: its outcome was unknown, as after a crash. If the cache has dropped it, the disk's content is read: a torn last line is discarded; damage further in stops the replica from opening. |
| Term and vote, fences, high-water marks | Written again (new file, synced) before the node acts on them. |
| Catalog store | Cached pages dropped before it is opened. |
| Restart counts | If unreadable, no restart is asked for. |

Limits:

- `posix_fadvise` is advisory. Pages still mapped, dirty or under writeback stay, some
  filesystems ignore it, and other systems than Linux have no equivalent here. Where that matters,
  reboot the host before returning a failed disk to service.
- A device is not remembered as bad: after a restart it takes writes again (including into space
  freed by moving data off it) until it fails again, which counts another restart.
- A disk that hangs instead of returning errors is not detected by any of this; requests time out
  and the node is backed off.
- A metadata replica whose disk is so bad that it cannot open stops the whole process from
  starting, its data node included; the supervisor's own back-off is then the only limit. The other
  voters keep the cluster going.
- A data node too old to report failed devices is handled as before (backed off on a failed
  write); upgrade data and metadata nodes to get the behaviour above.
- With `repair_interval_secs` 0 there is no rebuild controller: nothing moves off a failed device
  until `POST /v1/repair`.
- With the `aligned` and `io_uring` backends any write error counts, `ENOSPC` included, because
  it cannot be told apart from a failed sync: keep a file-backed device's filesystem from filling
  up. Each restart of such a device also skips up to 256 MiB of unused space.

`deploy/native/writeback-error-lab.sh` runs `tests/writeback_errors.rs` against real kernel
writeback errors (a device-mapper target that fails chosen blocks; needs sudo on a lab host).

### Preallocation

Off by default; implemented, and its effect on a cluster has not been measured.

```json
"data_node": {
  "listen": "10.0.0.1:7481",
  "preallocate": { "enabled": true, "ahead_bytes": 67108864, "step_bytes": 1048576 }
}
```

An append to a growing file makes the `fdatasync` that follows commit the file's new size and
block allocation too. With this setting a background thread per device keeps `ahead_bytes`
(default 64 MiB) of **written zeros** past the append position, `step_bytes` (default 1 MiB, a
multiple of 4 KiB) at a time, syncing each step, so that appends overwrite blocks that exist.
`fallocate` alone would leave unwritten extents, whose first write is a metadata change again.
`file` devices only: the node refuses the setting with an `aligned` or `io_uring` device.

What changes on disk and in reporting:

- **No new on-disk state.** Recovery still rests on one rule: nothing at or beyond a device file's
  length was ever handed out, so a reopened device appends from there. It holds after a crash at
  any point of preparation, with the setting turned on or off, and for an older binary;
- while the node runs, the file is longer than the space in use. `atlas_native_data_device_bytes`,
  the engine's `atlas_native_device_bytes` and reads and overwrites go by the space in use; `du`
  and `ls` see the prepared tail too;
- a clean stop (`SIGTERM` or `SIGINT`, which the node catches only with the setting on) cuts each
  file back to the space in use;
- **any other stop loses the prepared tail**: up to `ahead_bytes + step_bytes` per device, each
  time. It counts as used and is not reclaimed, because the end of the data cannot be told from
  the bytes (data may end in zeros). After a failed sync the file is also left as it is.
  Preparation starts at a device's first append, so restarts without writes lose nothing more.

Costs: every appended byte is written twice (the zeros, then the data), the preparer's own syncs
share the disk, a writer's `fdatasync` that overlaps a step also writes that step out, and each
device keeps up to `ahead_bytes` more of the filesystem occupied.

Writers do not wait for the preparer's syncs. An append that reaches into the range being zeroed
waits until the zeros are written (not synced); when appends catch up, the preparer skips a step
ahead of them. The preparer syncs through a file descriptor of its own, so it cannot take a
writeback error away from a writer; a failed preparer sync fails the device exactly as in
"Failed data syncs" above. A failed preparer write (a full filesystem, say) is logged, counted
and retried after 10 s, and appends go on without prepared space.

Metrics, present only with the setting on, labelled `node` and `device`:
`atlas_native_data_prealloc_ahead_bytes`, `..._written_bytes_total`,
`..._appends_total{prepared="true"|"false"}` (whether an append landed wholly in zeroed and synced
space), `..._append_waits_total` and `..._failures_total`.

`cargo test -p atlas-native --features prealloc-test` runs the existing tests with it on.
`deploy/bench-vs-ceph/prealloc-probe.sh` checks the mechanism on the host's filesystem and probes
the data writes; the `-prealloc` cases of `compare.sh` run the whole benchmark with it
([`NATIVE_DELTA_REDESIGN.md`](NATIVE_DELTA_REDESIGN.md), "Phase 4").

### Erasure coding

With `"erasure": "4+2"` in the `metadata` section, each extent of at least `erasure_min_bytes` is
split into 4 data shards and 2 Reed-Solomon parity shards, each on a different data node (and so a
different host), so a 4 MiB extent occupies 6 MiB instead of the 12 MiB three replicas take. Any
2 shards can be lost. Each shard carries its own SHA-256 next to the extent's.

- **Reads** fetch the data shards, preferring nodes that are up. A shard that fails its checksum or
  can't be read is replaced by a parity shard and the missing data is decoded.
- **Repair** (rebuild and scrub, below) reads every shard, rebuilds the missing or corrupt ones and
  writes each to a node holding no other shard of that extent. With more than m shards gone the extent is
  reported unrecoverable, as with replicas.
- **Writes** need a node per shard; a node that fails is swapped for a spare. With fewer than k+m
  nodes up, the write fails rather than storing a weaker layout.
- **Existing data** stays as written: changing or removing `erasure` affects new extents only, and
  coded and replicated extents can be mixed in one file. Clients that read data nodes directly
  (`docs/NATIVE_FS.md`) read the data shards and fall back to the leader when any is missing.

Measured codec speed on one core (4 MiB extents, release build): about 1.5 GiB/s to encode
(checksums included) and 2 GiB/s or more to rebuild two lost data shards, for both 4+2 and 8+3.

### Tiering

With `metadata.tiering` set, the leader of each group moves extents nobody has written or read
for `cold_after_secs` to object storage, freeing their space on the data nodes:

```json
"tiering": {
  "store": { "s3": {
    "endpoint": "http://rook-ceph-rgw-store.rook-ceph.svc", "bucket": "atlas-native-tier",
    "access_key_file": "/etc/atlas-native/s3/AWS_ACCESS_KEY_ID",
    "secret_key_file": "/etc/atlas-native/s3/AWS_SECRET_ACCESS_KEY" } },
  "cold_after_secs": 2592000
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `store` | required | `{"s3": {endpoint, bucket, access_key_file, secret_key_file, region?}}` for Ceph RGW (the default in the Helm chart, through an ObjectBucketClaim) or any S3-compatible store, through `atlas-driver-rgw::S3Target`; needs a build with the `s3` feature, which the image has. `{"dir": {"path"}}` uses a directory every metadata node mounts. |
| `prefix` | `atlas-native/` | Each group's objects go under `<prefix>g<N>/extents/<extent id>`. Clusters sharing a bucket need different prefixes. |
| `cold_after_secs` | 30 days | An extent is cold once this long has passed since it was written and since it was last read. Reads are tracked in the leader's memory: a new leader counts every extent as read when it took over. |
| `interval_secs` | 3600 | Time between tiering passes; 0 tiers only on `POST /v1/tier`. |
| `bytes_per_sec` | 64 MiB | Upload rate cap per group; 0 is unlimited. |
| `min_extent_bytes` | 1 MiB | Smaller extents stay on the data nodes, since each object costs a request. |

How it works:

- **A pass.** Tiering an extent reads and verifies it, then uploads it whole. A Raft command then
  records the object key on the extent and frees its replicas or shards. Each pass first deletes
  objects under the group's prefix that no extent references, such as an upload whose commit
  never happened.
- **Reads and writes.** Reads of a tiered extent fetch its object and verify the extent's SHA-256.
  Any metadata replica can serve them, with no data node involved. Writes to a tiered range
  install new extents on the data nodes as usual, and a partial write reads the rest of the
  extent from its object.
- **Snapshots, GC and repair.** Snapshots and clones keep sharing tiered extents. GC deletes an
  object once its extent is unreferenced, and a failed delete is retried by the next pass's sweep.
  Rebuild and scrub skip tiered extents; the object store provides their redundancy.
- **Direct reads.** FUSE clients reading data nodes directly fall back to the leader for tiered
  extents.

`POST /v1/tier` runs a pass on every group this node leads. `/v1/status` reports `last_tier`
(`candidates`, `tiered`, `bytes`, `deferred`, `orphans_deleted`), and `/metrics` has
`atlas_native_tiered_extents`, `atlas_native_tiered_bytes`, `atlas_native_extents_tiered_total`
and `atlas_native_object_reads_total`. Verified against Ceph RGW on the Rook lab (an
ObjectBucketClaim bucket): extents tiered, read back verified, and their objects deleted by GC
(`tests/tiering_s3.rs`, run with `ATLAS_NATIVE_S3_*`).

### Snapshot export and import

The same bucket holds snapshot exports, which outlive the cluster: any cluster configured with
the bucket and `prefix` can import them.

- **Layout.** An export is a manifest (`<prefix>exports/<name>/manifest.json`, listing each
  extent's offset, length and SHA-256) plus one blob per extent, named by its checksum
  (`<prefix>blobs/<sha256>`). Blobs are shared, so exporting a later snapshot of the same volume
  uploads only the extents that changed. Tiered extents are exported from their objects.
- **Export.** `POST /v1/snapshots/{id}/export` runs on the leader of the snapshot's group. The
  manifest is written last, so a failed or stopped export leaves no export behind; its uploaded
  blobs are reused by the next attempt.
- **Import.** `POST /v1/volumes/import` creates a volume of the export's size and writes every
  blob into it, refusing a blob whose length or SHA-256 doesn't match the manifest. A failed
  import leaves a partly written volume; delete it and import again.
- **Delete.** `DELETE /v1/exports/{name}` deletes the manifest, then every blob no other export
  references. While an export is uploading it keeps a marker (`<prefix>pending/<name>`)
  refreshed every minute, and a delete frees no blob while a marker newer than 10 minutes
  exists (the rest are freed by a later delete), so a delete never frees a blob an unfinished
  export reuses.

Exports and imports run one at a time per node, in the background, and fail if the node stops
leading the group; resubmit them to the new leader. `GET /v1/transfers/{id}` reports `state`
(`queued`, `running`, `done` or `failed`), `done`/`total` extents, `error`, and `result`
(`extents`, `uploaded`, `reused`, `bytes_uploaded` for an export; `bytes` for an import).

## HTTP API

| Method and path | Auth | Description |
| --- | --- | --- |
| `GET /healthz` | no | 200 while the process is up and no restart is wanted; 503 after a storage failure that a restart should cure ([Failed data syncs](#failed-data-syncs)). Use it as the liveness probe. |
| `GET /readyz` | no | 200 once every metadata group has a known leader (metadata role; a non-voter waiting to be added counts as ready) or the data node is serving; 503 otherwise, including after a fatal storage error, while a restart is wanted, and when no data device accepts writes. |
| `GET /metrics` | no | Prometheus text: Raft/transport, engine (including `atlas_native_client_sessions` and `atlas_native_file_locks`), data node (including `atlas_native_data_device_failed`), storage failures (`atlas_native_restart_required`, `atlas_native_failed_without_restart`, `atlas_native_failure_restarts`, `atlas_native_metadata_failed`), `atlas_native_{repair,gc}_{runs,errors}_total`, `atlas_native_client_sessions_expired_total` and `atlas_native_cache_leases{group}` (cache leases this node holds out as a group leader). |
| `GET /v1/status` | yes | Raft role/term/leader/indexes and voter flag (`metadata`, group 0; `metadata_groups` lists `{group, leader, ...}` for every group), `layout` (`extent_bytes`, `replicas`), per-data-node health, last repair result, data-node fence and failed devices, `failures` (components failed until a restart, and whether one is wanted), `restarts_counted`, `gpu_io` (a startup GPU/GDS capability probe, files only and unverified on real GPU hardware: [`NATIVE_GPU.md`](NATIVE_GPU.md)). |
| `GET /v1/volumes` | yes | Volumes in the applied catalog. |
| `POST /v1/volumes` | yes | `{"name": "...", "size_bytes": N, "id"?: "..."}` → 201 `{"id": "..."}`. An optional client-chosen `id` (1–64 of `[A-Za-z0-9-]`) makes the create idempotent: repeating it with the same parameters is a no-op, different parameters get 409. |
| `DELETE /v1/volumes/{id}` | yes | 204. |
| `POST /v1/volumes/{id}/resize` | yes | `{"size_bytes": N}` → 200. Grow only; a smaller size gets 409 (it would drop written data). |
| `PUT /v1/volumes/{id}/data?offset=N` | yes | Raw body written at any `offset` → 204. Extents sit on a fixed `extent_bytes` grid; a write covering part of an extent rewrites that extent with the old bytes merged in. |
| `GET /v1/volumes/{id}/data?offset=N&len=M` | yes | Raw bytes from any range within the volume, across extents; never-written bytes read as zeros. `len` above `max_request_bytes` gets 413. Runs a read barrier first (503 without a reachable leader) unless `&stale=1`. |
| `POST /v1/volumes/{id}/snapshots` | yes | `{"name": "...", "id"?: "..."}` → 201 `{"id": "..."}` (`id` as for volumes). |
| `POST /v1/snapshots/{id}/clone` | yes | `{"name": "...", "size_bytes"?: N, "id"?: "..."}` → 201 `{"id": "..."}`: a new volume sharing the snapshot's extents (copy-on-write; it survives deleting the snapshot and its source). `size_bytes` defaults to the snapshot's size and may only be larger. |
| `DELETE /v1/snapshots/{id}` | yes | 204. |
| `GET /v1/snapshots/{id}/data?offset=N&len=M` | yes | Same as the volume read, against the snapshot. |
| `GET /v1/members` | yes | `{"membership": {"type": "stable", "voters": [...]}, "addrs": {id: "host:port"}}` (`type` is `joint` with `old`/`new` mid-change); `addrs` are the Raft addresses learned from membership changes; `groups` lists `{group, membership}` for every metadata group. |
| `POST /v1/members` | yes | `{"voters": {"<id>": "<host:port>", ...}}`: move to exactly this voter set (leader only) and return once the final configuration has committed. 409 while another change is in flight. With several groups each node changes the groups it leads and answers 421 until every group has the new set, so repeat it across the metadata nodes until it returns 200. |
| `POST /v1/repair`, `POST /v1/gc` | yes | Run one pass now over the groups this node leads (421 if it leads none) and return the summed stats. |
| `POST /v1/tier` | yes | One tiering pass now over the groups this node leads (needs `metadata.tiering`). |
| `POST /v1/snapshots/{id}/export` | yes | `{"name": "..."}` (1-128 of `A-Za-z0-9._-`, not starting with `.`) → 202 `{"transfer": "t1"}`; on the leader of the snapshot's group (421 elsewhere). 409 if the export exists or is in progress. Needs `metadata.tiering`. |
| `POST /v1/volumes/import` | yes | `{"export": "...", "name": "...", "id"?: "..."}` → 202 `{"transfer": "t2", "id": "<volume>"}`: creates the volume at the export's size and fills it in the background. |
| `GET /v1/exports`, `GET /v1/exports/{name}` | yes | Complete exports (`name`, `snapshot_id`, `snapshot_name`, `size_bytes`, `extents`, `created_ms`), or one export's manifest. |
| `DELETE /v1/exports/{name}` | yes | 200 `{"blobs_deleted": N}`. |
| `GET /v1/transfers`, `GET /v1/transfers/{id}` | yes | This node's exports and imports (the last 100 finished are kept); see [Snapshot export and import](#snapshot-export-and-import). |
| `/v1/fs/...`, `/v1/fs-snapshots/...` | yes | Filesystems, inodes, file data and filesystem snapshots/clones: see `docs/NATIVE_FS.md`. |

Errors are JSON `{"error": "...", "code": "...", "leader": ...}`; `code` is a stable machine-readable
reason (`not_found`, `exists`, `invalid`, `read_only`, `not_leader`, `unavailable`, …). A mutation sent
to a follower returns **421** with the leader's node id in `leader` (null while unknown). Status codes: 400 bad request, 401 missing or
invalid token, 403 `forbidden` (a [scoped token](#api-tokens) whose roles or limits do not cover
the request), 404 unknown volume/snapshot/route (or no metadata role), 409 rejected by the state
machine, 413 body too large, 503 retryable (not enough data nodes, leadership changed, timeout).

The HTTP server is deliberately small: HTTP/1.1 persistent connections (closed on
`Connection: close`, HTTP/1.0, a malformed request or 30 s idle — keep client pools below that),
`Content-Length` bodies only (chunked requests get 411), 16 KiB of headers, 30 s socket timeouts,
optional TLS (`http_tls`). A small request on a reused connection costs about 60 µs locally; a new
connection per request costs about 7 ms.

## API tokens

`api_token_file` holds one token that may do everything. `api_tokens_file` adds named tokens that
may do less. Both can be set, either alone, or neither (open API).

```json
{"tokens": [
  {"name": "gateway", "token_file": "gateway", "roles": ["lifecycle", "data"]},
  {"name": "s3", "token_file": "s3", "roles": ["data"], "filesystems": ["team-share"]},
  {"name": "auditor", "token_file": "auditor", "roles": ["read-only"]},
  {"name": "officer", "token_file": "officer", "roles": ["retention-unlock"]}
]}
```

`token_file` is relative to the list's directory unless absolute. A token is at least 16
printable ASCII characters without spaces, and no two (the administrator's included) may be equal.
Names are 1-64 of `A-Za-z0-9._-`. A bad list stops the node at startup; the error names the token
and file, never a value.

| Role | May call |
| --- | --- |
| `read-only` | `GET`s: status, members, listings, volume, snapshot and file data, filesystem metadata, diffs, exports and transfers. |
| `data` | The data path of mounts, S3, NFS, SMB and `ublk`: everything under a filesystem's inodes, rename, sessions, locks, `statfs`, volume data (read and write), status and the volume and filesystem listings. |
| `lifecycle` | Create, resize, delete, snapshot, clone and **lock** volumes, filesystems and snapshots; quotas; activity; reduction; listings. No file or volume data. The CSI controller's role. |
| `replication` | What `atlas-native-replicate` calls on either side: filesystem listings, create filesystem, create and delete filesystem snapshots, diffs, file data reads, and `replica/apply`, `replica/inodes/*/data` and `replica/commit`. |
| `admin` | Every route the node knows **except unlock**, including membership changes, repair, GC, tiering, export and import, locality pins, replica promote and demote. |
| `retention-unlock` | `POST /v1/snapshots/{id}/unlock` and `POST /v1/fs-snapshots/{id}/unlock`, nothing else. No other role includes it. |

A token may list several roles. `filesystems` and `volumes` limit a token whose roles are only
`read-only` and `data` (a config error on any other): ids, or `["*"]` for all. A `data` token must
name at least one of the two; once either is given, an omitted one means none. A limited token
may use only the routes that name one of its filesystems (`<fs>@<snapshot>` counts as `<fs>`) or
volumes, `GET /v1/status`, and the three listings, which are cut down to what it may see.
Everything else, block snapshot data included, is refused.

Scoped tokens are deny by default. The node has one table of method and path
(`crates/atlas-native/src/node/authz.rs`); a request not in it is 403 for every scoped token,
`admin` included, and a test fails when the router has a route the table lacks. Paths are matched
exactly as the router reads them: no percent decoding or `..` handling, surrounding slashes
ignored. The `api_token_file` token is not subject to the table.

Refusals are written to stderr as `atlas-native-audit {json}` (event, token name, method, route
pattern, object id, reason, status), as is every authorized unlock (`retention_unlock`), and
counted in `atlas_native_api_denied_total{reason}`.

### Limits

- **HTTP API only.** The data-node and Raft transports have no roles. Whoever a data node accepts
  (see [`NATIVE_METADATA.md`](NATIVE_METADATA.md)) can read any
  extent, and without `grant_key_file` overwrite any, a locked snapshot's included. A limited
  mount with direct reads or writes needs data-node access that is not limited to its filesystem.
- **Ids that are not tied to a path.** A client session is not bound to a filesystem in the API: a
  `data` token that learns the session id of another filesystem's mount can renew, close or reopen
  that session through its own filesystem's routes. Likewise `direct-commit` or `direct-abort`
  with another file's lease id ends that lease. Both ids are random UUIDs; a session id also shows
  in its filesystem's lock listing.
- **Retention.** Only the node's snapshot locks need `retention-unlock`. S3 object retention is
  checked by the S3 gateway alone, so any token with `data` on that filesystem can delete the file
  ([`WORM.md`](WORM.md)). A `lifecycle` token can set a very long lock that only an unlock
  holder can clear, pinning capacity. The `api_token_file` token still clears locks.
- **Coarse roles.** `lifecycle`, `replication` and `admin` cannot be limited to some objects.
  `replication` reads every filesystem's data, deletes any unlocked filesystem snapshot and can
  create ordinary filesystems. A CSI node plugin needs `data` on all filesystems, so one
  compromised host reaches all file data. These are roles for components, not tenants or users.
- **Static.** Lists and token files are read at startup: adding, rotating or revoking a token needs
  a restart of every metadata node (and of the clients that hold it). Tokens do not expire. Every
  metadata node needs the same list.
- **Audit** is a log, not a durable record. At most 20 refusals a second of each kind (401, 403)
  are written; the next line says how many were dropped. `retention_unlock` records that the call
  was authorized, not that it succeeded.
- Bearer tokens cross the network in clear without `http_tls`. `/healthz`, `/readyz` and
  `/metrics` need no credential, as before.

### In the Helm chart

`scopedTokens.enabled=true` makes the nodes accept one token per component, from Secret
`<fullname>-api-tokens` (generated, or `scopedTokens.existingSecret`), and with
`useForComponents` (default) mounts each component's own:

| Key | Roles | Limits |
| --- | --- | --- |
| `gateway` | `lifecycle`, `data` (and `retention-unlock` with `gateway.retentionUnlock`) | none |
| `csi-controller` | `lifecycle` | none |
| `csi-node` | `data` | all filesystems |
| `s3`, `nfs`, `smb` | `data` | the filesystems of the buckets, exports or shares |
| `replicate` | `replication` | none |

`apiToken` stays, mounted by the nodes, as the administrator's. When turning this on for a running
release, upgrade first with `useForComponents=false`, wait for the metadata pods to roll, then set
it to `true`, so no component presents a token a node does not know yet. Restarting the CSI node
plugin breaks its mounts ([`NATIVE_CSI.md`](NATIVE_CSI.md)). Changing a token's value in the
Secret does not roll anything: restart the StatefulSet and the component.

For replication into this cluster set `scopedTokens.acceptReplication=true` and give the source's
`replication.target.tokenSecret`/`tokenKey` the `replicate` key. For the Atlas gateway set its
chart's `native.tokenSecret` to a copy of this Secret and `native.tokenSecretKey=gateway`; its
two-admin unlock then needs `gateway.retentionUnlock=true`, which also lets whoever holds that
token unlock alone.

## Operations

- **Maintenance**: the leader runs the rebuild controller (below) continuously and `gc_once`
  (reclaim unreferenced extents) every `gc_interval_secs`. `POST /v1/repair` still runs one
  full, unpaced repair pass on demand.
- **Dead connections**: a peer that vanishes without closing its sockets (a deleted pod, a
  powered-off host) is detected by its Raft senders: a connection is replaced when the peer has
  answered none of our requests for `max(40 ticks, 1 s)` or has reconnected to us since (it
  restarted). Counted in `atlas_native_transport_stale_reconnects_total`.
- **Failure**: a data node that fails I/O is backed off for 5 s and writes move to the next eligible
  node; reads fall back to other replicas. Losing the metadata leader triggers an election
  (typically well under a second with the default tick) and clients retry against the new leader.
- **Durability**: a follower fsyncs its log once per batch of AppendEntries, before acknowledging
  any of it; the leader writes proposals in parallel with replicating them and counts itself towards
  a quorum only for entries it has fsynced, so concurrent proposals share fsyncs (group commit). The
  catalog is written to disk only when the log is compacted: once 2048 entries have been applied
  since the last snapshot, everything but the newest 1024 is folded into it, so a briefly lagging
  follower still catches up from the log rather than a full catalog transfer. A restart replays
  the log since the last snapshot. A follower too far behind gets one InstallSnapshot per election
  timeout until it answers.
- **Slow disks**: the Raft core holds its lock while it fsyncs, so a disk whose fsyncs take hundreds
  of milliseconds can delay heartbeats past the election timeout (10–20 ticks) under write load and
  cause needless elections (clients retry through them). On such disks raise `metadata.tick_ms`
  (e.g. 200 for 2–4 s elections).
- **Restart**: all Raft state and data are crash-safe on disk, so stopping the process (any signal)
  needs no graceful path. After a storage error (a failed sync of a data device or of the metadata
  replica's state) the process exits non-zero so its supervisor restarts it from disk, a bounded
  number of times: see [Failed data syncs](#failed-data-syncs) and
  [`NATIVE_METADATA.md`](NATIVE_METADATA.md#failed-writes-and-syncs).
- **Logging**: startup prints the bound addresses to stderr; everything else is in `/metrics` and
  `/v1/status`.

### Rebuild and scrub

The leader of each metadata group runs a rebuild controller:

- **Finding lost nodes.** Every 5 s it probes each data node that isn't backed off. A node that
  has failed every request (probes, client I/O, repair) for `rebuild_delay_secs` counts as lost.
  A node that answers again within the delay, such as one that restarted, costs no rebuild.
- **Rebuild.** Extents with replicas or shards on lost nodes, or on devices that have refused
  writes for `rebuild_delay_secs` ([Failed data syncs](#failed-data-syncs)), are found from
  metadata alone, and
  rebuilt onto other nodes with the least spare redundancy first. A 4+2 extent that lost two
  shards goes before one that lost one, and a 3-replica extent down to one copy goes before one
  down to two. Parts on lost nodes are rebuilt without trying to read them. Extents that have
  lost more parts than their layout tolerates are counted and left alone until a node returns.
  An extent with no eligible target is retried after 30 s.
- **Scrub.** When nothing needs rebuilding, it reads back and verifies the next 64 extents in id
  order, resuming where it stopped. Missing and corrupt parts are rebuilt, so bit rot is caught.
  A pass over every extent starts every `repair_interval_secs`, and the finished pass is reported
  as `last_repair`.
- **Pacing.** Rebuild and scrub traffic are capped separately (`rebuild_bytes_per_sec`,
  `scrub_bytes_per_sec`). Client writes are held off only while a replacement is placed and
  committed, never for a whole pass.

`/v1/status` reports `rebuild` for each group the node leads: `lost_nodes`, `failed_devices`, `degraded_extents`,
`at_risk_extents` (one more lost part from unreadable), `unrecoverable_extents`, `rebuilt` and
`scrub` totals. `/metrics` has `atlas_native_lost_nodes`, `atlas_native_degraded_extents`,
`atlas_native_at_risk_extents`, `atlas_native_unrebuildable_extents`,
`atlas_native_rebuild_bytes_total`, `atlas_native_scrub_passes_total` and
`atlas_native_scrub_bytes_total`, labelled by node and group.

## Membership changes

The voter set lives in the replicated log. `POST /v1/members` on the leader moves it to a new set by
joint consensus: the leader appends a `joint` configuration (old + new; every election and commit
needs a majority of **both**), and once that commits it appends the `stable` new set. Each node
switches configuration as soon as the entry reaches its log, so there is no window with two
independent majorities. Only one change runs at a time; the request returns after the final entry
commits (bounded by `6 × proposal_timeout_ms`).

- **Adding** a node: start it with `metadata.bootstrap` set to the current voters (so it does not
  count itself in) and `peers` listing them; it waits as a non-voter. Then post the full new voter set
  including its address. The leader replicates the log (or a snapshot) to it as part of the change.
- **Removing** a node, including the leader: post the set without it. A leader that removes itself
  steps down once the final entry commits and the remaining voters elect a new leader. A removed
  follower may never receive the final entry; it cannot disrupt the new group (its pre-votes need a
  majority of the new set too) and should simply be shut down.
- Change one voter at a time where possible and keep the voter count odd. Data placement
  (`data_nodes`) is separate from Raft membership: removing a data node from the config makes repair
  re-replicate its extents onto the remaining nodes (`POST /v1/repair` to run it now).

## Metadata groups

`metadata.groups: N` shards the namespace across N Raft groups. Every metadata node runs a replica
of every group over its one `metadata.listen` port (frames carry their group), so the groups share
voters but elect leaders independently and spread leadership, commits and catalog memory across
the nodes. Each group keeps its own log, catalog and free list (`raft`/`engine` for group 0,
`raft-g<N>`/`engine-g<N>` for the rest) and its own fence on every data node.

- **Placement.** A new volume or filesystem goes to its id's home group (FNV-1a of the id, modulo
  N). Snapshots and clones stay in their source's group, because they share its extents. Data nodes
  are shared: each append is allocated by the data node, so groups never coordinate space.
- **Routing.** Any node accepts any request and finds the group holding the id. It checks its local
  catalogs first, and on a miss runs a read barrier on every group (`docs/NATIVE_METADATA.md`, "Read
  barriers") before answering 404, so a lagging replica never misses a committed object. A create
  or clone whose id another group already holds gets 409. Two concurrent creates of the same id
  that land in different groups can both succeed; give every object a unique id.
- **Lists** (`GET /v1/volumes`, `/v1/fs`) merge every group's catalog after a barrier on each.
- **Health.** `/readyz` waits for a leader in every group, and the driver reports a group without
  one as critical. `/metrics` labels engine metrics with `group`.
- **Changing N.** Raising it is safe: existing objects stay where they are and are found by lookup.
  Lowering it would orphan the objects in the removed groups, so a node whose data directory holds
  a group at or above the configured count refuses to start. Every metadata node must use the same
  value.

## Kubernetes

`Dockerfile.native` builds a slim image (`atlas-native-node`, uid 10001). `deploy/k8s/atlas-native.yaml`
runs it as a 3-replica StatefulSet in namespace `atlas-native`, each pod a combined metadata + data
node:

- one shared ConfigMap with `node_id: "${POD_NAME}"` (downward API) and peers/data nodes addressed by
  stable DNS `atlas-native-N.atlas-native.atlas-native.svc.cluster.local` through a headless Service
  with `publishNotReadyAddresses` (members must find each other before any is ready);
- per-pod state on a `volumeClaimTemplate` (5 Gi, default StorageClass), so Raft log, catalog and
  extents survive rescheduling;
- the API token from Secret `atlas-native-api`; ClusterIP Service `atlas-native-api:7480` for clients
  (mutations sent to a follower get 421 with the leader's pod name);
- probes on `/healthz` (startup, liveness; fails while a storage failure calls for a restart) and
  `/readyz` (readiness), a PDB of `maxUnavailable: 1`,
  non-root, read-only root filesystem, all capabilities dropped.

`scripts/deploy-native-remote.sh <host> [user] [--verify-failover]` builds the image with podman on a
k3s host, tags it by content id, imports it into containerd, creates the token Secret if missing and
applies the manifest with the image pinned to that tag and the pod template stamped with a manifest
hash. Pods therefore roll only when the image or manifest changed; re-running it is a no-op. It then
writes and reads a block through the leader, and with `--verify-failover` deletes the leader pod and
reads the block back from the newly elected one.

### Helm

`deploy/helm/atlas-native` is the production form of the same layout: configurable voter count,
replication factor and engine tuning, an API token Secret (generated and kept across upgrades, or
your own), optional Raft/data mutual TLS (an existing Secret or a cert-manager `Certificate` with
every pod name as a SAN), optional HTTPS with client-certificate auth, and a `ServiceMonitor`. See
its [README](../deploy/helm/atlas-native/README.md). `deploy/native/helm-live-check.sh <repo> <tag>`
installs it with both TLS layers on a throwaway PKI and verifies auth refusals, unaligned I/O,
leader-pod failover and that an unchanged `helm upgrade` restarts nothing.

## Smoke test

`deploy/native/smoke.sh [path/to/atlas-native-node]` starts three metadata and three data node
processes on localhost, writes and reads a block through the leader with `curl`, kills the leader,
and checks the new leader still serves the block and accepts writes.

`crates/atlas-native/tests/node.rs` runs the same topology in-process: auth, readiness, follower
421 with the leader hint, volume/snapshot round trips, request validation, metrics, background
repair after losing a data node, and config validation.

## Atlas gateway

`atlas-driver-native` is the gateway's `StorageDriver` for a native cluster. With
`ATLAS_NATIVE_ENABLE=1` (Helm: `native.enabled` in `deploy/helm/atlas`) the gateway registers
backend `bkd_native`, discovers it at startup and every `ATLAS_MONITOR_INTERVAL_SECS`, and serves:

- inventory: one cluster `cls_native_bkd_native`, one replicated pool `native` (replica size from
  `/v1/status` `layout`), block volumes `vol_native_<native id>` and filesystems
  `vol_native_fs_<native id>` (kind `filesystem`, see `docs/NATIVE_FS.md`); health is critical without a
  metadata leader and warn while a data node is down. Capacity is not reported (the nodes do not
  know their disks' size), so it stays empty instead of being invented;
- `POST /volumes` with `"kubernetes": {"backend_id": "bkd_native"}`: created synchronously through
  the leader (201, no job; `"kind": "filesystem"` makes a filesystem, `atlasctl create-volume
  --backend bkd_native [--kind filesystem]`), recorded under the request's tenant (quota admission and product
  bindings as for any volume); `DELETE /volumes/{id}`, `POST /volumes/{id}/expand`,
  `POST /volumes/{id}/snapshots`, `DELETE /snapshots/{id}` and `POST /snapshots/{id}/clone` /
  `restore` (a new volume from the snapshot, recorded as its dependent so the snapshot can't be
  deleted without `force` while it exists) likewise go straight to the cluster;
- block data: `PUT /volumes/{id}/data?offset=N` (raw body) and
  `GET /volumes/{id}/data?offset=N&len=M` proxy to the node data API for atlas-native volumes
  (operator role and the volume's tenant; at most 4 MiB per request, larger bodies get 413;
  ranges outside the volume get 400; volumes on other backends get 400).

Real mode (`ATLAS_NATIVE_DRIVER_MODE=real`) needs `ATLAS_NATIVE_ENDPOINTS` (comma-separated
`https://pod:7480` URLs of metadata nodes). Mutations are retried across the endpoints until the
leader accepts them (for up to 5 s while every node answers "not the leader"); reads use any
node. The driver picks the ids of new volumes and snapshots, so a create retried after an
ambiguous failure (a 503 or timeout after the proposal) cannot create a second object, and a
retried delete that finds the object gone counts as done. `ATLAS_NATIVE_TOKEN_FILE` is the API token,
`ATLAS_NATIVE_CA_CERT` a private CA for `http_tls`, and `ATLAS_NATIVE_CLIENT_CERT`/`_KEY` a client
certificate for clusters with `client_ca`. Fake mode keeps volumes in memory for demos and tests.

Verified live (2026-10-03) on the k3s lab: a gateway in real mode against a 3-pod Helm release
discovered `bkd_native` (pool `native`, 3 replicas, ok), created a 16 MiB volume (201, visible on
the nodes), showed its used extent after a write, took a snapshot, expanded the volume to 32 MiB,
cloned and restored the snapshot (the clone's bytes matched the snapshot; snapshot delete got 409
while they existed), then deleted everything on the cluster.
A 1 MiB write through the gateway's data route read back identically through the gateway and
straight from the nodes, and a clone of the snapshot served the same bytes through the gateway.
