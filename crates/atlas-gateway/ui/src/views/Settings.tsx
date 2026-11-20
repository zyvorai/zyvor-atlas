// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useUi, type Density, type Theme } from "../store/ui";
import { THEME_OPTIONS } from "../lib/themes";
import { SettingsPage, type SettingsBlock } from "../ui/templates/SettingsPage";
import { navCrumbs } from "../nav/routes";

const DENSITY_OPTIONS: { id: Density; title: string; hint: string }[] = [
  { id: "comfortable", title: "Comfortable", hint: "Default spacing for charts and tables" },
  { id: "compact", title: "Compact", hint: "Tighter rows — more density on large fleets" },
];

export default function Settings() {
  const theme = useUi((s) => s.theme);
  const setTheme = useUi((s) => s.setTheme);
  const density = useUi((s) => s.density);
  const setDensity = useUi((s) => s.setDensity);

  const blocks: SettingsBlock<Theme | Density>[] = [
    {
      label: "Shell theme",
      hint: "Night is graphite (#0b0b0f) with electric blue. Day is classic light shop (#F5F5F7 + Apple Blue). Cosmic Orange is reserved for the brand mark only.",
      value: theme,
      onChange: (v) => setTheme(v as Theme),
      options: THEME_OPTIONS,
      ariaLabel: "Shell theme",
    },
    {
      label: "Density",
      hint: "Applies to tables and page padding in this console.",
      value: density,
      onChange: (v) => setDensity(v as Density),
      options: DENSITY_OPTIONS,
      ariaLabel: "Density",
      columns: 2,
    },
  ];

  return (
    <SettingsPage
      crumbs={navCrumbs("settings")}
      eyebrow="CONSOLE · APPEARANCE"
      title="Settings"
      state="Look & feel and density for this browser. Night is the dark graphite shell; Day is the light shop shell."
      panelLabel="Appearance"
      blocks={blocks}
    />
  );
}
