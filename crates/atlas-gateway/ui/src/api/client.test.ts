// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import { apiError, isUnauthorized, jobIdOf } from "./client";

describe("apiError", () => {
  it("prefers gateway error envelope", () => {
    expect(apiError({ response: { data: { error: { message: "quota exceeded" } } } })).toBe(
      "quota exceeded",
    );
  });
  it("falls back to Error.message then generic", () => {
    expect(apiError(new Error("network down"))).toBe("network down");
    expect(apiError({})).toBe("request failed");
  });
});

describe("isUnauthorized", () => {
  it("detects HTTP 401", () => {
    expect(isUnauthorized({ response: { status: 401 } })).toBe(true);
    expect(isUnauthorized({ response: { status: 403 } })).toBe(false);
    expect(isUnauthorized({})).toBe(false);
  });
});

describe("jobIdOf", () => {
  it("reads job id from enqueue responses", () => {
    expect(jobIdOf({ job_id: "job_1" })).toBe("job_1");
    expect(jobIdOf({ resource: { job_id: "job_2" } })).toBe("job_2");
    expect(jobIdOf({})).toBeUndefined();
  });
});
