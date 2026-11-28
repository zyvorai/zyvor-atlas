<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native S3 gateway

The S3 gateway serves atlas-native filesystems ([`NATIVE_FS.md`](NATIVE_FS.md)) over the S3 API.
A bucket is one filesystem, or a snapshot of one; an object is a file. The objects are the same
files that FUSE mounts, the CSI driver and the NFS gateway ([`NATIVE_NFS.md`](NATIVE_NFS.md)) see.
Write `logs/2026/app.log` through S3 and it is the file `logs/2026/app.log` on every mount, and
the reverse.

`atlas-native-s3` (`crates/atlas-native-s3`) is a single binary built on
[s3s](https://github.com/s3s-project/s3s) (Apache-2.0). It talks to the cluster over its HTTP API
with the same client layer as the FUSE mount, so it needs no FUSE device and no privileges. The
image is `Dockerfile.native-s3`.

## Install

Create the filesystems first (`POST /v1/fs`, `atlasctl create-volume --kind filesystem --backend
bkd_native`, or a CSI PVC, whose PV `volumeHandle` is the filesystem id). Put the access keys in a
Secret, then enable the gateway in the `atlas-native` chart:

```sh
cat > credentials.json <<'EOF'
[
  {"access_key": "team-admin", "secret_key": "<at least 16 bytes>"},
  {"access_key": "team-reader", "secret_key": "<...>", "buckets": ["team-share"], "read_only": true}
]
EOF
kubectl -n atlas-native create secret generic atlas-s3-keys --from-file=credentials.json
```

```yaml
s3:
  enabled: true
  replicas: 2
  credentials:
    existingSecret: atlas-s3-keys
  buckets:
    - bucket: team-share
      fs: team-share
    - bucket: team-share-monday      # a snapshot, by id: always read-only
      fs: team-share
      snapshot: monday
    - bucket: team-share-ro          # a live filesystem served read-only
      fs: team-share
      read_only: true
  # tls: { existingSecret: atlas-s3-tls }   # tls.crt / tls.key: serve HTTPS
  # domains: [s3.example.com]               # virtual-hosted style: <bucket>.s3.example.com
```

```sh
aws --endpoint-url http://<service>:9000 s3 cp model.bin s3://team-share/models/model.bin
```

The chart adds a ConfigMap with the buckets, a Deployment of `s3.replicas` pods (non-root,
read-only root filesystem, all capabilities dropped) and a Service on port 9000. The gateway reaches
the storage nodes with the release's API token; with `httpTls.enabled` it trusts the Secret's
`ca.crt`. `httpTls.requireClientCert` is not supported by the chart yet; the binary itself takes
`--identity-file`. `s3.credentials.keys` takes the same list inline and renders it into a Secret,
so the secrets end up in the Helm release; prefer `existingSecret`.

Every request's state, multipart uploads included, lives in the filesystem, so any replica can
serve any request and the Deployment rolls like any stateless service. A bucket list or key file
the gateway can't use (a duplicate bucket, an unknown filesystem, a key granted a bucket that isn't
configured) stops the new pod at startup, and the rollout keeps the old pods serving.

## Mapping

| S3 | Filesystem |
|---|---|
| Bucket | One filesystem, or `fs@snapshot` (read-only) |
| Object `a/b/c` | File `c` in directory `a/b`; missing directories are created (mode 0755) |
| Key ending in `/` (`a/b/`) | Directory marker: the directory `a/b`, listed as an empty object |
| ETag, `Content-Type`, `Cache-Control`, `Content-Encoding`, `Content-Disposition`, `Content-Language`, `x-amz-meta-*` | Extended attribute `user.atlas.s3` on the file |
| Object tags | Extended attribute `user.atlas.s3.tags` on the file |
| In-progress multipart upload | Directory `.atlas_s3_uploads/<id>` (mode 0700), one file per part |
| Bucket creation date | Stamped on the root (`user.atlas.s3.created`) the first time a gateway serves it writable |

- **Atomic writes:** a PUT writes a hidden temporary file next to the object and renames it into
  place. S3, NFS and FUSE readers see the old object or the new one, never part of one. A failed or
  interrupted upload (bad checksum, quota, client gone) removes its temporary file.
- **Files written by other protocols** are objects too. Their ETag is derived from the inode, size
  and mtime (not an MD5) and changes whenever the file does. The gateway's own ETag (the MD5, or
  `<md5 of part MD5s>-<parts>` for multipart) is kept only while the file's size and mtime still
  match what the gateway recorded, so a file changed through NFS or a mount never carries a stale
  ETag.
