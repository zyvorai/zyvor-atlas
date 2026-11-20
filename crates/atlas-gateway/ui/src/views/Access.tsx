// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect, useState } from "react";
import { Copy, Trash2, UserPlus } from "lucide-react";
import { apiError, http, toast } from "../api/client";
import { Badge, Button, Field, Label, Select, TerminalPane } from "../ui/kit";
import { del } from "../ui/confirm";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";

type ConsoleUser = {
  username: string;
  role: string;
  level: number;
  disabled: boolean;
  created_by?: string | null;
  created_at?: string | null;
  kind: "bootstrap" | "user";
};

const ROLES = [
  { id: "viewer", label: "Viewer", level: 0, hint: "Read-only inventory & metrics" },
  { id: "operator", label: "Operator", level: 1, hint: "Volumes, snapshots, day-2 ops" },
  { id: "admin", label: "Admin", level: 2, hint: "Users, tokens, governance" },
] as const;

function roleBadge(role: string) {
  return role === "admin" ? "warning" : role === "operator" ? "info" : "neutral";
}

export default function Access() {
  const [users, setUsers] = useState<ConsoleUser[] | undefined>(undefined);
  const [usersErr, setUsersErr] = useState(false);
  const [usersBusy, setUsersBusy] = useState(false);
  const [newUser, setNewUser] = useState("");
  const [newPass, setNewPass] = useState("");
  const [newRole, setNewRole] = useState("operator");
  const [createBusy, setCreateBusy] = useState(false);

  const [subject, setSubject] = useState("veyron");
  const [role, setRole] = useState("operator");
  const [ttl, setTtl] = useState("3600");
  const [result, setResult] = useState<any>(null);
  const [busy, setBusy] = useState(false);
  const ttlNum = +ttl;
  const ttlInvalid = ttl.trim() === "" || Number.isNaN(ttlNum) || ttlNum < 60 || ttlNum > 7776000;
  const subjectInvalid = subject.trim() === "";
  const createInvalid =
    newUser.trim() === "" || newPass.length < 8 || !/^[A-Za-z0-9._-]+$/.test(newUser.trim());

  const loadUsers = async () => {
    setUsersBusy(true);
    setUsersErr(false);
    try {
      const r = await http.get("/auth/users");
      setUsers(Array.isArray(r.data) ? r.data : []);
    } catch (e) {
      setUsersErr(true);
      setUsers(undefined);
      toast(apiError(e), "err");
    } finally {
      setUsersBusy(false);
    }
  };

  useEffect(() => {
    // Kick off the initial load on mount; loadUsers sets its own busy/error flags synchronously
    // before the first await, which is what's flagged here — standard fetch-on-mount pattern.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void loadUsers();
  }, []);

  const n = users?.length || 0;
  return (
    <ListPage
      crumbs={navCrumbs("access")}
      eyebrow="GOVERNANCE · INDEX"
      title="Access"
      state={
        usersErr
          ? "Could not load console users — retry or check gateway auth."
          : usersBusy && users === undefined
            ? "Loading console users…"
            : `${n} console user${n === 1 ? "" : "s"} — privilege levels and scoped service-account JWTs.`
      }
    >
      <div className="at-stack">
      <div className="grid lg:grid-cols-2 gap-4">
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Create user</span>
          </div>
          <div className="at-form-grid">
            <div>
              <Label>Username</Label>
              <Field
                value={newUser}
                onChange={(e) => setNewUser(e.target.value)}
                placeholder="ops"
                autoComplete="off"
              />
            </div>
            <div>
              <Label>Password</Label>
              <Field
                type="password"
                value={newPass}
                onChange={(e) => setNewPass(e.target.value)}
                placeholder="at least 8 characters"
                autoComplete="new-password"
              />
            </div>
            <div>
              <Label>Privilege level</Label>
              <div className="at-select-grid" role="radiogroup" aria-label="Privilege level">
                {ROLES.map((r) => (
                  <button
                    key={r.id}
                    type="button"
                    role="radio"
                    aria-checked={newRole === r.id}
                    className={`at-select-tile${newRole === r.id ? " on" : ""}`}
                    onClick={() => setNewRole(r.id)}
                  >
                    <span className="at-select-title">{r.label}</span>
                    <span className="at-select-hint">Level {r.level} — {r.hint}</span>
                  </button>
                ))}
              </div>
            </div>
            {createInvalid && newUser.trim() !== "" && (
              <div className="text-xs text-danger">
                Username: [A-Za-z0-9._-]. Password: 8+ characters.
              </div>
            )}
            <div>
              <Button
                variant="primary"
                loading={createBusy}
                disabled={createInvalid}
                onClick={async () => {
                  setCreateBusy(true);
                  try {
                    await http.post("/auth/users", {
                      username: newUser.trim(),
                      password: newPass,
                      role: newRole,
                    });
                    toast(`user ${newUser.trim()} created`, "ok");
                    setNewUser("");
                    setNewPass("");
                    setNewRole("operator");
                    await loadUsers();
                  } catch (e) {
                    toast(apiError(e), "err");
                  } finally {
                    setCreateBusy(false);
                  }
                }}
              >
                <UserPlus size={14} /> Create user
              </Button>
            </div>
          </div>
        </div>

        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Privilege levels</span>
          </div>
          {ROLES.map((r) => (
            <div key={r.id} className="at-list-row">
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontWeight: 500, color: "var(--at-ink)" }}>{r.label}</div>
                <div className="at-sub" style={{ margin: 0 }}>{r.hint}</div>
              </div>
              <Badge kind={roleBadge(r.id)}>{`level ${r.level}`}</Badge>
            </div>
          ))}
          <div className="at-list-row" style={{ alignItems: "flex-start" }}>
            <p className="at-sub" style={{ margin: 0 }}>
              Bootstrap admin (<code className="mono">admin</code> / env password) is always level 2.
              Created users sign in on the login page with their own password.
            </p>
          </div>
        </div>
      </div>

      <Table
        soundings
        panelTitle="Console users"
        panelExtra={
          <button
            type="button"
            className="at-btn compact"
            disabled={usersBusy}
            onClick={() => void loadUsers()}
          >
            Refresh
          </button>
        }
        rows={users}
        error={usersErr}
        onRetry={() => void loadUsers()}
        empty="No users yet — create one above."
        rowKey={(u) => `${u.kind}:${u.username}`}
        cols={[
          {
            h: "User",
            f: (u) => (
              <span className="mono">
                {u.username}
                {u.kind === "bootstrap" ? " · bootstrap" : ""}
              </span>
            ),
            mono: true,
          },
          {
            h: "Role",
            f: (u) => (
              <Badge kind={roleBadge(u.role)} className="normal-case">
                {u.role}
              </Badge>
            ),
          },
          { h: "Level", f: (u) => u.level },
          {
            h: "Status",
            f: (u) =>
              u.disabled ? (
                <Badge kind="danger">disabled</Badge>
              ) : (
                <Badge kind="success">active</Badge>
              ),
          },
        ]}
        actions={(u) =>
          u.kind === "bootstrap" ? (
            <span className="text-xs text-muted-foreground">env</span>
          ) : (
            <div className="flex gap-1 items-center justify-end">
              <Select
                className="w-auto! text-xs py-1"
                value={u.role}
                onChange={async (e) => {
                  try {
                    await http.put(`/auth/users/${encodeURIComponent(u.username)}`, {
                      role: e.target.value,
                    });
                    toast("role updated", "ok");
                    await loadUsers();
                  } catch (err) {
                    toast(apiError(err), "err");
                  }
                }}
              >
                {ROLES.map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.label}
                  </option>
                ))}
              </Select>
              <button
                type="button"
                className="at-btn compact"
                title="Delete user"
                onClick={() =>
                  del(`user ${u.username}`, async () => {
                    try {
                      await http.delete(`/auth/users/${encodeURIComponent(u.username)}`);
                      toast("user deleted", "ok");
                      await loadUsers();
                    } catch (err) {
                      toast(apiError(err), "err");
                    }
                  })
                }
              >
                <Trash2 size={13} />
              </button>
            </div>
          )
        }
      />

      <div className="grid lg:grid-cols-2 gap-4">
        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Issue token</span>
          </div>
          <div className="at-form-grid">
            <div>
              <Label>Subject (service account)</Label>
              <Field value={subject} onChange={(e) => setSubject(e.target.value)} placeholder="veyron" />
              {subjectInvalid && <div className="text-xs text-danger mt-1">Subject is required.</div>}
            </div>
            <div>
              <Label>Role</Label>
              <div className="at-select-grid" role="radiogroup" aria-label="Token role">
                {ROLES.map((r) => (
                  <button
                    key={r.id}
                    type="button"
                    role="radio"
                    aria-checked={role === r.id}
                    className={`at-select-tile${role === r.id ? " on" : ""}`}
                    onClick={() => setRole(r.id)}
                  >
                    <span className="at-select-title">{r.label}</span>
                    <span className="at-select-hint">{r.hint}</span>
                  </button>
                ))}
              </div>
            </div>
            <div>
              <Label>TTL seconds (clamped 60…7776000)</Label>
              <Field type="number" min={60} value={ttl} onChange={(e) => setTtl(e.target.value)} />
              {ttlInvalid && (
                <div className="text-xs text-danger mt-1">Must be between 60 and 7,776,000 seconds.</div>
              )}
            </div>
            <div>
              <Button
                variant="primary"
                loading={busy}
                disabled={subjectInvalid || ttlInvalid}
                onClick={async () => {
                  setBusy(true);
                  try {
                    const r = await http.post("/auth/tokens", {
                      subject,
                      role,
                      ttl_secs: ttlNum,
                    });
                    setResult(r.data);
                    toast("token minted", "ok");
                  } catch (e) {
                    toast(apiError(e), "err");
                  } finally {
                    setBusy(false);
                  }
                }}
              >
                Mint token
              </Button>
            </div>
          </div>
        </div>

        <div className="at-panel">
          <div className="at-panel-bar">
            <span className="at-caption">Result</span>
          </div>
          <div style={{ padding: "var(--s5)" }}>
            {result ? (
              <div className="at-stack" style={{ gap: "var(--s3)" }}>
                <div className="flex gap-2 flex-wrap">
                  <Badge kind="info" className="normal-case">
                    subject: {result.subject}
                  </Badge>
                  <Badge kind="info" className="normal-case">
                    role: {result.role}
                  </Badge>
                  <Badge kind="neutral">level: {result.level}</Badge>
                  <Badge kind="neutral">exp: {result.expires_at}</Badge>
                </div>
                <div className="at-caption">Token</div>
                <TerminalPane title="jwt · bearer">
                  <span className="term-str">{result.token}</span>
                </TerminalPane>
                <div style={{ display: "flex", justifyContent: "flex-end", marginTop: -4 }}>
                  <button
                    type="button"
                    className="at-btn compact"
                    onClick={() => {
                      navigator.clipboard.writeText(result.token);
                      toast("copied", "ok");
                    }}
                  >
                    <Copy size={13} /> Copy
                  </button>
                </div>
                <div className="at-sub" style={{ margin: 0 }}>
                  Send as <code className="at-terminal-inline">Authorization: Bearer &lt;token&gt;</code>, or paste
                  it into the menu-bar key icon.
                </div>
              </div>
            ) : (
              <div className="at-sub" style={{ margin: 0 }}>Mint a token to see it here.</div>
            )}
          </div>
        </div>
      </div>
      </div>
    </ListPage>
  );
}
