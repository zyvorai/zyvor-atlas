// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Atlas sign-in — same 2-chapter Store shell as h2kvm-, Atlas copy + auth.
import { useEffect, useState, type FormEvent, useMemo } from "react";
import {
  ArrowRight,
  ChevronLeft,
  Eye,
  EyeOff,
  Loader2,
  Lock,
  Palette,
  User,
} from "lucide-react";
import { useUi, type Theme } from "../store/ui";
import { API_BASE, clearToasts } from "../api/client";
import {
  LoginDivider,
  LoginError,
  LoginField,
  LoginRemember,
  LoginSubmit,
  PremiumLoginShell,
} from "../ui/PremiumLoginShell";

const PRODUCT = "Atlas";
const REMEMBER_USER_KEY = "atlas.login-remember-user";
const REMEMBER_FLAG_KEY = "atlas.login-remember";
const DEFAULT_USER = "admin";

const THEME_OPTIONS: { id: Theme; label: string }[] = [
  { id: "night", label: "Night" },
  { id: "day", label: "Day" },
];

function LoginThemeSwitcher() {
  const theme = useUi((s) => s.theme);
  const setTheme = useUi((s) => s.setTheme);
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!open) return;
    const onPointer = (e: PointerEvent) => {
      const el = document.getElementById("atlas-login-theme");
      if (el && !el.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div id="atlas-login-theme" className="fixed top-4 right-4 z-50">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        title="Change theme"
        aria-label="Change theme"
        aria-expanded={open}
        className="inline-flex h-9 w-9 items-center justify-center rounded-full border border-black/10 bg-white/90 text-neutral-700 shadow-sm backdrop-blur"
      >
        <Palette className="w-4 h-4" aria-hidden />
      </button>
      {open && (
        <div
          className="absolute right-0 mt-2 min-w-[9rem] overflow-hidden rounded-xl border border-black/10 bg-white shadow-lg"
          role="group"
          aria-label="Visual theme"
        >
          {THEME_OPTIONS.map(({ id, label }) => (
            <button
              key={id}
              type="button"
              onClick={() => {
                setTheme(id);
                setOpen(false);
              }}
              className={`block w-full px-3 py-2.5 text-left text-sm transition-colors ${
                theme === id
                  ? "bg-[#0071E3]/10 text-[#0071E3] font-semibold"
                  : "text-neutral-700 hover:bg-neutral-50"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

type LoginStep = "identify" | "password";

export function Login() {
  const enter = useUi((s) => s.enter);
  const setToken = useUi((s) => s.setToken);
  const sessionHint = useUi((s) => s.sessionHint);
  const clearSessionHint = useUi((s) => s.clearSessionHint);
  const [step, setStep] = useState<LoginStep>("identify");
  const [username, setUsername] = useState(DEFAULT_USER);
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(false);
  const [showPassword, setShowPassword] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [ver, setVer] = useState("");
  const [health, setHealth] = useState<"ok" | "down" | "…">("…");
  const [ssoEnabled, setSsoEnabled] = useState(false);
  const hostLabel = typeof window !== "undefined" ? window.location.hostname : "";

  const loginDest = useMemo(() => {
    if (typeof window === 'undefined') return { host: '', origin: '', port: '', protocol: '' }
    const { hostname, origin, port, protocol } = window.location
    return {
      host: hostname || 'localhost',
      origin: origin || '',
      port: port || (protocol === 'https:' ? '443' : protocol === 'http:' ? '80' : ''),
      protocol: protocol.replace(':', '') || 'https',
    }
  }, [])

  const scrollToChapter = (id: string) => {
    document.getElementById(id)?.scrollIntoView({ behavior: 'smooth', block: 'start' })
  }

  const loginLocalNav = (
    <nav className="login-localnav" aria-label="Login chapters">
      <a href="#login-product" onClick={(e) => { e.preventDefault(); scrollToChapter('login-product') }}>Product</a>
      <a href="#login-machine" onClick={(e) => { e.preventDefault(); scrollToChapter('login-machine') }}>This machine</a>
      <a href="#login-sign-in" onClick={(e) => { e.preventDefault(); scrollToChapter('login-sign-in') }}>Sign in</a>
      {loginDest.host ? (
        <span className="login-localnav-host" title={loginDest.origin}>{loginDest.host}</span>
      ) : null}
    </nav>
  )

  useEffect(() => {
    document.querySelector<HTMLElement>('.login-store-scroll')?.scrollTo({ top: 0 })
  }, [])

  const loginMiddleChapters = (
    <>
      <section id="login-product" className="login-chapter login-chapter-product" aria-label="Product">
        <div className="login-chapter-inner">
          <p className="login-chapter-kicker">Product</p>
          <h2 className="login-hero-title">Atlas.</h2>
          <p className="login-tagline">Capacity, volumes, and Ceph — storage control plane for this cluster.</p>
          <div className="login-cta">
            <button type="button" className="login-cta-primary" onClick={() => scrollToChapter('login-machine')}>
              See this machine
            </button>
          </div>
        </div>
      </section>
      <section id="login-machine" className="login-chapter login-chapter-destination" aria-label="This machine">
        <div className="login-chapter-inner">
          <p className="login-chapter-kicker">This machine</p>
          <h2 className="login-dest-title">{loginDest.host || 'localhost'}.</h2>
          <p className="login-tagline">
            You are signing in to <strong style={{ color: '#fff', fontWeight: 600 }}>Atlas</strong> on
            this host — not a public cloud console.
          </p>
          <ul className="login-dest-facts">
            <li className="login-dest-fact">
              <span className="login-dest-fact-label">Product</span>
              <span className="login-dest-fact-value is-display">Atlas</span>
            </li>
            <li className="login-dest-fact">
              <span className="login-dest-fact-label">Host</span>
              <span className="login-dest-fact-value">{loginDest.host || '—'}</span>
            </li>
            <li className="login-dest-fact">
              <span className="login-dest-fact-label">Origin</span>
              <span className="login-dest-fact-value">{loginDest.origin || '—'}</span>
            </li>
            <li className="login-dest-fact">
              <span className="login-dest-fact-label">Protocol</span>
              <span className="login-dest-fact-value">
                {loginDest.protocol || '—'}
                {loginDest.port ? ` · ${loginDest.port}` : ''}
              </span>
            </li>
          </ul>
          <div className="login-cta">
            <button type="button" className="login-cta-primary" onClick={() => scrollToChapter('login-sign-in')}>
              Continue to sign in
            </button>
          </div>
        </div>
      </section>
    </>
  )

  useEffect(() => {
    document.title = `Sign in · ${PRODUCT} · ${loginDest.host || 'cluster'}`;
    clearToasts();
    localStorage.removeItem("atlas.login-remember-token");
    const remembered = localStorage.getItem(REMEMBER_FLAG_KEY) === "true";
    const savedUser = localStorage.getItem(REMEMBER_USER_KEY);
    if (remembered && savedUser) {
      // Restore a remembered login from localStorage on mount — one-time hydration, not a loop.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setUsername(savedUser);
      setRemember(true);
    }
    fetch("/version")
      .then((r) => r.json())
      .then((v) => setVer(v.version))
      .catch(() => {});
    fetch("/health")
      .then((r) => setHealth(r.ok ? "ok" : "down"))
      .catch(() => setHealth("down"));
    fetch(`${API_BASE}/auth/oidc/status`)
      .then((r) => r.json())
      .then((v) => setSsoEnabled(Boolean(v?.enabled)))
      .catch(() => setSsoEnabled(false));
    return () => {
      document.title = PRODUCT;
    };
  }, [loginDest.host]);

  const scrollToForm = () => {
    document.getElementById("login-sign-in")?.scrollIntoView({ behavior: "smooth", block: "start" });
  };

  const handleContinue = (e: FormEvent) => {
    e.preventDefault();
    if (!username.trim()) return;
    setError(null);
    clearSessionHint();
    setStep("password");
  };

  const handleBack = () => {
    setStep("identify");
    setPassword("");
    setShowPassword(false);
    setError(null);
  };

  const go = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy) return;
    const user = username.trim();
    clearSessionHint();
    setError(null);
    if (!user || !password) {
      setError("Enter username and password.");
      return;
    }
    setBusy(true);
    try {
      const res = await fetch(`${API_BASE}/auth/login`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username: user, password }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) {
        setError(
          body?.error?.message ||
            (res.status === 401 ? "Invalid username or password." : `Sign-in failed (${res.status}).`),
        );
        return;
      }
      const jwt = typeof body?.token === "string" ? body.token : "";
      if (!jwt) {
        setError("Gateway returned no session token.");
        return;
      }
      setToken(jwt, remember);
      enter(remember);
      if (remember) {
        localStorage.setItem(REMEMBER_USER_KEY, user);
        localStorage.setItem(REMEMBER_FLAG_KEY, "true");
      } else {
        localStorage.removeItem(REMEMBER_USER_KEY);
        localStorage.removeItem(REMEMBER_FLAG_KEY);
      }
    } catch {
      setError("Could not reach the gateway. Check the URL and try again.");
    } finally {
      setBusy(false);
    }
  };

  const displayError = error || sessionHint;

  const storeCta = (
    <>
      <a
        href="#login-sign-in"
        className="login-cta-primary"
        onClick={(e) => {
          e.preventDefault();
          scrollToForm();
        }}
      >
        Sign in
      </a>
      <a
        href="#login-sign-in"
        className="login-cta-secondary"
        onClick={(e) => {
          e.preventDefault();
          scrollToForm();
        }}
      >
        Continue
      </a>
    </>
  );

  const panelSubtitle =
    step === "password" ? (
      <>
        Enter the password for <span className="login-apple-host">{username.trim()}</span>
      </>
    ) : (
      "Sign in"
    );

  return (

    <PremiumLoginShell
      themeSwitcher={<>{loginLocalNav}<LoginThemeSwitcher /></>}
      middleChapters={loginMiddleChapters}
      logo={
        <img
          src="/zyvor-logo.png"
          alt="Zyvor"
          className="login-zyvor-logo"
          width={220}
          height={69}
          decoding="async"
        />
      }
      productName={PRODUCT}
      productWordmark={PRODUCT}
      heroTitle={
        <>
          Survey the cluster
          <br />
          before you steer it
        </>
      }
      heroSubheadline="Capacity, volumes, Ceph, and DataBridge — Atlas Storage Center for operators who chart the fleet, not a wall of widgets."
      heroCta={storeCta}
      chapterNote={`Zyvor · Atlas Storage Center · ${hostLabel || "gateway"}${ver ? ` · v${ver}` : ""} · ${health === "…" ? "checking" : health}`}
      panelSubtitle={panelSubtitle}
      panelHint={
        step === "identify" ? (
          <>
            Sign in as <span className="font-mono">admin</span> with the gateway password for this
            deployment.
          </>
        ) : null
      }
      showSignInChapter
      footer={
        <p className="login-license-notice">
          Copyright © {new Date().getFullYear()} ZyvorAI Labs · Zyvor Production Licence ·{" "}
          <a href="https://zyvor.dev">zyvor.dev</a>
        </p>
      }
    >
      <p className="login-sign-in-context">
        Signing in to <strong>Atlas</strong>
        {loginDest.host ? (
          <>
            {' '}
            on <span className="login-apple-host">{loginDest.host}</span>
          </>
        ) : null}
      </p>
      {step === "identify" ? (
        <form
          key="identify"
          onSubmit={handleContinue}
          autoComplete="on"
          aria-label="Account"
          className="login-apple-step text-left"
        >
          {displayError ? <LoginError message={displayError} /> : null}

          <div className="login-apple-fields">
            <LoginField label="Username" id="atlas-username">
              <User className="login-field-icon" />
              <input
                id="atlas-username"
                name="username"
                type="text"
                value={username}
                onChange={(e) => {
                  setUsername(e.target.value);
                  if (error) setError(null);
                }}
                className="login-input"
                placeholder="admin"
                autoComplete="username"
                autoFocus
                required
                disabled={busy}
              />
            </LoginField>
          </div>

          <LoginSubmit loading={false} disabled={!username.trim() || busy}>
            <span>Continue</span>
            <ArrowRight className="h-4 w-4" />
          </LoginSubmit>

          {ssoEnabled ? (
            <>
              <LoginDivider label="or" />
              <a href={`${API_BASE}/auth/oidc/login`} className="login-btn-secondary">
                Sign in with SSO
              </a>
            </>
          ) : null}
        </form>
      ) : (
        <form
          key="password"
          onSubmit={go}
          autoComplete="on"
          aria-label="Password"
          className="login-apple-step text-left"
        >
          <button type="button" onClick={handleBack} className="login-apple-identity" aria-label="Change account">
            <ChevronLeft aria-hidden className="h-4 w-4 shrink-0" />
            <span className="truncate">{username.trim()}</span>
          </button>
          <input type="text" name="username" value={username} autoComplete="username" readOnly hidden />

          {displayError ? <LoginError message={displayError} /> : null}

          <div className="login-apple-fields">
            <LoginField label="Password" id="atlas-password">
              <Lock className="login-field-icon" />
              <input
                id="atlas-password"
                name="password"
                type={showPassword ? "text" : "password"}
                value={password}
                onChange={(e) => {
                  setPassword(e.target.value);
                  if (error) setError(null);
                }}
                className="login-input pr-11"
                placeholder="Enter your password"
                autoComplete="current-password"
                autoFocus
                required
                disabled={busy}
              />
              <button
                type="button"
                onClick={() => setShowPassword(!showPassword)}
                className="absolute right-3.5 top-1/2 -translate-y-1/2 text-zinc-400 hover:text-zinc-700 transition-colors"
                aria-label={showPassword ? "Hide password" : "Show password"}
              >
                {showPassword ? <EyeOff className="h-4 w-4" /> : <Eye className="h-4 w-4" />}
              </button>
            </LoginField>
          </div>

          <LoginRemember
            checked={remember}
            onChange={setRemember}
            label="Remember username on this device"
            hint="Only your username is stored locally — never your password."
          />

          <LoginSubmit loading={busy} disabled={!password}>
            {busy ? (
              <>
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
                <span>Signing in…</span>
              </>
            ) : (
              <>
                <span>Sign In</span>
                <ArrowRight className="h-4 w-4" />
              </>
            )}
          </LoginSubmit>
        </form>
      )}
    </PremiumLoginShell>
  );
}