- **Deletes prune:** deleting the last object under `a/b/` removes the now-empty directories `a/b`
  and `a`, up to the first one that still has entries or is a directory marker.
- **Listings** are in S3 key order (UTF-8 bytes; a directory sorts as its name plus `/`), with
  `prefix`, any `delimiter`, `start-after`/`marker`, continuation tokens, `max-keys` (up to 1000)
  and `encoding-type=url`. A page walks only the directories the prefix and resume point need.
  Names starting with `.atlas_` are hidden.
- **Read-only buckets** (`read_only: true`, or any snapshot) refuse every write with
  `AccessDenied`.

## Supported operations

- **Objects:** `PutObject` (`Content-MD5`; CRC32/CRC32C/CRC64NVME/SHA-1/SHA-256 checksums,
  including streaming trailers), `GetObject` (ranges), `HeadObject`, both with
  `If-Match`, `If-None-Match`, `If-Modified-Since` and `If-Unmodified-Since`; `DeleteObject`,
  `DeleteObjects` (up to 1000 keys, quiet mode), `CopyObject` (`COPY` or `REPLACE` metadata and
  tags, within or across buckets, `x-amz-copy-source-if-*`).
- **Conditional writes:** `If-None-Match: *` and `If-Match: <etag>` on `PutObject`, `CopyObject`
  and `CompleteMultipartUpload` ([below](#conditional-writes)).
- **Object tagging:** `PutObjectTagging`, `GetObjectTagging`, `DeleteObjectTagging`, and
  `x-amz-tagging` on `PutObject`, `CopyObject` and `CreateMultipartUpload`
  ([below](#object-tagging)).
- **Multipart:** `CreateMultipartUpload`, `UploadPart`, `UploadPartCopy` (with ranges),
  `ListParts`, `ListMultipartUploads`, `CompleteMultipartUpload`, `AbortMultipartUpload`. Parts are
  numbered 1–10000; every part but the last must be at least 5 MiB.
- **Buckets:** `ListBuckets` (only the buckets the key may use), `HeadBucket`,
  `GetBucketLocation`, `ListObjects` and `ListObjectsV2`.
- **Object retention:** `PutObjectRetention` / `GetObjectRetention`, `Mode=COMPLIANCE` only.
  While the retention date is in the future the object cannot be deleted (`DeleteObject`,
  `DeleteObjects`) or overwritten (`PutObject`, `CopyObject`, multipart completion): `403
  AccessDenied`. A date can be extended, never shortened, and no S3 call removes it. GOVERNANCE is
  refused. The date lives in the object's xattr `user.atlas.s3.retain`. A write checks it again as
  it is published, in the same replicated step as the rename, so no write through any gateway
  replaces an object after its lock was confirmed. Deletes and concurrent retention changes are not
  atomic in that way (see [Limits](#limits)). Tags are not object data: a locked object can be
  tagged, as in AWS. See [`WORM.md`](WORM.md).
- **Auth:** AWS Signature V4 (headers, presigned URLs and streaming payloads), checked by s3s
  against the configured keys; s3s also accepts Signature V2 (untested here). Unsigned requests
  are refused.

Everything else (bucket creation and deletion, versioning, bucket-level object lock configuration, legal hold, lifecycle rules, bucket
policies and ACLs, bucket tagging, conditional deletes, server-side encryption, notifications, website
hosting) returns `NotImplemented`. Buckets are added and removed in the gateway's bucket list, not
over S3.

## Conditional writes

| Request | Condition holds when | Otherwise |
|---|---|---|
| `If-None-Match: *` | There is no object at the key | `412 PreconditionFailed` |
| `If-Match: "<etag>"` | The object's ETag is `<etag>` (quotes optional; a weak ETag never matches) | `412 PreconditionFailed`; `404 NoSuchKey` if there is no object |

Any other form (`If-Match: *`, `If-None-Match` with an ETag, both headers, a conditional copy of an
object onto itself, `If-Match` and the like on `DeleteObject` or in a `DeleteObjects` entry) is
`501 NotImplemented`: no condition is ever ignored.

The check and the publish are one step, whichever gateway replica handles each writer. The gateway
judges the object, then asks the cluster to rename its temporary file over the key only if the key
is still what was judged (absent, or the same inode with the same size, modification time and
recorded metadata) and not retention-locked. The metadata leader applies that as one Raft entry
(`POST /v1/fs/{fs}/rename-if`). If the key changed in between, the gateway judges again; so of any
number of concurrent `If-None-Match: *` writers exactly one succeeds, and a read-modify-write loop
with `If-Match` loses no update. A key that keeps changing for 16 attempts gets
`409 ConditionalRequestConflict`.

- A condition that fails before the body is read (PUT) or the parts are copied (multipart) gets the
  status above. A multipart completion that loses the race afterwards reports the error inside its
  `200` response, as any late completion error does, and keeps the upload.
- Directory markers (`a/b/`) are conditional through the marker attribute's create or replace.
- If a write's response is lost, the gateway's retry of the rename is recognised. If the key was
  changed again in between, the outcome can't be told and the request fails with `500`.
- Writes through mounts or NFS are not conditional, but a conditional S3 write sees their effect:
  a changed size or modification time changes the ETag.

**Upgrading.** The compare-and-swap is a new command in the metadata log.

| Cluster | Conditional writes | Other writes |
|---|---|---|
| Leader from an older release | `501 NotImplemented` | As before |
| New leader, some metadata node older (or, before first use, not heard from since the election) | `503 ServiceUnavailable` | As before; the retention check is not atomic with the rename |
| Every metadata node upgraded | Work | Check retention atomically |

Once the cluster has applied one such rename, which the first S3 write after the upgrade does, a new
leader no longer waits to hear from a metadata node that is down. **From then on do not downgrade
a metadata node, or add an older one**: older releases cannot decode the command. (Going back would
need the gateways downgraded first and the Raft log compacted past the last such entry; Atlas does
not enforce this and it has not been tested.) Upgrade **every gateway replica** before
relying on conditions: an older gateway checks `If-None-Match: *` before the write only, and ignores
conditions on copies, multipart completions and deletes.

## Object tagging

Up to 10 tags per object; keys of 1–128 characters, unique and case sensitive; values of up to 256
characters; letters, numbers, spaces and `_ . : / = + - @`; keys starting with `aws:` are refused.
A violation is `400 InvalidTag` and changes nothing.

- `x-amz-tagging` (URL encoded, `k1=v1&k2=v2`) on `PutObject` and `CreateMultipartUpload` is
  published with the object, atomically. An overwrite replaces the tags with its own (none if it
  has no header).
- `CopyObject` keeps the source's tags unless `x-amz-tagging-directive: REPLACE`.
- Changing tags changes neither the ETag nor the modification time, and never fails an `If-Match`.
- Tags live in the xattr `user.atlas.s3.tags`: they follow the file through renames, snapshots and
  replication, and a mount can read or change them (a value that isn't a tag list reads as no tags).
- Not provided: the `x-amz-tagging-count` header, `versionId` (refused unless `null`), bucket
  tagging, and anything that acts on tags (lifecycle rules, policies).

## Access control

- Each access key may use every bucket, or only the ones in its `buckets` list, and may be
  `read_only`. Anything else is `AccessDenied`; an unknown bucket is `NoSuchBucket`.
- **S3 keys bypass POSIX permissions.** The gateway acts with full access to the filesystems it
  serves; mode bits and POSIX ACLs on files apply to mounts and NFS, not to S3 requests. Files and
  directories it creates are owned by `s3.uid`/`s3.gid` (default root) with modes 0644 and 0755.
  Grant a key a filesystem only if its holder may read and change everything in it.
- Secret keys never appear in logs, errors or API responses; a malformed key file is reported by
  line number only.

## Errors that come from being a filesystem

| Situation | Response |
|---|---|
| `a/b` exists as a file and a PUT targets `a/b/c` (or the reverse: `a/b` is a directory) | 409 `ObjectConflict` |
| Key with an empty segment (`a//b`), a `.` or `..` segment, a segment over 255 bytes, NUL, or a segment starting with `.atlas_`; key over 1024 bytes | 400 `InvalidArgument` |
| Filesystem quota exhausted | 507 `QuotaExceeded` (the partial object is removed) |
| Body sent with a key ending in `/` | 400 `InvalidArgument` |

## Limits

- **Completing a multipart upload copies the parts** into the final object, so every byte is
  written twice and completion takes time in proportion to the object's size. s3s keeps the
  connection alive with whitespace until it's done (the response is a 200 whose body reports
  success or the error, as S3 does). A completion keeps running if the client disconnects.
  Parts count against the filesystem's quota until the upload completes or is aborted.
- **Abandoned multipart uploads stay** until aborted; there are no lifecycle rules to expire them.
  `ListMultipartUploads` finds them.
- Retention is enforced atomically against writes only. A `DeleteObject` racing the
  `PutObjectRetention` that locks its object can still delete it, and two concurrent
  `PutObjectRetention` calls can leave the earlier date. A directory marker's lock is likewise
  checked before, not with, the change to the marker.
- A `GET` or `HEAD` that races an overwrite of the same key can fail (`NoSuchKey`, or a broken
  response body): the replaced file is removed, not kept open for readers. Retry it.
- Copying an object onto itself rewrites its headers and tags in place, in separate steps.
- One bucket per filesystem; objects can't be larger than the filesystem allows, and a key's
  directories are real directories (see the table above).
- Virtual-hosted-style addressing (`s3.domains`, `--domain` on the binary) is implemented. The lab runs used
  path-style requests, except one on 2026-11-08: `deploy/native/s3-pattern-probe.py` sent `PUT`, `HEAD`, ranged `GET`,
  `ListObjectsV2`, multipart and `DELETE` with `Host: <bucket>.<domain>:<port>` to a plain-process gateway started with
  `--domain`, and all passed ([AI_STORAGE_RECIPES.md](AI_STORAGE_RECIPES.md)). Not verified: the Helm chart's `s3.domains`,
  wildcard DNS, HTTPS, or an AWS SDK or CRT client in this mode.
- Throughput is bounded by the gateway pod's cluster client: reads are fetched in 4 MiB chunks and
  writes go through the client's write-back path, as for a FUSE mount.

## Verification

Integration tests (`crates/atlas-native-s3/tests/s3.rs`) run the gateway against an in-process
3-metadata, 3-data-node cluster: signed and unsigned access, per-key grants, a 12 MiB multipart
upload, listing order and paging across delimiters and prefixes, headers, ranges, copies,
conditional requests, conflicts and refused keys, directory pruning, files written through the
filesystem client served as objects, and read-only buckets. For conditional writes: two gateways
with their own cluster sessions racing creates (one winner per key), an `If-Match` counter with no
lost increment and racing multipart completions; retention locks against concurrent writers; the
behaviour against a cluster that refuses the compare-and-swap (through a proxy standing in for an
older node, not an older binary); and tagging with its limits. The metadata capability gate and the
retry of an applied rename are tested in `crates/atlas-native/tests/conditional_rename.rs`.

Verified on the lab k3s (single node, one data node on the host's shared HDD) through the Helm
chart with two gateway replicas and aws-cli 2.37:

- A 200 MiB multipart upload (aws-cli's default 8 MiB parts) downloaded with the same SHA-256; the
  ETag carries the part count. `aws s3 sync` of a tree, then recursive, delimited and paged
  listings; headers and user metadata, ranges, server-side copies, `ObjectConflict`, 404s.
- A read-only key reads but can't write, sees only its bucket, and gets `AccessDenied` elsewhere;
  a wrong secret gets `SignatureDoesNotMatch`; a read-only bucket refuses writes and serves the same
  files. Presigned GET URLs work and stop working once altered.
- A 100 MiB upload into a filesystem with a 64 MiB quota fails with `QuotaExceeded`, leaving no
  object and no upload behind.
- One multipart upload created on one replica, parts uploaded to both, and listed and completed on
  the other, reads back intact.
- A snapshot bucket lists the snapshot's objects, refuses writes, and still serves an object
  deleted from the live bucket afterwards.
- Conditional requests: `If-None-Match` with the current ETag gets 304, a wrong `If-Match` 412, a
  wrong `x-amz-copy-source-if-match` 412.
- With `s3.tls` (a self-signed certificate for the Service name, trusted through
  `AWS_CA_BUNDLE`), a 20 MiB upload and download round-trip over HTTPS; plain HTTP to the port is
  refused.
- `aws s3 rm --recursive` removes the emptied directories; bucket creation dates stay fixed across
  writes; neither secret key nor any request signature appears in the gateway logs.

Throughput on this lab is bound by the shared HDD (the storage engine alone writes 4–6 MiB/s
there, see `NATIVE_FS.md`), so these numbers measure the disk, not the gateway: 200 MiB uploaded
in 32–71 s and downloaded in 1.5–5.4 s across two runs; a 64 MiB single PUT took 4.2 s, the same
64 MiB as a multipart upload 8.0 s (completion writes it again). There are no NVMe numbers yet.
