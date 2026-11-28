// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Who may call which `/v1/*` route (`docs/NATIVE_NODE.md`, "API tokens").
//!
//! Two kinds of bearer token exist. The single `api_token_file` token is the administrator: it
//! may call everything, as before. Scoped tokens (`api_tokens_file`) are named, carry roles and,
//! for data and read-only roles, the filesystems and volumes they may touch. A scoped token is
//! refused unless [`ROUTES`] names the exact method and path and one of its roles is listed
//! there, so a route nobody classified is closed to every scoped token.
//!
//! Authorization reads the same path segments the router matches on (no decoding, no
//! normalisation), so a path means the same thing to both.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::json;

use crate::{
    checksum::sha256,
    engine::NativeError,
    http::{Request, Response},
    metrics::PromText,
};

type Roles = u8;
const NONE: Roles = 0;
const READ: Roles = 1 << 0;
const DATA: Roles = 1 << 1;
const LIFE: Roles = 1 << 2;
const REPL: Roles = 1 << 3;
const ADMIN: Roles = 1 << 4;
/// Clearing a retention lock. No other role implies it, [`ADMIN`] included.
const UNLOCK: Roles = 1 << 5;
/// The roles a token limited to named filesystems or volumes may have.
const SCOPABLE: Roles = READ | DATA;

/// Shortest scoped token accepted. The single `api_token_file` token has no minimum.
const MIN_TOKEN_BYTES: usize = 16;
const MAX_NAME_BYTES: usize = 64;
/// Audit lines written per second for each of "unauthenticated" and "forbidden"; the rest are
/// counted and reported with the next line written.
const AUDIT_PER_SEC: u32 = 20;

/// What a route touches, which decides how a token limited to some objects may use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    /// The filesystem or volume the pattern names as `{fs}` or `{vol}`.
    Object,
    /// Node status: names no object.
    Status,
    /// A listing, cut down to the objects the token may see.
    Filesystems,
    FsSnapshots,
    Volumes,
    /// The cluster, or an object whose filesystem or volume the path does not name: closed to
    /// limited tokens.
    Cluster,
}

struct Route {
    method: &'static str,
    /// Path segments; `{...}` matches any one segment.
    pattern: &'static str,
    /// Scoped roles allowed. [`ADMIN`] is allowed everywhere except on [`UNLOCK`] routes.
    roles: Roles,
    on: On,
}

const fn r(method: &'static str, pattern: &'static str, roles: Roles, on: On) -> Route {
    Route {
        method,
        pattern,
        roles,
        on,
    }
}

