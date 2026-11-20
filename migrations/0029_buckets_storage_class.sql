-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Track which StorageClass an OBC bucket was provisioned against, so a Rook CephObjectStore
-- delete (DELETE /ceph/object-stores/{name}) can guard against buckets that still reference it —
-- mirrors storage_volumes.storage_class_name's existing role in the pool/filesystem delete guards.
ALTER TABLE storage_buckets ADD COLUMN storage_class TEXT;
