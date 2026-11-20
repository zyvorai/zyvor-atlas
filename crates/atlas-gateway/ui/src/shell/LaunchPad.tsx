// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Atlas Launch Pad — Mission Control analog: sectioned app grid over the canvas. */
import { useEffect, useMemo } from "react";
import { createPortal } from "react-dom";
import { useNavigate } from "react-router-dom";
import { X } from "lucide-react";
import { modulesForRole, SECTION_META, SECTIONS } from "../nav/routes";
import { NavAppIcon } from "../nav/NavAppIcon";
import { useUi } from "../store/ui";
import { cx } from "../lib/format";

export function LaunchPad({ open, onClose }: { open: boolean; onClose: () => void }) {
  const nav = useNavigate();
  const roleLevel = useUi((s) => s.roleLevel);
  const modules = useMemo(() => modulesForRole(roleLevel), [roleLevel]);
  const groups = useMemo(
    () =>
      SECTIONS.map((sec) => ({
        sec,
        label: SECTION_META[sec].short,
        items: modules.filter((m) => m.section === sec),
      })).filter((g) => g.items.length > 0),
    [modules],
  );

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return createPortal(
    <div className="at-launchpad-scrim" onMouseDown={onClose}>
      <div
        className="at-launchpad"
        role="dialog"
        aria-modal="true"
        aria-label="Launch pad"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="at-launchpad-head">
          <div>
            <div className="at-launchpad-kicker">Atlas</div>
            <h2 className="at-launchpad-title">Launch pad</h2>
          </div>
          <button type="button" className="at-iconbtn" aria-label="Close launch pad" onClick={onClose}>
            <X size={16} />
          </button>
        </div>
        <div className="at-launchpad-body">
          {groups.map((g) => (
            <section key={g.sec} className="at-launchpad-section">
              <h3 className="at-launchpad-section-title">{g.label}</h3>
              <div className="at-launchpad-grid">
                {g.items.map((m) => (
                  <button
                    key={m.id}
                    type="button"
                    className="at-launchpad-tile"
                    onClick={() => {
                      nav(m.path);
                      onClose();
                    }}
                  >
                    <NavAppIcon icon={m.icon} section={m.section} size={22} className="at-appicon-lg" />
                    <span className="at-launchpad-tile-label">{m.label}</span>
                  </button>
                ))}
              </div>
            </section>
          ))}
        </div>
        <p className={cx("at-launchpad-hint")}>Esc to close · ⌘⌥L / Ctrl-Alt-L</p>
      </div>
    </div>,
    document.body,
  );
}
