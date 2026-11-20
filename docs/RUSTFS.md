<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# RustFS — removed

The third-party **RustFS** product (`rustfs/rustfs`) is no longer a first-party
Atlas backend.

## What changed

- Crate `atlas-driver-rustfs` deleted.
- Console **Storage → RustFS**, admin proxy, drive/instance jobs, lab chart,
  and `deploy/rustfs-lab` removed.
- `POST /buckets` defaults to Ceph RGW (`bkd_ceph_lab`) via Rook
  `ObjectBucketClaim`.
- `backend_id: "bkd_rustfs_lab"` is rejected.
- Persisted `JobSpec` variants `bucket.create.rustfs` /
  `bucket.delete.rustfs` / `rustfs.drive.provision` / `rustfs.instance`
  still deserialize so old job rows do not poison the queue; dispatch
  returns a retirement error.

## What to use instead

| Need | Use |
| --- | --- |
| New buckets | omit `backend_id` or pass `"bkd_ceph_lab"` |
| Presigned upload/download | existing `/buckets/{id}/objects/*` on RGW |
| BYO S3 (MinIO, Garage, customer RustFS, AWS) | `atlas-driver-rgw::S3Target` + endpoint/credentials Secret |
| DataBridge object copy | external S3 endpoint or Ceph RGW backend id |
| State backup | `ATLAS_S3_ENDPOINT` / `ATLAS_RGW_PUBLIC_ENDPOINT` |

`BackendType::Rustfs` remains on the wire so existing inventory rows
deserialize. Do not register new backends of that type.

`ATLAS_RUSTFS_*` environment variables are ignored (a warning is logged
if `ATLAS_RUSTFS_ENABLE=1`).
