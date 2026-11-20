// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { beforeEach, describe, expect, it, vi } from "vitest";
import { filterNavRecents, getNavRecents, recordNavRecent } from "./navRecents";

const store = new Map<string, string>();

describe("navRecents", () => {
  beforeEach(() => {
    store.clear();
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => {
        store.set(k, v);
      },
      removeItem: (k: string) => {
        store.delete(k);
      },
      clear: () => {
        store.clear();
      },
    });
  });

  it("recordNavRecent keeps newest five unique ids", () => {
    recordNavRecent("volumes", "Volumes");
    recordNavRecent("ceph", "Ceph");
    recordNavRecent("volumes", "Volumes");
    recordNavRecent("jobs", "Jobs");
    recordNavRecent("alerts", "Alerts");
    recordNavRecent("audit", "Audit");
    recordNavRecent("metrics", "Metrics");
    const recents = getNavRecents();
    expect(recents.map((r) => r.id)).toEqual(["metrics", "audit", "alerts", "jobs", "volumes"]);
  });

  it("filterNavRecents drops entries the role cannot open", () => {
    recordNavRecent("volumes", "Volumes");
    recordNavRecent("overview", "Overview");
    const filtered = filterNavRecents(getNavRecents(), new Set(["overview"]));
    expect(filtered).toEqual([]);
  });
});
