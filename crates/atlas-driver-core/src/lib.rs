// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The pluggable storage-driver contract (PDF §17.2) plus the driver error type and a registry.
//!
//! A `StorageDriver` abstracts one storage backend (Ceph today; NFS/ZFS/SAN/cloud later). The
//! gateway and discovery worker depend only on this trait, never on backend internals.

use async_trait::async_trait;
use atlas_api_types::{
    CloneSnapshotRequest, CreateSnapshotRequest, CreateSnapshotResult, CreateVolumeRequest,
    CreateVolumeResult, DeleteSnapshotRequest, DeleteVolumeRequest, DiscoveryResult,
    ExpandVolumeRequest, MetricSample, StorageHealth, StoragePool, StorageVolume,
};

pub mod registry;
mod tls;
pub use registry::DriverRegistry;
pub use tls::trusted_http_client;

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The backend was reachable but returned an error (non-zero exit, API error).
    #[error("backend error: {0}")]
    Backend(String),
    /// The backend could not be reached (network, auth, missing binary).
    #[error("backend unreachable: {0}")]
    Unreachable(String),
    /// Failed to parse the backend's response.
    #[error("parse error: {0}")]
    Parse(String),
    /// This operation is not implemented in the current MVP slice.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

/// A backend storage driver. Read-only methods are live in MVP slice 1; write-path methods
/// return `NotImplemented` until slice 2.
#[async_trait]
pub trait StorageDriver: Send + Sync {
    /// Stable identifier for the backend this driver instance manages.
    fn backend_id(&self) -> &str;

    /// True for a driver backed by static fixtures rather than a real rescan of the backend.
    /// Discovery uses this to skip pruning inventory rows the driver's `discover()` didn't
    /// report: a fixture's volume list is a fixed snapshot, not the authoritative current
    /// state, so an image this pass didn't mention (e.g. one created directly via a job,
    /// bypassing the fixture) must not be treated as deleted.
    fn is_fixture(&self) -> bool {
        false
    }

    /// Full discovery pass: cluster + pools + osds + volumes + health.
    async fn discover(&self) -> Result<DiscoveryResult, DriverError>;

    async fn health(&self) -> Result<StorageHealth, DriverError>;
    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError>;
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError>;
    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError>;

    // ---- write path (MVP slice 2) ----
    async fn create_volume(
        &self,
        _req: CreateVolumeRequest,
    ) -> Result<CreateVolumeResult, DriverError> {
        Err(DriverError::NotImplemented("create_volume"))
    }
    async fn expand_volume(&self, _req: ExpandVolumeRequest) -> Result<(), DriverError> {
        Err(DriverError::NotImplemented("expand_volume"))
    }
    async fn delete_volume(&self, _req: DeleteVolumeRequest) -> Result<(), DriverError> {
        Err(DriverError::NotImplemented("delete_volume"))
    }
    async fn create_snapshot(
        &self,
        _req: CreateSnapshotRequest,
    ) -> Result<CreateSnapshotResult, DriverError> {
        Err(DriverError::NotImplemented("create_snapshot"))
    }
    async fn clone_snapshot(
        &self,
        _req: CloneSnapshotRequest,
    ) -> Result<CreateVolumeResult, DriverError> {
        Err(DriverError::NotImplemented("clone_snapshot"))
    }
    async fn delete_snapshot(&self, _req: DeleteSnapshotRequest) -> Result<(), DriverError> {
        Err(DriverError::NotImplemented("delete_snapshot"))
    }
    /// Recent activity windows (oldest first) of a filesystem volume, for ransomware assessment.
    /// Each is a JSON object with `bytes_written`, `files_modified`, `files_renamed`,
    /// `files_deleted` and `high_entropy_ratio`.
    async fn filesystem_activity(
        &self,
        _native_volume_id: &str,
    ) -> Result<Vec<serde_json::Value>, DriverError> {
        Err(DriverError::NotImplemented("filesystem_activity"))
    }
    /// Retention-locks (WORM) a snapshot for `retain_secs` from now and returns the Unix second it
    /// unlocks at. A lock can be extended but never shortened; delete is refused until it expires.
    async fn lock_snapshot(
        &self,
        _native_snapshot_id: &str,
        _retain_secs: u64,
    ) -> Result<u64, DriverError> {
        Err(DriverError::NotImplemented("lock_snapshot"))
    }

    // ---- block data path (backends whose volumes the gateway can read/write directly) ----
    /// `len` bytes at `offset` of a block volume; never-written bytes read as zeros.
    async fn read_volume(
        &self,
        _volume_id: &str,
        _offset: u64,
        _len: u64,
    ) -> Result<Vec<u8>, DriverError> {
        Err(DriverError::NotImplemented("read_volume"))
    }
    async fn write_volume(
        &self,
        _volume_id: &str,
        _offset: u64,
        _data: Vec<u8>,
    ) -> Result<(), DriverError> {
        Err(DriverError::NotImplemented("write_volume"))
    }

    // ---- Ceph-native introspection (read-only; other backends return NotImplemented) ----
    /// `ceph status -f json` — cluster health, mon quorum, mgr, osdmap, pgmap, client I/O.
    async fn ceph_status(&self) -> Result<serde_json::Value, DriverError> {
        Err(DriverError::NotImplemented("ceph_status"))
    }
    /// `ceph osd tree -f json` — the CRUSH hierarchy (roots → hosts → OSDs).
    async fn ceph_osd_tree(&self) -> Result<serde_json::Value, DriverError> {
        Err(DriverError::NotImplemented("ceph_osd_tree"))
    }
    /// `ceph df detail -f json` — cluster + per-pool capacity/usage/objects.
    async fn ceph_df(&self) -> Result<serde_json::Value, DriverError> {
        Err(DriverError::NotImplemented("ceph_df"))
    }
    /// `ceph osd df -f json` — per-OSD utilization (size/used/avail/%, PG count).
    async fn ceph_osd_df(&self) -> Result<serde_json::Value, DriverError> {
        Err(DriverError::NotImplemented("ceph_osd_df"))
    }
}
