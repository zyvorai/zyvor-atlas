// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Atlas shell: slim translucent global nav with section flyouts, a section local nav, and a
// full-screen navigation sheet on narrow viewports.
import { useEffect, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";
import {
  Archive,
  Bell,
  Camera,
  CircleUserRound,
  Cloud,
  HardDrive as HardDriveIcon,
  LayoutGrid,
  Loader2,
  LogOut,
  Menu,
  Pause,
  Play,
  Search,
  Settings as SettingsIcon,
  X,
  type LucideIcon,
} from "lucide-react";
import {
  activeModuleFromPath,
  modulesForRole,
  navLabelForPath,
  shortcutTargets,
} from "../nav/routes";
import { useNavGroups } from "../nav/useNavGroups";
import { NavFilter, NavSectionLinks, NavRecents } from "./NavPanel";
import { MobileJumpNav } from "./MobileJumpNav";
import { LocalNav, PrimaryNav } from "./SectionNav";
import { LaunchPad } from "./LaunchPad";
import { filterNavRecents } from "../lib/navRecents";
import { roleLabel, ROLE_ADMIN, ROLE_OPERATOR } from "../lib/auth";
import { http, isUnauthorized } from "../api/client";
import { useAlerts, useCephHealthRollup, useClusters, useJobs } from "../api/hooks";
import { useUi } from "../store/ui";
import { cx } from "../lib/format";
import { onSendPrompt } from "../lib/prompts";
import { Button, Field, Modal } from "../ui/kit";
import { ThemeMenu } from "../ui/ThemeMenu";
import { useDismiss } from "../ui/useDismiss";

function Clock() {
  const [t, setT] = useState(new Date());
  useEffect(() => {
    const id = setInterval(() => setT(new Date()), 1000);
    return () => clearInterval(id);
  }, []);
  return (
    <span className="at-theme-hint mono">
      {t.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
    </span>
  );
}

function MenuBar({
  onSpotlight,
  onOpenNav,
  onLaunchPad,
}: {
  onSpotlight: () => void;
  onOpenNav: () => void;
  onLaunchPad: () => void;
}) {
  const { data: clusters, isError: clustersErrored, error: clustersError } = useClusters();
  const { data: jobs } = useJobs();
  const { data: openAlerts } = useAlerts("open");
  const { data: rollup, isError: rollupErrored } = useCephHealthRollup();
  const runningJobs = (jobs || []).filter((j) =>
    ["running", "queued", "verifying", "pending"].includes(j.state),
  );
  const authFailed = clustersErrored && isUnauthorized(clustersError);
  const unreachable = clustersErrored && !authFailed && !clusters;
  const h = authFailed
    ? "unauthenticated"
    : unreachable
      ? "unreachable"
      : !rollupErrored && rollup?.state
        ? rollup.state
        : clusters?.[0]?.health || "unknown";
  const healthWhy = useMemo(() => {
    if (h === "unauthenticated") return "API requests rejected — check your session";
    if (h === "unreachable") return "Gateway unreachable — retrying";
    if (h === "ok" || h === "healthy" || h === "unknown") return undefined;
    return (
      rollup?.summary ||
      openAlerts?.[0]?.title ||
      openAlerts?.[0]?.description ||
      (h === "critical" ? "Cluster health critical" : "Cluster health degraded")
    );
  }, [h, openAlerts, rollup]);
  const healthClass =
    h === "ok" || h === "healthy"
      ? "ok"
      : h === "warn" || h === "degraded"
        ? "warn"
        : h === "rebuilding"
          ? "info"
          : h === "at_risk"
            ? "at-risk"
            : h === "critical" || h === "unauthenticated"
              ? "crit"
              : "muted";
  const healthLabel =
    h === "ok"
      ? "HEALTH_OK"
      : h === "healthy"
        ? "HEALTHY"
        : h === "warn"
          ? "HEALTH_WARN"
          : h === "degraded"
            ? "DEGRADED"
            : h === "rebuilding"
              ? "REBUILDING"
              : h === "at_risk"
                ? "AT RISK"
                : h === "critical"
                  ? "HEALTH_ERR"
                  : h === "unauthenticated"
                    ? "AUTH"
                    : h === "unreachable"
                      ? "UNREACHABLE"
                      : String(h).toUpperCase();

  const [tokenOpen, setTokenOpen] = useState(false);
  const [panel, setPanel] = useState<"jobs" | "bell" | "account" | null>(null);
  const toggle = (p: "jobs" | "bell" | "account") => setPanel((v) => (v === p ? null : p));
  const trayRef = useDismiss<HTMLDivElement>(panel !== null, () => setPanel(null));
  const token = useUi((s) => s.token);
  const role = useUi((s) => s.role);
  const roleLevel = useUi((s) => s.roleLevel);
  const setToken = useUi((s) => s.setToken);
  const paused = useUi((s) => s.paused);
  const togglePaused = useUi((s) => s.togglePaused);
  const [draft, setDraft] = useState(token);
  useEffect(() => {
    // Re-seed the editable draft from the real token each time the editor opens — intentional
    // reset-on-open, not a state sync loop.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    if (tokenOpen) setDraft(token);
  }, [tokenOpen, token]);
  const nav = useNavigate();
  const go = (path: string) => {
    nav(path);
    setPanel(null);
  };

  return (
    <header className="at-rail">
      <div className="at-rail-inner">
        <div className="at-rail-brand">
          <button
            type="button"
            className="at-iconbtn at-menu-burger"
            title="Open navigation"
            aria-label="Open navigation"
            onClick={onOpenNav}
          >
            <Menu size={18} strokeWidth={2} aria-hidden />
          </button>

          <NavLink to="/" end className="at-mark" title="Atlas — Storage Center" aria-label="Atlas — Storage Center">
            <span className="at-brand-logo" aria-hidden>
              <img src="/zyvor-mark.svg" alt="" width={16} height={16} />
            </span>
            <span className="at-brand-name">Atlas</span>
          </NavLink>
        </div>

        <PrimaryNav />

        <div className="at-rail-actions" ref={trayRef}>
          <div className="at-rail-tray">
            <button
              type="button"
              className="at-iconbtn"
              title="Search — ⌘K / Ctrl+K"
              aria-label="Search pools, volumes, hosts"
              onClick={onSpotlight}
            >
              <Search size={16} strokeWidth={2} aria-hidden />
            </button>

            <button
              type="button"
              className="at-iconbtn at-desk-only"
              title="Launch pad — ⌘⌥L"
              aria-label="Open launch pad"
              onClick={onLaunchPad}
            >
              <LayoutGrid size={16} strokeWidth={2} aria-hidden />
            </button>

            <div className="contents" onClickCapture={() => setPanel(null)}>
              <ThemeMenu />
            </div>

            {runningJobs.length > 0 && (
              <button
                type="button"
                className="at-iconbtn at-desk-only"
                title={`${runningJobs.length} job(s) running`}
                aria-label={`${runningJobs.length} job(s) running`}
                aria-expanded={panel === "jobs"}
                onClick={() => toggle("jobs")}
              >
                <Loader2 size={16} className="animate-spin" strokeWidth={2} aria-hidden />
              </button>
            )}

            <button
              type="button"
              className="at-iconbtn"
              title="Alerts"
              aria-label="Open alerts"
              aria-expanded={panel === "bell"}
              onClick={() => toggle("bell")}
            >
              <Bell size={16} strokeWidth={2} aria-hidden />
              {(openAlerts?.length || 0) > 0 && (
                <span className="at-badge">{Math.min(99, openAlerts!.length)}</span>
              )}
            </button>

            <button
              type="button"
              className="at-iconbtn at-desk-only"
              title={paused ? "Resume live telemetry" : "Pause live telemetry"}
              aria-label={paused ? "Resume live telemetry" : "Pause live telemetry"}
              aria-pressed={paused}
              onClick={togglePaused}
            >
              {paused ? <Play size={16} strokeWidth={2} aria-hidden /> : <Pause size={16} strokeWidth={2} aria-hidden />}
            </button>

            <button
              type="button"
              className={cx("at-health", healthClass)}
              title={healthWhy || healthLabel}
              onClick={() => nav("/ceph")}
            >
              <span className="dot" aria-hidden />
              <span className="at-health-label">{healthLabel}</span>
            </button>

            <button
              type="button"
              className="at-iconbtn"
              title="Account"
              aria-label="Account"
              aria-expanded={panel === "account"}
              onClick={() => toggle("account")}
            >
              <CircleUserRound size={17} strokeWidth={2} aria-hidden />
            </button>
          </div>

          {panel === "account" && (
            <div className="at-theme-menu at-rail-flyout" role="menu" aria-label="Control Center">
              <div className="at-theme-menu-label">Control Center</div>
              <div className="at-theme-item">
                <span>{roleLabel(role)}</span>
                <Clock />
              </div>
              <button type="button" className="at-theme-item" onClick={() => { onLaunchPad(); setPanel(null); }}>
                Launch pad
              </button>
              <button type="button" className="at-theme-item" onClick={() => { togglePaused(); setPanel(null); }}>
                {paused ? "Resume live telemetry" : "Pause live telemetry"}
              </button>
              {runningJobs.length > 0 && (
                <button type="button" className="at-theme-item" onClick={() => go("/jobs")}>
                  Running jobs ({runningJobs.length})
                </button>
              )}
              {roleLevel >= ROLE_ADMIN && (
                <button type="button" className="at-theme-item" onClick={() => go("/settings")}>
                  <span>Settings</span>
                  <SettingsIcon size={14} aria-hidden />
                </button>
              )}
              <button type="button" className="at-theme-item" onClick={() => { setTokenOpen(true); setPanel(null); }}>
                Auth token…
              </button>
              <button
                type="button"
                className="at-theme-item"
                onClick={() => {
                  useUi.getState().signOut();
                  setPanel(null);
                }}
              >
                <span>Sign out</span>
                <LogOut size={14} aria-hidden />
              </button>
            </div>
          )}
          {panel === "jobs" && (
            <div className="at-theme-menu at-theme-menu-wide at-rail-flyout" role="dialog" aria-label="Running jobs">
              <div className="at-theme-menu-label">Running jobs</div>
              {runningJobs.slice(0, 8).map((j) => (
                <button key={j.id} type="button" className="at-theme-item" onClick={() => go("/jobs")}>
                  <span className="truncate">{j.job_type}</span>
                  <span className="at-theme-hint mono">{j.progress_percent ?? 0}%</span>
                </button>
              ))}
            </div>
          )}
          {panel === "bell" && (
            <div className="at-theme-menu at-theme-menu-wide at-rail-flyout" role="dialog" aria-label="Open alerts">
              <div className="at-theme-menu-label">Open alerts</div>
              {(openAlerts || []).slice(0, 8).map((a) => (
                <button key={a.id} type="button" className="at-theme-item" onClick={() => go("/alerts")}>
                  <span className="truncate">{a.title}</span>
                </button>
              ))}
              {!openAlerts?.length && <div className="at-theme-item">No open alerts.</div>}
            </div>
          )}
        </div>
      </div>

      <Modal
        open={tokenOpen}
        onClose={() => setTokenOpen(false)}
        title="Auth token"
        footer={
          <>
            <Button onClick={() => setTokenOpen(false)}>Cancel</Button>
            <Button
              variant="primary"
              onClick={() => {
                setToken(draft.trim());
                setTokenOpen(false);
              }}
            >
              Save
            </Button>
          </>
        }
      >
        <div className="text-sm text-muted-foreground mb-3">
          Optional JWT override. Console login already mints a session token.
        </div>
        <Field
          type="password"
          autoComplete="off"
          placeholder="eyJhbGciOi…"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
      </Modal>
    </header>
  );
}

function MobileNavDrawer({ open, onClose }: { open: boolean; onClose: () => void }) {
  const loc = useLocation();
  const roleLevel = useUi((s) => s.roleLevel);
  const navRecents = useUi((s) => s.navRecents);
  const { grouped, filter, setFilter, isSectionClosed, toggleSection } = useNavGroups(roleLevel);
  const recents = useMemo(() => {
    const valid = new Set(modulesForRole(roleLevel).map((m) => m.id));
    return filterNavRecents(navRecents, valid);
  }, [roleLevel, navRecents]);

  useEffect(() => {
    onClose();
  }, [loc.pathname]); // eslint-disable-line react-hooks/exhaustive-deps
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
    <div className="at-drawer-scrim" onMouseDown={onClose}>
      <aside
        className="at-drawer"
        role="dialog"
        aria-modal="true"
        aria-label="Navigation"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="at-drawer-head">
          <span className="at-drawer-title">Navigate</span>
          <button type="button" className="at-iconbtn" aria-label="Close navigation" autoFocus onClick={onClose}>
            <X size={18} aria-hidden />
          </button>
        </div>
        <div className="at-drawer-body">
          <NavFilter value={filter} onChange={setFilter} className="at-drawer-filter" />
          <NavRecents
            recents={recents}
            linkClass={(active) => cx("at-drawer-item", active && "on")}
            onNavigate={onClose}
          />
          <NavSectionLinks
            groups={grouped}
            isSectionClosed={isSectionClosed}
            onToggleSection={toggleSection}
            linkClass={(active) => cx("at-drawer-item", active && "on")}
            onNavigate={onClose}
          />
        </div>
      </aside>
    </div>,
    document.body,
  );
}

interface SpotItem {
  label: string;
  sub: string;
  path: string;
  icon: LucideIcon;
}

function Spotlight({ open, onClose }: { open: boolean; onClose: () => void }) {
  const nav = useNavigate();
  const roleLevel = useUi((s) => s.roleLevel);
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const [resources, setResources] = useState<SpotItem[]>([]);
  useEffect(() => {
    // Clear the search + selection each time the palette opens — reset-on-open, not a loop.
    if (open) {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setQ("");
      setSel(0);
    }
  }, [open]);
  useEffect(() => {
    // Selection index must snap back to the top result whenever the query changes.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setSel(0);
  }, [q]);
  useEffect(() => {
    if (!open) return;
    if (roleLevel < ROLE_OPERATOR) {
      // Below-operator roles never see resource results — clear rather than leave stale data.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setResources([]);
      return;
    }
    Promise.allSettled([
      http.get("/volumes"),
      http.get("/buckets"),
      http.get("/snapshots"),
      http.get("/backups"),
    ]).then(([v, b, s, bk]) => {
      const out: SpotItem[] = [];
      if (v.status === "fulfilled")
        v.value.data.forEach((x: { name: string; id: string }) =>
          out.push({ label: x.name, sub: "volume", path: `/volumes?focus=${x.id}`, icon: HardDriveIcon }),
        );
      if (b.status === "fulfilled")
        b.value.data.forEach((x: { bucket_name?: string; id: string }) =>
          out.push({
            label: x.bucket_name || x.id,
            sub: "bucket",
            path: "/buckets",
            icon: Cloud,
          }),
        );
      if (s.status === "fulfilled")
        s.value.data.forEach((x: { name: string }) =>
          out.push({ label: x.name, sub: "snapshot", path: "/snapshots", icon: Camera }),
        );
      if (bk.status === "fulfilled")
        bk.value.data.forEach((x: { id: string }) =>
          out.push({ label: x.id, sub: "backup", path: "/backups", icon: Archive }),
        );
      setResources(out);
    });
  }, [open, roleLevel]);

  const modItems: SpotItem[] = modulesForRole(roleLevel).map((m) => ({
    label: m.label,
    sub: m.section.toLowerCase(),
    path: m.path,
    icon: m.icon,
  }));
  const ql = q.toLowerCase();
  const items = [...modItems, ...resources]
    .filter((it) => !q || it.label.toLowerCase().includes(ql) || it.sub.includes(ql))
    .slice(0, 40);
  if (!open) return null;
  const close = () => {
    setQ("");
    setSel(0);
    onClose();
  };
  const go = (i: number) => {
    const it = items[i];
    if (it) {
      nav(it.path);
      close();
    }
  };
  return (
    <div className="at-scrim" onMouseDown={close}>
      <div className="at-palette" role="dialog" aria-modal="true" aria-label="Command palette" onMouseDown={(e) => e.stopPropagation()}>
        <input
          autoFocus
          className="at-palette-input"
          placeholder="Go to, create, or ask…"
          aria-label="Search"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setSel((i) => Math.min(items.length - 1, i + 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setSel((i) => Math.max(0, i - 1));
            } else if (e.key === "Enter") go(sel);
            else if (e.key === "Escape") close();
          }}
        />
        <div className="at-palette-list">
          {items.map((it, i) => (
            <button
              key={`${it.path}-${it.label}-${i}`}
              type="button"
              className={cx("at-pitem", i === sel && "sel")}
              onMouseEnter={() => setSel(i)}
              onClick={() => go(i)}
            >
              <it.icon size={16} aria-hidden />
              <span className="flex-1 truncate">{it.label}</span>
              <span className="k">{it.sub}</span>
            </button>
          ))}
          {!items.length && (
            <div className="at-loading">No matches.</div>
          )}
        </div>
      </div>
    </div>
  );
}

