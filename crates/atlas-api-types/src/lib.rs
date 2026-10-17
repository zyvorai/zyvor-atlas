// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Shared serde DTOs for the Atlas storage control plane.
//!
//! These types are the stable wire/domain contract between the gateway, the drivers, and the
//! inventory read model. They intentionally model a *backend-agnostic* storage resource (PDF §3.3):
//! a volume may be backed by Ceph RBD today and SAN/NFS/ZFS/cloud tomorrow.

use serde::{Deserialize, Serialize};

/// Backend technology kind. Ceph is the first driver; the rest are placeholders for future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    Ceph,
    Nfs,
    Zfs,
    Longhorn,
    /// Deprecated: the first-party RustFS product driver was removed. Kept so existing
    /// inventory rows and API payloads with `backend_type: "rustfs"` still deserialize.
    /// New object backends use Ceph RGW (`Ceph`) or a generic S3 endpoint via `atlas-driver-rgw`.
    Rustfs,
    San,
    CloudBlock,
    /// Kubernetes CSI / StorageClass view (not a physical backend of its own).
    Kubernetes,
    /// An atlas-native-node cluster (`atlas-driver-native`).
    Native,
    /// A WEKA cluster, discovered read-only (`atlas-driver-weka`).
    Weka,
    /// The disks of a Mac, internal and external (`atlas-driver-macos`).
    Macos,
}

/// How a backend is operated (PDF §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    ManagedRook,
    External,
    ReadOnly,
}

/// Normalized health for clusters/pools/volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Warn,
    Critical,
    #[default]
    Unknown,
}

/// Atlas's own operator-facing severity rollup, synthesized from raw Ceph status/osd-tree/osd-df
/// (see `atlas_driver_ceph::health_rollup`) and reused for the per-volume Protection Status
/// verdict (`atlas_inventory::protection`). This is a heuristic collapse for UX, not a Ceph API
/// concept — the ordering below (used for "worst signal wins" comparisons) is a judgment call,
/// not a Ceph-defined invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealthState {
    #[default]
    Healthy,
    Degraded,
    Rebuilding,
    AtRisk,
    Critical,
}

/// The logical kind of a storage volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKind {
    Block,
    Filesystem,
    Object,
}

/// A storage backend registration (row in `storage_backends`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackend {
    pub id: String,
    pub name: String,
    pub backend_type: BackendType,
    pub mode: BackendMode,
    pub status: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Secret *reference* only — never a raw keyring/secret (PDF §14.1).
    pub connection_ref: Option<String>,
    /// Set via `POST /backends/{id}/cordon`; a cordoned backend rejects new provisioning.
    #[serde(default)]
    pub cordoned: bool,
}

/// Backend capability flags (PDF §12.1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub block: bool,
    pub file: bool,
    pub object: bool,
    pub snapshots: bool,
    pub clone: bool,
    pub expansion: bool,
    pub replication: bool,
}

/// A discovered storage cluster (Ceph cluster or managed storage cluster).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCluster {
    pub id: String,
    pub backend_id: String,
    pub name: String,
    pub native_fsid: Option<String>,
    #[serde(default)]
    pub health: Health,
    pub raw_capacity_bytes: Option<i64>,
    pub used_capacity_bytes: Option<i64>,
    pub available_capacity_bytes: Option<i64>,
}

/// A normalized storage pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePool {
    pub id: String,
    pub cluster_id: String,
    pub name: String,
    /// rbd, cephfs_data, cephfs_metadata, rgw, other.
    pub kind: String,
    pub device_class: Option<String>,
    pub replica_size: Option<i64>,
    pub used_bytes: Option<i64>,
    pub max_bytes: Option<i64>,
    #[serde(default)]
    pub health: Health,
}

/// A Zyvor storage volume abstraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageVolume {
    pub id: String,
    pub cluster_id: Option<String>,
    pub pool_id: Option<String>,
    pub name: String,
    pub kind: VolumeKind,
    pub backend_native_id: Option<String>,
    pub size_bytes: i64,
    pub used_bytes: Option<i64>,
    pub state: String,
    #[serde(default)]
    pub health: Health,
    pub kubernetes_namespace: Option<String>,
    pub pvc_name: Option<String>,
    pub storage_class_name: Option<String>,
}

