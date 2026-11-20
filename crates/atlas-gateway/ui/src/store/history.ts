// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Client-side rolling history of /metrics/summary so the Command Deck can draw live trends
// (the gateway keeps only point-in-time snapshots; real time-series lives in Prometheus).
import type { MetricHistoryPoint, MetricsSummary } from "../api/types";

export interface Sample {
  t: number;
  usedPct: number;
  usedBytes: number;
  readIops: number;
  writeIops: number;
}

const MAX = 60;
let buf: Sample[] = [];
let last: { t: number; r: number; w: number } | null = null;
const subs = new Set<() => void>();

export function recordSummary(s: MetricsSummary) {
  const now = Date.now();
  const r = s.client_io?.read_ops_total || 0;
  const w = s.client_io?.write_ops_total || 0;
  let readIops = 0;
  let writeIops = 0;
  if (last) {
    const dt = Math.max(1, (now - last.t) / 1000);
    readIops = Math.max(0, (r - last.r) / dt);
    writeIops = Math.max(0, (w - last.w) / dt);
  }
  last = { t: now, r, w };
  const sample: Sample = { t: now, usedPct: s.used_capacity_percent || 0, usedBytes: s.used_capacity_bytes || 0, readIops, writeIops };
  // De-dupe rapid identical samples (react-query may re-emit).
  if (buf.length && now - buf[buf.length - 1].t < 800) buf[buf.length - 1] = sample;
  else buf = [...buf, sample].slice(-MAX);
  subs.forEach((f) => f());
}

// Seed the rolling buffer once from the gateway's persisted time-series so trends are populated
// immediately on load (and survive reloads), instead of drawing from an empty client-side buffer.
export function seed(points: MetricHistoryPoint[]) {
  if (buf.length || !points.length) return; // only when empty — never clobber live samples
  const out: Sample[] = [];
  let prev: { t: number; r: number; w: number } | null = null;
  for (const p of points) {
    const t = Date.parse(p.ts);
    const usedPct = p.raw_capacity_bytes > 0 ? (p.used_capacity_bytes / p.raw_capacity_bytes) * 100 : 0;
    let readIops = 0;
    let writeIops = 0;
    if (prev) {
      const dt = Math.max(1, (t - prev.t) / 1000);
      readIops = Math.max(0, (p.read_ops - prev.r) / dt);
      writeIops = Math.max(0, (p.write_ops - prev.w) / dt);
    }
    prev = { t, r: p.read_ops, w: p.write_ops };
    out.push({ t, usedPct, usedBytes: p.used_capacity_bytes, readIops, writeIops });
  }
  buf = out.slice(-MAX);
  if (prev) last = prev; // prime the delta baseline so the next live sample is continuous
  subs.forEach((f) => f());
}

export function history(): Sample[] {
  return buf;
}
export function onHistory(fn: () => void): () => void {
  subs.add(fn);
  return () => {
    subs.delete(fn);
  };
}
