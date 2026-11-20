// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Shell look & feel — Night / Day product shells. */
import type { Theme } from "../store/ui";

export const THEME_OPTIONS: { id: Theme; title: string; hint: string }[] = [
  { id: "night", title: "Night", hint: "Graphite canvas · electric blue" },
  { id: "day", title: "Day", hint: "Light shop · Apple Blue" },
];

export function themeTitle(id: Theme): string {
  return THEME_OPTIONS.find((t) => t.id === id)?.title ?? id;
}