function AgentToast() {
  const [q, setQ] = useState<string | null>(null);
  useEffect(() => {
    let t: ReturnType<typeof setTimeout>;
    return onSendPrompt((prompt) => {
      setQ(prompt);
      clearTimeout(t);
      t = setTimeout(() => setQ(null), 3400);
    });
  }, []);
  return (
    <div className={cx("at-toast", q && "on")} role="status">
      <span>Handed to agent:</span>
      <span className="q">{q}</span>
    </div>
  );
}

let welcomed = false;

const SHORTCUTS: [string, string][] = [
  ["H", "Go to Overview"],
  ["V", "Volumes (operator+)"],
  ["O", "Observatory"],
  ["A", "Alerts (operator+)"],
  ["J", "Jobs (operator+)"],
  ["C", "Ceph (operator+)"],
  ["G", "Settings (admin)"],
  ["⌘K / Ctrl-K", "Open command palette"],
  ["⌘⌥L / Ctrl-Alt-L", "Open launch pad"],
  ["↑ ↓ / Enter", "Navigate & open in palette"],
  ["?", "Show this help"],
  ["Esc", "Close dialogs / menus"],
];

export function Shell() {
  const spotOpen = useUi((s) => s.spotlightOpen);
  const setSpot = useUi((s) => s.setSpotlight);
  const launchPadOpen = useUi((s) => s.launchPadOpen);
  const setLaunchPad = useUi((s) => s.setLaunchPad);
  const roleLevel = useUi((s) => s.roleLevel);
  const recordRecent = useUi((s) => s.recordRecent);
  const [helpOpen, setHelpOpen] = useState(false);
  const [navOpen, setNavOpen] = useState(false);
  const loc = useLocation();
  const nav = useNavigate();

  useEffect(() => {
    const mod = activeModuleFromPath(loc.pathname);
    if (mod && !mod.hiddenFromNav) recordRecent(mod.id, mod.label);
  }, [loc.pathname, recordRecent]);

  useEffect(() => {
    const shortcuts = shortcutTargets(roleLevel);
    const h = (e: KeyboardEvent) => {
      const typing = ["INPUT", "TEXTAREA", "SELECT"].includes((e.target as HTMLElement)?.tagName)
        || (e.target as HTMLElement)?.isContentEditable;
      if ((e.metaKey || e.ctrlKey) && e.altKey && e.key.toLowerCase() === "l") {
        e.preventDefault();
        setLaunchPad(true);
        return;
      }
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setSpot(true);
      } else if (e.key === "?" && !typing) {
        e.preventDefault();
        setHelpOpen(true);
      } else if (!typing && !e.metaKey && !e.ctrlKey && !e.altKey) {
        const path = shortcuts.get(e.key.toLowerCase());
        if (path) {
          e.preventDefault();
          nav(path);
        }
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [setSpot, setLaunchPad, nav, roleLevel]);
  useEffect(() => {
    const label = navLabelForPath(loc.pathname);
    if (label) document.title = `Atlas · ${label}`;
  }, [loc.pathname]);
  useEffect(() => {
    if (!welcomed) {
      welcomed = true;
      import("../api/client").then((c) => c.toast("Welcome to Atlas Storage Center", "ok"));
    }
  }, []);

  const active = activeModuleFromPath(loc.pathname);
  const hasLocalNav = !!active && active.id !== "overview";

  return (
    <div className={cx("at-app", hasLocalNav && "has-localnav")}>
      <a className="a-skip" href="#atlas-main">
        Skip to content
      </a>
      <div className="at-chrome">
        <MenuBar
          onSpotlight={() => setSpot(true)}
          onOpenNav={() => setNavOpen(true)}
          onLaunchPad={() => setLaunchPad(true)}
        />
        <LocalNav />
        <MobileJumpNav />
      </div>
      <main id="atlas-main" className="at-main" tabIndex={-1}>
        <div key={loc.pathname} className="at-main-scroll">
          <Outlet />
        </div>
      </main>
      <Spotlight open={spotOpen} onClose={() => setSpot(false)} />
      <LaunchPad open={launchPadOpen} onClose={() => setLaunchPad(false)} />
      <MobileNavDrawer open={navOpen} onClose={() => setNavOpen(false)} />
      <AgentToast />
      <Modal open={helpOpen} onClose={() => setHelpOpen(false)} title="Keyboard shortcuts">
        <div className="space-y-2">
          {SHORTCUTS.map(([k, d]) => (
            <div key={k} className="flex items-center justify-between text-sm">
              <span className="text-muted-foreground">{d}</span>
              <kbd className="at-kbd">{k}</kbd>
            </div>
          ))}
        </div>
      </Modal>
    </div>
  );
}
