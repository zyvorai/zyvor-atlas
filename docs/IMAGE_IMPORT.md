<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Importing a raw disk image into an atlas-native volume

`POST /api/atlas/v1/volumes/{id}/import` queues a job that streams a **raw** disk image from an HTTP(S)
server into an existing atlas-native volume, then proves the volume holds it. It is the Atlas end of a
VMware exit: Scout scores the estate, h2kvm repairs and converts the guest, and this job lands the disk.

What it does not do is read qcow2, VMDK or any other container format, or write to Ceph RBD, NFS or ZFS
volumes. See [Limits](#limits).

## Turn it on

The gateway fetches whatever host the request names, so the feature is **off** until you list the hosts it
may fetch from:

```bash
ATLAS_IMPORT_ALLOWED_HOSTS=images.internal,10.0.0.5   # comma-separated names or IP literals
```

A request for any other host is refused with `403`. URLs with embedded credentials are refused, because the
job record stores the URL; serve the image from a host that does not need them, and pin it by checksum.
Redirects are never followed, so an allowed host cannot send the gateway somewhere else.

## Request

```bash
curl -X POST "$ATLAS/api/atlas/v1/volumes/$VOLUME/import" \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{
        "url": "http://10.0.0.5:8099/web-prod-01.raw",
        "sha256": "<64 hex characters, the SHA-256 of the whole image>",
        "size_bytes": 42949672960
      }'
```

| Field | Meaning |
|---|---|
| `url` | `http` or `https` URL of the raw image. |
| `sha256` | SHA-256 of the whole image. Required: the import succeeds only if the data matches. |
| `size_bytes` | Optional image length, checked against what the server sends and against the volume size. |
| `resume_from_job` | Optional id of an earlier, finished import job for the same volume whose progress this one continues. |

The volume must already exist, be an atlas-native volume, and be large enough. The caller needs the operator
role and access to the volume's tenant, the same as writing block data. The response is `202` with a job id;
progress, the result and cancel are the ordinary job API (`GET /jobs/{id}`, `POST /jobs/{id}/cancel`).

## How it works

1. The image is read in 4 MiB blocks. Each block is hashed and, unless it is all zeros, written to the volume.
   A volume reads never-written bytes as zeros, so a sparse image stays sparse and a mostly empty 40 GiB disk
   transfers quickly. The count of skipped bytes is in the result.
2. Every 64 MiB the job records `bytes_done` and the percentage on its row.
3. When the stream ends, its SHA-256 is compared with the one you gave. A mismatch fails the job.
4. The volume is then **read back and hashed**, and that hash must match too. This is the check that the bytes
   on the volume, not only the bytes that crossed the wire, are the image.

The result of a successful job:

```json
{ "volume_id": "vol_native_...", "bytes": 42949672960, "sha256": "...", "verified": true,
  "zero_bytes_skipped": 31457280000, "resumed_from": 0 }
```

## Resuming

A job that fails part-way is retried (five times by default, `ATLAS_IMPORT_MAX_RETRIES`), and each retry
continues from the recorded `bytes_done` instead of starting over. To start a new job after the retries run
out, or after a gateway restart interrupted one, pass the old job as `resume_from_job`.

On resume the bytes already on the volume are read back and hashed again before new data is written, so a
volume that was damaged in the meantime, or a source that changed, fails the final checksum instead of
producing a silently wrong disk. If the server does not support `Range` requests, it is streamed from the
start and the part already written is skipped.

A checksum mismatch throws the recorded progress away, because some of what was written may be wrong; the next
attempt starts from zero.

## Limits

- **Raw images only.** There is no qcow2 reader in the gateway. Convert first (`qemu-img convert -O raw`), or
  have the converter emit raw.
- **atlas-native volumes only.** Ceph RBD, NFS and ZFS volumes are not supported; RBD would need a job pod with
  the Ceph tools.
- **It runs on the job worker.** Atlas has one worker, so a long import holds back every other job until it
  finishes. The job timeout for imports is a day (`ATLAS_IMPORT_TIMEOUT_SECS`) instead of two hours. A
  dedicated import worker is the fix and is not built.
- **A failed import leaves the volume as it is.** That is what makes resuming possible. Delete the volume to
  discard the partial data. Nothing stops two imports into the same volume at once.
- **Single stream.** One HTTP connection, in order. There is no parallel or ranged transfer.
- **The host check is by name.** It does not defend against a name that is later pointed at another address.
  Use IP literals or hosts you control.

## Testing

`cargo test -p atlas-jobs image_import` covers the streaming algorithm (sparse blocks, resume, damaged resume,
both checksum failures, a short or oversized image, progress marks) and the HTTP source (range and non-range
servers, refused redirects). `cargo test -p atlas-gateway --test image_import` runs the route and the job end to
end against the fixture native driver and a local HTTP server. It has not been run against a real atlas-native
cluster or a multi-gigabyte image.
