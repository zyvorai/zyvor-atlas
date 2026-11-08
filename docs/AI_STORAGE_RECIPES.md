<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# AI storage recipes: KV-cache cold tier and vector-database snapshots

Two recipes for pointing AI software at Atlas native storage. Both are **documentation plus a CPU-only probe**.
Neither has been run with the third-party software it describes:

- No LMCache, vLLM, Qdrant, Milvus, pgvector or Lance process has been started against Atlas. The probe
  (`deploy/native/s3-pattern-probe.py`) replays request shapes read from LMCache's source with plain HTTP, and
  races a file-system primitive that Lance's source relies on. It shows that the Atlas gateway and mount accept
  those operations. It does not show that the products work.
- No performance, capacity or GPU claim is made. The only host these were run on has no GPU.
- Anything about third-party software below comes from the project's own documentation or source, linked in
  [Sources](#sources) with the version or commit that was read (all read on 2026-11-08). A statement we could
  not confirm is marked **UNVERIFIED** or left out. Those projects change; re-read their pages before relying on
  a key name here.

Related pages: [AI_INFERENCE.md](AI_INFERENCE.md) (KV-cache storage interfaces and the generic smoke test),
[AI_DATA_SERVICES.md](AI_DATA_SERVICES.md) (the `atlas_data` companion service), [NATIVE_FS.md](NATIVE_FS.md),
[NATIVE_S3.md](NATIVE_S3.md), [WORM.md](WORM.md).

## A. LMCache with Atlas as the cold tier

### What LMCache documents

LMCache's latest release at the time of reading was v0.5.7. It documents two modes: multiprocess (MP) mode, where
storage backends are "L2 adapters" selected by repeatable `--l2-adapter '<JSON>'` arguments to `lmcache server`,
and an in-process mode that the v0.5.7 docs mark deprecated, configured with `remote_url` or
`remote_storage_plugins`. The adapters below are the same in v0.5.7 and in the development branch we read
(`a4a6888`, 2026-11-08): the three docs pages and the sources of the `s3`, `fs` and `fs_native` adapters and `connector.cpp` were byte-identical.

| LMCache adapter (MP mode) | Required keys | Notable optional keys |
|---|---|---|
| `fs` | `base_path` | `relative_tmp_dir`, `read_ahead_size`, `use_odirect` (default `false`) |
| `fs_native` | `base_path` | `num_workers`, `relative_tmp_dir`, `use_odirect` (default `false`), `read_ahead_size`, `max_capacity_gb`, `num_read_workers`, `read_max_bytes_in_flight` |
| `s3` | `s3_endpoint`, `s3_region` | `disable_tls`, `aws_access_key_id`, `aws_secret_access_key`, `s3_prefer_http2`, `s3_num_io_threads`, `s3_enable_s3express`, `max_capacity_gb` |

Every adapter also accepts `"shared": true` for a storage domain several LMCache instances share; the docs'
example is an NFS export (`{"type": "fs", "base_path": "/mnt/nfs/lmcache", "shared": true}`).

LMCache owns serialisation, chunking (`--chunk-size`, in tokens), eviction and what goes in an object. Atlas stores
and returns bytes. Nothing here depends on, or changes, how LMCache sizes its objects.

### Option 1: the filesystem adapter on an `atlas-native-mount` path

```sh
# once: a filesystem volume (see AI_INFERENCE.md for the single-copy profile)
atlasctl create-volume kv-cache --kind filesystem --backend bkd_native
# on the node that runs `lmcache server`
atlas-native-mount --endpoint http://m1:7400,http://m2:7400,http://m3:7400 \
  --token-file /etc/atlas-native/token --fs <fs-id> /mnt/kv-cache
lmcache server ... \
  --l2-adapter '{"type": "fs", "base_path": "/mnt/kv-cache/lmcache"}'
```

(`atlas-native-mount` flags are in [NATIVE_FS.md](NATIVE_FS.md#atlas-native-mount); give it every metadata node,
as [AI_INFERENCE.md](AI_INFERENCE.md#lab-result-2026-11-01) explains.) A CSI `PersistentVolumeClaim` or the NFS
gateway work the same way as far as LMCache is concerned: it only sees a directory.

What the adapter does to the directory, from `fs_l2_adapter.py`, `fs_native_l2_adapter.py`/`connector.cpp` and
the legacy `fs_connector.py`, and what Atlas gives for each:

| LMCache behaviour (source) | Atlas |
|---|---|
| Writes each object to a temporary name (`<name>.tmp` next to the final file, or inside `relative_tmp_dir`) and publishes it with `os.replace` / `std::filesystem::rename` | `rename`, including over an existing entry and across directories, is supported ([NATIVE_FS.md](NATIVE_FS.md)); `kvcache-smoke.sh` checks that a replacement is never seen half-written |
| Skips a store if the final file already exists; a failed store removes its temporary file | Plain `stat`, `unlink` |
| No `fsync`, `flock`, `fcntl` or other lock call anywhere in `fs_l2_adapter.py`, `fs_connector.py` or `connector.cpp` (searched) | Nothing to satisfy. Atlas does support POSIX byte-range locks, and `fsync` sends buffered data to the cluster |
| `use_odirect` defaults to `false`. `fs` falls back to buffered I/O when the buffer is unaligned or the filesystem answers `EINVAL`/`EOPNOTSUPP`; `fs_native` instead reports a runtime error on a misaligned buffer and pads each object to `--l1-align-bytes` | `O_DIRECT` is supported through the mount "with no alignment requirement" ([NATIVE_FS.md](NATIVE_FS.md#direct-io)), so neither path should be refused. Start with `use_odirect: false`, as LMCache's own docs advise, and turn it on only after the smoke test passes on the same mount |
| File names are `<model with "/" replaced by "-SEP-">@<rank>@<group>@<hash>[...].data` | Ordinary names; a name over 255 bytes is the usual POSIX limit (the S3 gateway enforces it, see below). Very long model names are the only way to hit it |
| `max_capacity_gb` is client-side accounting; the `fs_native` source says a positive value "does **not** bound disk usage on its own" (eviction runs only with an `eviction` block) | Atlas filesystems have no size limit but can have a quota; a refused write returns `EDQUOT`, possibly from a later `write`/`fsync`/`close` ([NATIVE_FS.md](NATIVE_FS.md)). LMCache's behaviour on `EDQUOT` was not read |

Sharing one mount path between several LMCache instances: Atlas gives close-to-open consistency across mounts
(data is visible to another mount after the writer closes or fsyncs and the reader opens). Because the adapter
publishes by rename, a reader should see an old object or a whole new one. That is reasoning from the two
documents, **UNVERIFIED** with LMCache.

### Option 2: the S3 adapter on the Atlas S3 gateway

The documented keys:

```sh
lmcache server ... --l2-adapter '{"type": "s3",
  "s3_endpoint": "<bucket>.<domain>[:port]", "s3_region": "us-east-1",
  "disable_tls": true,
  "aws_access_key_id": "...", "aws_secret_access_key": "..."}'
```

Three findings from LMCache's source matter here, and one documentation conflict:

1. **Virtual-hosted addressing only.** The docstring of `S3L2AdapterConfig` says `s3_endpoint` is a bucket URL "using
   **virtual-hosted** style", that the bucket name must be part of the host "because requests are signed and routed
   against this Host header (path-style addressing is not supported)". The adapter builds every request with
   `Host: <endpoint>` and the path `/<key>`; there is no option for the other style. The in-process S3 connector
   builds requests the same way.
2. **Custom endpoints work, with that constraint.** `disable_tls` is documented as bypassing TLS "when pointing at a
   plain-HTTP endpoint (e.g. a local MinIO)", credentials can be static keys in the adapter config, and the docs name MinIO and Ceph RGW as
   S3-compatible targets. The AWS CRT client is constructed with a region and no endpoint argument, so the host it
   connects to can only come from the `Host` header (our inference from the code, **UNVERIFIED** against the CRT).
3. **Exact-size loads.** A load succeeds only when the stored object is exactly the destination buffer size; a
   larger object is refused and a shorter one is a miss, and either way the engine recomputes the chunk.
4. **Conflict in LMCache's pages.** The MP docs' MinIO example uses `"s3_endpoint": "minio.local:9000"` (no bucket in
   the host), and the in-process page uses `remote_url: "s3://your-bucket-name"`, which the code turns into
   `Host: your-bucket-name`. The docstring in the source says the bucket must be in the host. We follow the source;
   if your LMCache version behaves differently, the gateway accepts both styles (see the probe), so only LMCache's side needs checking.

What this means on Atlas:

- The gateway must be started with a virtual-hosted domain (`s3.domains` in the Helm chart, `--domain` on the
  binary), and `<bucket>.<domain>` must resolve to the gateway from the LMCache host (wildcard DNS, or one record
  per bucket). Path-style, which the gateway always accepts, is not usable by this adapter.
- [NATIVE_S3.md](NATIVE_S3.md) used to say virtual-hosted style was "implemented but not yet verified live". The probe
  below exercised it through a running gateway (see the lab result); it has still not been exercised by LMCache's CRT client.
- The adapter's requests are `PutObject` (with `Content-Type: application/octet-stream`), `HeadObject`,
  `GetObject`, `DeleteObject` and `ListObjectsV2` (`list-type=2`, `max-keys`, `prefix`, `continuation-token`). The
  CRT client splits large transfers: its `GetObject` page says it works by "parallelizing multiple part-sized `GetObject` with range
  requests", and `awscrt` documents a multipart-upload threshold for `PUT_OBJECT`. The gateway supports all of these,
  including ranges and multipart (parts of 5 MiB or more; completion copies the parts, so it is slower than a single
  PUT).
- Object names are `<model>@<rank:08x>@<group:x>@<hash>[...]` with the model's slashes kept, so a model such as
  `meta-llama/Llama-3.1-8B` becomes a directory `meta-llama/` in the filesystem. The gateway refuses empty path
  segments, `.`/`..`, segments over 255 bytes or starting with `.atlas_`, and keys over 1024 bytes. A model name that
  contains none of those is fine.
- The bucket is a filesystem, so the same objects are files under the same filesystem on any mount; that also means a
  bucket can be listed, snapshotted and retention-locked like any other filesystem ([Part B](#b-vector-database-snapshots)).

Choosing between the two is an operational decision, not a performance one (no numbers were measured): the mount
path needs the FUSE client on the GPU host and has no addressing requirement; the S3 path needs no client software
beyond LMCache's but needs wildcard DNS and a gateway domain.

### Relation to `tools/atlas_data/kv_connector.py`

`KVConnector` and `KVAdapter` are a different thing. They store opaque blocks in the `atlas_data` service's own
tenant-scoped, content-addressed store over its HTTP RPC, with a `vLLM-style` shape and no vLLM import. They are not
LMCache's L2 adapters, LMCache does not call them, and neither has been tested against vLLM or LMCache
([AI_DATA_SERVICES.md](AI_DATA_SERVICES.md#prefix-blocks)). LMCache documents `plugin` and `native_plugin` L2 adapter
types that load a user-supplied class implementing its `L2AdapterInterface`; a wrapper around `KVAdapter` could use
that hook, but nobody has written or run one, and the filesystem and S3 paths above need no code.

### Probe

`deploy/native/s3-pattern-probe.py` (standard library only) has two subcommands. Credentials come from
`S3_ACCESS_KEY` and `S3_SECRET_KEY`.

```sh
# request shapes of the LMCache S3 adapter, path-style and virtual-hosted (Host: <bucket>.<domain>:<port>)
python3 deploy/native/s3-pattern-probe.py s3 --endpoint http://gw:9000 --bucket kvfs --addressing path
python3 deploy/native/s3-pattern-probe.py s3 --endpoint http://gw:9000 --bucket kvfs \
  --addressing virtual --domain s3.example.com
# the create-if-absent primitive Lance's local commit uses, on a mount
python3 deploy/native/s3-pattern-probe.py link-race /mnt/kv-cache
python3 -m unittest deploy/native/test_s3_pattern_probe.py        # offline tests
```

It checks: `HEAD` of a missing key is 404; `PUT` with `Content-Type`; `HEAD` length; full `GET`; part-sized parallel
ranged `GET`s; overwrite; `ListObjectsV2` paging by prefix; optional multipart upload; `DELETE`. It also prints
one `INFO` line, never a failure: how many of 8 concurrent create-only (`If-None-Match: *`) `PUT`s of one new key
succeeded. It measures no throughput and does not use the AWS CRT client, so a pass proves the gateway accepts
these request shapes, nothing about LMCache. The existing [`kvcache-smoke.sh`](AI_INFERENCE.md#smoke-test) covers the
direct-I/O and rename patterns of the file path.

### Lab result (2026-11-08)

On the shared dev host (no GPU), inside a `rust:1.98-slim` container: a throwaway cluster of three metadata and three
data nodes (replicas 3, extent size 4 MiB, debug build of `25cc186` with the container's Rust 1.98.x (the `rust-toolchain.toml` pin was removed in the scratch copy)), the S3 gateway as a plain process
started with `--domain s3.lab.test`, one filesystem served as bucket `kvfs`, and `atlas-native-mount` over the same
filesystem for the file checks. The probe's PUT, GET and DELETE statuses and byte comparisons, in raw form:

```
=== path-style
PASS  HEAD of a missing key is 404 (got 404)
PASS  PUT with Content-Type application/octet-stream (got 200 )
PASS  HEAD returns Content-Length 8388608 (got 200 8388608)
PASS  GET returns the exact bytes (got 200, 8388608 bytes)
PASS  8 part-sized ranged GETs (1024 KiB, 8 in flight) answer 206 with the right bytes
PASS  overwriting a key with a new PUT replaces the object
PASS  ListObjectsV2 with prefix, max-keys=2 and continuation tokens returns all 5 keys in 3 pages
PASS  multipart upload (5 MiB + 1 MiB parts) completes and reads back exactly (complete=200 get=200)
PASS  DELETE removes every probe key (statuses [204], then HEAD [404])
INFO  8 of 8 concurrent PUTs of one new key with If-None-Match: * succeeded (statuses [200 x8]); ...
=== virtual-hosted-style   (Host: kvfs.s3.lab.test:39019, connection to 127.0.0.1:39019)
 ... the same nine PASS lines and the same INFO line ...
=== link-race on FUSE mount
PASS  20 rounds of 8 concurrent os.link() onto one name: exactly one wins, the others get EEXIST
```

The mount run was repeated once with the same result, and `kvcache-smoke.sh` (16 blocks of 2 MiB) also passed on
that mount in the first run. The probe's signer was cross-checked against `botocore`'s SigV4 implementation in a
`python:3.12` container (same signature for a `PUT` with a query string and an `@` in the key). What this shows:

- The gateway accepts the LMCache S3 adapter's request shapes with LMCache-shaped keys (an `@`-containing name under a
  model directory), in **virtual-hosted style with a port in the `Host` header**, which [NATIVE_S3.md](NATIVE_S3.md) listed as
  unverified. It was a plain-process gateway with `--domain`, not the Helm chart's `s3.domains`, and the client was a
  Python script, not the AWS CRT.
- Eight concurrent create-only `PUT`s of one key **all succeeded**, which is what [NATIVE_S3.md](NATIVE_S3.md) says to expect
  (the condition is checked before the write, not atomically). A store that enforces `If-None-Match: *` would let one
  win. This is a count from one run, not a bound, but it shows the race is reachable.
- `link()` onto an existing name returned `EEXIST` for every loser, 20 rounds of 8 threads, on the FUSE mount.

It does not show that LMCache, vLLM or Lance work against Atlas, says nothing about speed, and used a loaded shared host.


### Not done for Part A

- LMCache (either mode) has not been run against an Atlas mount or bucket; neither has vLLM.
- The AWS CRT client's use of the `Host` header, its multipart threshold and part size, and whether it sends
  checksum headers were not tested; the probe signs payload hashes itself.
- `fs_native` was not read beyond its configuration and `connector.cpp`'s rename, open and alignment calls.
- LMCache's behaviour on `EDQUOT`, `ENOSPC` or HTTP 507 was not read.
- LMCache eviction against an Atlas filesystem (deleting objects frees space only after GC and any snapshots that
  hold them; see [NATIVE_METADATA.md](NATIVE_METADATA.md)).

## B. Vector-database snapshots

### The Atlas side

An Atlas native filesystem snapshot is a metadata operation: it "copies a filesystem's inode table under a snapshot id
and takes a reference on every extent it points to; no data is copied" ([NATIVE_FS.md](NATIVE_FS.md)). It is committed as
one replicated `SnapshotFs` operation, so the snapshot holds the filesystem as of one point in the metadata log.
Data a mount still buffers (write-back) is not in it until the application's `fsync`/close sends it. That makes a
snapshot **crash-consistent, not application-consistent**: it is the state the cluster held, as if the machine had lost
power at that instant.

```sh
atlasctl snapshot-volume vol_native_fs_<id> --name pre-upgrade          # the reply identifies snap_native_fs_<id>
atlasctl lock-snapshot snap_native_fs_<id> --retain-secs 604800          # optional: 7-day retention lock
atlasctl restore-snapshot snap_native_fs_<id> --name restored            # a NEW filesystem; the original is untouched
atlas-native-mount --endpoint ... --token-file ... --fs <new-fs-id> /mnt/restored
atlas-native-mount --endpoint ... --token-file ... --fs <fs-id> --snapshot <snapshot-id> /mnt/peek   # read-only view
```

`atlas-native-mount` takes the native ids (the part after the `vol_native_fs_` / `snap_native_fs_` prefix of the gateway ids).
A lock cannot be shortened or removed and blocks deletion for every caller, admins included; it is software retention
and trusts the node clock ([WORM.md](WORM.md)). Restore creates a new filesystem; there is no in-place rollback. The
command is `snapshot-volume`; there is no `atlasctl create-snapshot`.

### Matrix

| | What the project itself documents | Where Atlas fits |
|---|---|---|
| **pgvector** (Postgres) | pgvector's README: it "uses the write-ahead log (WAL), which allows for replication and point-in-time recovery", so backup is PostgreSQL's. The PostgreSQL manual describes (a) a "consistent snapshot" of the whole data directory, which "will work even while the database server is running" but restarts as a crash with WAL replay, needs the WAL in the same snapshot, and is unsafe if data, WAL or tablespaces sit on different filesystems unless the snapshots are simultaneous; and (b) continuous archiving with `pg_basebackup`, `archive_command`, or the low-level `pg_backup_start`/`pg_backup_stop` API, which "is immune to file system changes during the backup" | B1: archive and base backups on an Atlas filesystem, then snapshot and lock that filesystem. B2: whole `PGDATA` on one Atlas filesystem, snapshot it. B2 is **untested**: no Postgres has been run on an Atlas mount |
| **Qdrant** | Own snapshot API: `POST /collections/{name}/snapshots` makes a tar file of one collection on one node (a distributed cluster needs one per node); restore through `PUT .../snapshots/recover`, upload, or `--snapshot` at start-up, to the same or next minor version only. Whole-storage snapshots (`POST /snapshots`) are for single-node deployments. Snapshots can be stored on the local file system (`snapshots_path`) or in an S3-compatible service (`snapshots_config` with `endpoint_url`). **Live storage "requires block-level access to storage devices with a POSIX-compatible file system"; "Qdrant won't work with Network file systems such as NFS, or Object storage systems such as S3"** | Use Qdrant's snapshots, store the snapshot files on Atlas, then snapshot and lock that filesystem. Do not put Qdrant's live storage on an `atlas-native-mount`, NFS or S3 path: Qdrant's page says network filesystems do not work, and `atlas-native-mount` is a network filesystem client (reading that sentence to cover it is our interpretation; not tested) |
| **LanceDB / Lance** | A Lance table is a directory of data files plus immutable, numbered manifests; "each commit creates a new immutable table version through atomic storage operations", which "rely on atomic write operations provided by the underlying object store": rename-if-not-exists or put-if-not-exists, so that exactly one concurrent writer wins. Local writes are not fsynced unless `local_fsync` is `"true"`. On `s3://`, concurrent writers need the store to enforce `If-None-Match: *` atomically, which Lance does not check | Snapshot a Lance directory on an Atlas mount when no writer is active, or with `local_fsync` on; time travel and tags are Lance's own, a snapshot adds a copy-free point in the filesystem. Concurrent writers over the **S3 gateway are not safe**: Atlas documents its `If-None-Match: *` as not atomic. Concurrent writers over a mount depend on `link()` exclusivity (probe) |
| **Milvus** | Not covered by a filesystem snapshot in anything we read. Milvus has its own tool, [`milvus-backup`](https://github.com/zilliztech/milvus-backup), "a command-line tool and API service for backing up and restoring Milvus data" that runs "while the Milvus cluster remains available" and writes backups to object storage | Use `milvus-backup`. We did not evaluate pointing its backup storage at the Atlas S3 gateway, so no recipe is given |

### pgvector (PostgreSQL) recipes

**B1, archive target on Atlas (follows the PostgreSQL manual, §25.3).** PostgreSQL runs on whatever storage it already
uses. WAL archives and base backups go to an Atlas filesystem mounted at `/mnt/pg-archive`, and the Atlas filesystem
is what gets snapshotted and locked.

```sh
# postgresql.conf (the manual's example command; it refuses to overwrite an existing archive file)
#   wal_level = replica
#   archive_mode = on
#   archive_command = 'test ! -f /mnt/pg-archive/wal/%f && cp %p /mnt/pg-archive/wal/%f'
mkdir -p /mnt/pg-archive/wal /mnt/pg-archive/base
pg_basebackup -D /mnt/pg-archive/base/2026-11-08 -Ft -X stream -P     # self-contained base backup
pg_verifybackup --no-parse-wal /mnt/pg-archive/base/2026-11-08        # tar-format backups: WAL is not parsed
atlasctl snapshot-volume vol_native_fs_<archive-fs> --name pg-2026-11-08
atlasctl lock-snapshot snap_native_fs_<id> --retain-secs 2592000
```

Notes from the manual: the `-X stream` method makes a "completely standalone backup" (a postmaster can start in the
target directory without the WAL archive); `pg_basebackup` writes a backup manifest that `pg_verifybackup` checks (the manual says
WAL verification works only for plain-format backups, so a tar backup needs `--no-parse-wal`); the archive command should
refuse to overwrite an existing archive file. The manual calls its `archive_command` "an example, not a
recommendation". The WAL archive keeps growing; its retention is yours. Restore: `atlasctl restore-snapshot`, mount the
new filesystem, and follow the manual's archive-recovery steps (§25.3.5) with `restore_command` reading from it.
The `pg_basebackup` flags above are the manual's; the combination was **not run** on Atlas.

**B2, `PGDATA` on one Atlas filesystem (follows §25.2).** The manual's conditions, applied: the whole data directory
(including `pg_wal`, and any tablespace) must be on **one** filesystem so that one snapshot captures them together;
you then copy or keep the snapshot, and starting a server on it replays WAL as after a crash. The manual suggests a
`CHECKPOINT` first to shorten recovery.

```sh
psql -c 'CHECKPOINT'
atlasctl snapshot-volume vol_native_fs_<pgdata-fs> --name pgdata-2026-11-08
```

This relies on the snapshot being crash-consistent and on Postgres's own `fsync` discipline reaching the Atlas cluster
through the mount. No Postgres has been run on `atlas-native-mount`, and Atlas's own filesystem tests (pjdfstest, git)
do not cover a database, so treat this as an experiment and restore-test it before depending on it. The low-level API
(`pg_backup_start`, copy, `pg_backup_stop`, keep the returned `backup_label`) is the manual's answer when snapshots
cannot be simultaneous; it needs a connection held open for the duration and WAL archiving, and is not scripted here.

### Qdrant recipe

```sh
curl -X POST "http://qdrant:6333/collections/docs/snapshots"             # synchronous; returns the snapshot name
# config.yaml: storage.snapshots_path points at an Atlas mount (UNVERIFIED on a network mount),
# or storage.snapshots_config selects S3 with endpoint_url at the Atlas gateway (see below)
atlasctl snapshot-volume vol_native_fs_<snapshots-fs> --name qdrant-docs-2026-11-08
atlasctl lock-snapshot snap_native_fs_<id> --retain-secs 2592000
```

Qdrant's page documents `snapshots_storage: s3` with `bucket`, `region`, `access_key`, `secret_key` and
`endpoint_url` ("S3-Compatible Storage URL"). It does not say whether the client uses path-style or virtual-hosted
addressing, so pairing it with the Atlas gateway is **UNVERIFIED**; if it needs virtual-hosted, the gateway domain and
wildcard DNS of Part A apply. The page also advises a separate `temp_path` for "a slow network attached disk". It does
not say whether `snapshots_path` may be a network filesystem, so keep `snapshots_path` local and copy the files if in
doubt. In a cluster, take one snapshot per node. A restore needs the same or next minor Qdrant version.

### Lance recipe

Lance's atomicity comes from the store, so the question for Atlas is which primitive it offers:

- **On a mount.** Lance's local commit handler (non-Windows) is `ConditionalPutCommitHandler`, which calls the Rust
  `object_store` crate's `put_opts` with `PutMode::Create`. That crate's local backend publishes a create-only write by
  staging a file and calling `std::fs::hard_link`, treating `AlreadyExists` as a lost race (`CommitConflict` in Lance).
  We did not confirm that Lance's local path uses that backend in every case. Atlas supports hard links; the probe's
  `link-race` subcommand checks that concurrent `link()` calls onto one name produce exactly one winner. Atlas's FUSE
  client does not support `RENAME_NOREPLACE`/`RENAME_EXCHANGE`, which matters only if a version of Lance switches to
  them.
- **Over S3.** Lance's guide warns that concurrent writes through `s3://` need a store that atomically enforces
  `If-None-Match: *`, that some S3-compatible stores "accept this header but ignore the condition", and that Lance
  "does not check". [NATIVE_S3.md](NATIVE_S3.md) states the gateway checks `If-None-Match: *` before the write, not
  atomically with the rename, so two concurrent creators can both succeed. Use a single writer, or Lance's documented
  custom commit lock, if you put a Lance table in an Atlas bucket. The S3 options Lance documents for a custom endpoint
  are `endpoint`, `region`, `allow_http`, and `virtual_hosted_style_request` (default `false`, which suits the
  gateway's always-available path-style).

Snapshot procedure for a directory on an Atlas mount, with no writer running (or with the dataset opened using
`storage_options={"local_fsync": "true"}`, which makes Lance sync data, transaction and manifest files before a write
returns):

```sh
atlasctl snapshot-volume vol_native_fs_<lance-fs> --name lance-2026-11-08
atlasctl lock-snapshot snap_native_fs_<id> --retain-secs 2592000
```

Because manifests are immutable and numbered, the snapshot contains whichever versions had committed. Lance's own
time travel and tags work on the restored copy; we have not run Lance on Atlas.

### Milvus

Milvus documents its own tool, `milvus-backup`; Atlas has nothing to add that we have verified.

### Not done for Part B

- No vector database was started. No `pg_basebackup`, Qdrant snapshot or Lance write was made on Atlas.
- No restore was tested for any product; only the `link-race` primitive and the S3 probe were run.
- Qdrant on `snapshots_path` over a network mount, Qdrant with the Atlas gateway as `endpoint_url`, and Milvus Backup
  against the gateway are **UNVERIFIED**.
- Retention-locked snapshots protect the snapshot; they say nothing about whether the database inside is restorable.
  Test a restore (`restore-snapshot`, mount, start the product) on a schedule. See also
  [APPLICATION_RECOVERY.md](APPLICATION_RECOVERY.md).

## Sources

All read on 2026-11-08. "Pinned" means the file at that exact commit or tag.

LMCache (latest release v0.5.7; development branch commit `a4a688827def88f1df3e9af5fad43245913fca36`):

- https://docs.lmcache.ai/mp/l2_storage/s3.html, https://docs.lmcache.ai/mp/l2_storage/fs.html,
  https://docs.lmcache.ai/mp/l2_storage/fs_native.html, https://docs.lmcache.ai/mp/l2_storage/index.html
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/lmcache/v1/distributed/l2_adapters/s3_l2_adapter.py
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/lmcache/v1/distributed/l2_adapters/fs_l2_adapter.py
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/lmcache/v1/distributed/l2_adapters/fs_native_l2_adapter.py
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/csrc/storage_backends/fs/connector.cpp
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/lmcache/v1/distributed/key_codec.py
- https://github.com/LMCache/LMCache/tree/v0.5.7/docs/source/kv_cache/storage_backends (in-process `fs.rst`, `s3.rst`) and
  https://github.com/LMCache/LMCache/tree/v0.5.7/lmcache/inprocess/v1/storage_backend/connector (`s3_connector.py`,
  `fs_connector.py`, `s3_adapter.py`)
- https://github.com/LMCache/LMCache/blob/a4a688827def88f1df3e9af5fad43245913fca36/docs/source/mp/l2_storage/plugin.rst
- AWS CRT: https://github.com/awslabs/aws-c-s3/blob/main/README.md,
  https://github.com/awslabs/aws-c-s3/blob/main/docs/GetObject.md,
  https://github.com/awslabs/aws-crt-python/blob/main/awscrt/s3.py

PostgreSQL 18 manual:

- https://www.postgresql.org/docs/current/backup-file.html (§25.2)
- https://www.postgresql.org/docs/current/continuous-archiving.html (§25.3, including §25.3.4 low-level API)
- https://www.postgresql.org/docs/current/app-pgbasebackup.html
- https://www.postgresql.org/docs/current/functions-admin.html (backup control functions)
- https://github.com/pgvector/pgvector/blob/724588a5504b0e9c462e3126563370e8d50a01eb/README.md ("Is replication supported?")

Qdrant:

- https://qdrant.tech/documentation/snapshots/ (snapshots, restore, S3 storage; the page is also served at
  `/documentation/operations/snapshots/`)
- https://qdrant.tech/documentation/installation/ (storage requirements)

Lance and LanceDB:

- https://github.com/lance-format/lance/blob/3bb1550f4f27adf90948ff36c2318768e675b599/docs/src/format/table/transaction.md
- https://github.com/lance-format/lance/blob/3bb1550f4f27adf90948ff36c2318768e675b599/docs/src/format/table/layout.md
- https://github.com/lance-format/lance/blob/3bb1550f4f27adf90948ff36c2318768e675b599/docs/src/guide/object_store.md
- https://github.com/lance-format/lance/blob/3bb1550f4f27adf90948ff36c2318768e675b599/docs/src/quickstart/versioning.md
- https://github.com/lance-format/lance/blob/3bb1550f4f27adf90948ff36c2318768e675b599/rust/lance-table/src/io/commit.rs
- https://github.com/apache/arrow-rs-object-store/blob/v0.14.2/src/local.rs (the `object_store` version Lance pins)
- https://docs.lancedb.com/tables/versioning (LanceDB: a table "keeps a history of committed versions")

Milvus:

- https://github.com/zilliztech/milvus-backup/blob/0ef2802f44ee6aff26c403b45651d835d3291703/README.md
- https://milvus.io/docs/milvus_backup_overview.md (the page names Milvus Backup as the backup tool; it does not discuss
  filesystem snapshots)
