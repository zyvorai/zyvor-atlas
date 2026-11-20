// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import type { StorageBucket } from "../api/types";
import BucketSettings, { buildLifecycle, buildRule, escapeXml, parseLifecycle, parseVersions } from "./BucketSettings";

const bucket: StorageBucket = {
  id: "bkt_1",
  tenant_id: "global",
  name: "photos",
  bucket_name: "photos",
  backend_id: "bkd_ceph_lab",
  state: "bound",
};

describe("BucketSettings", () => {
  beforeEach(() => {
    stubFetch();
    // 404 everywhere = a bucket with nothing configured.
    vi.spyOn(http, "request").mockResolvedValue({ status: 404, data: "" } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders the settings panel for an unconfigured bucket", async () => {
    renderView(<BucketSettings bucket={bucket} onClose={() => {}} />);
    await flushQueries();
    expect(document.body.textContent).toContain("photos · settings");
    expect(document.body.textContent).toContain("Versioning");
  });

  it("renders nothing without a bucket", async () => {
    const { container } = renderView(<BucketSettings bucket={null} onClose={() => {}} />);
    await flushQueries();
    expect(container).toBeEmptyDOMElement();
  });
});

describe("lifecycle XML", () => {
  it("round-trips a rule and keeps the raw XML of untouched rules", () => {
    const xml = buildLifecycle([buildRule("expire-tmp", "tmp/", "7", "3")]);
    const rules = parseLifecycle(xml);
    expect(rules).toHaveLength(1);
    expect(rules[0]).toMatchObject({ id: "expire-tmp", prefix: "tmp/", status: "Enabled", days: "7", noncurrentDays: "3" });
    expect(rules[0].raw).toContain("<Days>7</Days>");
  });

  it("escapes XML in ids and prefixes", () => {
    expect(escapeXml(`a&b<"c">`)).toBe("a&amp;b&lt;&quot;c&quot;&gt;");
    expect(buildRule("x&y", "p<", "1", "")).toContain("<ID>x&amp;y</ID>");
  });
});

describe("versions XML", () => {
  it("parses versions and delete markers", () => {
    const xml =
      '<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">' +
      "<Version><Key>a.txt</Key><VersionId>v2</VersionId><IsLatest>true</IsLatest><LastModified>2026-09-28T10:00:00Z</LastModified><Size>5</Size></Version>" +
      "<DeleteMarker><Key>a.txt</Key><VersionId>v3</VersionId><IsLatest>false</IsLatest><LastModified>2026-09-28T11:00:00Z</LastModified></DeleteMarker>" +
      "</ListVersionsResult>";
    const rows = parseVersions(xml);
    expect(rows).toHaveLength(2);
    expect(rows.find((r) => r.deleteMarker)?.versionId).toBe("v3");
    expect(rows.find((r) => !r.deleteMarker)).toMatchObject({ key: "a.txt", size: "5", isLatest: true });
  });
});
