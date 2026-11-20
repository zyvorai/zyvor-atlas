// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useCallback, useMemo, useState } from "react";
import { useLocation } from "react-router-dom";
import {
  SECTION_META,
  SECTIONS,
  modulesForRole,
  type NavModule,
  type SectionId,
} from "./routes";

const SECTION_COLLAPSE_PREFIX = "atlas.sidebar-section-";

function loadSectionCollapsed(id: string): boolean {
  try {
    const raw = localStorage.getItem(`${SECTION_COLLAPSE_PREFIX}${id}`);
    if (raw === "0") return false;
    if (raw === "1") return true;
  } catch {
    /* ignore */
  }
  return false;
}

export function isModuleActive(module: NavModule, pathname: string): boolean {
  if (module.path === "/") return pathname === "/";
  return pathname === module.path || pathname.startsWith(`${module.path}/`);
}

export type NavGroup = {
  sec: SectionId;
  short: string;
  collapsible: boolean;
  items: NavModule[];
};

export function useNavGroups(actorLevel: number) {
  const { pathname } = useLocation();
  const [filter, setFilter] = useState("");
  const [sectionCollapsed, setSectionCollapsed] = useState<Record<string, boolean>>(() =>
    Object.fromEntries(SECTIONS.map((sec) => [sec, loadSectionCollapsed(sec)])),
  );

  const visibleModules = useMemo(() => modulesForRole(actorLevel), [actorLevel]);
  const filterNorm = filter.trim().toLowerCase();

  const grouped = useMemo((): NavGroup[] => {
    return SECTIONS.map((sec) => ({
      sec,
      short: SECTION_META[sec].short,
      collapsible: SECTION_META[sec].collapsible,
      items: visibleModules.filter(
        (m) =>
          m.section === sec &&
          (!filterNorm || m.label.toLowerCase().includes(filterNorm) || m.section.toLowerCase().includes(filterNorm)),
      ),
    })).filter((g) => g.items.length > 0);
  }, [visibleModules, filterNorm]);

  const isSectionClosed = useCallback(
    (group: NavGroup): boolean => {
      const hasActive = group.items.some((m) => isModuleActive(m, pathname));
      if (hasActive || filterNorm) return false;
      if (!group.collapsible) return false;
      return Boolean(sectionCollapsed[group.sec]);
    },
    [pathname, filterNorm, sectionCollapsed],
  );

  const toggleSection = useCallback((sec: SectionId) => {
    setSectionCollapsed((prev) => {
      const next = !prev[sec];
      try {
        localStorage.setItem(`${SECTION_COLLAPSE_PREFIX}${sec}`, next ? "1" : "0");
      } catch {
        /* ignore */
      }
      return { ...prev, [sec]: next };
    });
  }, []);

  const spotlightModules = useMemo(() => visibleModules, [visibleModules]);

  return {
    grouped,
    filter,
    setFilter,
    filterNorm,
    isSectionClosed,
    toggleSection,
    spotlightModules,
    visibleModules,
  };
}
