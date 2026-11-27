// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, describe, expect, it, vi } from "vitest";
import { applyTheme, parseThemePref, resolveTheme } from "./themes";

function stubScheme(dark: boolean) {
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => ({ matches: dark, addEventListener: () => {}, removeEventListener: () => {} })),
  );
}

describe("themes", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("follows the OS when nothing, or nonsense, is stored", () => {
    expect(parseThemePref(null)).toBe("system");
    expect(parseThemePref("")).toBe("system");
    expect(parseThemePref("sepia")).toBe("system");
  });

  it("keeps an explicit choice, including ones stored by earlier versions", () => {
    expect(parseThemePref("dark")).toBe("dark");
    expect(parseThemePref("light")).toBe("light");
    expect(parseThemePref("night")).toBe("dark");
    expect(parseThemePref("day")).toBe("light");
    expect(parseThemePref("carbon")).toBe("dark");
    expect(parseThemePref("apple-lite")).toBe("light");
  });

  it("lets a manual override beat the OS", () => {
    stubScheme(true);
    expect(resolveTheme("system")).toBe("dark");
    expect(resolveTheme("light")).toBe("light");
    stubScheme(false);
    expect(resolveTheme("system")).toBe("light");
    expect(resolveTheme("dark")).toBe("dark");
  });

  it("writes data-theme on the root element", () => {
    stubScheme(true);
    applyTheme("system");
    expect(document.documentElement.dataset.theme).toBe("dark");
    applyTheme("light");
    expect(document.documentElement.dataset.theme).toBe("light");
  });
});
