// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
/** Page glyphs on a tinted rounded tile (launch pad, navigation sheet). */
import type { LucideIcon } from "lucide-react";
import { cx } from "../lib/format";

export function NavAppIcon({
  icon: Icon,
  size = 16,
  className,
}: {
  icon: LucideIcon;
  size?: number;
  className?: string;
}) {
  return (
    <span className={cx("at-appicon", className)} aria-hidden>
      <Icon size={size} strokeWidth={2} />
    </span>
  );
}