/// Every `/v1/*` route of the node. A test compares this table with the router's source.
#[rustfmt::skip]
const ROUTES: &[Route] = &[
    r("GET",    "v1/status",                          READ | DATA | LIFE | REPL, On::Status),
    r("GET",    "v1/members",                         READ,        On::Cluster),
    r("POST",   "v1/members",                         NONE,        On::Cluster),
    r("GET",    "v1/reduction",                       READ | LIFE, On::Cluster),
    r("POST",   "v1/repair",                          NONE,        On::Cluster),
    r("POST",   "v1/gc",                              NONE,        On::Cluster),
    r("POST",   "v1/tier",                            NONE,        On::Cluster),
    r("GET",    "v1/exports",                         READ,        On::Cluster),
    r("GET",    "v1/exports/{name}",                  READ,        On::Cluster),
    r("DELETE", "v1/exports/{name}",                  NONE,        On::Cluster),
    r("GET",    "v1/transfers",                       READ,        On::Cluster),
    r("GET",    "v1/transfers/{id}",                  READ,        On::Cluster),

    r("GET",    "v1/volumes",                         READ | DATA | LIFE, On::Volumes),
    r("POST",   "v1/volumes",                         LIFE,        On::Cluster),
    r("POST",   "v1/volumes/import",                  NONE,        On::Cluster),
    r("DELETE", "v1/volumes/{vol}",                   LIFE,        On::Object),
    r("POST",   "v1/volumes/{vol}/resize",            LIFE,        On::Object),
    r("PUT",    "v1/volumes/{vol}/data",              DATA,        On::Object),
    r("GET",    "v1/volumes/{vol}/data",              READ | DATA, On::Object),
    r("POST",   "v1/volumes/{vol}/snapshots",         LIFE,        On::Object),

    r("DELETE", "v1/snapshots/{id}",                  LIFE,        On::Cluster),
    r("POST",   "v1/snapshots/{id}/lock",             LIFE,        On::Cluster),
    r("POST",   "v1/snapshots/{id}/unlock",           UNLOCK,      On::Cluster),
    r("POST",   "v1/snapshots/{id}/clone",            LIFE,        On::Cluster),
    r("GET",    "v1/snapshots/{id}/data",             READ | DATA, On::Cluster),
    r("POST",   "v1/snapshots/{id}/export",           NONE,        On::Cluster),

    r("GET",    "v1/fs",                              READ | DATA | LIFE | REPL, On::Filesystems),
    r("POST",   "v1/fs",                              LIFE | REPL, On::Cluster),
    r("GET",    "v1/fs-snapshots",                    READ | LIFE | REPL, On::FsSnapshots),
    r("DELETE", "v1/fs-snapshots/{id}",               LIFE | REPL, On::Cluster),
    r("POST",   "v1/fs-snapshots/{id}/lock",          LIFE,        On::Cluster),
    r("POST",   "v1/fs-snapshots/{id}/unlock",        UNLOCK,      On::Cluster),
    r("GET",    "v1/fs-snapshots/{id}/diff",          READ | REPL, On::Cluster),
    r("POST",   "v1/fs-snapshots/{id}/clone",         LIFE,        On::Cluster),

    r("DELETE", "v1/fs/{fs}",                         LIFE,        On::Object),
    r("GET",    "v1/fs/{fs}/activity",                READ | LIFE, On::Object),
    r("GET",    "v1/fs/{fs}/statfs",                  READ | DATA | LIFE, On::Object),
    r("GET",    "v1/fs/{fs}/quota",                   READ | LIFE, On::Object),
    r("PUT",    "v1/fs/{fs}/quota",                   LIFE,        On::Object),
    r("GET",    "v1/fs/{fs}/locality",                READ,        On::Object),
    r("POST",   "v1/fs/{fs}/locality/pin",            NONE,        On::Object),
    r("POST",   "v1/fs/{fs}/sessions",                DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/sessions/{id}/recalls",   DATA,        On::Object),
    r("POST",   "v1/fs/{fs}/sessions/{id}/recalls/done", DATA,     On::Object),
    r("POST",   "v1/fs/{fs}/sessions/{id}/renew",     DATA,        On::Object),
    r("DELETE", "v1/fs/{fs}/sessions/{id}",           DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/locks",                   READ | DATA, On::Object),
    r("POST",   "v1/fs/{fs}/rename",                  DATA,        On::Object),
    r("POST",   "v1/fs/{fs}/snapshots",               LIFE | REPL, On::Object),
    r("POST",   "v1/fs/{fs}/replica/apply",           REPL,        On::Object),
    r("PUT",    "v1/fs/{fs}/replica/inodes/{ino}/data", REPL,      On::Object),
    r("POST",   "v1/fs/{fs}/replica/commit",          REPL,        On::Object),
    r("POST",   "v1/fs/{fs}/promote",                 NONE,        On::Object),
    r("POST",   "v1/fs/{fs}/demote",                  NONE,        On::Object),

    r("GET",    "v1/fs/{fs}/inodes/{ino}",            READ | DATA, On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/attr",       DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/lookup",     READ | DATA, On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/entries",    READ | DATA, On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/entries",    DATA,        On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/unlink",     DATA,        On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/rmdir",      DATA,        On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/links",      DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/xattrs",     READ | DATA, On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/xattrs/{name}", READ | DATA, On::Object),
    r("PUT",    "v1/fs/{fs}/inodes/{ino}/xattrs/{name}", DATA,     On::Object),
    r("DELETE", "v1/fs/{fs}/inodes/{ino}/xattrs/{name}", DATA,     On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/locks",      DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/locks",      READ | DATA, On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/locks/release", DATA,     On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/target",     READ | DATA, On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/data",       READ | DATA | REPL, On::Object),
    r("PUT",    "v1/fs/{fs}/inodes/{ino}/data",       DATA,        On::Object),
    r("GET",    "v1/fs/{fs}/inodes/{ino}/layout",     READ | DATA, On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/direct-write",  DATA,     On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/direct-commit", DATA,     On::Object),
    r("POST",   "v1/fs/{fs}/inodes/{ino}/direct-abort",  DATA,     On::Object),
];

/// The path segments the router and the authorization both match on.
pub(super) fn segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

impl Route {
    fn matches(&self, method: &str, segs: &[&str]) -> bool {
        self.method == method
            && self.pattern.split('/').count() == segs.len()
            && self
                .pattern
                .split('/')
                .zip(segs)
                .all(|(p, s)| p.starts_with('{') || p == *s)
    }

    fn permits(&self, roles: Roles) -> bool {
        let wanted = if self.roles == UNLOCK {
            UNLOCK
        } else {
            self.roles | ADMIN
        };
        roles & wanted != 0
    }

    /// The segment naming the object the route is about (the pattern's first `{...}`).
    fn object<'a>(&self, segs: &[&'a str]) -> Option<(&'static str, &'a str)> {
        self.pattern
            .split('/')
            .zip(segs)
            .find(|(p, _)| p.starts_with('{'))
            .map(|(p, s)| (p, *s))
    }
}

fn classify(method: &str, segs: &[&str]) -> Option<&'static Route> {
    ROUTES.iter().find(|r| r.matches(method, segs))
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RoleName {
    ReadOnly,
    Data,
    Lifecycle,
    Replication,
    Admin,
    RetentionUnlock,
}

impl RoleName {
    fn bit(self) -> Roles {
        match self {
            Self::ReadOnly => READ,
            Self::Data => DATA,
            Self::Lifecycle => LIFE,
            Self::Replication => REPL,
            Self::Admin => ADMIN,
            Self::RetentionUnlock => UNLOCK,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenFile {
    tokens: Vec<TokenSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenSpec {
    name: String,
    /// File holding the token; a relative path starts at the token list's directory.
    token_file: std::path::PathBuf,
    roles: Vec<RoleName>,
    #[serde(default)]
    filesystems: Option<Vec<String>>,
    #[serde(default)]
    volumes: Option<Vec<String>>,
}

/// The objects of one kind a limited token may touch.
#[derive(Debug, Clone)]
enum Ids {
    All,
    Only(BTreeSet<String>),
}

impl Ids {
    fn contains(&self, id: &str) -> bool {
        match self {
            Ids::All => true,
            Ids::Only(set) => set.contains(id),
        }
    }
}

struct Limit {
    filesystems: Ids,
    volumes: Ids,
}

struct Scoped {
    name: String,
    digest: [u8; 32],
    roles: Roles,
    limit: Option<Limit>,
}

/// Cuts a listing down to what a limited token may see.
pub(super) struct Filter {
    list: &'static str,
    field: &'static str,
    allowed: BTreeSet<String>,
}

/// An admitted request: what is left to do to its response.
#[derive(Default)]
pub(super) struct Permit(Option<Filter>);

impl Permit {
    pub(super) fn apply(self, resp: Response) -> Response {
        let Some(f) = self.0 else { return resp };
        if resp.status != 200 {
            return resp;
        }
        let filtered = serde_json::from_slice::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|mut v| {
                v.get_mut(f.list)?.as_array_mut()?.retain(|item| {
                    item[f.field]
                        .as_str()
                        .is_some_and(|id| f.allowed.contains(id))
                });
                Some(v)
            });
        // Never pass an unfiltered listing on.
        match filtered {
            Some(v) => Response::json(200, &v),
            None => Response::text(500, "cannot filter the listing for this token"),
        }
    }
}

#[derive(Default)]
struct Throttle {
    /// Start of the current second, lines written in it, lines dropped since the last one.
    window: Mutex<Option<(Instant, u32, u64)>>,
    total: AtomicU64,
}

impl Throttle {
    /// Counts a denial; `Some(dropped)` if it should be written.
    fn admit(&self, now: Instant) -> Option<u64> {
        self.total.fetch_add(1, Ordering::Relaxed);
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let (start, written, dropped) = w.get_or_insert((now, 0, 0));
        if now.duration_since(*start) >= Duration::from_secs(1) {
            (*start, *written) = (now, 0);
        }
        if *written >= AUDIT_PER_SEC {
            *dropped += 1;
            return None;
        }
        *written += 1;
        Some(std::mem::take(dropped))
    }
}

/// The node's API credentials.
#[derive(Default)]
pub(super) struct ApiAuth {
    /// Digest of the `api_token_file` token.
    admin: Option<[u8; 32]>,
    scoped: Vec<Scoped>,
    unauthenticated: Throttle,
    forbidden: Throttle,
}

enum Who<'a> {
    Admin,
    Scoped(&'a Scoped),
}

fn invalid<T>(what: impl Into<String>) -> Result<T, NativeError> {
    Err(NativeError::Invalid(what.into()))
}

fn read_token(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(|t| t.trim().to_string())
}

/// Compares two digests in time that does not depend on where they differ.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn limit(spec: &TokenSpec, roles: Roles) -> Result<Option<Limit>, NativeError> {
    let name = &spec.name;
    let given = spec.filesystems.is_some() || spec.volumes.is_some();
    if roles & !SCOPABLE != 0 {
        if given {
            return invalid(format!(
                "api_tokens_file: token {name}: filesystems and volumes only limit tokens whose \
                 roles are read-only and data"
            ));
        }
        return Ok(None);
    }
    if !given {
        if roles & DATA != 0 {
            return invalid(format!(
                "api_tokens_file: token {name}: the data role needs filesystems or volumes \
                 ([\"*\"] for all)"
            ));
        }
        return Ok(None);
    }
    let ids = |list: &Option<Vec<String>>, what: &str| -> Result<Ids, NativeError> {
        let list = list.as_deref().unwrap_or_default();
        if list == ["*"] {
            return Ok(Ids::All);
        }
        if list
            .iter()
            .any(|id| id.is_empty() || id.contains(['*', '/', '@']))
        {
            return invalid(format!(
                "api_tokens_file: token {name}: {what} must be ids (no '/', '@' or '*') or \
                 exactly [\"*\"]"
            ));
        }
        Ok(Ids::Only(list.iter().cloned().collect()))
    };
    let l = Limit {
        filesystems: ids(&spec.filesystems, "filesystems")?,
        volumes: ids(&spec.volumes, "volumes")?,
    };
    let empty = |i: &Ids| matches!(i, Ids::Only(s) if s.is_empty());
    if empty(&l.filesystems) && empty(&l.volumes) {
        return invalid(format!(
            "api_tokens_file: token {name}: filesystems and volumes are both empty"
        ));
    }
    Ok(match (&l.filesystems, &l.volumes) {
        (Ids::All, Ids::All) => None,
        _ => Some(l),
    })
}

impl ApiAuth {
    /// Reads the credentials. Errors name tokens and files, never token values.
    pub(super) fn load(
        admin_file: Option<&Path>,
        tokens_file: Option<&Path>,
    ) -> Result<Self, NativeError> {
        let admin = admin_file.map(read_token).transpose()?;
        if admin.as_deref() == Some("") {
            return invalid("api_token_file is empty");
        }
        let mut auth = Self {
            admin: admin.map(|t| sha256(t.as_bytes())),
            ..Self::default()
        };
        let Some(path) = tokens_file else {
            return Ok(auth);
        };
        let raw = std::fs::read_to_string(path).map_err(|e| {
            NativeError::Invalid(format!("api_tokens_file {}: {e}", path.display()))
        })?;
        // The parse error itself is not quoted: it can repeat the file's text.
        let file: TokenFile = serde_json::from_str(&raw).map_err(|e| {
            NativeError::Invalid(format!(
                "api_tokens_file {} is not a valid token list (line {}, column {})",
                path.display(),
                e.line(),
                e.column()
            ))
        })?;
        if file.tokens.is_empty() {
            return invalid("api_tokens_file lists no tokens");
        }
        let dir = path.parent().unwrap_or(Path::new(""));
        for (i, spec) in file.tokens.iter().enumerate() {
            let name = &spec.name;
            if name.is_empty()
                || name.len() > MAX_NAME_BYTES
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return invalid(format!(
                    "api_tokens_file: entry {i}: name must be 1-{MAX_NAME_BYTES} characters of \
                     [A-Za-z0-9._-]"
                ));
            }
            if auth.scoped.iter().any(|t| &t.name == name) {
                return invalid(format!("api_tokens_file: token {name} is listed twice"));
            }
            let roles = spec.roles.iter().fold(NONE, |acc, r| acc | r.bit());
            if roles == NONE {
                return invalid(format!("api_tokens_file: token {name} has no roles"));
            }
            let file = dir.join(&spec.token_file);
            let token = read_token(&file).map_err(|e| {
                NativeError::Invalid(format!(
                    "api_tokens_file: token {name}: {}: {e}",
                    file.display()
                ))
            })?;
            if token.len() < MIN_TOKEN_BYTES || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return invalid(format!(
                    "api_tokens_file: token {name}: {} must hold at least {MIN_TOKEN_BYTES} \
                     printable ASCII characters without spaces",
                    file.display()
                ));
            }
            let digest = sha256(token.as_bytes());
            // A shared value would make one token silently act as the other.
            if auth.admin.is_some_and(|a| a == digest) {
                return invalid(format!(
                    "api_tokens_file: token {name} has the same value as api_token_file"
                ));
            }
            if let Some(other) = auth.scoped.iter().find(|t| t.digest == digest) {
                return invalid(format!(
                    "api_tokens_file: tokens {} and {name} have the same value",
                    other.name
                ));
            }
            let limit = limit(spec, roles)?;
            auth.scoped.push(Scoped {
                name: name.clone(),
                digest,
                roles,
                limit,
            });
        }
        Ok(auth)
    }

    /// With no credentials configured the API is open, as before.
    fn required(&self) -> bool {
        self.admin.is_some() || !self.scoped.is_empty()
    }

    fn authenticate(&self, header: Option<&String>) -> Option<Who<'_>> {
        let got = sha256(header?.strip_prefix("Bearer ")?.as_bytes());
        // Every credential is compared, whichever matches.
        let admin = self.admin.as_ref().is_some_and(|a| same(a, &got));
        let scoped = self
            .scoped
            .iter()
            .fold(None, |found, t| if same(&t.digest, &got) { Some(t) } else { found });
        if admin {
            Some(Who::Admin)
        } else {
            scoped.map(Who::Scoped)
        }
    }

    /// Whether `req` may go on to the router; the refusal to send if not.
    pub(super) fn admit(&self, req: &Request) -> Result<Permit, Response> {
        if !self.required() {
            return Ok(Permit::default());
        }
        let segs = segments(&req.path);
        let route = classify(&req.method, &segs);
        let Some(who) = self.authenticate(req.headers.get("authorization")) else {
            if let Some(dropped) = self.unauthenticated.admit(Instant::now()) {
                audit(&self.line("denied", 401, None, req, route, "token", dropped));
            }
            return Err(Response::text(401, "missing or invalid bearer token"));
        };
        let (name, verdict) = match who {
            Who::Admin => (None, Ok(Permit::default())),
            Who::Scoped(t) => (Some(t.name.as_str()), judge(t, route, &segs)),
        };
        match verdict {
            Ok(permit) => {
                // Clearing a retention lock is recorded whoever does it.
                if route.is_some_and(|r| r.roles == UNLOCK) {
                    let name = name.or(Some("api_token_file"));
                    audit(&self.line("retention_unlock", 0, name, req, route, "allowed", 0));
                }
                Ok(permit)
            }
            Err(why) => {
                if let Some(dropped) = self.forbidden.admit(Instant::now()) {
                    audit(&self.line("denied", 403, name, req, route, why, dropped));
                }
                Err(Response::json(
                    403,
                    &json!({
                        "error": format!(
                            "token {} may not make this request ({why})",
                            name.unwrap_or_default()
                        ),
                        "code": "forbidden",
                        "leader": null,
                    }),
                ))
            }
        }
    }

    /// One audit record. Nothing in it is free text from the client: the method is one of a
    /// fixed set, the route is the table's pattern, and the object id is reduced to id
    /// characters and withheld if it is one of the tokens.
    #[allow(clippy::too_many_arguments)]
    fn line(
        &self,
        event: &str,
        status: u16,
        token: Option<&str>,
        req: &Request,
        route: Option<&Route>,
        reason: &str,
        dropped: u64,
    ) -> String {
        let method = ["GET", "PUT", "POST", "DELETE", "HEAD", "PATCH", "OPTIONS"]
            .into_iter()
            .find(|m| *m == req.method)
            .unwrap_or("other");
        let segs = segments(&req.path);
        let object = route
            .and_then(|r| r.object(&segs))
            .map(|(_, id)| self.printable(id));
        let mut v = json!({
            "event": event,
            "token": token,
            "method": method,
            "route": route.map_or("unclassified", |r| r.pattern),
            "object": object,
            "reason": reason,
        });
        if status != 0 {
            v["status"] = json!(status);
        }
        if dropped > 0 {
            v["dropped_before"] = json!(dropped);
        }
        v.to_string()
    }

    fn printable(&self, id: &str) -> String {
        let digest = sha256(id.as_bytes());
        let secret = self
            .admin
            .iter()
            .chain(self.scoped.iter().map(|t| &t.digest))
            .any(|d| same(d, &digest));
        let plain = id.len() <= MAX_NAME_BYTES
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b));
        if secret || !plain {
            "(withheld)".into()
        } else {
            id.into()
        }
    }

    /// Denial counters, when the API requires a token.
    pub(super) fn render_metrics(&self, node: &str) -> Option<String> {
        if !self.required() {
            return None;
        }
        let name = "atlas_native_api_denied_total";
        let mut p = PromText::new();
        let fam = p.family(name, "counter", "API requests refused for their credentials.");
        for (reason, t) in [
            ("unauthenticated", &self.unauthenticated),
            ("forbidden", &self.forbidden),
        ] {
            fam.sample(
                name,
                &[("node", node), ("reason", reason)],
                t.total.load(Ordering::Relaxed),
            );
        }
        Some(p.finish())
    }
}

