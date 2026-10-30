// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// TypeScript mirrors of the Atlas DTOs (crates/atlas-api-types/src/lib.rs).

export type Health = "ok" | "warn" | "critical" | "unknown";
export type ClusterHealthState = "healthy" | "degraded" | "rebuilding" | "at_risk" | "critical";

export interface VolumeProtectionStatus {
  volume_id: string;
  volume_name: string;
  storage_backend: string;
  replication_factor?: number | null;
  last_snapshot_at?: string | null;
  last_backup_at?: string | null;
  last_backup_location?: string | null;
  dr_role?: string | null;
  dr_state?: string | null;
  rpo_target_seconds?: number | null;
  rpo_target_source: "policy" | "schedule" | "none";
  rpo_current_seconds?: number | null;
  rto_target_seconds?: number | null;
  rto_note: string;
  verdict: ClusterHealthState;
  verdict_reasons: string[];
}

export interface StorageVolume {
  id: string;
  cluster_id?: string | null;
  pool_id?: string | null;
  name: string;
  kind: "block" | "filesystem" | "object";
  backend_native_id?: string | null;
  size_bytes: number;
  used_bytes?: number | null;
  state: string;
  health: Health;
  kubernetes_namespace?: string | null;
  pvc_name?: string | null;
  storage_class_name?: string | null;
}

export interface StorageSnapshot {
  id: string;
  tenant_id: string;
  volume_id: string;
  name: string;
  backend_native_id?: string | null;
  consistency: string;
  state: string;
  protected: boolean;
  parent_snapshot_id?: string | null;
  created_at?: string | null;
}

export interface StorageBucket {
  id: string;
  tenant_id: string;
  name: string;
  bucket_name?: string | null;
  endpoint?: string | null;
  region?: string | null;
  secret_ref?: string | null;
  namespace?: string | null;
  /** Which storage backend owns the bucket (e.g. `bkd_ceph_lab`). Null on legacy rows — those are Ceph RGW. */
  backend_id?: string | null;
  state: string;
  created_at?: string | null;
}

export interface BackupRecord {
  id: string;
  tenant_id: string;
  volume_id: string;
  snapshot_id?: string | null;
  bucket_id: string;
  object_key: string;
  format: string;
  checksum?: string | null;
  state: string;
  created_at?: string | null;
}

export interface SnapshotSchedule {
  id: string;
  tenant_id: string;
  volume_id: string;
  kind: "snapshot" | "backup";
  bucket_id?: string | null;
  mode: string;
  interval_secs: number;
  keep: number;
  enabled: boolean;
  last_run_at?: string | null;
  next_run_at: string;
  created_at?: string | null;
}

export interface TenantQuota {
  tenant_id: string;
  max_bytes: number;
  max_volumes: number;
  used_bytes: number;
  volume_count: number;
}

export interface TenantPolicy {
  tenant_id: string;
  intent: string;
  storage_class: string;
  access_mode: string;
  volume_mode: string;
}

export interface AlertRecord {
  id: string;
  severity: string;
  source: string;
  resource_type: string;
  resource_id: string;
  title: string;
  description: string;
  evidence?: unknown;
  state: string;
  created_at?: string | null;
  resolved_at?: string | null;
  acknowledged_at?: string | null;
  acknowledged_by?: string | null;
  silenced_until?: string | null;
}

export type AdvisorMode = "local" | "auto" | "llm";

export interface AdvisorAction {
  priority: number;
  title: string;
  rationale: string;
  inspect: string;
}

export interface AdvisorEvidence {
  capacity_used_percent: number;
  days_to_full: number | null;
  open_alerts: number;
  critical_alerts: number;
  warning_alerts: number;
  failed_jobs_15m: number;
  degraded_objects: number;
  unfound_objects: number;
  alert_titles: string[];
}

export interface AdvisorResponse {
  mode: "local" | "local_fallback" | "llm";
  risk_score: number;
  risk_level: "low" | "moderate" | "high" | "critical";
  summary: string;
  evidence: AdvisorEvidence;
  actions: AdvisorAction[];
  warnings: string[];
  can_execute: false;
}

export interface IncidentSignal {
  source: string;
  severity: string;
  title: string;
  resource_type: string;
  resource_id: string;
}

export interface CorrelatedIncident {
  id: string;
  category: string;
  severity: string;
  confidence: number;
  title: string;
  likely_cause: string;
  signals: IncidentSignal[];
  inspect: string[];
}

export interface IncidentsResponse {
  generated_at: string;
  count: number;
  incidents: CorrelatedIncident[];
  can_execute: false;
}

export interface RiskProjection {
  risk_score: number;
  risk_level: "low" | "moderate" | "high" | "critical";
  capacity_used_percent: number;
  days_to_full: number | null;
}

