// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! `/v1/fs/...`: the inode-based file API. Paths address inodes by number; names travel in JSON
//! bodies (or a percent-encoded `name` query on lookup). A snapshot tree is read as `<fs>@<id>`.
//! Only the leader of the metadata group holding the filesystem answers, unless a read passes
//! `?barrier=1` (any replica, after a read barrier: still linearizable) or `?stale=1` (any
//! replica, as far as it has applied). Listings span every group.

use std::time::Duration;

use serde_json::json;

use super::{
    body_json, cache_leases::Change, client_id, query_u64, read_range, MetaGroup, NodeShared,
};
use crate::{
    engine::{LockRequest, NativeEngine, NativeError, NewNode, ObjectKind, DEFAULT_DIFF_LIMIT},
    http::{Request, Response},
    leases::{LockKind, Session},
    namespace::{FsQuota, ReplicaInode, SetAttr, XattrMode},
    raft::Role,
};

type Routed = Result<Response, NativeError>;

/// Inodes a locality query or pin walks unless the request says otherwise.
const DEFAULT_MAX_INODES: usize = 100_000;
/// Extents one pin request examines unless it says otherwise.
const DEFAULT_PIN_EXTENTS: usize = 256;

/// How current a request needs this replica's catalog of one group to be.
fn gate(req: &Request, e: &NativeEngine) -> Result<(), NativeError> {
    // A follower's catalog can lag the leader, so a client could miss its own writes there.
    // `?barrier=1` first catches this replica up to the leader's commit index; `?stale=1` opts
    // into reading whatever it has applied.
    let get = req.method == "GET";
    if get && req.query.contains_key("barrier") {
        e.read_barrier()
    } else if !get || !req.query.contains_key("stale") {
        e.ensure_leader()
    } else {
        Ok(())
    }
}

/// A listing over every group: with one group the usual gate, with several a read barrier on
/// each (no node need lead them all) unless `?stale=1`.
fn list<T>(
    sh: &NodeShared,
    req: &Request,
    each: impl Fn(&NativeEngine) -> Result<Vec<T>, NativeError>,
) -> Result<Vec<T>, NativeError> {
    match sh.groups.as_slice() {
        [g] => gate(req, &g.engine)?,
        _ if req.query.contains_key("stale") => {}
        _ => sh.barrier_all()?,
    }
    let mut all = Vec::new();
    for g in &sh.groups {
        all.extend(each(&g.engine)?);
    }
    Ok(all)
}

/// The group holding filesystem `fs` (`<fs>@<snapshot>` names a snapshot tree of it).
fn fs_group<'a>(sh: &'a NodeShared, fs: &str) -> Result<(usize, &'a MetaGroup), NativeError> {
    let id = fs.split_once('@').map_or(fs, |(fs, _)| fs);
    sh.route(ObjectKind::Filesystem, id)
}

pub(super) fn route(sh: &NodeShared, req: &Request, segs: &[&str]) -> Routed {
    match (req.method.as_str(), segs) {
        ("GET", ["v1", "fs"]) => {
            let mut all = list(sh, req, |e| e.filesystems())?;
            all.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Response::json(200, &json!({ "filesystems": all })))
        }
        ("POST", ["v1", "fs"]) => {
            let body = parse(req)?;
            let name = str_field(&body, "name")?;
            let id = client_id(&body).map_err(bad)?;
            let extent_bytes = match body.get("extent_bytes") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or_else(|| {
                    NativeError::Invalid("extent_bytes must be an integer".into())
                })?),
            };
            let e = &sh.route(ObjectKind::Filesystem, &id)?.1.engine;
            gate(req, e)?;
            if body["replica"].as_bool().unwrap_or(false) {
                let grid = extent_bytes.ok_or_else(|| {
                    NativeError::Invalid("a replica needs the source's extent_bytes".into())
                })?;
                return Ok(Response::json(
                    201,
                    &json!({ "id": e.create_replica(id, name, grid)? }),
                ));
            }
            Ok(Response::json(
                201,
                &json!({ "id": e.create_fs_with(id, name, extent_bytes)? }),
            ))
        }
        ("GET", ["v1", "fs-snapshots"]) => {
            let mut all = list(sh, req, |e| e.fs_snapshots())?;
            all.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(Response::json(200, &json!({ "snapshots": all })))
        }
        (_, ["v1", "fs-snapshots", id, ..]) => {
            let (g, group) = sh.route(ObjectKind::FsSnapshot, id)?;
            gate(req, &group.engine)?;
            snapshot_route(sh, &group.engine, g, req, segs)
        }
        (_, ["v1", "fs", fs, ..]) => {
            let (g, group) = fs_group(sh, fs)?;
            gate(req, &group.engine)?;
            fs_route(sh, &group.engine, g, req, segs)
        }
        _ => Ok(Response::text(404, "no such route")),
    }
}

