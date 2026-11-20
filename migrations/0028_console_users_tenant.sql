-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Console users are now tenant-scoped for read isolation: a viewer/operator only sees their own
-- tenant's volumes/buckets on list/get endpoints; admin remains cross-tenant regardless of this
-- column's value (see require_tenant/tenant_scope in crates/atlas-gateway/src/auth.rs).
ALTER TABLE console_users ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'global';
