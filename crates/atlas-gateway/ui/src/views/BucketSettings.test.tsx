// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
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

type Reply = { status: number; data: string };

/** Answers each proxied call by method and subresource; anything unlisted is "never configured". */
function store(replies: Record<string, Reply>) {
  return vi.spyOn(http, "request").mockImplementation(((cfg: { method: string; url: string }) =>
    Promise.resolve(replies[`${cfg.method} ${cfg.url.split("?")[1]}`] ?? { status: 404, data: "" })) as never);
}
const versioned: Reply = {
  status: 200,
  data: "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
};
const failure: Reply = {
  status: 502,
  data: '{"error":{"code":"DRIVER","message":"no reachable Kubernetes cluster"}}',
};
const writes = (spy: ReturnType<typeof store>) =>
  spy.mock.calls.filter(([cfg]) => (cfg as { method: string }).method !== "GET");

async function open(tab?: string) {
  renderView(<BucketSettings bucket={bucket} onClose={() => {}} />);
  if (tab) fireEvent.click(await screen.findByRole("button", { name: tab }));
}
const button = (name: string) => screen.getByRole("button", { name });

describe("BucketSettings reads and writes", () => {
  beforeEach(() => stubFetch());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("enables versioning through the subresource proxy", async () => {
    const spy = store({
      "GET versioning": { status: 200, data: "<VersioningConfiguration/>" },
      "PUT versioning": { status: 200, data: "" },
    });
    await open();
    await screen.findByText("Unversioned");
    fireEvent.click(button("Enable"));
    await waitFor(() => expect(writes(spy)).toHaveLength(1));
    expect(writes(spy)[0][0]).toMatchObject({
      method: "PUT",
      url: "/buckets/bkt_1/subresource?versioning",
      headers: { "Content-Type": "application/xml" },
      data: expect.stringContaining("<Status>Enabled</Status>"),
    });
  });

  it("does not offer to change versioning it could not read", async () => {
    store({ "GET versioning": failure });
    await open();
    expect(await screen.findByRole("alert")).toHaveTextContent("no reachable Kubernetes cluster");
    expect(button("Enable")).toBeDisabled();
    expect(button("Suspend")).toBeDisabled();
  });

  it("shows an unconfigured bucket as private, with no rules and Object Lock off", async () => {
    store({});
    await open("Access");
    expect(await screen.findByText("private")).toBeInTheDocument();
    expect(button("Private")).toBeEnabled();
    fireEvent.click(button("Lifecycle"));
    expect(await screen.findByText("No lifecycle rules.")).toBeInTheDocument();
    expect(button("Add rule")).toBeEnabled();
    fireEvent.click(button("Object Lock"));
    expect(await screen.findByText("not enabled")).toBeInTheDocument();
  });

  it("never reports a bucket as private when the policy read failed", async () => {
    store({ "GET policy": failure });
    await open("Access");
    expect(await screen.findByRole("alert")).toHaveTextContent("no reachable Kubernetes cluster");
    expect(screen.getByText("unknown")).toBeInTheDocument();
    expect(screen.queryByText("private")).toBeNull();
    for (const name of ["Private", "Public read…", "Apply policy"]) expect(button(name)).toBeDisabled();
    expect(screen.getByLabelText("Bucket policy JSON")).toBeDisabled();
  });

  it("treats Atlas's own 404 and a missing bucket as errors, not as unconfigured", async () => {
    store({
      "GET policy": { status: 404, data: '{"error":{"code":"NOT_FOUND","message":"bucket secret gone"}}' },
      "GET lifecycle": { status: 404, data: "<Error><Code>NoSuchBucket</Code><Message>no bucket</Message></Error>" },
    });
    await open("Access");
    expect(await screen.findByRole("alert")).toHaveTextContent("bucket secret gone");
    fireEvent.click(button("Lifecycle"));
    expect(await screen.findByText("no bucket")).toBeInTheDocument();
    expect(screen.queryByText("No lifecycle rules.")).toBeNull();
  });

  it("cannot overwrite lifecycle rules it could not read", async () => {
    const spy = store({ "GET lifecycle": failure });
    await open("Lifecycle");
    expect(await screen.findByRole("alert")).toHaveTextContent("no reachable Kubernetes cluster");
    expect(button("Add rule")).toBeDisabled();
    expect(writes(spy)).toHaveLength(0);
  });

  it("reports a failed Object Lock read and recovers on retry", async () => {
    const replies: Record<string, Reply> = { "GET object-lock": failure };
    store(replies);
    await open("Object Lock");
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't read Object Lock.");
    expect(screen.queryByText("not enabled")).toBeNull();
    replies["GET object-lock"] = {
      status: 200,
      data: "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>",
    };
    fireEvent.click(button("Retry"));
    expect(await screen.findByText("enabled")).toBeInTheDocument();
  });

  it("reports a failed version listing instead of an empty one", async () => {
    store({ "GET versioning": versioned, "GET versions": failure });
    await open("Versions");
    expect(await screen.findByRole("alert")).toHaveTextContent("no reachable Kubernetes cluster");
    expect(screen.queryByText("No object versions.")).toBeNull();
  });

  it("says when the version listing is only the first page", async () => {
    store({
      "GET versioning": versioned,
      "GET versions": { status: 200, data: "<ListVersionsResult><IsTruncated>true</IsTruncated></ListVersionsResult>" },
    });
    await open("Versions");
    expect(await screen.findByText(/Only the first page of versions/)).toBeInTheDocument();
  });

  it("labels access from the stored policy, not from unsaved edits", async () => {
    store({ "GET policy": { status: 200, data: '{"Statement":[{"Effect":"Deny"}]}' } });
    await open("Access");
    expect(await screen.findByText("custom policy")).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Bucket policy JSON"), {
      target: { value: '{"Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject"}]}' },
    });
    expect(screen.getByText("custom policy")).toBeInTheDocument();
    expect(screen.queryByText("public read")).toBeNull();
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
