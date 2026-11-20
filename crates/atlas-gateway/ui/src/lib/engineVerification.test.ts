// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import { ENGINE_VERIFICATION, stageBadge, verificationFor } from "./engineVerification";

describe("engineVerification", () => {
  it("covers all six engines", () => {
    expect(ENGINE_VERIFICATION.map((e) => e.engine).sort()).toEqual(
      ["mariadb", "mongodb", "mysql", "oracle", "postgres", "sqlserver"].sort(),
    );
  });
  it("marks every engine's cutover live; heterogeneous validate stays advisory", () => {
    expect(stageBadge(verificationFor("postgres"), "cdc")).toBe("live");
    expect(stageBadge(verificationFor("postgres"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("mysql"), "cdc")).toBe("live");
    expect(stageBadge(verificationFor("mysql"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("mariadb"), "cdc")).toBe("live");
    expect(stageBadge(verificationFor("mariadb"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("mongodb"), "cdc")).toBe("live");
    expect(stageBadge(verificationFor("mongodb"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("oracle"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("sqlserver"), "cutover")).toBe("live");
    expect(stageBadge(verificationFor("oracle"), "validate")).toBe("pending");
  });
});
