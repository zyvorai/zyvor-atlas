// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Source-level guarantees for the observability and infrastructure pages: colour only through the
// --a-* tokens, page styles inside the components layer, and none of the retired copy.
// @ts-expect-error the project ships no Node type declarations
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// Injected by Vitest. Stylesheets are read from disk because Vitest blanks CSS imports.
declare const __dirname: string;

const PAGES = [
  "Observatory",
  "IoHeatmap",
  "OpsAdvisor",
  "Activity",
  "Alerts",
  "Jobs",
  "Audit",
  "Ceph",
  "Disks",
  "MacDisks",
  "FastStorage",
  "Maintenance",
  "DR",
  "Simple",
];
const STYLED = PAGES.filter((p) => p !== "Alerts");

const sources = import.meta.glob("./*.tsx", { query: "?raw", import: "default", eager: true }) as Record<
  string,
  string
>;

const COLOUR = /#[0-9a-fA-F]{3,8}\b|\b(?:rgb|rgba|hsl|hsla|oklab|oklch)\(/;
const OLD_TOKEN = /var\(\s*--(?:at-|d[1-5]\b|page-inset)/;
const OLD_COPY = /soundings|bathymet|\bledger\b|\bestate\b|eyebrow="[A-Z ]+·/i;

describe("page contract", () => {
  it.each(PAGES)("%s uses theme tokens and plain copy", (page) => {
    // The `soundings` prop is the shared Table's name for its panel layout, not visible copy.
    const source = sources[`./${page}.tsx`].replace(/^\s*soundings\s*$/gm, "");
    expect(source).not.toMatch(COLOUR);
    expect(source).not.toMatch(OLD_TOKEN);
    expect(source).not.toMatch(OLD_COPY);
  });

  it.each(STYLED)("%s.css stays in the components layer and uses theme tokens", (page) => {
    const css = (readFileSync(`${__dirname}/${page}.css`, "utf8") as string).replace(/\/\*[\s\S]*?\*\//g, "");
    expect(css.trimStart().startsWith("@layer components {")).toBe(true);
    expect(css).not.toMatch(COLOUR);
    expect(css).not.toMatch(OLD_TOKEN);
  });
});