/// An OSD entry (Ceph object storage daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Osd {
    pub id: i64,
    pub cluster_id: String,
    pub up: bool,
    pub in_cluster: bool,
    pub device_class: Option<String>,
    pub host: Option<String>,
    pub used_bytes: Option<i64>,
    pub capacity_bytes: Option<i64>,
}

/// Normalized cluster health snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealth {
    #[serde(default)]
    pub status: Health,
    pub summary: String,
    pub raw_capacity_bytes: Option<i64>,
    pub used_capacity_bytes: Option<i64>,
    pub available_capacity_bytes: Option<i64>,
    #[serde(default)]
    pub recovering: bool,
    #[serde(default)]
    pub degraded_objects: i64,
}

/// A single normalized metric sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSample {
    pub name: String,
    pub value: f64,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

/// The full result of a driver discovery pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    pub cluster: StorageCluster,
    pub pools: Vec<StoragePool>,
    pub osds: Vec<Osd>,
    pub volumes: Vec<StorageVolume>,
    pub health: StorageHealth,
}

/// A Kubernetes StorageClass as surfaced by the live k8s driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageClassInfo {
    pub name: String,
    pub provisioner: String,
    pub reclaim_policy: Option<String>,
    pub volume_binding_mode: Option<String>,
    pub allow_volume_expansion: Option<bool>,
    /// Ceph-backed classes are tagged so the UI can badge them (PDF §7.1 labels).
    #[serde(default)]
    pub is_ceph: bool,
    /// Known CSI backend, when recognized. Unknown provisioners remain unclassified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_type: Option<String>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Write-path request types. Defined now for a stable API surface; the driver
// write methods return `NotImplemented` until MVP slice 2.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVolumeRequest {
    pub tenant_id: String,
    pub name: String,
    pub size_bytes: i64,
    #[serde(default = "default_block")]
    pub kind: VolumeKind,
    /// Intent policy name (e.g. "production", "database"); resolved by atlas-policy.
    pub policy: Option<String>,
    /// Explicit pool/StorageClass override (bypasses policy placement).
    pub pool: Option<String>,
    /// Product ownership for the resulting volume (recorded in product_bindings).
    #[serde(default)]
    pub owner: Option<Owner>,
    /// Kubernetes provisioning options (the MVP write path creates a PVC).
    #[serde(default)]
    pub kubernetes: Option<K8sVolumeOpts>,
}

fn default_block() -> VolumeKind {
    VolumeKind::Block
}

/// Product ownership of a storage resource (PDF §5.3, §11 product_bindings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub product: String,
    pub resource_type: String,
    pub resource_id: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "data_disk".into()
}