fn snapshot_route(
    sh: &NodeShared,
    e: &NativeEngine,
    group: usize,
    req: &Request,
    segs: &[&str],
) -> Routed {
    match (req.method.as_str(), segs) {
        ("DELETE", ["v1", "fs-snapshots", id]) => {
            e.delete_fs_snapshot(id)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs-snapshots", id, "lock"]) => {
            let secs = body_json(req)
                .ok()
                .and_then(|b| b["retain_secs"].as_u64())
                .filter(|s| *s > 0);
            let Some(secs) = secs else {
                return Ok(Response::text(
                    400,
                    "body must be {\"retain_secs\": positive integer}",
                ));
            };
            let until = e.lock_fs_snapshot(id, secs)?;
            Ok(Response::json(200, &json!({ "retain_until_unix": until })))
        }
        ("GET", ["v1", "fs-snapshots", id, "diff"]) => {
            let after = match req.query.get("after") {
                Some(a) => a.parse().map_err(|_| {
                    NativeError::Invalid("query parameter after must be an integer".into())
                })?,
                None => 0,
            };
            let limit = match req.query.get("limit") {
                Some(l) => l.parse().map_err(|_| {
                    NativeError::Invalid("query parameter limit must be an integer".into())
                })?,
                None => DEFAULT_DIFF_LIMIT,
            };
            let from = req.query.get("from").map(|f| pct_decode(f)).transpose()?;
            Ok(Response::json(
                200,
                &json!(e.fs_diff(id, from.as_deref(), after, limit)?),
            ))
        }
        ("POST", ["v1", "fs-snapshots", id, "clone"]) => {
            let body = parse(req)?;
            let name = str_field(&body, "name")?;
            let fid = client_id(&body).map_err(bad)?;
            // A clone shares the snapshot's extents, so it lives in the snapshot's group.
            sh.claim(ObjectKind::Filesystem, &fid, group)?;
            Ok(Response::json(
                201,
                &json!({ "id": e.clone_fs_as(fid, id, name)? }),
            ))
        }
        _ => Ok(Response::text(404, "no such route")),
    }
}

