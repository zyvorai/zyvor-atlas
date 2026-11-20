// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export const cx = (...a: ClassValue[]) => twMerge(clsx(a));

export function fmtBytes(n?: number | null): string {
  const v = Number(n) || 0;
  const u = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = 0;
  let x = v;
  while (x >= 1024 && i < u.length - 1) {
    x /= 1024;
    i++;
  }
  return `${i ? x.toFixed(1) : x} ${u[i]}`;
}

/** Like fmtBytes, but unknown/null capacity shows as an em dash instead of a misleading "0 B". */
export function fmtBytesOpt(n?: number | null): string {
  if (n == null || Number.isNaN(Number(n))) return "—";
  return fmtBytes(n);
}

/** Split fmtBytes into magnitude + unit for big readouts; null when capacity is unknown. */
export function fmtBytesParts(n?: number | null): { mag: string; unit: string } | null {
  if (n == null || Number.isNaN(Number(n))) return null;
  const s = fmtBytes(n);
  const sp = s.lastIndexOf(" ");
  if (sp < 0) return { mag: s, unit: "" };
  return { mag: s.slice(0, sp), unit: s.slice(sp + 1) };
}

/**
 * Actionable fill projection for Command Deck / Observatory.
 * Tiny growth rates produce absurd horizons (hundreds of thousands of days) — treat those as steady.
 */
export function fmtForecastFill(
  days: number | null | undefined,
  growthBytesPerDay?: number | null,
): string | null {
  if (days == null || !Number.isFinite(days) || days <= 0) return null;
  // > ~10 years is not an operator-actionable signal
  if (days > 3650) return null;
  // Sub-MiB/day drift is noise on Ceph labs — don't claim a fill date.
  if (growthBytesPerDay != null && growthBytesPerDay > 0 && growthBytesPerDay < 1024 * 1024) {
    return null;
  }
  const growth =
    growthBytesPerDay != null && growthBytesPerDay >= 1024 * 1024
      ? ` · +${fmtBytes(growthBytesPerDay)}/day`
      : "";
  if (days >= 365) return `Full in ~${(days / 365).toFixed(1)}y${growth}`;
  if (days >= 45) return `Full in ~${Math.round(days)}d${growth}`;
  return `Full in ~${days < 10 ? days.toFixed(1) : Math.round(days)}d${growth}`;
}

export function forecastUrgency(
  days: number | null | undefined,
): "danger" | "warning" | "muted" | null {
  if (days == null || !Number.isFinite(days) || days <= 0 || days > 3650) return null;
  if (days <= 3) return "danger";
  if (days <= 14) return "warning";
  return "muted";
}

export const num = (n?: number | null) => (Number(n) || 0).toLocaleString();

/** SI compaction for machine counters (ops, objects) — never locale digit grouping. */
export function fmtSi(n?: number | null, digits = 1): string {
  const v = Number(n) || 0;
  const abs = Math.abs(v);
  if (abs < 1000) return String(Math.round(v));
  const units = ["K", "M", "B", "T"];
  let x = abs;
  let i = -1;
  while (x >= 1000 && i < units.length - 1) {
    x /= 1000;
    i++;
  }
  const body = x >= 100 ? x.toFixed(0) : x.toFixed(digits);
  return `${v < 0 ? "-" : ""}${body}${units[i]}`;
}

/** Integer percent for depth readouts. */
export function fmtPct(n?: number | null): number {
  const v = Number(n);
  if (!Number.isFinite(v)) return 0;
  return Math.max(0, Math.min(100, Math.round(v)));
}

export function timeAgo(iso?: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso).getTime();
  if (Number.isNaN(d)) return iso;
  const s = Math.floor((Date.now() - d) / 1000);
  // A future timestamp (e.g. a schedule's next_run_at) makes `s` negative — "-3595s ago" instead
  // of "in 59m" otherwise, since every unit branch below treated any s < 60 as "just now, past".
  if (s < 0) {
    const f = -s;
    if (f < 60) return `in ${f}s`;
    if (f < 3600) return `in ${Math.floor(f / 60)}m`;
    if (f < 86400) return `in ${Math.floor(f / 3600)}h`;
    return `in ${Math.floor(f / 86400)}d`;
  }
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export const gib = (g: number) => g * 1024 * 1024 * 1024;

export function healthKind(h?: string): "success" | "warning" | "danger" | "neutral" {
  if (h === "ok") return "success";
  if (h === "warn" || h === "warning") return "warning";
  if (h === "critical" || h === "unauthenticated" || h === "auth") return "danger";
  return "neutral";
}

export function stateKind(s?: string): "success" | "warning" | "danger" | "info" | "neutral" {
  const ok = ["succeeded", "verified", "bound", "available", "ok", "completed", "ready"];
  const bad = ["failed", "critical", "error", "lost"];
  const run = ["running", "queued", "pending", "verifying", "creating", "progressing"];
  if (ok.includes(s || "")) return "success";
  if (bad.includes(s || "")) return "danger";
  if (run.includes(s || "")) return "info";
  if (s === "open" || s === "warn" || s === "warning") return "warning";
  return "neutral";
}
