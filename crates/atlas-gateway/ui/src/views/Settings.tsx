// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useUi, type Density, type Theme } from "../store/ui";
import { THEME_OPTIONS } from "../lib/themes";
import { SettingsPage, type SettingsBlock } from "../ui/templates/SettingsPage";
import { navCrumbs } from "../nav/routes";

const DENSITY_OPTIONS: { id: Density; title: string; hint: string }[] = [
  { id: "comfortable", title: "Comfortable", hint: "Roomier charts and tables" },
  { id: "compact", title: "Compact", hint: "Tighter rows for large fleets" },
];

export default function Settings() {
  const theme = useUi((s) => s.theme);
  const setTheme = useUi((s) => s.setTheme);
  const density = useUi((s) => s.density);
  const setDensity = useUi((s) => s.setDensity);

  const blocks: SettingsBlock<Theme | Density>[] = [
    {
      label: "Theme",
      hint: "System follows this device. Light and Dark stay fixed.",
      value: theme,
      onChange: (v) => setTheme(v as Theme),
      options: THEME_OPTIONS,
      ariaLabel: "Theme",
    },
    {
      label: "Density",
      hint: "Sets table rows and spacing.",
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
      eyebrow="This browser"
      title="Settings"
      state="Choose how the console looks."
      panelLabel="Appearance"
      blocks={blocks}
    />
  );
}
