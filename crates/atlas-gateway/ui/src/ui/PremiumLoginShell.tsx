// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/**
 * Apple Store chapter login — same shell as h2kvm- / Zeus OS.
 * Full-bleed white hero (brand → title → lede → pill CTAs), second chapter for credentials.
 */
import type { ReactNode } from "react";
import { AlertCircle } from "lucide-react";
import "../zyvor-premium-login.css";

export type PremiumLoginPill = {
  icon?: ReactNode;
  label: string;
};

export type PremiumLoginShellProps = {
  logo?: ReactNode;
  productName: string;
  productWordmark?: string;
  productSubtitle?: string;
  heroTitle?: ReactNode;
  heroSubheadline?: ReactNode;
  heroCta?: ReactNode;
  chapterNote?: ReactNode;
  pills?: PremiumLoginPill[];
  panelTitle?: string;
  panelSubtitle?: ReactNode;
  panelHint?: ReactNode;
  footer?: ReactNode;
  formClassName?: string;
  showSignInChapter?: boolean;
  middleChapters?: ReactNode;
  themeSwitcher?: ReactNode;
  children?: ReactNode;
};

export function PremiumLoginShell({
  logo,
  productName,
  productWordmark,
  productSubtitle,
  heroTitle = (
    <>
      Survey the cluster
      <br />
      before you steer it
    </>
  ),
  heroSubheadline,
  heroCta,
  chapterNote = "Storage control plane · sign in to continue",
  panelTitle,
  panelSubtitle,
  panelHint,
  footer,
  formClassName = "",
  showSignInChapter = true,
  middleChapters,
  themeSwitcher,
  children,
}: PremiumLoginShellProps) {
  const tagline =
    heroSubheadline ??
    productSubtitle ??
    "Capacity, volumes, Ceph, and DataBridge — Atlas Storage Center for operators who chart the fleet, not a wall of widgets.";
  const formHeading =
    panelSubtitle ?? (panelTitle && panelTitle !== "Sign in" ? panelTitle : "Sign in");
  const wordmark = (productWordmark ?? productName).trim() || "Atlas";

  return (
    <div className="login-page login-store-page min-h-screen flex flex-col">
      {themeSwitcher}
      <main className="login-store-scroll" aria-label="Sign in">
        <section className="login-chapter login-chapter-hero" aria-label={productName}>
          <div className="login-chapter-inner">
            {logo ? <div className="login-logo inline-flex mb-5">{logo}</div> : null}
            <p className="login-wordmark" aria-label={productName}>
              {wordmark}
            </p>
            <h1 className="login-hero-title">{heroTitle}</h1>
            {tagline ? <p className="login-tagline">{tagline}</p> : null}
            {heroCta ? <div className="login-cta">{heroCta}</div> : null}
            {chapterNote ? <p className="login-chapter-note">{chapterNote}</p> : null}
          </div>
        </section>

        {middleChapters}

        {showSignInChapter && children ? (
          <section
            id="login-sign-in"
            className="login-chapter login-chapter-sign-in"
            aria-label="Credentials"
          >
            <div className="login-chapter-inner login-sign-in-inner">
              <p className="login-form-heading">{formHeading}</p>
              <div className={`login-card ${formClassName}`.trim()}>{children}</div>
              {panelHint ? <p className="login-hint">{panelHint}</p> : null}
            </div>
          </section>
        ) : null}
      </main>

      {footer}
    </div>
  );
}

export function LoginError({
  message,
  variant: _variant,
}: {
  message: string;
  variant?: "credentials" | "network" | "generic";
}) {
  return (
    <div
      className="flex items-start gap-2.5 bg-red-50 border border-red-200 rounded-xl p-3 mb-6 login-shake"
      role="alert"
      aria-live="assertive"
    >
      <AlertCircle className="h-4 w-4 text-red-600 shrink-0 mt-0.5" aria-hidden />
      <div>
        <p className="text-sm font-medium text-red-700">Unable to sign in</p>
        <p className="text-sm text-red-600/90 mt-0.5">{message}</p>
      </div>
    </div>
  );
}

export function LoginField({
  label,
  id,
  children,
}: {
  label: string;
  id: string;
  children: ReactNode;
}) {
  return (
    <div className="mb-4">
      <label htmlFor={id} className="block text-xs font-medium text-zinc-500 mb-1.5">
        {label}
      </label>
      <div className="relative group">{children}</div>
    </div>
  );
}

export function LoginSubmit({
  loading,
  disabled,
  children,
  className = "",
}: {
  loading?: boolean;
  disabled?: boolean;
  children: ReactNode;
  className?: string;
}) {
  return (
    <button type="submit" disabled={disabled || loading} className={`login-btn-primary ${className}`.trim()}>
      {children}
    </button>
  );
}

export function LoginRemember({
  checked,
  onChange,
  label = "Remember me on this device",
  hint,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: string;
  hint?: string;
}) {
  return (
    <div className="mt-5">
      <label className="flex items-center gap-2.5 cursor-pointer select-none">
        <input
          type="checkbox"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
          className="w-4 h-4 rounded border-zinc-300 accent-[#0071e3]"
        />
        <span className="text-sm text-zinc-500">{label}</span>
      </label>
      {hint ? <p className="text-xs text-zinc-400 mt-1.5 ml-[1.625rem]">{hint}</p> : null}
    </div>
  );
}

export function LoginDivider({ label = "or" }: { label?: string }) {
  return (
    <div className="relative py-3 mt-2 text-center text-xs uppercase tracking-[0.18em] text-zinc-400">
      <span className="relative z-1 px-3 bg-white">{label}</span>
      <div className="absolute inset-x-0 top-1/2 -translate-y-1/2 border-t border-zinc-200" />
    </div>
  );
}

/** @deprecated typo guard — use PremiumLoginShell */
export const PremumLoginShell = PremiumLoginShell;