/// Kubernetes-specific volume options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sVolumeOpts {
    /// Explicit Atlas backend for this PVC; omit for the existing Ceph default.
    #[serde(default)]
    pub backend_id: Option<String>,
    pub namespace: Option<String>,
    #[serde(default = "default_true")]
    pub create_pvc: bool,
    #[serde(default)]
    pub access_modes: Vec<String>,
    pub volume_mode: Option<String>,
    /// Explicit StorageClass; otherwise resolved from policy.
    pub storage_class: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A resolved placement decision (atlas-policy output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placement {
    pub intent: String,
    pub storage_class: String,
    pub access_mode: String,
    pub volume_mode: String,
    /// The volume kind this placement actually provisions (e.g. a named `shared` policy always
    /// means CephFS, i.e. `Filesystem`, regardless of what the caller passed as `kind`). Callers
    /// must build the create job from this field, not the request's raw `kind`, or a
    /// caller-defaulted/incorrect `kind` silently mis-tags the volume in inventory and it gets
    /// pruned by discovery on drivers that only enumerate one storage kind (e.g. real Ceph's
    /// RBD-only discovery treats a mistagged CephFS volume as a deleted RBD image).
    pub kind: VolumeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVolumeResult {
    pub volume_id: String,
    pub backend_native_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandVolumeRequest {
    pub volume_id: String,
    pub new_size_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteVolumeRequest {
    pub volume_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSnapshotRequest {
    pub volume_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSnapshotResult {
    pub snapshot_id: String,
    pub backend_native_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneSnapshotRequest {
    pub snapshot_id: String,
    pub new_volume_name: String,
    /// Size of the new volume; the snapshot's size when unset.
    #[serde(default)]
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSnapshotRequest {
    pub snapshot_id: String,
}

// ---------------------------------------------------------------------------
// Jobs & snapshots (read models for slice 2).
// ---------------------------------------------------------------------------

/// Async job states (PDF §10.5).
pub mod job_state {
    pub const PENDING: &str = "pending";
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const VERIFYING: &str = "verifying";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
}

/// A job record as surfaced by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub tenant_id: String,
    pub job_type: String,
    pub state: String,
    pub requested_by: String,
    pub progress_percent: i64,
    pub error: Option<String>,
    #[serde(default)]
    pub result: serde_json::Value,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// A protection schedule: every `interval_secs`, snapshot or back up a volume, retaining `keep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSchedule {
    pub id: String,
    pub tenant_id: String,
    pub volume_id: String,
    /// "snapshot" (default) or "backup".
    pub kind: String,
    /// Target bucket id (backup schedules only).
    pub bucket_id: Option<String>,
    /// Backup mode ("manifest" or "data"); ignored for snapshot schedules.
    pub mode: String,
    pub interval_secs: i64,
    pub keep: i64,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: String,
    pub created_at: Option<String>,
}

/// A per-tenant policy override: remaps an intent to a specific placement (PDF §14).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantPolicy {
    pub tenant_id: String,
    pub intent: String,
    pub storage_class: String,
    pub access_mode: String,
    pub volume_mode: String,
}

/// A per-tenant storage quota plus current usage (PDF §14 multi-tenancy). `0` limits = unlimited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantQuota {
    pub tenant_id: String,
    /// Max total provisioned volume bytes for the tenant (0 = unlimited).
    pub max_bytes: i64,
    /// Max number of volumes for the tenant (0 = unlimited).
    pub max_volumes: i64,
    /// Current total provisioned bytes across the tenant's volumes.
    pub used_bytes: i64,
    /// Current number of the tenant's volumes.
    pub volume_count: i64,
}

/// An object-storage bucket (RGW/S3) as surfaced by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBucket {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    /// Which backend provisioned this bucket (e.g. `bkd_ceph_lab`, `bkd_rustfs_lab`). `None` for
    /// rows created before this field existed — routes must treat that the same as Ceph, the
    /// historical default before any other backend could create buckets.
    pub backend_id: Option<String>,
    /// The actual bucket name RGW assigned (OBC may generate one).
    pub bucket_name: Option<String>,
    /// S3 endpoint (e.g. http://rook-ceph-rgw-...:80).
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Reference to the Kubernetes Secret holding S3 credentials — never the keys (PDF §14.1).
    pub secret_ref: Option<String>,
    /// Namespace of the OBC / Secret / ConfigMap.
    pub namespace: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
}

/// A backup record + manifest pointer (PDF §16.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    pub tenant_id: String,
    pub volume_id: String,
    pub snapshot_id: Option<String>,
    pub bucket_id: String,
    pub object_key: String,
    pub format: String,
    pub checksum: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
}

/// An alert record (PDF §15.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: String,
    pub severity: String,
    pub source: String,
    pub resource_type: String,
    pub resource_id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub evidence: serde_json::Value,
    pub state: String,
    pub created_at: Option<String>,
    pub resolved_at: Option<String>,
    /// Manual lifecycle (day-2): operator acknowledgement + webhook silence window.
    #[serde(default)]
    pub acknowledged_at: Option<String>,
    #[serde(default)]
    pub acknowledged_by: Option<String>,
    #[serde(default)]
    pub silenced_until: Option<String>,
}

/// A snapshot record as surfaced by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSnapshot {
    pub id: String,
    pub tenant_id: String,
    pub volume_id: String,
    pub name: String,
    pub backend_native_id: Option<String>,
    pub consistency: String,
    pub state: String,
    pub protected: bool,
    pub parent_snapshot_id: Option<String>,
    pub created_at: Option<String>,
}

// ---------------------------------------------------------------------------
// DataBridge — cloud-to-edge database migration (PDF §DataBridge)
// ---------------------------------------------------------------------------

/// A registered source database in a cloud (AWS RDS/Aurora, GCP Cloud SQL) or a generic endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationSource {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    /// postgres | mysql | mariadb | oracle | sqlserver | mongodb
    pub kind: String,
    /// rds | aurora | cloudsql | generic
    pub cloud: String,
    pub endpoint: Option<String>,
    pub port: Option<i64>,
    pub database: Option<String>,
    /// k8s Secret holding the source credentials — never the credentials themselves.
    pub secret_ref: Option<String>,
    pub secret_namespace: Option<String>,
    pub tls_mode: String,
    /// fake | real — fake serves a canned schema so the pipeline runs with no cloud creds.
    pub driver_mode: String,
    pub state: String,
    /// Discovered schema/tables/sizes/version/extensions (opaque JSON).
    #[serde(default)]
    pub discovered: serde_json::Value,
    pub created_at: Option<String>,
}

