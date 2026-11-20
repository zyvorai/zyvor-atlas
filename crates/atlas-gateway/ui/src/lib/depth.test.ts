// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import { depth, depthWidth } from "./depth";

describe("depth", () => {
  it("maps occupancy onto the bathymetric ramp", () => {
    expect(depth(0).cls).toBe("dp1");
    expect(depth(39).name).toBe("shoal");
    expect(depth(40).name).toBe("shelf");
    expect(depth(60).name).toBe("slope");
    expect(depth(75).name).toBe("deep");
    expect(depth(90).name).toBe("abyssal");
    expect(depth(100).cls).toBe("dp5");
  });
  it("clamps null/NaN to shoal", () => {
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
