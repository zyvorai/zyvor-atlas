// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Small formatting helpers local to the graphics, so they stay self-contained.

/** A finite, non-negative number: the only kind of measurement the graphics draw. */
export function isMeasure(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0;
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

export function formatBytes(v: number): string {
  let x = v;
  let i = 0;
  while (x >= 1024 && i < UNITS.length - 1) {
    x /= 1024;
    i++;
  }
  const body = i === 0 || Number.isInteger(x) ? String(Math.round(x)) : x.toFixed(1);
  return `${body} ${UNITS[i]}`;
}

/** Whole percent that never rounds a non-empty value to 0% or a non-full one to 100%. */
export function formatPercent(pct: number): string {
  if (pct > 0 && pct < 1) return "<1%";
  if (pct > 99 && pct < 100) return "99%";
  return `${Math.round(pct)}%`;
}

/** Fill level in percent, or `null` when it cannot be known from the inputs. */
export function percentOf(used?: number | null, capacity?: number | null): number | null {
  if (!isMeasure(used) || !isMeasure(capacity) || capacity === 0) return null;
  return (used / capacity) * 100;
}

export type FillLevel = "normal" | "warn" | "fail";

export interface FillThresholds {
  /** Percent at which a fill is flagged "Filling up". Default 70. */
  warnAt?: number;
  /** Percent at which a fill is flagged "Nearly full". Default 85. */
  failAt?: number;
}

export function fillLevel(pct: number, { warnAt = 70, failAt = 85 }: FillThresholds = {}): FillLevel {
  if (pct >= failAt) return "fail";
  if (pct >= warnAt) return "warn";
  return "normal";
}

export const FILL_WORD: Record<Exclude<FillLevel, "normal">, string> = {
  warn: "Filling up",
  fail: "Nearly full",
};

export function formatClock(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}
