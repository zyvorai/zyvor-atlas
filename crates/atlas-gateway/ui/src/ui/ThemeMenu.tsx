// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useState } from "react";
import { Check, Monitor, Moon, Sun } from "lucide-react";
import { THEME_OPTIONS, themeTitle, type ThemePref } from "../lib/themes";
import { useUi } from "../store/ui";
import { cx } from "../lib/format";
import { useDismiss } from "./useDismiss";

const ICONS: Record<ThemePref, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };

/** Appearance picker: follow the OS, or override it. Used by the global nav and the sign-in page. */
export function ThemeMenu() {
  const theme = useUi((s) => s.theme);
  const setTheme = useUi((s) => s.setTheme);
  const [open, setOpen] = useState(false);
  const ref = useDismiss<HTMLDivElement>(open, () => setOpen(false));
  const Icon = ICONS[theme];

  return (
    <div className="relative" ref={ref}>
      <button
        type="button"
        className="at-iconbtn"
        title={`Appearance: ${themeTitle(theme)}`}
        aria-label="Change theme"
        aria-expanded={open}
        aria-haspopup="menu"
        onClick={() => setOpen((v) => !v)}
      >
        <Icon size={16} strokeWidth={2} aria-hidden />
      </button>
      {open && (
        <div className="at-theme-menu" role="menu" aria-label="Appearance">
          <div className="at-theme-menu-label">Appearance</div>
          {THEME_OPTIONS.map((opt) => {
            const OptIcon = ICONS[opt.id];
            return (
              <button
                key={opt.id}
                type="button"
                role="menuitemradio"
                aria-checked={theme === opt.id}
                className={cx("at-theme-item", theme === opt.id && "on")}
                onClick={() => {
                  setTheme(opt.id);
                  setOpen(false);
                }}
              >
                <OptIcon size={15} aria-hidden />
                <span className="grow">{opt.title}</span>
                {theme === opt.id ? <Check size={15} aria-hidden /> : null}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
