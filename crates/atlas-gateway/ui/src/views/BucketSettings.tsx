// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Per-bucket settings: versioning, lifecycle, access policy, and object versions. Every call goes
// through the gateway's generic bucket-subresource proxy (`/buckets/{id}/subresource?<name>`),
// which signs the request against whichever backend owns the bucket (Ceph RGW, or any
// bring-your-own S3 endpoint) and returns its XML/JSON verbatim — parsed here, not re-declared.
// Quota is Ceph-specific (set at bucket-create time; see the Buckets page) and not shown here.
import { useCallback, useEffect, useState } from "react";
import { apiError, http, toast } from "../api/client";
import type { StorageBucket } from "../api/types";
import { Badge, Button, SlideOver, Tabs } from "../ui/kit";
import { Table } from "../ui/Table";
import { fmtBytes } from "../lib/format";
import "./BucketSettings.css";

const S3_NS = "http://s3.amazonaws.com/doc/2006-03-01/";
const TABS = ["Versioning", "Object Lock", "Lifecycle", "Access", "Versions"] as const;
type TabName = (typeof TABS)[number];

interface Raw {
  status: number;
  text: string;
}

/** One proxied call. Resolves on any HTTP status so callers can read the store's error body; a 401
 * rejects, so the client's interceptor ends the session as it does everywhere else. */
async function call(method: string, url: string, body?: string, contentType?: string): Promise<Raw> {
  const r = await http.request({
    method,
    url,
    data: body,
    headers: body !== undefined && contentType ? { "Content-Type": contentType } : undefined,
    responseType: "text",
    transformResponse: [(d: unknown) => d],
    validateStatus: (status) => status !== 401,
  });
  return { status: r.status, text: typeof r.data === "string" ? r.data : "" };
}

const ok = (r: Raw) => r.status >= 200 && r.status < 300;

/** A readable message from an S3 XML error, an Atlas JSON error, or the raw text. */
function errText(r: Raw): string {
  const m = r.text.match(/<Message>([^<]*)<\/Message>/);
  if (m) return m[1];
  try {
    const j = JSON.parse(r.text);
    return j?.error?.message || j?.message || `HTTP ${r.status}`;
  } catch {
    return r.text.slice(0, 160) || `HTTP ${r.status}`;
  }
}

/** A 404 from the object store means "never configured". A 404 that is Atlas's own JSON envelope
 * (unknown bucket or credentials secret), or that says the bucket is gone, is a failure. */
const notConfigured = (r: Raw) =>
  r.status === 404 && !r.text.trimStart().startsWith("{") && !r.text.includes("<Code>NoSuchBucket</Code>");

/** Reads a subresource: its body, or `null` when it was never configured. Throws on anything else, so
 * a failed read is never shown as an empty or private bucket. */
async function read(url: string): Promise<string | null> {
  const r = await call("GET", url);
  if (ok(r)) return r.text;
  if (notConfigured(r)) return null;
  throw new Error(errText(r));
}

function ReadError({ what, message, onRetry }: { what: string; message: string; onRetry: () => void }) {
  return (
    <div className="at-table-error" role="alert">
      <span>Couldn&apos;t read {what}.</span>
      <span className="detail mono">{message}</span>
      <Button size="sm" onClick={onRetry}>
        Retry
      </Button>
    </div>
  );
}

const s3Url = (id: string, sub: string) => `/buckets/${encodeURIComponent(id)}/subresource?${sub}`;

function parseXml(text: string): Document {
  return new DOMParser().parseFromString(text, "application/xml");
}
const firstText = (el: Element | Document, name: string): string =>
  el.getElementsByTagName(name)[0]?.textContent?.trim() ?? "";

export const escapeXml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&apos;");

// ---------------------------------------------------------------- lifecycle model

export interface LifecycleRule {
  id: string;
  prefix: string;
  status: string;
  days: string;
  noncurrentDays: string;
  /** The rule's XML as the object store returned it — re-sent untouched so unedited rules (transitions, ...) survive. */
  raw: string;
}

export function parseLifecycle(text: string): LifecycleRule[] {
  const doc = parseXml(text);
  const ser = new XMLSerializer();
  return Array.from(doc.getElementsByTagName("Rule")).map((el) => ({
    id: firstText(el, "ID"),
    prefix: firstText(el, "Prefix"),
    status: firstText(el, "Status"),
    days: el.getElementsByTagName("Expiration")[0]
      ? firstText(el.getElementsByTagName("Expiration")[0], "Days")
      : "",
    noncurrentDays: el.getElementsByTagName("NoncurrentVersionExpiration")[0]
      ? firstText(el.getElementsByTagName("NoncurrentVersionExpiration")[0], "NoncurrentDays")
      : "",
    raw: ser.serializeToString(el),
  }));
}

