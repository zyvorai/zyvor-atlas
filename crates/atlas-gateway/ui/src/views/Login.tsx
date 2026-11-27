// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Atlas sign-in: a centred headline, the two-step form, and the facts about this deployment.
import { useEffect, useState, type FormEvent, useMemo } from "react";
import { ArrowRight, ChevronLeft, CircleAlert, Eye, EyeOff, Loader2 } from "lucide-react";
import { useUi } from "../store/ui";
import { API_BASE, clearToasts } from "../api/client";
import { ThemeMenu } from "../ui/ThemeMenu";
import "./Login.css";

const PRODUCT = "Atlas";
const REMEMBER_USER_KEY = "atlas.login-remember-user";
const REMEMBER_FLAG_KEY = "atlas.login-remember";
const DEFAULT_USER = "admin";

/** Decorative: three stacked storage layers. Carries no data. */
function LayerArt() {
  return (
    <svg className="lg-art" viewBox="0 0 240 170" aria-hidden focusable="false">
      <defs>
        <linearGradient id="lg-top" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" className="stop-a" />
          <stop offset="1" className="stop-b" />
        </linearGradient>
      </defs>
      {[2, 1, 0].map((i) => (
        <g key={i} className={`layer layer-${i}`}>
          <path className="side" d={`M24 ${52 + i * 30}v10q0 5 7 9l78 40q11 5 22 0l78-40q7-4 7-9V${52 + i * 30}z`} />
          <path
            className="face"
            d={`M109 ${12 + i * 30}q11-5 22 0l78 40q14 7 0 14l-78 40q-11 5-22 0l-78-40q-14-7 0-14z`}
          />
        </g>
      ))}
    </svg>
  );
}

function LoginError({ message }: { message: string }) {
  return (
    <div className="lg-error" role="alert" aria-live="assertive">
      <CircleAlert size={18} aria-hidden />
      <div>
        <p className="lg-error-title">Unable to sign in</p>
        <p>{message}</p>
      </div>
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
  const healthText = health === "…" ? "Checking" : health === "ok" ? "Reachable" : "Not reachable";
  const facts: [string, string][] = [
    ["Host", loginDest.host || "—"],
    ["Origin", loginDest.origin || "—"],
    ["Protocol", `${loginDest.protocol || "—"}${loginDest.port ? ` · ${loginDest.port}` : ""}`],
    ["Version", ver ? `v${ver}` : "—"],
  ];

  return (
    <div className="lg">
      <header className="lg-nav">
        <div className="lg-nav-inner">
          <span className="at-mark">
            <img src="/zyvor-mark.svg" alt="Zyvor" width={16} height={16} />
            <span className="at-brand-name">{PRODUCT}</span>
          </span>
          <span className="lg-nav-host" title={loginDest.origin}>
            {loginDest.host}
          </span>
          <ThemeMenu />
        </div>
      </header>

      <main className="lg-main">
        <div className="lg-hero">
          <LayerArt />
          <h1 className="lg-title">Atlas.</h1>
          <p className="lg-tagline">Volumes, pools and protection on one storage control plane.</p>
        </div>

        <section className="lg-card" aria-labelledby="lg-form-title">
          <h2 id="lg-form-title" className="lg-card-title">
            Sign in
          </h2>
          <p className="lg-context">
            {step === "password" ? (
              <>
                Enter the password for <strong>{username.trim()}</strong>
              </>
            ) : (
              <>
                to Atlas
                {loginDest.host ? (
                  <>
                    {" "}
                    on <strong>{loginDest.host}</strong>
                  </>
                ) : null}
              </>
            )}
          </p>

          {step === "identify" ? (
            <form key="identify" onSubmit={handleContinue} autoComplete="on" aria-label="Account" className="lg-form">
              {displayError ? <LoginError message={displayError} /> : null}
              <div>
                <label htmlFor="atlas-username">Username</label>
                <input
                  id="atlas-username"
                  name="username"
                  type="text"
                  value={username}
                  onChange={(e) => {
                    setUsername(e.target.value);
                    if (error) setError(null);
                  }}
                  className="field lg-input"
                  placeholder="admin"
                  autoComplete="username"
                  autoFocus
                  required
                  disabled={busy}
                />
              </div>
              <button type="submit" className="at-btn primary large" disabled={!username.trim() || busy}>
                <span>Continue</span>
                <ArrowRight size={17} aria-hidden />
              </button>
              {ssoEnabled ? (
                <>
                  <div className="lg-or">or</div>
                  <a href={`${API_BASE}/auth/oidc/login`} className="at-btn large">
                    Sign in with SSO
                  </a>
                </>
              ) : null}
              <p className="lg-hint">
                Sign in as <span className="mono">admin</span> with the gateway password for this deployment.
              </p>
            </form>
          ) : (
            <form key="password" onSubmit={go} autoComplete="on" aria-label="Password" className="lg-form">
              <button type="button" onClick={handleBack} className="lg-identity" aria-label="Change account">
                <ChevronLeft size={16} aria-hidden />
                <span className="truncate">{username.trim()}</span>
              </button>
              <input type="text" name="username" value={username} autoComplete="username" readOnly hidden />

              {displayError ? <LoginError message={displayError} /> : null}

              <div>
                <label htmlFor="atlas-password">Password</label>
                <div className="relative">
                  <input
                    id="atlas-password"
                    name="password"
                    type={showPassword ? "text" : "password"}
                    value={password}
                    onChange={(e) => {
                      setPassword(e.target.value);
                      if (error) setError(null);
                    }}
                    className="field lg-input lg-input-reveal"
                    placeholder="Enter your password"
                    autoComplete="current-password"
                    autoFocus
                    required
                    disabled={busy}
                  />
                  <button
                    type="button"
                    onClick={() => setShowPassword(!showPassword)}
                    className="at-iconbtn lg-reveal"
                    aria-label={showPassword ? "Hide password" : "Show password"}
                  >
                    {showPassword ? <EyeOff size={17} aria-hidden /> : <Eye size={17} aria-hidden />}
                  </button>
                </div>
              </div>

              <div>
                <label className="lg-remember">
                  <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
                  <span>Keep me signed in on this device</span>
                </label>
                <p className="lg-remember-hint">Stores your session token and username here. Never your password.</p>
              </div>

              <button type="submit" className="at-btn primary large" disabled={!password || busy}>
                {busy ? (
                  <>
                    <Loader2 size={17} className="animate-spin" aria-hidden />
                    <span>Signing in…</span>
                  </>
                ) : (
                  <span>Sign In</span>
                )}
              </button>
            </form>
          )}
        </section>

        <section className="lg-facts" aria-labelledby="lg-facts-title">
          <h2 id="lg-facts-title">This deployment.</h2>
          <p>You are signing in to Atlas on this host, not a public cloud console.</p>
          <dl>
            <div>
              <dt>Gateway</dt>
              <dd className={`lg-health ${health === "ok" ? "ok" : health === "down" ? "down" : ""}`}>{healthText}</dd>
            </div>
            {facts.map(([k, v]) => (
              <div key={k}>
                <dt>{k}</dt>
                <dd>{v}</dd>
              </div>
            ))}
          </dl>
        </section>
      </main>

      <footer className="lg-footer">
        Copyright © {new Date().getFullYear()} ZyvorAI Labs · Zyvor Production Licence ·{" "}
        <a href="https://zyvor.dev">zyvor.dev</a>
      </footer>
    </div>
  );
}