/// The edge target DB cluster (CloudNativePG / MySQL operator) on Ceph-backed storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeDbCluster {
    pub id: String,
    pub tenant_id: String,
    pub plan_id: Option<String>,
    /// postgres | mysql | mongodb
    pub engine: String,
    /// cnpg | percona | psmdb
    pub operator: String,
    pub namespace: String,
    pub cr_name: Option<String>,
    pub storage_class: String,
    pub wal_storage_class: Option<String>,
    pub instances: i64,
    pub size_bytes: i64,
    pub service_endpoint: Option<String>,
    pub secret_ref: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
}

/// A Debezium CDC stream keeping the edge DB in sync with the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdcStream {
    pub id: String,
    pub tenant_id: String,
    pub plan_id: Option<String>,
    pub engine: String,
    pub connect_name: Option<String>,
    pub connector_name: Option<String>,
    pub topic_prefix: Option<String>,
    pub state: String,
    pub lag_bytes: i64,
    pub lag_seconds: i64,
    pub last_source_lsn: Option<String>,
    pub last_applied_lsn: Option<String>,
    pub events_total: i64,
    pub lag_updated_at: Option<String>,
    #[serde(default)]
    pub restart_count: i64,
    pub created_at: Option<String>,
}

/// An object-storage migration: a cloud object store (AWS S3, Google Cloud Storage via
/// S3-interop, or any S3-compatible endpoint) copied into a Ceph RGW bucket. Credentials
/// live only in the referenced k8s Secrets — never in this record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMigration {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub source_provider: String,
    pub source_endpoint: String,
    pub source_region: String,
    pub source_bucket: String,
    pub source_prefix: Option<String>,
    pub source_secret_ref: Option<String>,
    pub dest_provider: String,
    pub dest_endpoint: String,
    pub dest_region: String,
    pub dest_bucket: String,
    pub dest_secret_ref: Option<String>,
    pub secret_namespace: String,
    pub mode: String,
    pub state: String,
    pub objects_total: i64,
    pub objects_done: i64,
    pub bytes_total: i64,
    pub bytes_done: i64,
    pub verified: bool,
    #[serde(default)]
    pub concurrency: Option<i64>,
    #[serde(default)]
    pub part_size_mb: Option<i64>,
    #[serde(default)]
    pub throughput_mbps: f64,
    #[serde(default)]
    pub started_at: Option<String>,
    pub last_error: Option<String>,
    pub job_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// A migration plan: source -> edge, with assessment, CDC, cutover state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub source_id: String,
    pub edge_cluster_id: Option<String>,
    pub cdc_stream_id: Option<String>,
    pub readiness_score: i64,
    #[serde(default)]
    pub assessment: serde_json::Value,
    pub rollback_window_secs: i64,
    pub cutover_at: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
}

/// One table's comparison result inside a validation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationTableResult {
    pub table: String,
    pub source_rows: i64,
    pub edge_rows: i64,
    pub checksum_match: bool,
}

/// A validation run comparing source vs edge (row counts / checksums / schema diff).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRun {
    pub id: String,
    pub tenant_id: String,
    pub plan_id: String,
    pub kind: String,
    pub state: String,
    pub tables_total: i64,
    pub tables_mismatched: i64,
    #[serde(default)]
    pub summary: serde_json::Value,
    pub created_at: Option<String>,
    pub completed_at: Option<String>,
}

/// A cutover: freeze source, drain CDC lag, switch endpoint, open a rollback window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cutover {
    pub id: String,
    pub tenant_id: String,
    pub plan_id: String,
    pub state: String,
    pub from_endpoint: Option<String>,
    pub to_endpoint: Option<String>,
    pub drain_deadline: Option<String>,
    pub rollback_deadline: Option<String>,
    pub created_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Block I/O operation class recorded by the Atlas I/O sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoOp {
    Read,
    Write,
    Flush,
    Discard,
    Other,
}

impl IoOp {
    pub fn as_str(self) -> &'static str {
        match self {
            IoOp::Read => "read",
            IoOp::Write => "write",
            IoOp::Flush => "flush",
            IoOp::Discard => "discard",
            IoOp::Other => "other",
        }
    }
}

