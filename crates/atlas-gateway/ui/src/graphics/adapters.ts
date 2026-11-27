// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Pure mappings from Atlas API DTOs to graphic inputs. Anything the API did not report stays
// `null`; nothing here estimates or fills in a value.
import type { IoHeatSeries, MetricHistoryPoint, MetricsSummary, Osd, StoragePool } from "../api/types";
import { isMeasure } from "./format";
import type { HeroNode, PoolUsage, TrendPoint, TrendSeries } from "./types";

/** Group `/osds` by host. Returns `null` while the list is not loaded. */
export function topologyFromOsds(osds?: readonly Osd[] | null): HeroNode[] | null {
  if (!osds) return null;
  const byHost = new Map<string | null, Osd[]>();
  for (const osd of osds) {
    const host = osd.host || null;
    const list = byHost.get(host);
    if (list) list.push(osd);
    else byHost.set(host, [osd]);
  }
  return [...byHost.entries()]
    .sort(([a], [b]) => {
      if (a === null) return 1;
      if (b === null) return -1;
      return a.localeCompare(b, undefined, { numeric: true });
    })
    .map(([host, list]) => ({
      host,
      disks: [...list]
        .sort((a, b) => (a.osd_num ?? a.id) - (b.osd_num ?? b.id))
        .map((o) => ({
          id: `${o.cluster_id}/${o.id}`,
          label: `osd.${o.osd_num ?? o.id}`,
          up: o.up,
          inCluster: o.in_cluster,
          deviceClass: o.device_class,
          usedBytes: o.used_bytes,
          capacityBytes: o.capacity_bytes,
        })),
    }));
}

/** Distinct reported replica sizes, ascending. Pools that do not report one are skipped. */
export function replicaSizesFromPools(pools?: readonly StoragePool[] | null): number[] {
  const sizes = new Set<number>();
  for (const p of pools ?? []) {
    if (isMeasure(p.replica_size) && p.replica_size > 0) sizes.add(p.replica_size);
  }
  return [...sizes].sort((a, b) => a - b);
}

/**
 * What `StoragePool.max_bytes` holds differs by driver, so the caller has to say:
 * - `total`: the pool's size (ZFS, NFS, WEKA, macOS drivers).
 * - `available`: space still free (the real Ceph driver's `max_avail`). Ceph's `bytes_used`
 *   counts replicas while `max_avail` does not, so no fill level is derived from the pair; use
 *   {@link poolUsageFromCephDf} for Ceph's own percentage.
 */
export type MaxBytesMeaning = "total" | "available";

export function poolUsageFromPools(
  pools: readonly StoragePool[] | null | undefined,
  meaning: MaxBytesMeaning | ((pool: StoragePool) => MaxBytesMeaning),
): PoolUsage[] | null {
  if (!pools) return null;
  return pools.map((p) => {
    const kind = typeof meaning === "function" ? meaning(p) : meaning;
    return {
      id: p.id,
      name: p.name,
      health: p.health,
      usedBytes: p.used_bytes ?? null,
      capacityBytes: kind === "total" ? (p.max_bytes ?? null) : null,
      availableBytes: kind === "available" ? (p.max_bytes ?? null) : null,
    };
  });
}

interface CephDfPool {
  id?: number;
  name?: string;
  stats?: { stored?: number; max_avail?: number; percent_used?: number };
}

/** Map `/ceph/df` (`ceph df detail`). `percent_used` is a 0-1 fraction computed by Ceph. */
export function poolUsageFromCephDf(df: unknown): PoolUsage[] | null {
  const pools = (df as { pools?: unknown } | null | undefined)?.pools;
  if (!Array.isArray(pools)) return null;
  return (pools as CephDfPool[]).map((p, i) => {
    const fraction = p.stats?.percent_used;
    return {
      id: String(p.id ?? p.name ?? i),
      name: p.name ?? `pool ${p.id ?? i}`,
      usedBytes: p.stats?.stored ?? null,
      availableBytes: p.stats?.max_avail ?? null,
      percentUsed: isMeasure(fraction) ? fraction * 100 : null,
    };
  });
}

/** Props for `CapacityRing` from `/metrics/summary`. A zero raw capacity means "not surveyed". */
export function capacityFromSummary(summary?: MetricsSummary | null): {
  usedBytes: number | null;
  totalBytes: number | null;
} {
  const total = summary?.raw_capacity_bytes;
  if (!summary || !isMeasure(total) || total === 0) return { usedBytes: null, totalBytes: null };
  return { usedBytes: summary.used_capacity_bytes, totalBytes: total };
}

type Counter = "read_bytes" | "write_bytes" | "read_ops" | "write_ops";

function rates(history: readonly MetricHistoryPoint[], key: Counter): TrendPoint[] {
  const rows = history
    .map((p) => ({ t: Date.parse(p.ts), total: p[key] }))
    .filter((r) => Number.isFinite(r.t))
    .sort((a, b) => a.t - b.t);
  const out: TrendPoint[] = [];
  for (let i = 1; i < rows.length; i++) {
    const seconds = (rows[i].t - rows[i - 1].t) / 1000;
    if (seconds <= 0) continue;
    const delta = rows[i].total - rows[i - 1].total;
    // A counter that went backwards was reset: that interval's rate is unknown, not zero.
    out.push({ t: rows[i].t, v: Number.isFinite(delta) && delta >= 0 ? delta / seconds : null });
  }
  return out;
}

/** Bytes per second from the cumulative counters of `/metrics/history`. */
export function throughputSeries(history?: readonly MetricHistoryPoint[] | null): TrendSeries[] | null {
  if (!history) return null;
  return [
    { id: "read", label: "Read", points: rates(history, "read_bytes") },
    { id: "write", label: "Write", points: rates(history, "write_bytes") },
  ];
}

/** Operations per second from the cumulative counters of `/metrics/history`. */
export function iopsSeries(history?: readonly MetricHistoryPoint[] | null): TrendSeries[] | null {
  if (!history) return null;
  return [
    { id: "read", label: "Read", points: rates(history, "read_ops") },
    { id: "write", label: "Write", points: rates(history, "write_ops") },
  ];
}

/**
 * Peak latency (µs) per column of up to two `/io/heatmap` series, e.g. one device's read and
 * write. A column without any I/O has no latency and becomes a gap.
 */
export function latencySeries(series?: readonly IoHeatSeries[] | null): TrendSeries[] | null {
  if (!series) return null;
  return series.slice(0, 2).map((s) => ({
    id: `${s.device}/${s.op}`,
    label: `${s.device} ${s.op} peak`,
    points: [...s.columns]
      .sort((a, b) => a.start_unix - b.start_unix)
      .map((c) => ({ t: c.start_unix * 1000, v: c.count > 0 ? c.peak_us : null })),
  }));
}
