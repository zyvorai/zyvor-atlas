// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
/**
 * Appearance. The preference is "system" (follow the OS, the default), "light" or "dark"; the
 * resolved theme is written to <html data-theme="dark|light">, which design/tokens.css selects on.
 */

export type ThemePref = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

export const THEME_STORAGE_KEY = "atlas.theme";

export const THEME_OPTIONS: { id: ThemePref; title: string; hint: string }[] = [
  { id: "system", title: "System", hint: "Match this device" },
  { id: "light", title: "Light", hint: "White and soft grey" },
  { id: "dark", title: "Dark", hint: "True black" },
];

/** Values written by earlier versions of the console. */
const LEGACY: Record<string, ThemePref> = {
  night: "dark",
  carbon: "dark",
  day: "light",
  "apple-lite": "light",
};

export function parseThemePref(raw: string | null | undefined): ThemePref {
  if (raw === "light" || raw === "dark" || raw === "system") return raw;
  return (raw && LEGACY[raw]) || "system";
}

export function themeTitle(id: ThemePref): string {
  return THEME_OPTIONS.find((t) => t.id === id)?.title ?? id;
}

function darkQuery(): MediaQueryList | null {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return null;
  return window.matchMedia("(prefers-color-scheme: dark)");
}

export function resolveTheme(pref: ThemePref): ResolvedTheme {
  if (pref !== "system") return pref;
  return darkQuery()?.matches ? "dark" : "light";
}

export function applyTheme(pref: ThemePref) {
  if (typeof document !== "undefined") document.documentElement.dataset.theme = resolveTheme(pref);
}

/** Calls `onChange` whenever the OS switches between light and dark. Returns an unsubscribe. */
export function watchSystemTheme(onChange: () => void): () => void {
  const q = darkQuery();
  if (!q?.addEventListener) return () => {};
  q.addEventListener("change", onChange);
  return () => q.removeEventListener("change", onChange);
}
