// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// react-query hooks for the Atlas API. Queries auto-refetch; write helpers live in views.
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { http } from "./client";
import { useUi } from "../store/ui";
import type {
  ActivityEvent, AlertRecord, AuditRow, BackupRecord, CephNodeDevice, ClusterHealthState, JobRecord, MetricForecast, MetricHistoryPoint, MetricSample, MetricsSummary, Osd,
  SnapshotSchedule, StorageBucket, StorageCluster, StoragePool, StorageSnapshot, StorageVolume,
  TenantPolicy, TenantQuota, VolumeProtectionStatus, ZfsBlockDevice,
  MigrationSource, MigrationPlan, EdgeDbCluster, CdcStream, ValidationRun, ObjectMigration,
  IoAiReport, NvmeReport, ObjectStoreReport, SizingReport
} from "./types";

const g = async <T,>(path: string): Promise<T> => (await http.get<T>(path)).data;

function useApiQuery<T>(key: unknown[], path: string, refetch = 8000, enabled = true) {
  // Pause auto-refresh globally when the user toggles it (menu bar).
  return useQuery<T>({
    queryKey: key,
    queryFn: () => g<T>(path),
    refetchInterval: () => (useUi.getState().paused ? false : refetch),
    enabled,
  });
}

export const useSummary = () => useApiQuery<MetricsSummary>(["summary"], "/metrics/summary", 6000);
export const useHistory = () =>
  useApiQuery<MetricHistoryPoint[]>(["history"], "/metrics/history?minutes=60", 30000);
export const useForecast = () =>
  useApiQuery<MetricForecast>(["forecast"], "/metrics/forecast?minutes=1440", 30000);
export const useClusters = () => useApiQuery<StorageCluster[]>(["clusters"], "/clusters", 10000);
export const usePools = () => useApiQuery<StoragePool[]>(["pools"], "/pools", 10000);
export const useOsds = () => useApiQuery<Osd[]>(["osds"], "/osds", 10000);
export const useNodes = () => useApiQuery<{ host: string }[]>(["nodes"], "/nodes", 15000);
export const useZfsDevices = () =>
  useApiQuery<ZfsBlockDevice[]>(["zfs-devices"], "/zfs/devices", 15000);
export const useCephNodeDevices = (nodeName?: string) =>
  useApiQuery<CephNodeDevice[]>(
    ["ceph-node-devices", nodeName],
    `/ceph/nodes/${nodeName}/devices`,
    15000,
    !!nodeName,
  );
export const useVolumes = (state?: string, tenant?: string, backend?: string, kind?: string) => {
  const qs = new URLSearchParams();
  if (state) qs.set("state", state);
  if (tenant) qs.set("tenant", tenant);
  if (backend) qs.set("backend", backend);
  if (kind) qs.set("kind", kind);
  const s = qs.toString();
  return useApiQuery<StorageVolume[]>(["volumes", state, tenant, backend, kind], `/volumes${s ? "?" + s : ""}`, 6000);
};
export const useSnapshots = () => useApiQuery<StorageSnapshot[]>(["snapshots"], "/snapshots", 6000);
export const useSchedules = (volumeId?: string) =>
  useApiQuery<SnapshotSchedule[]>(["schedules", volumeId], `/schedules${volumeId ? "?volume_id=" + volumeId : ""}`, 8000);
export const useBackups = (volumeId?: string) =>
  useApiQuery<BackupRecord[]>(["backups", volumeId], `/backups${volumeId ? "?volume_id=" + volumeId : ""}`, 6000);
export const useBuckets = () => useApiQuery<StorageBucket[]>(["buckets"], "/buckets", 8000);
export const useAlerts = (state?: string) =>
  useApiQuery<AlertRecord[]>(["alerts", state], `/alerts${state ? "?state=" + state : ""}`, 6000);
export const useJobs = () => useApiQuery<JobRecord[]>(["jobs"], "/jobs", 4000);
export const useAudit = (params: string) => useApiQuery<AuditRow[]>(["audit", params], `/audit${params}`, 10000);
export const useEvents = (limit = 100) =>
  useApiQuery<ActivityEvent[]>(["events", limit], `/events?limit=${limit}`, 8000);
export const useTenants = () => useApiQuery<TenantQuota[]>(["tenants"], "/tenants", 10000);
export const useTenantPolicies = (id: string) =>
  useApiQuery<TenantPolicy[]>(["tenantPolicies", id], `/tenants/${id}/policies`, 15000, !!id);
export const useCephMetrics = (prefix?: string) =>
  useApiQuery<MetricSample[]>(["cephMetrics", prefix], `/metrics/ceph${prefix ? "?prefix=" + prefix : ""}`, 8000);
export const usePolicies = () => useApiQuery<any[]>(["policies"], "/policies", 60000);
export type MacDevice = {
  device: string;
  internal: boolean;
  bus: string | null;
  media_name: string | null;
  smart: string | null;
  disk_image: boolean;
  containers: string[];
  erase_refusal: string | null;
};
/** Whole disks of one Mac backend (`bkd_macos` or an agent's `bkd_macos_<name>`). */
export const useMacDevices = (backend?: string) =>
  useApiQuery<MacDevice[]>(
    ["macos-devices", backend],
    `/macos/devices?backend=${encodeURIComponent(backend ?? "")}`,
    15000,
    !!backend,
  );
/** Local snapshot dates of one Mac backend (the Mac as a whole; macOS has no per-volume ones). */
export const useMacSnapshots = (backend?: string) =>
  useApiQuery<string[]>(
    ["macos-snapshots", backend],
    `/macos/snapshots?backend=${encodeURIComponent(backend ?? "")}`,
    15000,
    !!backend,
  );