export function buildRule(id: string, prefix: string, days: string, noncurrentDays: string): string {
  const exp = days ? `<Expiration><Days>${escapeXml(days)}</Days></Expiration>` : "";
  const nc = noncurrentDays
    ? `<NoncurrentVersionExpiration><NoncurrentDays>${escapeXml(noncurrentDays)}</NoncurrentDays></NoncurrentVersionExpiration>`
    : "";
  return `<Rule><ID>${escapeXml(id)}</ID><Filter><Prefix>${escapeXml(prefix)}</Prefix></Filter><Status>Enabled</Status>${exp}${nc}</Rule>`;
}

export const buildLifecycle = (rules: string[]) =>
  `<LifecycleConfiguration xmlns="${S3_NS}">${rules.join("")}</LifecycleConfiguration>`;

// ---------------------------------------------------------------- panel

export default function BucketSettings({ bucket, onClose }: { bucket: StorageBucket | null; onClose: () => void }) {
  const [tab, setTab] = useState<TabName>("Versioning");
  const [versioning, setVersioning] = useState<string | null>(null);
  const [versioningError, setVersioningError] = useState<string | null>(null);
  const id = bucket ? bucket.id : "";
  const name = bucket ? bucket.bucket_name || bucket.name || bucket.id : "";

  const loadVersioning = useCallback(async () => {
    if (!id) return;
    try {
      const r = await call("GET", s3Url(id, "versioning"));
      if (!ok(r)) throw new Error(errText(r));
      setVersioning(firstText(parseXml(r.text), "Status") || "Unversioned");
      setVersioningError(null);
    } catch (e) {
      setVersioning("Unknown");
      setVersioningError(apiError(e));
    }
  }, [id]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount: the loader's state updates are the result of the request, not a render loop
    if (id) loadVersioning();
  }, [id, loadVersioning]);

  if (!bucket) return null;
  const tabs = TABS.filter((t) => t !== "Versions" || versioning === "Enabled" || versioning === "Suspended");
  const close = () => {
    setTab("Versioning");
    setVersioning(null);
    setVersioningError(null);
    onClose();
  };

  return (
    <SlideOver open={!!bucket} onClose={close} title={<span className="mono">{name} · settings</span>} width={620}>
      <Tabs tabs={[...tabs]} value={tabs.includes(tab) ? tab : "Versioning"} onChange={(t) => setTab(t as TabName)} />
      {tab === "Versioning" && (
        <VersioningTab id={id} status={versioning} error={versioningError} onChanged={loadVersioning} />
      )}
      {tab === "Object Lock" && <ObjectLockTab id={id} />}
      {tab === "Lifecycle" && <LifecycleTab bucketId={id} />}
      {tab === "Access" && <AccessTab id={id} bucket={name} />}
      {tab === "Versions" && <VersionsTab id={id} />}
    </SlideOver>
  );
}

// ---------------------------------------------------------------- versioning

