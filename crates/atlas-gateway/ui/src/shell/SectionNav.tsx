// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Desktop navigation: one slim row of sections, each opening a full-width flyout, plus the
// section's own local nav underneath. The narrow-viewport equivalents are MobileJumpNav and the
// navigation sheet in Shell.tsx.
import { useEffect, useMemo, useRef, useState } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { ChevronDown } from "lucide-react";
import { activeModuleFromPath, navSections, type NavModule, type SectionId } from "../nav/routes";
import { useUi } from "../store/ui";
import { cx } from "../lib/format";

const OVERVIEW_ID = "overview";

type OpenState = { sec: SectionId; pinned: boolean; path: string } | null;

export function PrimaryNav() {
  const roleLevel = useUi((s) => s.roleLevel);
  const { pathname } = useLocation();
  const sections = useMemo(() => navSections(roleLevel), [roleLevel]);
  const active = activeModuleFromPath(pathname);
  const activeSec = active && active.id !== OVERVIEW_ID ? active.section : null;
  const [state, setState] = useState<OpenState>(null);
  // A flyout belongs to the page it was opened on: navigating away closes it.
  const open = state && state.path === pathname ? state : null;
  const navRef = useRef<HTMLElement>(null);
  const timer = useRef<number | undefined>(undefined);

  const cancel = () => window.clearTimeout(timer.current);
  const later = (fn: () => void, ms: number) => {
    cancel();
    timer.current = window.setTimeout(fn, ms);
  };
  const close = () => {
    cancel();
    setState(null);
  };
  useEffect(() => cancel, []);

  const isOpen = open !== null;
  useEffect(() => {
    if (!isOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const nav = navRef.current;
      if (nav?.contains(document.activeElement)) {
        nav.querySelector<HTMLButtonElement>('button[aria-expanded="true"]')?.focus();
      }
      setState(null);
    };
    const onPointer = (e: PointerEvent) => {
      if (!navRef.current?.contains(e.target as Node)) setState(null);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onPointer);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onPointer);
    };
  }, [isOpen]);

  const focusFirstLink = () =>
    window.setTimeout(() => navRef.current?.querySelector<HTMLAnchorElement>(".at-flyout a")?.focus(), 0);

  return (
    <>
      <nav
        ref={navRef}
        className="at-top-nav"
        aria-label="Primary"
        onPointerLeave={(e) => {
          if (e.pointerType === "mouse") later(() => setState(null), 240);
        }}
        onPointerEnter={cancel}
        onBlur={(e) => {
          const next = e.relatedTarget as Node | null;
          if (next && !e.currentTarget.contains(next)) close();
        }}
      >
        <div className="at-nav-item" onPointerEnter={close}>
          <NavLink to="/" end className={({ isActive }) => cx("at-top-link", isActive && "on")}>
            Overview
          </NavLink>
        </div>
        {sections.map((g) => {
          const expanded = open?.sec === g.sec;
          const panelId = `at-flyout-${g.sec.replace(/\s+/g, "-").toLowerCase()}`;
          const shortcuts = g.items.filter((m) => m.pinned && m.shortcut);
          return (
            <div key={g.sec} className="at-nav-item">
              <button
                type="button"
                className={cx("at-top-link", activeSec === g.sec && "on", expanded && "open")}
                aria-expanded={expanded}
                aria-controls={expanded ? panelId : undefined}
                onPointerEnter={(e) => {
                  if (e.pointerType !== "mouse") return;
                  later(
                    () => setState((s) => (s?.sec === g.sec ? s : { sec: g.sec, pinned: false, path: pathname })),
                    open ? 70 : 160,
                  );
                }}
                onClick={() => {
                  cancel();
                  setState(expanded && open?.pinned ? null : { sec: g.sec, pinned: true, path: pathname });
                }}
                onKeyDown={(e) => {
                  if (e.key !== "ArrowDown") return;
                  e.preventDefault();
                  setState({ sec: g.sec, pinned: true, path: pathname });
                  focusFirstLink();
                }}
              >
                {g.short}
                <ChevronDown size={11} strokeWidth={2.5} aria-hidden />
              </button>
              {expanded && (
                <div id={panelId} className="at-flyout" role="group" aria-label={`${g.short} menu`}>
                  <div className="at-flyout-inner">
                    <div className="at-flyout-main">
                      <div className="at-flyout-label">{g.short}</div>
                      {g.items.map((m, i) => (
                        <FlyoutLink key={m.id} module={m} delay={i * 22} onClick={close} />
                      ))}
                    </div>
                    {shortcuts.length > 0 && (
                      <div className="at-flyout-aside">
                        <div className="at-flyout-label">Keyboard</div>
                        {shortcuts.map((m) => (
                          <span key={m.id}>
                            <kbd className="at-kbd">{m.shortcut}</kbd>
                            {m.label}
                          </span>
                        ))}
                      </div>
                    )}
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </nav>
      {open ? <div className="at-flyout-scrim" aria-hidden onPointerDown={close} /> : null}
    </>
  );
}

function FlyoutLink({ module: m, delay, onClick }: { module: NavModule; delay: number; onClick: () => void }) {
  return (
    <NavLink
      to={m.path}
      end={m.path === "/"}
      className={({ isActive }) => (isActive ? "on" : "")}
      style={{ animationDelay: `${delay}ms` }}
      onClick={onClick}
    >
      {m.label}
    </NavLink>
  );
}

/** The current section's pages, one click apart. Hidden on Overview. */
export function LocalNav() {
  const roleLevel = useUi((s) => s.roleLevel);
  const { pathname } = useLocation();
  const active = activeModuleFromPath(pathname);
  const group = useMemo(
    () => (active ? navSections(roleLevel).find((g) => g.sec === active.section) : undefined),
    [active, roleLevel],
  );
  if (!active || active.id === OVERVIEW_ID || !group) return null;
  return (
    <div className="at-localnav">
      <div className="at-localnav-inner">
        <span className="at-localnav-title">{group.short}</span>
        <nav className="at-localnav-links" aria-label={`${group.short} pages`}>
          {group.items.map((m) => (
            <NavLink key={m.id} to={m.path} className={({ isActive }) => (isActive ? "on" : "")}>
              {m.label}
            </NavLink>
          ))}
        </nav>
      </div>
    </div>
  );
}
