// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import { HardDrive } from "lucide-react";
import {
  activeModuleFromPath,
  canModuleAccess,
  modulesForRole,
  shortcutTargets,
  type NavModule,
} from "./routes";
import { ROLE_ADMIN, ROLE_OPERATOR, ROLE_VIEWER } from "../lib/auth";

describe("routes", () => {
  it("activeModuleFromPath picks longest prefix", () => {
    expect(activeModuleFromPath("/")?.id).toBe("overview");
    expect(activeModuleFromPath("/volumes")?.id).toBe("volumes");
    expect(activeModuleFromPath("/pools/pool-1")?.id).toBe("pool-detail");
    expect(activeModuleFromPath("/databridge/plans")?.id).toBe("migration-plans");
    expect(activeModuleFromPath("/databridge/plans/plan-9")?.id).toBe("plan-detail");
    expect(activeModuleFromPath("/ops-advisor")?.id).toBe("ops-advisor");
  });

  it("modulesForRole hides operator and admin entries from viewers", () => {
    const viewerIds = new Set(modulesForRole(ROLE_VIEWER).map((m) => m.id));
    expect(viewerIds.has("overview")).toBe(true);
    expect(viewerIds.has("volumes")).toBe(false);
    expect(viewerIds.has("tenants")).toBe(false);
    expect(viewerIds.has("ops-advisor")).toBe(false);
  });

  it("canModuleAccess respects minRole on detail routes", () => {
    const pool: NavModule = {
      id: "pool-detail",
      codename: "pool",
      label: "Pool",
      path: "/pools/:id",
      icon: HardDrive,
      section: "STORAGE",
      minRole: "operator",
    };
    expect(canModuleAccess(pool, ROLE_VIEWER)).toBe(false);
    expect(canModuleAccess(pool, ROLE_OPERATOR)).toBe(true);
  });

  it("shortcutTargets only includes pinned shortcuts the role can reach", () => {
    const viewer = shortcutTargets(ROLE_VIEWER);
    expect(viewer.get("h")).toBe("/");
    expect(viewer.has("v")).toBe(false);
    expect(viewer.has("g")).toBe(false);

    const admin = shortcutTargets(ROLE_ADMIN);
    expect(admin.get("g")).toBe("/settings");
    expect(admin.get("v")).toBe("/volumes");
  });
});