/// One completed block I/O, after the kernel issue→complete pair is joined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BioEvent {
    pub issued_ns: u64,
    pub completed_ns: u64,
    pub queued_ns: u64,
    pub bytes: u32,
    pub op: IoOp,
    pub major: u32,
    pub minor: u32,
    pub pid: u32,
    pub cgroup_id: u64,
    pub comm: String,
}

impl BioEvent {
    pub fn latency_us(&self) -> u64 {
        self.completed_ns.saturating_sub(self.issued_ns) / 1_000
    }

    pub fn queue_us(&self) -> u64 {
        self.queued_ns / 1_000
    }

    pub fn dev_key(&self) -> u64 {
        ((self.major as u64) << 32) | self.minor as u64
    }
}

/// Histogram of I/O latency for one (device, op) pair. Buckets are log2 microseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoHistogram {
    pub device: String,
    pub volume_id: Option<String>,
    pub op: IoOp,
    pub buckets: Vec<u64>,
    pub count: u64,
    pub sum_us: u64,
    pub bytes: u64,
    /// Requests that waited in an I/O scheduler queue before issue, and their total wait.
    /// `sum_us` / the buckets are issue→complete only and exclude this wait.
    #[serde(default)]
    pub queued: u64,
    #[serde(default)]
    pub queue_sum_us: u64,
}

impl IoHistogram {
    pub fn p99_us(&self) -> u64 {
        percentile_us(&self.buckets, self.count, 99)
    }

    pub fn p50_us(&self) -> u64 {
        percentile_us(&self.buckets, self.count, 50)
    }
}

fn percentile_us(buckets: &[u64], count: u64, pct: u64) -> u64 {
    if count == 0 || buckets.is_empty() {
        return 0;
    }
    let target = count.saturating_mul(pct).div_ceil(100).max(1);
    let mut acc = 0u64;
    for (i, c) in buckets.iter().enumerate() {
        acc = acc.saturating_add(*c);
        if acc >= target {
            return 1u64 << i.min(63);
        }
    }
    1u64 << buckets.len().saturating_sub(1).min(63)
}

/// Workload attribution row (cgroup / pid → volume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoWorkload {
    pub cgroup_id: u64,
    pub pid: u32,
    pub comm: String,
    pub device: String,
    pub volume_id: Option<String>,
    pub ops: u64,
    pub bytes: u64,
    pub sum_us: u64,
}

/// Observe-first I/O sensor health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoSensorHealth {
    pub mode: String,
    pub programs_loaded: Vec<String>,
    pub programs_missing: Vec<String>,
    pub pin_dir: String,
    pub events_seen: u64,
    pub events_dropped: u64,
    pub map_cardinality: u64,
    pub cardinality_cap: u64,
}

/// Kernel-aggregated block I/O issued by Atlas Native processes, per (device, op, cgroup).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoNativeStat {
    pub device: String,
    pub major: u32,
    pub minor: u32,
    pub op: IoOp,
    pub cgroup_id: u64,
    pub ios: u64,
    pub bytes: u64,
    pub avg_us: u64,
    pub max_us: u64,
    pub errors: u64,
}

/// One Atlas Native request at or above the slow-I/O threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoNativeSlow {
    pub device: String,
    pub major: u32,
    pub minor: u32,
    pub op: IoOp,
    pub pid: u32,
    pub comm: String,
    pub cgroup_id: u64,
    pub sector: u64,
    pub bytes: u32,
    pub latency_us: u64,
    pub error: i32,
}

/// Atlas Native I/O view: which processes are tracked, kernel stats, recent slow requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IoNative {
    pub process_names: Vec<String>,
    pub tracked_pids: Vec<u32>,
    /// Where the native maps are pinned, if pinning succeeded.
    pub pinned: Option<String>,
    pub slow_threshold_us: u64,
    pub stats: Vec<IoNativeStat>,
    pub slow: Vec<IoNativeSlow>,
    pub dropped: u64,
}

/// Read-only RCA hint produced from histograms + attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoRca {
    pub volume_id: Option<String>,
    pub device: String,
    pub verdict: String,
    pub p50_us: u64,
    pub p99_us: u64,
    pub hottest_comm: Option<String>,
    pub notes: Vec<String>,
}

/// Time-limited write-freeze lease. Expired or agent-down ⇒ fail open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoLease {
    pub id: String,
    pub device: String,
    pub volume_id: Option<String>,
    pub action: String,
    pub expires_unix: u64,
    pub reason: String,
}