fn fs_route(
    sh: &NodeShared,
    e: &NativeEngine,
    group: usize,
    req: &Request,
    segs: &[&str],
) -> Routed {
    let g = &sh.groups[group];
    match (req.method.as_str(), segs) {
        ("DELETE", ["v1", "fs", fs]) => {
            e.delete_fs(fs)?;
            Ok(Response::text(204, ""))
        }
        ("GET", ["v1", "fs", fs, "statfs"]) => Ok(Response::json(200, &json!(e.fs_statfs(fs)?))),
        ("GET", ["v1", "fs", fs, "quota"]) => Ok(Response::json(
            200,
            &json!(e.fs_quota(fs)?.unwrap_or_default()),
        )),
        ("PUT", ["v1", "fs", fs, "quota"]) => {
            let body = parse(req)?;
            let obj = body
                .as_object()
                .ok_or_else(|| NativeError::Invalid("quota body must be a JSON object".into()))?;
            // A misspelt limit would otherwise silently clear it.
            if let Some(k) = obj
                .keys()
                .find(|k| !matches!(k.as_str(), "max_bytes" | "max_inodes"))
            {
                return Err(NativeError::Invalid(format!("unknown quota field {k:?}")));
            }
            let quota: FsQuota = serde_json::from_value(body)
                .map_err(|err| NativeError::Invalid(format!("quota body: {err}")))?;
            e.fs_set_quota(fs, quota)?;
            Ok(Response::json(200, &json!(quota)))
        }
        ("GET", ["v1", "fs", fs, "locality"]) => {
            let path = match req.query.get("path") {
                Some(p) => pct_decode(p)?,
                None => "/".into(),
            };
            let max_inodes = match req.query.get("max_inodes") {
                Some(m) => m.parse().map_err(|_| {
                    NativeError::Invalid("query parameter max_inodes must be an integer".into())
                })?,
                None => DEFAULT_MAX_INODES,
            };
            Ok(Response::json(
                200,
                &json!(e.fs_locality(fs, &path, max_inodes)?),
            ))
        }
        ("POST", ["v1", "fs", fs, "locality", "pin"]) => {
            let body = parse(req)?;
            let hosts: Vec<String> = serde_json::from_value(body["hosts"].clone())
                .map_err(|err| NativeError::Invalid(format!("body field \"hosts\": {err}")))?;
            let opt_u64 = |key: &str, default: usize| match &body[key] {
                serde_json::Value::Null => Ok(default),
                v => v.as_u64().map(|n| n as usize).ok_or_else(|| {
                    NativeError::Invalid(format!("body field {key:?} must be an integer"))
                }),
            };
            let report = e.fs_pin(
                fs,
                body["path"].as_str().unwrap_or("/"),
                &hosts,
                body["after"].as_str(),
                opt_u64("max_extents", DEFAULT_PIN_EXTENTS)?,
                opt_u64("max_inodes", DEFAULT_MAX_INODES)?,
            )?;
            Ok(Response::json(200, &json!(report)))
        }
        ("POST", ["v1", "fs", fs, "sessions"]) => {
            let body = parse(req)?;
            let id = str_field(&body, "session")?;
            let cache = body["cache"].as_bool().unwrap_or(false);
            let s = e.open_session(fs, id, u64_field(&body, "ttl_ms")?, cache)?;
            Ok(session_json(id, s))
        }
        ("GET", ["v1", "fs", fs, "sessions", id, "recalls"]) => {
            let wait = req
                .query
                .get("wait_ms")
                .and_then(|w| w.parse().ok())
                .map_or(Duration::ZERO, Duration::from_millis);
            Ok(Response::json(
                200,
                &json!({ "inos": g.leases.recalls(fs, id, wait, &sh.stop)? }),
            ))
        }
        ("POST", ["v1", "fs", fs, "sessions", id, "recalls", "done"]) => {
            let inos: Vec<u64> = serde_json::from_value(parse(req)?["inos"].clone())
                .map_err(|err| NativeError::Invalid(format!("body field \"inos\": {err}")))?;
            g.leases.give_back(fs, id, &inos)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs", _, "sessions", id, "renew"]) => {
            Ok(session_json(id, e.renew_session(id)?))
        }
        ("DELETE", ["v1", "fs", _, "sessions", id]) => {
            e.close_session(id)?;
            g.leases.forget_session(id)?;
            Ok(Response::text(204, ""))
        }
        ("GET", ["v1", "fs", fs, "locks"]) => Ok(Response::json(200, &json!(e.fs_locks(fs)?))),
        ("POST", ["v1", "fs", fs, "rename"]) => {
            let body = parse(req)?;
            let (parent, name) = (u64_field(&body, "parent")?, str_field(&body, "name")?);
            let (new_parent, new_name) = (
                u64_field(&body, "new_parent")?,
                str_field(&body, "new_name")?,
            );
            let mut inos = vec![parent, new_parent];
            inos.extend(child(e, fs, parent, name));
            inos.extend(child(e, fs, new_parent, new_name));
            let _change = change(g, req, fs, &inos)?;
            e.fs_rename(fs, parent, name, new_parent, new_name)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs", fs, "snapshots"]) => {
            let body = parse(req)?;
            let name = str_field(&body, "name")?;
            let id = client_id(&body).map_err(bad)?;
            // A snapshot shares the filesystem's extents, so it lives in the filesystem's group.
            sh.claim(ObjectKind::FsSnapshot, &id, group)?;
            Ok(Response::json(
                201,
                &json!({ "id": e.snapshot_fs_as(id, fs, name)? }),
            ))
        }
        ("POST", ["v1", "fs", fs, "replica", "apply"]) => {
            let part: ReplicaPart = serde_json::from_slice(&req.body)
                .map_err(|err| NativeError::Invalid(format!("invalid increment: {err}")))?;
            let mut inos: Vec<u64> = part.inodes.iter().map(|i| i.ino).collect();
            inos.extend(&part.removed);
            let _change = change(g, req, fs, &inos)?;
            e.replica_apply(fs, part.from, part.to, part.inodes, part.removed)?;
            Ok(Response::text(204, ""))
        }
        ("PUT", ["v1", "fs", fs, "replica", "inodes", ino, "data"]) => {
            let ino: u64 = ino
                .parse()
                .map_err(|_| NativeError::Invalid(format!("inode {ino:?} is not a number")))?;
            let offset = match query_u64(req, "offset") {
                Ok(o) => o,
                Err(r) => return Ok(r),
            };
            let _change = change(g, req, fs, &[ino])?;
            e.replica_write(fs, ino, offset, &req.body)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs", fs, "replica", "commit"]) => {
            let c: ReplicaCommit = serde_json::from_slice(&req.body)
                .map_err(|err| NativeError::Invalid(format!("invalid commit: {err}")))?;
            // The replica's snapshot shares its extents, so it lives in the filesystem's group.
            sh.claim(ObjectKind::FsSnapshot, &c.snapshot, group)?;
            e.replica_commit(fs, c.from, c.to, c.snapshot, c.next_ino)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs", fs, "promote"]) => {
            let force = body_or_empty(req)?["force"].as_bool().unwrap_or(false);
            e.replica_promote(fs, force)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["v1", "fs", fs, "demote"]) => {
            let body = parse(req)?;
            e.replica_demote(
                fs,
                str_field(&body, "snapshot")?.into(),
                str_field(&body, "base")?.into(),
            )?;
            Ok(Response::text(204, ""))
        }
        (method, ["v1", "fs", fs, "inodes", ino, rest @ ..]) => {
            let ino: u64 = ino
                .parse()
                .map_err(|_| NativeError::Invalid(format!("inode {ino:?} is not a number")))?;
            inode_route(sh, g, req, method, fs, ino, rest)
        }
        _ => Ok(Response::text(404, "no such route")),
    }
}