fn audit(line: &str) {
    eprintln!("atlas-native-audit {line}");
}

/// Whether scoped token `t` may call `route`; why not, otherwise.
fn judge(t: &Scoped, route: Option<&Route>, segs: &[&str]) -> Result<Permit, &'static str> {
    let route = route.ok_or("unclassified route")?;
    if !route.permits(t.roles) {
        return Err("role");
    }
    let Some(limit) = &t.limit else {
        return Ok(Permit::default());
    };
    let listing = |list: &'static str, field: &'static str, ids: &Ids| {
        Permit(match ids {
            Ids::All => None,
            Ids::Only(allowed) => Some(Filter {
                list,
                field,
                allowed: allowed.clone(),
            }),
        })
    };
    match route.on {
        On::Status => Ok(Permit::default()),
        On::Filesystems => Ok(listing("filesystems", "id", &limit.filesystems)),
        On::FsSnapshots => Ok(listing("snapshots", "fs_id", &limit.filesystems)),
        On::Volumes => Ok(listing("volumes", "id", &limit.volumes)),
        On::Cluster => Err("limited token"),
        On::Object => match route.object(segs) {
            // `<fs>@<snapshot>` is a snapshot tree of `<fs>` (the engine checks it is one).
            Some(("{fs}", id))
                if limit
                    .filesystems
                    .contains(id.split_once('@').map_or(id, |(fs, _)| fs)) =>
            {
                Ok(Permit::default())
            }
            Some(("{vol}", id)) if limit.volumes.contains(id) => Ok(Permit::default()),
            _ => Err("object not in scope"),
        },
    }
}

#[cfg(test)]
mod tests;