export const useBackends = () => useApiQuery<any[]>(["backends"], "/backends", 20000);
export const useBackendsSummary = () => useApiQuery<any[]>(["backends-summary"], "/backends/summary", 15000);

// Day-2 operations.
export const useMaintenance = () => useApiQuery<{ paused: boolean }>(["maintenance"], "/maintenance", 8000);
export const usePreflight = () =>
  useApiQuery<{ ready: boolean; checks: { check: string; ok: boolean; detail: string }[]; blockers: string[] }>(
    ["preflight"], "/upgrade/preflight", 10000);
export const useOrphans = () => useApiQuery<{ orphan_backups: any[]; count: number }>(["orphans"], "/maintenance/orphans", 20000);
export const useDrStatus = () =>
  useApiQuery<{
    peers: number;
    mirrors: number;
    primary: number;
    secondary: number;
    worst_rpo_seconds: number | null;
    control_plane_ready?: boolean;
    dataplane_verified?: boolean;
    verified?: boolean;
    note?: string;
  }>(["dr-status"], "/dr/status", 10000);
export const useDrPeers = () => useApiQuery<any[]>(["dr-peers"], "/dr/peers", 15000);
export const useDrMirrors = () => useApiQuery<any[]>(["dr-mirrors"], "/dr/mirrors", 10000);
export const useDrPreflight = () =>
  useApiQuery<{
    ready: boolean;
    control_plane_ready?: boolean;
    dataplane_verified?: boolean;
    checks: { id: string; ok: boolean; detail: string }[];
    blockers: string[];
    warnings?: string[];
  }>(["dr-preflight"], "/dr/preflight", 10000);
export const useProtectionStatus = (params?: { tenant?: string; verdict?: string }) => {
  const qs = new URLSearchParams();
  if (params?.tenant) qs.set("tenant", params.tenant);
  if (params?.verdict) qs.set("verdict", params.verdict);
  const suffix = qs.toString() ? `?${qs.toString()}` : "";
  return useApiQuery<VolumeProtectionStatus[]>(["protection-status", params?.tenant, params?.verdict], `/protection-status${suffix}`, 15000);
};
export const useVolumeProtection = (id: string) =>
  useApiQuery<VolumeProtectionStatus>(["volume-protection", id], `/volumes/${id}/protection`, 15000, !!id);
export const useCephStatus = () => useApiQuery<any>(["ceph-status"], "/ceph/status", 8000);
export const useCephHealthRollup = () =>
  useApiQuery<{
    state: ClusterHealthState;
    summary: string;
    reasons: string[];
    raw_status: string;
    osds_up: number;
    osds_in: number;
    osds_total: number;
    pgs_total: number;
    pgs_not_clean: number;
    recovering: boolean;
  }>(["ceph-health-rollup"], "/ceph/health-rollup", 8000);
export const useCephOsdTree = () => useApiQuery<any>(["ceph-osd-tree"], "/ceph/osd-tree", 15000);
export const useCephOsdDf = () => useApiQuery<any>(["ceph-osd-df"], "/ceph/osd-df", 12000);
export const useCephDf = () => useApiQuery<any>(["ceph-df"], "/ceph/df", 12000);
export const useStorageClasses = () => useApiQuery<any[]>(["scs"], "/storage-classes", 20000);
export const useRbdImages = (pool: string) =>
  useApiQuery<{ pool: string; images: string[] }>(["rbd", pool], `/rbd-images?pool=${pool}`, 8000);

// ---- DataBridge ----
export const useSources = () => useApiQuery<MigrationSource[]>(["db-sources"], "/databridge/sources", 6000);
export const useSource = (id: string) => useApiQuery<MigrationSource>(["db-source", id], `/databridge/sources/${id}`, 4000, !!id);
export const usePlans = () => useApiQuery<MigrationPlan[]>(["db-plans"], "/databridge/plans", 6000);
export const usePlan = (id: string) => useApiQuery<MigrationPlan>(["db-plan", id], `/databridge/plans/${id}`, 4000);
export const useEdgeClusters = () => useApiQuery<EdgeDbCluster[]>(["db-edge"], "/databridge/edge-clusters", 8000);
export const useObjectMigrations = () => useApiQuery<ObjectMigration[]>(["db-object"], "/databridge/object", 4000);
export const useCdcStreams = () => useApiQuery<CdcStream[]>(["db-cdc"], "/databridge/cdc-streams", 4000);
export const useValidations = (planId?: string) =>
  useApiQuery<ValidationRun[]>(["db-val", planId], `/databridge/validations${planId ? "?plan_id=" + planId : ""}`, 6000);

export function useInvalidate() {
  const qc = useQueryClient();
  return (...keys: string[]) => keys.forEach((k) => qc.invalidateQueries({ queryKey: [k] }));
}

// Fast storage (NVMe health, host sizing, object stores, AI I/O shapes). Slow-moving, so refreshed lazily.
export const useCephNvme = () => useApiQuery<NvmeReport>(["ceph-nvme"], "/ceph/nvme", 30000);
export const useCephSizing = () => useApiQuery<SizingReport>(["ceph-sizing"], "/ceph/sizing", 60000);
export const useCephObjectStore = () =>
  useApiQuery<ObjectStoreReport>(["ceph-osd-objectstore"], "/ceph/osd-objectstore", 60000);
export const useIoAi = () => useApiQuery<IoAiReport>(["io-ai"], "/io/ai", 15000);