export interface WhatIfResponse {
  horizon_days: number;
  baseline: RiskProjection;
  projected: RiskProjection;
  risk_delta: number;
  actions: AdvisorAction[];
  assumptions: string[];
  can_execute: false;
}

export interface MetricAnomaly {
  id: string;
  metric: string;
  label: string;
  severity: "warning" | "high" | "critical";
  score: number;
  current: number;
  baseline: number;
  median_absolute_deviation: number;
  change_percent: number;
  direction: "higher";
  explanation: string;
  inspect: string;
}

export interface AnomaliesResponse {
  generated_at: string;
  window_minutes: number;
  sample_count: number;
  telemetry_status: "fresh" | "stale" | "unavailable";
  latest_sample_age_minutes: number | null;
  sensitivity: number;
  model: "robust_median_mad_v1";
  anomalies: MetricAnomaly[];
  warnings: string[];
  can_execute: false;
}

export interface JobRecord {
  id: string;
  tenant_id: string;
  job_type: string;
  state: string;
  requested_by: string;
  progress_percent: number;
  error?: string | null;
  result?: unknown;
  created_at?: string | null;
  updated_at?: string | null;
}

export interface StorageCluster {
  id: string;
  backend_id: string;
  name: string;
  native_fsid?: string | null;
  health: Health;
  raw_capacity_bytes?: number | null;
  used_capacity_bytes?: number | null;
  available_capacity_bytes?: number | null;
}

export interface StoragePool {
  id: string;
  cluster_id: string;
  name: string;
  kind: string;
  device_class?: string | null;
  replica_size?: number | null;
  used_bytes?: number | null;
  max_bytes?: number | null;
  health: Health;
}

export interface Osd {
  id: number;
  cluster_id: string;
  osd_num?: number;
  up: boolean;
  in_cluster: boolean;
  device_class?: string | null;
  host?: string | null;
  used_bytes?: number | null;
  capacity_bytes?: number | null;
}

export interface ZfsBlockDevice {
  path: string;
  size_bytes: number;
  read_only: boolean;
  has_children: boolean;
  fstype?: string | null;
  pttype?: string | null;
  mounted_at?: string | null;
  wipefs_signatures: string[];
  member_of_zpool?: string | null;
  is_root_or_boot_disk: boolean;
  status: "empty" | "has_data" | "mounted" | "zpool_member" | "read_only" | "root_or_boot";
}

// Passed through verbatim from Rook's own `local-device-<node>` discovery ConfigMap schema.
export interface CephNodeDevice {
  name: string;
  size?: number;
  filesystem?: string;
  empty?: boolean;
  type?: string;
  rotational?: boolean;
  class?: "nvme" | "ssd" | "hdd";
  [key: string]: unknown;
}

export interface MetricsSummary {
  raw_capacity_bytes: number;
  used_capacity_bytes: number;
  available_capacity_bytes: number;
  used_capacity_percent: number;
  clusters: number;
  pools: number;
  volumes: number;
  snapshots: number;
  buckets: number;
  backups: number;
  client_io: {
    read_ops_total: number;
    write_ops_total: number;
    read_bytes_total: number;
    write_bytes_total: number;
  };
  recovery: {
    pg_recovering: number;
    pg_backfilling: number;
    objects_degraded: number;
    objects_misplaced: number;
    objects_unfound: number;
  };
}

export interface MetricSample {
  name: string;
  value: number;
  labels: Record<string, string>;
}

// One row of the unified activity feed (GET /events) — job | audit | alert normalized.
export interface ActivityEvent {
  ts: string;
  kind: "job" | "audit" | "alert";
  id: string;
  severity: string;
  title: string;
  detail: string;
  resource_type: string;
  resource_id: string;
  actor: string;
}

// GET /metrics/forecast — least-squares fill projection; days_to_full is null when not growing.
export interface MetricForecast {
  samples: number;
  window_minutes: number;
  used_capacity_bytes: number;
  raw_capacity_bytes: number;
  growth_bytes_per_day: number;
  days_to_full: number | null;
}

// One persisted time-series row from GET /metrics/history (read_ops/write_ops are cumulative totals).
export interface MetricHistoryPoint {
  ts: string;
  raw_capacity_bytes: number;
  used_capacity_bytes: number;
  volumes: number;
  snapshots: number;
  read_bytes: number;
  write_bytes: number;
  read_ops: number;
  write_ops: number;
  jobs_running: number;
  alerts_open: number;
}

export interface AuditRow {
  id: number;
  tenant_id?: string | null;
  actor_id: string;
  action: string;
  resource_type: string;
  resource_id: string;
  status: string;
  request?: unknown;
  result?: unknown;
  created_at: string;
}

export interface JobAccepted {
  job_id?: string;
  state?: string;
  resource?: Record<string, unknown>;
  links?: { job?: string };
}

