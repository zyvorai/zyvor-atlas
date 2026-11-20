// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useMemo } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { Compass } from "lucide-react";
import { activeModuleFromPath, modulesForRole, SECTION_META, SECTIONS } from "../nav/routes";
import { useUi } from "../store/ui";

/** Narrow-viewport jump select (h2kvm DesktopMobileJumpNav pattern). */
export function MobileJumpNav() {
  const loc = useLocation();
  const nav = useNavigate();
  const roleLevel = useUi((s) => s.roleLevel);
  const modules = useMemo(() => modulesForRole(roleLevel), [roleLevel]);
  const active = activeModuleFromPath(loc.pathname);
  const value = active && !active.hiddenFromNav ? active.id : modules[0]?.id ?? "overview";
  const groupLabel = active ? SECTION_META[active.section].short : "Navigate";

  const groups = useMemo(() => {
    return SECTIONS.map((sec) => ({
      sec,
      label: SECTION_META[sec].short,
      items: modules.filter((m) => m.section === sec),
    })).filter((g) => g.items.length > 0);
  }, [modules]);

  return (
    <nav className="at-mobile-jump" aria-label="Jump navigation">
      <label htmlFor="atlas-mobile-jump" className="sr-only">
        Jump to page
      </label>
      <Compass size={16} className="at-mobile-jump-icon" aria-hidden />
      <select
        id="atlas-mobile-jump"
        className="at-mobile-jump-select"
        value={value}
        onChange={(e) => {
          const m = modules.find((x) => x.id === e.target.value);
          if (m) nav(m.path);
        }}
      >
        {groups.map((g) => (
          <optgroup key={g.sec} label={g.label}>
            {g.items.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
              </option>
            ))}
          </optgroup>
        ))}
      </select>
      <span className="at-mobile-jump-group">{groupLabel}</span>
    </nav>
  );
}