fn inode_route(
    sh: &NodeShared,
    g: &MetaGroup,
    req: &Request,
    method: &str,
    fs: &str,
    ino: u64,
    rest: &[&str],
) -> Routed {
    let e = &g.engine;
    let attr = |a| Ok(Response::json(200, &json!(a)));
    let change = |inos: &[u64]| change(g, req, fs, inos);
    match (method, rest) {
        ("GET", []) => {
            let lease = lease(g, req, fs, &[ino])?;
            leased(e.fs_getattr(fs, ino)?, lease)
        }
        ("POST", ["attr"]) => {
            let a: SetAttr = serde_json::from_slice(&req.body)
                .map_err(|err| NativeError::Invalid(format!("invalid attributes: {err}")))?;
            let _change = change(&[ino])?;
            attr(e.fs_setattr(fs, ino, a)?)
        }
        ("GET", ["lookup"]) => {
            let name = req
                .query
                .get("name")
                .ok_or_else(|| NativeError::Invalid("query parameter name is required".into()))
                .and_then(|n| pct_decode(n))?;
            // The directory's lease covers the name, the child's its attributes; each is read
            // after its lease is granted.
            let dir = lease(g, req, fs, &[ino])?;
            let found = e.fs_lookup(fs, ino, &name)?;
            let lease = match dir {
                Some(_) => lease(g, req, fs, &[found.ino])?,
                None => None,
            };
            leased(e.fs_getattr(fs, found.ino)?, lease)
        }
        ("GET", ["entries"]) => {
            let after = req.query.get("after").map(|a| pct_decode(a)).transpose()?;
            let limit = match req.query.get("limit") {
                Some(l) => l.parse().map_err(|_| {
                    NativeError::Invalid("query parameter limit must be an integer".into())
                })?,
                None => usize::MAX,
            };
            Ok(Response::json(
                200,
                &json!({ "entries": e.fs_readdir_page(fs, ino, after.as_deref(), limit)? }),
            ))
        }
        ("POST", ["entries"]) => {
            let node: NewNode = serde_json::from_slice(&req.body)
                .map_err(|err| NativeError::Invalid(format!("invalid node: {err}")))?;
            let _change = change(&[ino])?;
            Ok(Response::json(201, &json!(e.fs_mknode(fs, ino, node)?)))
        }
        ("POST", ["unlink"]) => {
            let body = parse(req)?;
            let name = str_field(&body, "name")?;
            let mut inos = vec![ino];
            inos.extend(child(e, fs, ino, name));
            let _change = change(&inos)?;
            e.fs_unlink(fs, ino, name)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["rmdir"]) => {
            let body = parse(req)?;
            let name = str_field(&body, "name")?;
            let mut inos = vec![ino];
            inos.extend(child(e, fs, ino, name));
            let _change = change(&inos)?;
            e.fs_rmdir(fs, ino, name)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["links"]) => {
            let body = parse(req)?;
            let parent = u64_field(&body, "parent")?;
            let _change = change(&[ino, parent])?;
            attr(e.fs_link(fs, ino, parent, str_field(&body, "name")?)?)
        }
        ("GET", ["xattrs"]) => Ok(Response::json(
            200,
            &json!({ "names": e.fs_listxattr(fs, ino)? }),
        )),
        ("GET", ["xattrs", name]) => Ok(Response::bytes(
            200,
            e.fs_getxattr(fs, ino, &pct_decode(name)?)?,
        )),
        ("PUT", ["xattrs", name]) => {
            let mode = match req.query.get("mode").map(String::as_str) {
                None | Some("set") => XattrMode::Set,
                Some("create") => XattrMode::Create,
                Some("replace") => XattrMode::Replace,
                Some(m) => return Err(NativeError::Invalid(format!("unknown xattr mode {m:?}"))),
            };
            let _change = change(&[ino])?;
            e.fs_setxattr(fs, ino, &pct_decode(name)?, &req.body, mode)?;
            Ok(Response::text(204, ""))
        }
        ("DELETE", ["xattrs", name]) => {
            let _change = change(&[ino])?;
            e.fs_removexattr(fs, ino, &pct_decode(name)?)?;
            Ok(Response::text(204, ""))
        }
        ("POST", ["locks"]) => {
            let body = parse(req)?;
            let kind = match str_field(&body, "kind")? {
                "unlock" => None,
                k => Some(lock_kind(k)?),
            };
            let (start, end) = lock_range(|k| body[k].as_u64())?;
            e.set_lock(
                fs,
                ino,
                LockRequest {
                    session: str_field(&body, "session")?.into(),
                    owner: u64_field(&body, "owner")?,
                    kind,
                    start,
                    end,
                    pid: body["pid"].as_u64().unwrap_or(0) as u32,
                },
            )?;
            Ok(Response::text(204, ""))
        }
        ("GET", ["locks"]) => {
            let q = |k: &str| req.query.get(k);
            let num = |k: &str| q(k).and_then(|v| v.parse::<u64>().ok());
            let session = q("session").ok_or_else(|| {
                NativeError::Invalid("query parameter session is required".into())
            })?;
            let owner = num("owner")
                .ok_or_else(|| NativeError::Invalid("query parameter owner is required".into()))?;
            let kind = lock_kind(q("kind").map_or("write", String::as_str))?;
            let (start, end) = lock_range(num)?;
            Ok(Response::json(
                200,
                &json!({ "conflict": e.test_lock(fs, ino, session, owner, kind, start, end)? }),
            ))
        }
        ("POST", ["locks", "release"]) => {
            let body = parse(req)?;
            e.release_lock_owner(
                fs,
                ino,
                str_field(&body, "session")?,
                u64_field(&body, "owner")?,
            )?;
            Ok(Response::text(204, ""))
        }
        ("GET", ["target"]) => Ok(Response::json(
            200,
            &json!({ "target": e.fs_readlink(fs, ino)? }),
        )),
        ("GET", ["data"]) => {
            let (offset, len) = match read_range(sh, req) {
                Ok(r) => r,
                Err(r) => return Ok(r),
            };
            Ok(Response::bytes(200, e.read_file(fs, ino, offset, len)?))
        }
        ("GET", ["layout"]) => {
            let (offset, len) = match read_range(sh, req) {
                Ok(r) => r,
                Err(r) => return Ok(r),
            };
            Ok(Response::json(
                200,
                &json!(e.file_layout(fs, ino, offset, len)?),
            ))
        }
        ("PUT", ["data"]) => {
            let offset = match query_u64(req, "offset") {
                Ok(o) => o,
                Err(r) => return Ok(r),
            };
            let _change = change(&[ino])?;
            attr(e.write_file(fs, ino, offset, &req.body)?)
        }
        _ => Ok(Response::text(404, "no such route")),
    }
}