function VersioningTab({
  id,
  status,
  error,
  onChanged,
}: {
  id: string;
  status: string | null;
  error: string | null;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const set = async (next: "Enabled" | "Suspended") => {
    setBusy(true);
    try {
      const body = `<VersioningConfiguration xmlns="${S3_NS}"><Status>${next}</Status></VersioningConfiguration>`;
      const r = await call("PUT", s3Url(id, "versioning"), body, "application/xml");
      if (!ok(r)) throw new Error(errText(r));
      toast(`versioning ${next.toLowerCase()}`, "ok");
      onChanged();
    } catch (e) {
      toast(`versioning: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };
  const unknown = status === null || status === "Unknown";
  return (
    <div>
      {error && <ReadError what="versioning" message={error} onRetry={onChanged} />}
      <p className="bs-status">
        Status:{" "}
        <Badge kind={status === "Enabled" ? "success" : status === "Suspended" ? "warning" : "neutral"} dot>
          {status ?? "…"}
        </Badge>
      </p>
      <p className="bs-note">
        With versioning enabled every overwrite and delete keeps the previous version (a delete adds a delete
        marker). Suspending stops creating new versions but keeps the existing ones.
      </p>
      <div className="flex gap-2">
        <Button variant="primary" loading={busy} disabled={unknown || status === "Enabled"} onClick={() => set("Enabled")}>
          Enable
        </Button>
        <Button loading={busy} disabled={status !== "Enabled"} onClick={() => set("Suspended")}>
          Suspend
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- object lock

function ObjectLockTab({ id }: { id: string }) {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [mode, setMode] = useState("GOVERNANCE");
  const [days, setDays] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const xml = await read(s3Url(id, "object-lock"));
      const doc = xml === null ? null : parseXml(xml);
      setEnabled(doc !== null && firstText(doc, "ObjectLockEnabled") === "Enabled");
      setMode((doc && firstText(doc, "Mode")) || "GOVERNANCE");
      setDays(doc ? firstText(doc, "Days") : "");
      setError(null);
    } catch (e) {
      setEnabled(null);
      setError(apiError(e));
    }
  }, [id]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount: the loader's state updates are the result of the request, not a render loop
    load();
  }, [load]);

  const save = async (clearRule: boolean) => {
    setBusy(true);
    try {
      const rule =
        !clearRule && days.trim()
          ? `<Rule><DefaultRetention><Mode>${mode}</Mode><Days>${escapeXml(days.trim())}</Days></DefaultRetention></Rule>`
          : "";
      const body = `<ObjectLockConfiguration xmlns="${S3_NS}"><ObjectLockEnabled>Enabled</ObjectLockEnabled>${rule}</ObjectLockConfiguration>`;
      const r = await call("PUT", s3Url(id, "object-lock"), body, "application/xml");
      if (!ok(r)) throw new Error(errText(r));
      toast(clearRule ? "default retention cleared" : "default retention set", "ok");
      await load();
    } catch (e) {
      toast(`object lock: ${apiError(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  const retry = () => {
    setError(null);
    load();
  };

  if (error) return <ReadError what="Object Lock" message={error} onRetry={retry} />;
  if (enabled === null) return <div className="bs-note">Loading…</div>;
  if (!enabled) {
    return (
      <div>
        <p className="bs-status">
          Object Lock: <Badge kind="neutral" dot>not enabled</Badge>
        </p>
        <div className="bs-note">
          Object Lock (versioned, write-once-read-many retention) can only be turned on when a bucket is
          created, and this bucket does not have it. This console&apos;s create form does not offer it; a
          new bucket gets it through the API (<code>POST /buckets</code> with{" "}
          <code>&quot;object_lock&quot;: true</code>).
        </div>
      </div>
    );
  }
  return (
    <div>
      <p className="bs-status">
        Object Lock: <Badge kind="warning" dot>enabled</Badge>
      </p>
      <div className="at-caption">Default retention (applied to new objects with no explicit retention)</div>
      <div className="bs-retention">
        <select className="field" aria-label="Retention mode" value={mode} onChange={(e) => setMode(e.target.value)}>
          <option value="GOVERNANCE">Governance (can be overridden by a permitted user)</option>
          <option value="COMPLIANCE">Compliance (cannot be shortened or removed by anyone)</option>
        </select>
        <input
          className="field bs-days"
          type="number"
          min={1}
          aria-label="Retention days"
          placeholder="days"
          value={days}
          onChange={(e) => setDays(e.target.value)}
        />
      </div>
      <div className="bs-actions">
        <Button variant="primary" loading={busy} disabled={!days.trim()} onClick={() => save(false)}>
          Set default retention
        </Button>
        <Button loading={busy} onClick={() => save(true)}>Clear default retention</Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- lifecycle

function LifecycleTab({ bucketId }: { bucketId: string }) {
  const [rules, setRules] = useState<LifecycleRule[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [id, setId] = useState("");
  const [prefix, setPrefix] = useState("");
  const [days, setDays] = useState("");
  const [ncDays, setNcDays] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const xml = await read(s3Url(bucketId, "lifecycle"));
      setRules(xml === null ? [] : parseLifecycle(xml));
      setError(null);
    } catch (e) {
      // Saving replaces the whole configuration, so unread rules must never look like "no rules".
      setRules(null);
      setError(apiError(e));
    }
  }, [bucketId]);
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount: the loader's state updates are the result of the request, not a render loop
    load();
  }, [load]);

  const save = async (next: LifecycleRule[]) => {
    setBusy(true);
    try {
      const r = next.length
        ? await call("PUT", s3Url(bucketId, "lifecycle"), buildLifecycle(next.map((x) => x.raw)), "application/xml")
        : await call("DELETE", s3Url(bucketId, "lifecycle"));
      if (!ok(r) && r.status !== 404) throw new Error(errText(r));
      toast("lifecycle saved", "ok");
      await load();
    } catch (e) {
      toast(`lifecycle: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  const add = () => {
    const ruleId = id.trim();
    if (!ruleId || (!days.trim() && !ncDays.trim())) {
      toast("rule needs an ID and an expiration (days and/or noncurrent days)", "err");
      return;
    }
    const raw = buildRule(ruleId, prefix.trim(), days.trim(), ncDays.trim());
    save([...(rules ?? []), { id: ruleId, prefix, status: "Enabled", days, noncurrentDays: ncDays, raw }]);
    setId("");
    setPrefix("");
    setDays("");
    setNcDays("");
  };

  return (
    <div>
      <Table
        rows={rules ?? undefined}
        error={!!error}
        errorDetail={error ?? undefined}
        onRetry={() => {
          setError(null);
          load();
        }}
        rowKey={(r) => r.id + r.raw.length}
        cols={[
          { h: "ID", f: (r) => r.id || "—", mono: true },
          { h: "Prefix", f: (r) => r.prefix || "(all)", mono: true },
          { h: "Status", f: (r) => <Badge kind={r.status === "Enabled" ? "success" : "neutral"}>{r.status || "?"}</Badge> },
          { h: "Expire", f: (r) => (r.days ? `${r.days} d` : "—") },
          { h: "Noncurrent", f: (r) => (r.noncurrentDays ? `${r.noncurrentDays} d` : "—") },
        ]}
        actions={(r) => (
          <Button size="sm" variant="danger" loading={busy} onClick={() => save((rules ?? []).filter((x) => x !== r))}>
            Delete
          </Button>
        )}
        empty="No lifecycle rules."
      />
      <div className="bs-form">
        <div className="at-caption">Add rule</div>
        <input className="field" placeholder="rule id" aria-label="Rule ID" value={id} onChange={(e) => setId(e.target.value)} />
        <input className="field" placeholder="prefix (empty = whole bucket)" aria-label="Prefix" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
        <input className="field" type="number" min={1} placeholder="expire current versions after (days)" aria-label="Expire current versions after (days)" value={days} onChange={(e) => setDays(e.target.value)} />
        <input className="field" type="number" min={1} placeholder="expire noncurrent versions after (days, optional)" aria-label="Expire noncurrent versions after (days)" value={ncDays} onChange={(e) => setNcDays(e.target.value)} />
        <div>
          <Button variant="primary" loading={busy} disabled={rules === null} onClick={add}>
            Add rule
          </Button>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- access policy

const publicReadPolicy = (bucket: string) =>
  JSON.stringify(
    {
      Version: "2012-10-17",
      Statement: [
        {
          Effect: "Allow",
          Principal: { AWS: ["*"] },
          Action: ["s3:GetObject"],
          Resource: [`arn:aws:s3:::${bucket}/*`],
        },
      ],
    },
    null,
    2,
  );

/** True when the policy is the anonymous "allow s3:GetObject" preset (any principal spelling). */
function isPublicRead(policyText: string): boolean {
  try {
    const stmts = (JSON.parse(policyText) as { Statement?: unknown }).Statement;
    const list = Array.isArray(stmts) ? stmts : [stmts];
    return (
      list.length > 0 &&
      list.every((raw) => {
        const st = raw as { Effect?: string; Principal?: unknown; Action?: unknown };
        const actions = ([] as unknown[]).concat(st.Action ?? []);
        const principal = JSON.stringify(st.Principal ?? null);
        return (
          st.Effect === "Allow" &&
          (st.Principal === "*" || principal.includes('"*"')) &&
          actions.length > 0 &&
          actions.every((a) => a === "s3:GetObject")
        );
      })
    );
  } catch {
    return false;
  }
}

function AccessTab({ id, bucket }: { id: string; bucket: string }) {
  const [text, setText] = useState("");
  // What the store holds: `undefined` until it has been read, `null` when there is no policy.
  const [policy, setPolicy] = useState<string | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [confirmPublic, setConfirmPublic] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const raw = await read(s3Url(id, "policy"));
      let shown = raw ?? "";
      try {
        if (raw !== null) shown = JSON.stringify(JSON.parse(raw), null, 2);
      } catch {
        // Not JSON: show what the store returned.
      }
      setPolicy(raw);
      setText(shown);
      setError(null);
    } catch (e) {
      setPolicy(undefined);
      setError(apiError(e));
    }
  }, [id]);
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount: the loader's state updates are the result of the request, not a render loop
    load();
  }, [load]);
  const unread = policy === undefined;
  const access = unread
    ? error
      ? "unknown"
      : "…"
    : policy === null
      ? "private"
      : isPublicRead(policy)
        ? "public read"
        : "custom policy";

  const put = async (json: string) => {
    setBusy(true);
    try {
      JSON.parse(json);
      const r = await call("PUT", s3Url(id, "policy"), json, "application/json");
      if (!ok(r)) throw new Error(errText(r));
      toast("policy applied", "ok");
      setConfirmPublic(false);
      await load();
    } catch (e) {
      toast(`policy: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };
  const makePrivate = async () => {
    setBusy(true);
    try {
      const r = await call("DELETE", s3Url(id, "policy"));
      if (!ok(r) && r.status !== 404) throw new Error(errText(r));
      toast("bucket is private", "ok");
      await load();
    } catch (e) {
      toast(`policy: ${e instanceof Error ? e.message : String(e)}`, "err");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      {error && (
        <ReadError
          what="the bucket policy"
          message={error}
          onRetry={() => {
            setError(null);
            load();
          }}
        />
      )}
      <p className="bs-status">
        Access:{" "}
        <Badge kind={unread ? "neutral" : policy === null ? "success" : "warning"} dot>
          {access}
        </Badge>
      </p>
      <div className="bs-actions bs-gap">
        <Button loading={busy} disabled={unread} onClick={makePrivate}>
          Private
        </Button>
        {!confirmPublic ? (
          <Button disabled={unread} onClick={() => setConfirmPublic(true)}>
            Public read…
          </Button>
        ) : (
          <Button variant="danger" loading={busy} onClick={() => put(publicReadPolicy(bucket))}>
            Confirm: make every object world-readable
          </Button>
        )}
      </div>
      <textarea
        className="field mono bs-policy"
        aria-label="Bucket policy JSON"
        placeholder='Bucket policy JSON (empty = private). Example: {"Version":"2012-10-17","Statement":[...]}'
        value={text}
        disabled={unread}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="bs-actions">
        <Button variant="primary" loading={busy} disabled={unread || !text.trim()} onClick={() => put(text)}>
          Apply policy
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- versions

interface ObjectVersion {
  key: string;
  versionId: string;
  isLatest: boolean;
  size: string;
  lastModified: string;
  deleteMarker: boolean;
}

export function parseVersions(text: string): ObjectVersion[] {
  const doc = parseXml(text);
  const rows: ObjectVersion[] = [];
  for (const el of Array.from(doc.getElementsByTagName("Version"))) {
    rows.push({
      key: firstText(el, "Key"),
      versionId: firstText(el, "VersionId"),
      isLatest: firstText(el, "IsLatest") === "true",
      size: firstText(el, "Size"),
      lastModified: firstText(el, "LastModified"),
      deleteMarker: false,
    });
  }
  for (const el of Array.from(doc.getElementsByTagName("DeleteMarker"))) {
    rows.push({
      key: firstText(el, "Key"),
      versionId: firstText(el, "VersionId"),
      isLatest: firstText(el, "IsLatest") === "true",
      size: "",
      lastModified: firstText(el, "LastModified"),
      deleteMarker: true,
    });
  }
  return rows.sort((a, b) => a.key.localeCompare(b.key) || b.lastModified.localeCompare(a.lastModified));
}

function VersionsTab({ id }: { id: string }) {
  const [rows, setRows] = useState<ObjectVersion[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => {
    try {
      const r = await call("GET", s3Url(id, "versions"));
      if (!ok(r)) throw new Error(errText(r));
      setRows(parseVersions(r.text));
      setTruncated(firstText(parseXml(r.text), "IsTruncated") === "true");
      setError(null);
    } catch (e) {
      setRows(null);
      setTruncated(false);
      setError(apiError(e));
    }
  }, [id]);
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- fetch-on-mount: the loader's state updates are the result of the request, not a render loop
    load();
  }, [load]);
  const reload = () => {
    setError(null);
    load();
  };
  return (
    <div>
      <div className="bs-actions bs-gap">
        <Button onClick={reload}>Refresh</Button>
      </div>
      {truncated && (
        <p className="bs-note">Only the first page of versions is shown; the object store has more.</p>
      )}
      <Table
        rows={rows ?? undefined}
        error={!!error}
        errorDetail={error ?? undefined}
        onRetry={reload}
        rowKey={(v) => `${v.key}@${v.versionId}${v.deleteMarker ? "#dm" : ""}`}
        cols={[
          { h: "Key", f: (v) => v.key, mono: true },
          { h: "Version", f: (v) => v.versionId || "null", mono: true },
          { h: "Latest", f: (v) => (v.isLatest ? "yes" : "") },
          { h: "Size", f: (v) => (v.deleteMarker ? "—" : fmtBytes(Number(v.size) || 0)) },
          { h: "Modified", f: (v) => v.lastModified },
          { h: "Marker", f: (v) => (v.deleteMarker ? <Badge kind="warning">delete marker</Badge> : "") },
        ]}
        empty="No object versions."
      />
    </div>
  );
}
