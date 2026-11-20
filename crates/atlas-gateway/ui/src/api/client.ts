// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// axios client for the Atlas REST API + SSE job watcher + toast/job glue.
import axios from "axios";
import { useUi } from "../store/ui";
import type { JobRecord } from "../api/types";

export const API_BASE = "/api/atlas/v1";

export const http = axios.create({ baseURL: API_BASE });

// Toast bus (declared early so the 401 interceptor can clear toasts without TDZ issues).
type Toast = { id: number; kind: "info" | "ok" | "err"; msg: string };
let toastId = 1;
const toastSubs = new Set<(t: Toast) => void>();
const clearToastSubs = new Set<() => void>();
export function toast(msg: string, kind: Toast["kind"] = "info") {
  const t = { id: toastId++, kind, msg };
  toastSubs.forEach((f) => f(t));
}
export function clearToasts() {
  clearToastSubs.forEach((f) => f());
}
export function onToast(fn: (t: Toast) => void): () => void {
  toastSubs.add(fn);
  return () => {
    toastSubs.delete(fn);
  };
}
export function onClearToasts(fn: () => void): () => void {
  clearToastSubs.add(fn);
  return () => {
    clearToastSubs.delete(fn);
  };
}

http.interceptors.request.use((cfg) => {
  const token = useUi.getState().token;
  if (token) cfg.headers.Authorization = `Bearer ${token}`;
  return cfg;
});

// A 401 means the gateway has ATLAS_AUTH_REQUIRED=1 and our stored token is missing, expired, or
// revoked. Bounce back to the login gate silently — a toast on the sign-in page feels like a bug
// (especially after deploy / first visit with a stale remembered session). Login shows a soft hint.
let unauthorizedHandled = false;
http.interceptors.response.use(
  (res) => res,
  (err) => {
    if (err?.response?.status === 401 && !unauthorizedHandled) {
      unauthorizedHandled = true;
      const { entered, token } = useUi.getState();
      // Only bounce when we were already inside the shell — login probes use raw fetch and stay put.
      if (entered) {
        useUi.getState().signOut(
          token
            ? "Your previous session ended — paste a token to continue."
            : "This gateway requires a bearer token.",
        );
        clearToasts();
      }
      setTimeout(() => {
        unauthorizedHandled = false;
      }, 3000);
    }
    return Promise.reject(err);
  },
);

// Normalize the gateway's `{error:{message}}` envelope into a readable Error.
export function apiError(e: unknown): string {
  const anyE = e as { response?: { data?: { error?: { message?: string } } }; message?: string };
  return anyE?.response?.data?.error?.message || anyE?.message || "request failed";
}

export function isUnauthorized(e: unknown): boolean {
  return (e as { response?: { status?: number } })?.response?.status === 401;
}

/**
 * Track a job to completion via the SSE endpoint, updating the global job store
 * and toasting on terminal state. Falls back to polling if EventSource fails.
 */
export function watchJob(jobId: string, label: string, onDone?: () => void) {
  const ui = useUi.getState();
  ui.trackJob(jobId, label);
  toast(`${label} queued…`, "info");

  let settled = false;
  const finish = (state: string, error?: string | null) => {
    if (settled) return;
    settled = true;
    useUi.getState().updateJob(jobId, { state, progress: state === "succeeded" ? 100 : undefined, error });
    toast(state === "succeeded" ? `${label} ✓` : `${label} ✗ ${error || "failed"}`, state === "succeeded" ? "ok" : "err");
    onDone?.();
    setTimeout(() => useUi.getState().clearDoneJobs(), 4000);
  };

  try {
    const es = new EventSource(`${API_BASE}/jobs/${jobId}/watch`);
    es.addEventListener("job", (ev) => {
      try {
        const j = JSON.parse((ev as MessageEvent).data) as JobRecord;
        useUi.getState().updateJob(jobId, { state: j.state, progress: j.progress_percent, error: j.error });
        if (j.state === "succeeded" || j.state === "failed") {
          es.close();
          finish(j.state, j.error);
        }
      } catch {
        /* ignore */
      }
    });
    es.addEventListener("error", () => {
      es.close();
      // SSE dropped — poll a few times as a fallback.
      pollJob(jobId, label, finish);
    });
  } catch {
    pollJob(jobId, label, finish);
  }
}

async function pollJob(jobId: string, label: string, finish: (s: string, e?: string | null) => void) {
  // Poll for as long as the job's own reported state is still non-terminal — some jobs (e.g.
  // claiming a raw disk as a Ceph OSD via Rook) legitimately run for many minutes. A fixed
  // iteration cap here would fabricate a "failed" state for a job that's still genuinely in
  // progress server-side, which is actively misleading (an operator could see "failed" and retry
  // while the first attempt is still claiming the disk). Only give up on repeated *request*
  // failures (the status endpoint itself unreachable), never on elapsed time.
  let consecutiveFailures = 0;
  const MAX_CONSECUTIVE_FAILURES = 20; // ~30s of the status request itself failing outright
  for (;;) {
    await new Promise((r) => setTimeout(r, 1500));
    try {
      const { data } = await http.get<JobRecord>(`/jobs/${jobId}`);
      consecutiveFailures = 0;
      useUi.getState().updateJob(jobId, { state: data.state, progress: data.progress_percent, error: data.error });
      if (data.state === "succeeded" || data.state === "failed") return finish(data.state, data.error);
    } catch {
      consecutiveFailures++;
      if (consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
        // Lost the ability to even ask about this job — a different situation from the job
        // itself failing, so this must never read like a job failure: no "✗", and the tracked
        // job's state is left alone rather than forced to "failed" (which would misleadingly
        // badge it as a terminal failure in the header's Running-jobs flyout).
        toast(`lost connection tracking "${label}" — check Jobs for its real status`, "info");
        return;
      }
    }
  }
}

// Extract the job id from a 202 response body (varies by endpoint).
export function jobIdOf(resp: unknown): string | undefined {
  const r = resp as { job_id?: string; resource?: { job_id?: string } };
  return r?.job_id || r?.resource?.job_id;
}

/** Fire a write request; if it returns a job id, watch it to completion. Toasts + invalidates. */
export async function submitJob(
  method: "post" | "put" | "delete",
  path: string,
  body: unknown,
  label: string,
  after?: () => void,
) {
  try {
    const { data } = await http.request({ method, url: path, data: body });
    const jid = jobIdOf(data);
    if (jid) watchJob(jid, label, after);
    else {
      toast(`${label} ✓`, "ok");
      after?.();
    }
    return data;
  } catch (e) {
    // A 401 already gets its own explanation + login redirect from the response interceptor above;
    // piling on a second, request-specific toast ("create: invalid token: InvalidToken") just as the
    // screen is about to change is confusing rather than helpful.
    if (!isUnauthorized(e)) toast(`${label}: ${apiError(e)}`, "err");
    throw e;
  }
}

/** A plain write (no job), with toast + invalidate. */
export async function submit(method: "post" | "put" | "delete", path: string, body: unknown, label: string, after?: () => void) {
  try {
    const { data } = await http.request({ method, url: path, data: body });
    toast(`${label} ✓`, "ok");
    after?.();
    return data;
  } catch (e) {
    if (!isUnauthorized(e)) toast(`${label}: ${apiError(e)}`, "err");
    throw e;
  }
}
