// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import {
  forecastUrgency,
  fmtBytes,
  fmtBytesOpt,
  fmtForecastFill,
  fmtPct,
  fmtSi,
  healthKind,
  stateKind,
} from "./format";

describe("fmtBytes", () => {
  it("formats zero and small values without decimals", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(512)).toBe("512 B");
  });
  it("scales through KiB/GiB", () => {
    expect(fmtBytes(1024)).toBe("1.0 KiB");
    expect(fmtBytes(1024 ** 3)).toBe("1.0 GiB");
  });
});

describe("fmtBytesOpt", () => {
  it("uses an em dash for unknown capacity", () => {
    expect(fmtBytesOpt(null)).toBe("—");
    expect(fmtBytesOpt(undefined)).toBe("—");
    expect(fmtBytesOpt(NaN)).toBe("—");
  });
});

describe("fmtForecastFill / forecastUrgency", () => {
  it("hides absurd horizons", () => {
    expect(fmtForecastFill(4000)).toBeNull();
    expect(forecastUrgency(4000)).toBeNull();
  });
  it("hides sub-MiB/day noise even when days look finite", () => {
    expect(fmtForecastFill(120, 64 * 1024)).toBeNull();
  });
  it("formats actionable horizons", () => {
    expect(fmtForecastFill(2)).toBe("Full in ~2.0d");
    expect(fmtForecastFill(60)).toBe("Full in ~60d");
    expect(fmtForecastFill(400)).toMatch(/^Full in ~1\./);
    expect(fmtForecastFill(30, 5 * 1024 ** 3)).toBe("Full in ~30d · +5.0 GiB/day");
    expect(forecastUrgency(2)).toBe("danger");
    expect(forecastUrgency(10)).toBe("warning");
    expect(forecastUrgency(30)).toBe("muted");
  });
});

describe("fmtSi / fmtPct", () => {
  it("compacts counters without locale grouping", () => {
    expect(fmtSi(999)).toBe("999");
    expect(fmtSi(1500)).toBe("1.5K");
    expect(fmtSi(1_200_000)).toBe("1.2M");
  });
  it("clamps percent", () => {
    expect(fmtPct(-5)).toBe(0);
    expect(fmtPct(50.4)).toBe(50);
    expect(fmtPct(150)).toBe(100);
  });
});

describe("healthKind / stateKind", () => {
  it("maps known health and job states", () => {
    expect(healthKind("ok")).toBe("success");
    expect(healthKind("critical")).toBe("danger");
    expect(stateKind("succeeded")).toBe("success");
    expect(stateKind("failed")).toBe("danger");
    expect(stateKind("running")).toBe("info");
  });
});
