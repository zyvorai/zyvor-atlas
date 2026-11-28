// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Source-level guarantees: colour only through the --a-* tokens, motion only when allowed.
// @ts-expect-error the project ships no Node type declarations
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// Injected by Vitest. The stylesheet is read from disk because Vitest blanks CSS imports.
declare const __dirname: string;
const stylesheet = readFileSync(`${__dirname}/graphics.css`, "utf8") as string;

const sources = import.meta.glob(["./*.{ts,tsx}", "!./*.test.{ts,tsx}"], {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

const css = stylesheet.replace(/\/\*[\s\S]*?\*\//g, "");

const ALLOWED = [
  "bg",
  "surface",
  "surface-2",
  "ink",
  "ink-2",
  "line",
  "blue",
  "ok",
  "warn",
  "fail",
  "radius",
  "font",
  "mono",
].map((n) => `--a-${n}`);

const COLOUR_LITERAL = /#[0-9a-f]{3,8}\b|\b(?:rgb|rgba|hsl|hsla|oklab|oklch)\(/i;

describe("graphics contract", () => {
  it("reads the stylesheet", () => {
    expect(css).toContain(".ag-ring-arc");
  });

  it("reads only the agreed tokens, each with a fallback", () => {
    const uses = [...css.matchAll(/var\(\s*(--a-[a-z0-9-]+)\s*(,?)/g)];
    expect(uses.length).toBeGreaterThan(0);
    for (const [, name, comma] of uses) {
      expect(ALLOWED).toContain(name);
      expect(comma, `${name} needs a fallback`).toBe(",");
    }
  });

  it("has colour literals only as token fallbacks", () => {
    const stray = css
      .split("\n")
      .filter((line) => COLOUR_LITERAL.test(line) && !/^\s*--ag-[a-z0-9-]+:\s*var\(--a-/.test(line));
    expect(stray).toEqual([]);
  });

  it("keeps colour out of the components", () => {
    expect(Object.keys(sources)).toContain("./ClusterHero.tsx");
    for (const [file, text] of Object.entries(sources)) {
      expect(text, file).not.toMatch(COLOUR_LITERAL);
      expect(text, file).not.toMatch(/--a-[a-z]/);
    }
  });

  it("animates only when reduced motion is not requested", () => {
    const start = css.indexOf("@media (prefers-reduced-motion: no-preference)");
    expect(start).toBeGreaterThan(0);
    expect(css.slice(0, start)).not.toMatch(/\b(animation|transition)\s*:|@keyframes/);
    // The motion block is the last thing in the file.
    let depth = 0;
    let end = -1;
    for (let i = css.indexOf("{", start); i < css.length; i++) {
      if (css[i] === "{") depth++;
      else if (css[i] === "}" && --depth === 0) {
        end = i;
        break;
      }
    }
    expect(css.slice(end + 1).trim()).toBe("");
    for (const [file, text] of Object.entries(sources)) {
      expect(text, file).not.toMatch(/<animate|requestAnimationFrame/);
    }
  });
});
