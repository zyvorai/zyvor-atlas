// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Original line-and-gradient spot art for empty states. Colour comes from graphics.css only.
import { useId, type ReactNode } from "react";
import "./graphics.css";

export type SpotKind =
  | "cluster"
  | "volume"
  | "pool"
  | "snapshot"
  | "backup"
  | "bucket"
  | "chart"
  | "alert";

/** `useId` output made safe for use inside `url(#...)`. */
export function useSvgId(prefix: string): string {
  return `${prefix}-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
}

function art(kind: SpotKind, face: string, accent: string): ReactNode {
  switch (kind) {
    case "cluster":
      return (
        <>
          <path className="ag-spot-link" d="M80 34v14M38 62V48h84v14" />
          <circle cx="80" cy="26" r="9" fill={accent} />
          {[14, 58, 102].map((x) => (
            <g key={x}>
              <rect className="ag-spot-body" x={x} y="62" width="44" height="40" rx="9" fill={face} />
              <path className="ag-spot-line" d={`M${x + 10} 76h24M${x + 10} 86h24`} />
            </g>
          ))}
        </>
      );
    case "volume":
      return (
        <>
          <path className="ag-spot-body" d="M44 36v50c0 8 16 14 36 14s36-6 36-14V36" fill={face} />
          <ellipse className="ag-spot-body" cx="80" cy="36" rx="36" ry="14" fill={face} />
          <path className="ag-spot-line" d="M44 61c0 8 16 14 36 14s36-6 36-14" />
          <circle cx="116" cy="88" r="14" fill={accent} />
          <path className="ag-spot-on-accent" d="M116 82v12M110 88h12" />
        </>
      );
    case "pool":
      return (
        <>
          {[70, 50, 30].map((y, i) => (
            <path
              key={y}
              className="ag-spot-body"
              d={`M80 ${y}l46 18-46 18-46-18Z`}
              fill={i === 2 ? accent : face}
            />
          ))}
        </>
      );
    case "snapshot":
      return (
        <>
          <rect className="ag-spot-ghost" x="34" y="24" width="72" height="56" rx="10" />
          <rect className="ag-spot-body" x="52" y="42" width="76" height="60" rx="10" fill={face} />
          <circle cx="90" cy="72" r="15" fill={accent} />
          <circle className="ag-spot-hole" cx="90" cy="72" r="6" />
        </>
      );
    case "backup":
      return (
        <>
          <rect className="ag-spot-body" x="34" y="62" width="92" height="40" rx="10" fill={face} />
          <path className="ag-spot-line" d="M46 82h34" />
          <circle cx="112" cy="82" r="4" fill={accent} />
          <circle cx="80" cy="36" r="22" fill={accent} />
          <path className="ag-spot-on-accent" d="M80 47V27M71 35l9-9 9 9" />
        </>
      );
    case "bucket":
      return (
        <>
          <path className="ag-spot-body" d="M42 40l10 56c1 5 13 9 28 9s27-4 28-9l10-56" fill={face} />
          <ellipse className="ag-spot-body" cx="80" cy="40" rx="38" ry="12" fill={face} />
          <rect x="66" y="64" width="18" height="18" rx="4" fill={accent} />
          <circle className="ag-spot-ghost" cx="94" cy="80" r="10" />
        </>
      );
    case "chart":
      return (
        <>
          <rect className="ag-spot-body" x="24" y="22" width="112" height="80" rx="12" fill={face} />
          <path className="ag-spot-line" d="M38 86h84" />
          <path className="ag-spot-trace" d="M38 72c12 0 14-22 28-22s14 16 26 16 16-28 30-28" />
        </>
      );
    case "alert":
      return (
        <>
          <path
            className="ag-spot-body"
            d="M80 22c-18 0-28 14-28 30v20L40 86h80l-12-14V52c0-16-10-30-28-30Z"
            fill={face}
          />
          <path className="ag-spot-line" d="M70 96c2 6 6 9 10 9s8-3 10-9" />
          <circle cx="108" cy="38" r="12" fill={accent} />
        </>
      );
  }
}

export interface SpotIllustrationProps {
  kind: SpotKind;
  /** Rendered width in px; shrinks with its container. Default 160. */
  size?: number;
  /** Accessible name. Without one the art is decorative and hidden from assistive tech. */
  title?: string;
  className?: string;
}

export function SpotIllustration({ kind, size = 160, title, className = "" }: SpotIllustrationProps) {
  const face = useSvgId("ag-face");
  const accent = useSvgId("ag-accent");
  return (
    <svg
      className={`ag ag-spot ${className}`.trim()}
      data-kind={kind}
      width={size}
      viewBox="0 0 160 124"
      role={title ? "img" : undefined}
      aria-label={title}
      aria-hidden={title ? undefined : true}
      focusable="false"
    >
      <defs>
        <linearGradient id={face} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" className="ag-stop-face-top" />
          <stop offset="1" className="ag-stop-face-bottom" />
        </linearGradient>
        <linearGradient id={accent} x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" className="ag-stop-accent-top" />
          <stop offset="1" className="ag-stop-accent-bottom" />
        </linearGradient>
      </defs>
      <ellipse className="ag-spot-shadow" cx="80" cy="112" rx="52" ry="6" />
      <g className="ag-spot-float">{art(kind, `url(#${face})`, `url(#${accent})`)}</g>
    </svg>
  );
}