// ---- DataBridge (cloud-to-edge DB migration) ----
export interface DiscoveredTable { schema: string; name: string; est_rows: number; size_bytes: number; has_primary_key: boolean; }
export interface DiscoveredSchema {
  engine?: string; version?: string; databases?: string[];
  tables?: DiscoveredTable[]; extensions?: string[]; total_size_bytes?: number; cdc_capable?: boolean;
}
export interface MigrationSource {
  id: string; tenant_id: string; name: string; kind: string; cloud: string;
  endpoint?: string | null; port?: number | null; database?: string | null;
  secret_ref?: string | null; secret_namespace?: string | null; tls_mode: string;
  driver_mode: string; state: string; discovered?: DiscoveredSchema; created_at?: string | null;
}
export interface MigrationPlan {
  id: string; tenant_id: string; name: string; source_id: string;
  edge_cluster_id?: string | null; cdc_stream_id?: string | null;
  readiness_score: number; assessment?: any; rollback_window_secs: number;
  cutover_at?: string | null; state: string; created_at?: string | null;
}
export interface EdgeDbCluster {
  id: string; tenant_id: string; plan_id?: string | null; engine: string; operator: string;
  namespace: string; cr_name?: string | null; storage_class: string; wal_storage_class?: string | null;
  instances: number; size_bytes: number; service_endpoint?: string | null; secret_ref?: string | null;
  state: string; created_at?: string | null;
}
export interface CdcStream {
  id: string; tenant_id: string; plan_id?: string | null; engine: string;
  connect_name?: string | null; connector_name?: string | null; topic_prefix?: string | null;
  state: string; lag_bytes: number; lag_seconds: number; last_source_lsn?: string | null;
  last_applied_lsn?: string | null; events_total: number; lag_updated_at?: string | null; created_at?: string | null;
}
export interface ValidationRun {
  id: string; tenant_id: string; plan_id: string; kind: string; state: string;
  tables_total: number; tables_mismatched: number; summary?: any; created_at?: string | null; completed_at?: string | null;
}
export interface Cutover {
  id: string; tenant_id: string; plan_id: string; state: string;
  from_endpoint?: string | null; to_endpoint?: string | null;
  drain_deadline?: string | null; rollback_deadline?: string | null; created_at?: string | null; completed_at?: string | null;
}

export interface ObjectMigration {
  id: string;
  name: string;
  source_endpoint: string;
  source_bucket: string;
  source_prefix?: string | null;
  dest_endpoint: string;
  dest_bucket: string;
  mode: string;
  state: string;
  objects_total: number;
  objects_done: number;
  bytes_total: number;
  bytes_done: number;
  verified: boolean;
  throughput_mbps?: number;
  last_error?: string | null;
  job_id?: string | null;
  created_at?: string | null;
}

// ---- Fast storage: NVMe health, host sizing, object stores, AI I/O shapes ----
export interface NvmeDrive {
  devid: string;
  host: string;
  dev: string;
  daemons?: string[];
  scraped: boolean;
  wear_percent?: number | null;
  available_spare_percent?: number | null;
  media_errors?: number | null;
  temperature_c?: number | null;
  critical_warning?: number | null;
  concerns: string[];
  error?: string;
}
export interface NvmeReport {
  summary: { total: number; scraped: number; with_concerns: number; worn_at_percent: number; truncated: boolean };
  devices: NvmeDrive[];
}
export interface SizingHost {
  host: string;
  nvme_devices: number;
  osds: number;
  needs_cpu_cores: number;
  needs_memory_gib: number;
  status: "ok" | "undersized" | "unknown";
  node?: string;
  has_cpu_cores?: number;
  has_memory_gib?: number;
  shortfalls?: string[];
}
export interface SizingReport {
  thresholds: { cores_per_nvme_device: number; ram_base_gib: number; ram_per_osd_gib: number; note: string };
  summary: { hosts: number; undersized: number; unknown: number };
  hosts: SizingHost[];
}
export interface ObjectStoreReport {
  summary: {
    total: number;
    by_objectstore: Record<string, number>;
    by_version: Record<string, number>;
    mixed_versions: boolean;
  };
  osds: { id: number; hostname?: string | null; osd_objectstore?: string | null; ceph_version?: string | null }[];
}
export type IoShapeName = "large_write" | "bulk_read" | "small_read" | "mixed" | "unknown";
export interface IoAiDevice {
  device: string;
  shape: IoShapeName;
  avg_read_bytes: number;
  avg_write_bytes: number;
  p99_read_us: number;
  p99_write_us: number;
  finding: string | null;
}
export interface IoAiReport {
  status: "ok" | "unavailable";
  devices: IoAiDevice[];
  detail: string | null;
}
