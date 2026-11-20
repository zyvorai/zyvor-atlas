// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Atlas app tiles — colored rounded glyphs (Soundings, not Mac Tahoe squircles). */
import type { LucideIcon } from "lucide-react";
import type { SectionId } from "./routes";
import { cx } from "../lib/format";

const SECTION_TINT: Record<SectionId, string> = {
  STORAGE: "at-appicon-storage",
  "DATA PROTECTION": "at-appicon-protect",
  DATABRIDGE: "at-appicon-bridge",
  OBSERVABILITY: "at-appicon-observe",
  GOVERNANCE: "at-appicon-govern",
  INFRASTRUCTURE: "at-appicon-infra",
};

export function NavAppIcon({
  icon: Icon,
  section,
  size = 16,
  className,
}: {
  icon: LucideIcon;
  section: SectionId;
  size?: number;
  className?: string;
}) {
  return (
    <span className={cx("at-appicon", SECTION_TINT[section], className)} aria-hidden>
      <Icon size={size} strokeWidth={2} />
    </span>
  );
}
