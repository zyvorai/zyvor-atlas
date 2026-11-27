// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { describe, expect, it } from "vitest";
import { depth, depthWidth } from "./depth";

describe("depth", () => {
  it("maps occupancy onto the fill-level ramp", () => {
    expect(depth(0).cls).toBe("dp1");
    expect(depth(39).name).toBe("low");
    expect(depth(40).name).toBe("moderate");
    expect(depth(60).name).toBe("elevated");
    expect(depth(75).name).toBe("high");
    expect(depth(90).name).toBe("critical");
    expect(depth(100).cls).toBe("dp5");
  });
  it("clamps null/NaN to the lowest level", () => {
    expect(depth(null).cls).toBe("dp1");
    expect(depth(undefined).cls).toBe("dp1");
  });
});

describe("depthWidth", () => {
  it("keeps a hairline at 0%", () => {
    expect(depthWidth(0)).toBe("0.6%");
    expect(depthWidth(50)).toBe("50%");
    expect(depthWidth(100)).toBe("100%");
  });
});
