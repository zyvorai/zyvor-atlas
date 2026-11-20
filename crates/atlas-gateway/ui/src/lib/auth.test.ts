// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import { normalizeNavRole, roleFromToken, roleLabel, roleLevel } from "./auth";

describe("auth", () => {
  it("roleLevel maps server role strings", () => {
    expect(roleLevel("viewer")).toBe(0);
    expect(roleLevel("storage.operator")).toBe(1);
    expect(roleLevel("storage.admin")).toBe(2);
    expect(roleLevel("product.service.zeus")).toBe(1);
  });

  it("normalizeNavRole collapses storage.* aliases", () => {
    expect(normalizeNavRole("storage.security")).toBe("admin");
    expect(normalizeNavRole("operator")).toBe("operator");
  });

  it("roleFromToken decodes JWT payload role claim", () => {
    const payload = btoa(JSON.stringify({ role: "operator" }));
    const token = `hdr.${payload}.sig`;
    expect(roleFromToken(token)).toBe("operator");
    expect(roleFromToken("")).toBe("viewer");
  });

  it("roleLabel capitalizes nav roles", () => {
    expect(roleLabel("admin")).toBe("Admin");
  });
});