/// The client session a request comes from (`?session=`), if it names one.
fn session_of(req: &Request) -> Option<&str> {
    req.query.get("session").map(String::as_str)
}

/// The inode `name` in `dir` names, if any.
fn child(e: &NativeEngine, fs: &str, dir: u64, name: &str) -> Option<u64> {
    e.fs_lookup(fs, dir, name).ok().map(|a| a.ino)
}

/// Starts a change to `inos`, first recalling other sessions' cache leases on them.
fn change<'a>(
    g: &'a MetaGroup,
    req: &Request,
    fs: &str,
    inos: &[u64],
) -> Result<Change<'a>, NativeError> {
    let term = g.raft.status()?.term;
    let grace = g.engine.caching_since_before(term)?;
    g.leases.begin(term, grace, fs, inos, session_of(req))
}

/// With `?lease=1&session=<id>` on the leader, grants the session a cache lease on `inos` once
/// its leadership is confirmed; the caller reads what it returns after this.
fn lease(
    g: &MetaGroup,
    req: &Request,
    fs: &str,
    inos: &[u64],
) -> Result<Option<Duration>, NativeError> {
    let Some(session) = session_of(req).filter(|_| req.query.contains_key("lease")) else {
        return Ok(None);
    };
    if fs.contains('@') {
        return Ok(None);
    }
    let status = g.raft.status()?;
    if status.role != Role::Leader {
        return Ok(None);
    }
    g.engine.session(session)?;
    g.engine.read_barrier()?;
    g.leases.grant(status.term, fs, inos, session)
}

