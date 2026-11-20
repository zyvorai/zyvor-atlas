// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

export const NAV_RECENTS_KEY = "atlas.nav-recents";
export const MAX_RECENTS = 5;

export type NavRecent = { id: string; label: string };

export function loadNavRecents(): NavRecent[] {
  try {
    const raw = localStorage.getItem(NAV_RECENTS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as NavRecent[];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

export function persistNavRecents(recents: NavRecent[]): void {
  try {
    localStorage.setItem(NAV_RECENTS_KEY, JSON.stringify(recents.slice(0, MAX_RECENTS)));
  } catch {
    /* ignore quota */
  }
}

export function nextNavRecents(prev: NavRecent[], id: string, label: string): NavRecent[] {
  if (!id || id === "overview") return prev;
  return [{ id, label }, ...prev.filter((r) => r.id !== id)].slice(0, MAX_RECENTS);
}

/** @deprecated prefer store — kept for tests */
export function recordNavRecent(id: string, label: string): void {
  persistNavRecents(nextNavRecents(loadNavRecents(), id, label));
}

/** @deprecated prefer store */
export function getNavRecents(): NavRecent[] {
  return loadNavRecents();
}

export function filterNavRecents(recents: NavRecent[], validIds: ReadonlySet<string>): NavRecent[] {
  return recents.filter((r) => validIds.has(r.id));
}