/// Attributes, with the length of the cache lease granted on them if any.
fn leased(a: impl serde::Serialize, lease: Option<Duration>) -> Routed {
    let mut v = json!(a);
    if let (Some(l), Some(o)) = (lease, v.as_object_mut()) {
        o.insert("lease_ms".into(), json!(l.as_millis() as u64));
    }
    Ok(Response::json(200, &v))
}

fn session_json(id: &str, s: Session) -> Response {
    Response::json(
        200,
        &json!({ "session": id, "ttl_ms": s.ttl_ms, "expires_ms": s.expires_ms }),
    )
}

fn lock_kind(k: &str) -> Result<LockKind, NativeError> {
    match k {
        "read" => Ok(LockKind::Read),
        "write" => Ok(LockKind::Write),
        _ => Err(NativeError::Invalid(format!(
            "lock kind must be read, write or unlock, not {k:?}"
        ))),
    }
}

/// `start` (default 0) and inclusive `end` (default end of file).
fn lock_range(field: impl Fn(&str) -> Option<u64>) -> Result<(u64, u64), NativeError> {
    Ok((
        field("start").unwrap_or(0),
        field("end").unwrap_or(u64::MAX),
    ))
}

/// Body of `POST /v1/fs/{fs}/replica/apply`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaPart {
    from: Option<String>,
    to: String,
    #[serde(default)]
    inodes: Vec<ReplicaInode>,
    #[serde(default)]
    removed: Vec<u64>,
}

/// Body of `POST /v1/fs/{fs}/replica/commit`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaCommit {
    from: Option<String>,
    to: String,
    snapshot: String,
    next_ino: u64,
}

/// A JSON body, or an empty object when there is none.
fn body_or_empty(req: &Request) -> Result<serde_json::Value, NativeError> {
    if req.body.is_empty() {
        return Ok(json!({}));
    }
    parse(req)
}

fn bad(r: Response) -> NativeError {
    NativeError::Invalid(String::from_utf8_lossy(&r.body).into_owned())
}

fn parse(req: &Request) -> Result<serde_json::Value, NativeError> {
    body_json(req).map_err(bad)
}

fn str_field<'a>(body: &'a serde_json::Value, key: &str) -> Result<&'a str, NativeError> {
    body[key]
        .as_str()
        .ok_or_else(|| NativeError::Invalid(format!("body field {key:?} (string) is required")))
}

fn u64_field(body: &serde_json::Value, key: &str) -> Result<u64, NativeError> {
    body[key]
        .as_u64()
        .ok_or_else(|| NativeError::Invalid(format!("body field {key:?} (integer) is required")))
}

/// Decodes `%XX` escapes (and `+` as a space) into a UTF-8 string.
fn pct_decode(s: &str) -> Result<String, NativeError> {
    let bad = || NativeError::Invalid(format!("malformed percent-encoding in {s:?}"));
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hex = [bytes.next().ok_or_else(bad)?, bytes.next().ok_or_else(bad)?];
                let hex = std::str::from_utf8(&hex).map_err(|_| bad())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|_| NativeError::Invalid("name is not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::pct_decode;

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(pct_decode("a%20b+c%2Fd%C3%A9").unwrap(), "a b c/dé");
        assert!(pct_decode("%4").is_err());
        assert!(pct_decode("%zz").is_err());
        assert!(pct_decode("%FF").is_err());
    }
}
