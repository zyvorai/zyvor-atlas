// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Objects as files: a bucket is one native filesystem, an object key `a/b/c` is the file `c` in
//! directory `a/b`, and a key ending in `/` is a directory marker. Every call here blocks (it
//! talks to the cluster through [`Ops`]); the S3 service runs them on the blocking pool.
//!
//! A PUT writes a hidden temporary file next to the object and renames it into place, so readers
//! (S3, NFS or a FUSE mount) see the old object or the new one, never a partial one. ETag and the
//! object's headers live in one extended attribute, valid while the file's size and mtime match
//! what was recorded; a file changed through another protocol gets an ETag derived from its inode,
//! size and mtime instead of an MD5.

use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use atlas_native::{engine::Attr, NodeType, ROOT_INO};
use atlas_native_fuse::ops::{Errno, Ops, HIDDEN_PREFIX};
use s3s::crypto::{Checksum, Md5};
use serde::{Deserialize, Serialize};

/// Names starting with this are the gateway's (and the FUSE client's) own: never valid in a key,
/// never listed.
pub const RESERVED_PREFIX: &str = ".atlas_";
const TEMP_PREFIX: &str = ".atlas_s3_tmp_";
const UPLOADS_DIR: &str = ".atlas_s3_uploads";
const META_XATTR: &str = "user.atlas.s3";
const UPLOAD_XATTR: &str = "user.atlas.s3.upload";
/// On an object: Unix seconds until which it is retention-locked (decimal, compliance mode).
const RETAIN_XATTR: &str = "user.atlas.s3.retain";
/// On the root: when the gateway first served the bucket (decimal ns since the epoch).
const CREATED_XATTR: &str = "user.atlas.s3.created";
pub const MAX_KEY_BYTES: usize = 1024;
pub const MIN_PART_BYTES: u64 = 5 << 20;
pub const MAX_PART_NUMBER: i32 = 10_000;
/// Bytes moved per read or write call when copying or streaming.
pub const CHUNK: usize = 4 << 20;
/// Object attributes fetched at once for a listing page.
const LIST_PARALLELISM: usize = 8;

const _: () = assert!(HIDDEN_PREFIX.len() > RESERVED_PREFIX.len());

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such key")]
    NoSuchKey,
    #[error("no such upload")]
    NoSuchUpload,
    #[error("{0}")]
    InvalidKey(String),
    #[error("{0}")]
    Conflict(String),
    #[error("the bucket is read-only")]
    ReadOnly,
    #[error("{0}")]
    Locked(String),
    #[error("the filesystem's quota is exhausted")]
    Quota,
    #[error("{0}")]
    InvalidPart(String),
    #[error("part {0} is smaller than 5 MiB and not the last part")]
    TooSmall(i32),
    #[error("the Content-MD5 does not match the body")]
    BadDigest,
    #[error("{0}")]
    Invalid(String),
    #[error("filesystem error (errno {0})")]
    Io(Errno),
}

fn io(e: Errno) -> StoreError {
    match e {
        libc::EDQUOT => StoreError::Quota,
        libc::EROFS => StoreError::ReadOnly,
        libc::ENAMETOOLONG => {
            StoreError::InvalidKey("a key segment is longer than 255 bytes".into())
        }
        e => StoreError::Io(e),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// ETag of empty content (directory markers).
pub const EMPTY_ETAG: &str = "d41d8cd98f00b204e9800998ecf8427e";

/// A key split into its directories and, unless it is a directory marker (`a/b/`), its name.
#[derive(Debug, PartialEq, Eq)]
pub struct KeyPath<'a> {
    pub dirs: Vec<&'a str>,
    pub name: Option<&'a str>,
}

/// Splits `key` into path segments, refusing keys that have no file path: empty segments (`a//b`,
/// a leading `/`), `.` and `..`, segments over 255 bytes, NUL, and reserved names.
pub fn parse_key(key: &str) -> Result<KeyPath<'_>, StoreError> {
    let bad = |why: &str| {
        Err(StoreError::InvalidKey(format!(
            "the key cannot be stored as a file path: {why}"
        )))
    };
    if key.is_empty() {
        return bad("it is empty");
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(StoreError::InvalidKey(format!(
            "keys are limited to {MAX_KEY_BYTES} bytes"
        )));
    }
    let (body, marker) = match key.strip_suffix('/') {
        Some(b) => (b, true),
        None => (key, false),
    };
    if body.is_empty() {
        return bad("it is `/`");
    }
    let mut parts: Vec<&str> = body.split('/').collect();
    for p in &parts {
        if p.is_empty() {
            return bad("it has an empty segment (`//` or a leading `/`)");
        }
        if *p == "." || *p == ".." {
            return bad("it has a `.` or `..` segment");
        }
        if p.len() > atlas_native::namespace::MAX_NAME_BYTES {
            return bad("a segment is longer than 255 bytes");
        }
        if p.contains('\0') {
            return bad("it contains NUL");
        }
        if p.starts_with(RESERVED_PREFIX) {
            return bad("segments starting with `.atlas_` are reserved");
        }
    }
    let name = if marker { None } else { parts.pop() };
    Ok(KeyPath { dirs: parts, name })
}

/// Standard headers and user metadata stored with an object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Headers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// The extended attribute on an object's file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ObjectMeta {
    etag: String,
    size: u64,
    mtime_ns: i64,
    #[serde(flatten)]
    headers: Headers,
}

/// The extended attribute on a multipart upload's directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadMeta {
    key: String,
    initiated_ns: i64,
    #[serde(flatten)]
    headers: Headers,
}

#[derive(Debug, Clone)]
pub struct Object {
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub etag: String,
    pub headers: Headers,
}

#[derive(Debug, Clone)]
pub enum Listed {
    Object { key: String, object: Object },
    Prefix(String),
}

#[derive(Debug, Clone)]
pub struct Part {
    pub number: i32,
    pub size: u64,
    pub mtime_ns: i64,
    pub etag: String,
}

#[derive(Debug, Clone)]
pub struct Upload {
    pub key: String,
    pub id: String,
    pub initiated_ns: i64,
}

/// Bytes being written to a hidden temporary file, with their MD5.
pub struct Writer {
    parent: u64,
    ino: u64,
    tmp: String,
    offset: u64,
    md5: Md5,
}

impl Writer {
    pub fn len(&self) -> u64 {
        self.offset
    }

    pub fn is_empty(&self) -> bool {
        self.offset == 0
    }
}

/// One exported filesystem.
pub struct Bucket {
    pub name: String,
    pub ops: Ops,
    /// Read-only by configuration (a snapshot bucket is read-only regardless).
    pub read_only: bool,
    /// Owner of the files and directories the gateway creates.
    pub uid: u32,
    pub gid: u32,
}

impl Bucket {
    pub fn is_read_only(&self) -> bool {
        self.read_only || self.ops.read_only()
    }

    fn writable(&self) -> Result<(), StoreError> {
        if self.is_read_only() {
            Err(StoreError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Creation time reported for the bucket: the filesystem root's ctime.
    /// The bucket's creation date: the root's stamp, else (never stamped) its ctime, which
    /// moves with every change to the top directory.
    pub fn created_ns(&self) -> Result<i64, StoreError> {
        match self.ops.getxattr(ROOT_INO, CREATED_XATTR) {
            Ok(v) => {
                if let Some(ns) = std::str::from_utf8(&v).ok().and_then(|s| s.parse().ok()) {
                    return Ok(ns);
                }
            }
            Err(libc::ENODATA) => {}
            Err(e) => return Err(io(e)),
        }
        Ok(self.ops.getattr(ROOT_INO).map_err(io)?.ctime_ns)
    }

    /// Stamps the root with the current time unless it already is (or the bucket is read-only).
    pub fn stamp_created(&self) -> Result<(), StoreError> {
        if self.is_read_only() {
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        match self.ops.setxattr(
            ROOT_INO,
            CREATED_XATTR,
            now.to_string().as_bytes(),
            true,
            false,
        ) {
            Ok(()) | Err(libc::EEXIST) => Ok(()),
            Err(e) => Err(io(e)),
        }
    }

    fn lookup(&self, parent: u64, name: &str) -> Result<Option<Attr>, StoreError> {
        match self.ops.lookup(parent, name) {
            Ok(a) => Ok(Some(a)),
            Err(libc::ENOENT) => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    /// The directory at `dirs`, if every segment exists and is a directory.
    fn find_dir(&self, dirs: &[&str]) -> Result<Option<u64>, StoreError> {
        let mut ino = ROOT_INO;
        for d in dirs {
            match self.lookup(ino, d)? {
                Some(a) if a.kind == NodeType::Dir => ino = a.ino,
                _ => return Ok(None),
            }
        }
        Ok(Some(ino))
    }

    /// The directory at `dirs`, creating missing segments.
    fn make_dirs(&self, dirs: &[&str]) -> Result<u64, StoreError> {
        let mut ino = ROOT_INO;
        for (i, d) in dirs.iter().enumerate() {
            let a = match self.lookup(ino, d)? {
                Some(a) => a,
                None => {
                    match self
                        .ops
                        .mknode(ino, d, NodeType::Dir, None, 0o755, self.uid, self.gid)
                    {
                        Ok(a) => a,
                        Err(libc::EEXIST) => {
                            self.lookup(ino, d)?.ok_or(StoreError::Io(libc::ENOENT))?
                        }
                        Err(e) => return Err(io(e)),
                    }
                }
            };
            if a.kind != NodeType::Dir {
                return Err(StoreError::Conflict(format!(
                    "`{}` is an object, so no key can continue it with `/`",
                    dirs[..=i].join("/")
                )));
            }
            ino = a.ino;
        }
        Ok(ino)
    }

    fn meta<T: serde::de::DeserializeOwned>(
        &self,
        ino: u64,
        xattr: &str,
    ) -> Result<Option<T>, StoreError> {
        match self.ops.getxattr(ino, xattr) {
            Ok(v) => Ok(serde_json::from_slice(&v).ok()),
            Err(libc::ENODATA) => Ok(None),
            Err(libc::ENOENT) => Err(StoreError::NoSuchKey),
            Err(e) => Err(io(e)),
        }
    }

    /// Unix seconds the object is retention-locked until (0 = not locked).
    fn retained_until(&self, ino: u64) -> Result<u64, StoreError> {
        match self.ops.getxattr(ino, RETAIN_XATTR) {
            Ok(v) => Ok(std::str::from_utf8(&v)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0)),
            Err(libc::ENODATA | libc::ENOENT) => Ok(0),
            Err(e) => Err(io(e)),
        }
    }

    fn refuse_if_retained(&self, ino: u64) -> Result<(), StoreError> {
        let until = self.retained_until(ino)?;
        if until > unix_now() {
            return Err(StoreError::Locked(format!(
                "the object is retention-locked until {until} (unix seconds)"
            )));
        }
        Ok(())
    }

    /// The object's retention-lock expiry (0 = none).
    pub fn retention(&self, key: &str) -> Result<u64, StoreError> {
        let o = self.head(key)?;
        self.retained_until(o.ino)
    }

    /// Locks the object against delete and overwrite until `until`. Only ever extended.
    pub fn set_retention(&self, key: &str, until: u64) -> Result<(), StoreError> {
        self.writable()?;
        let o = self.head(key)?;
        let cur = self.retained_until(o.ino)?;
        if until < cur {
            return Err(StoreError::Locked(format!(
                "the object is locked until {cur}; retention cannot be shortened"
            )));
        }
        self.ops
            .setxattr(o.ino, RETAIN_XATTR, until.to_string().as_bytes(), false, false)
            .map_err(io)
    }

    fn is_marker(&self, dir: u64) -> Result<bool, StoreError> {
        Ok(self.meta::<ObjectMeta>(dir, META_XATTR)?.is_some())
    }

    fn object(&self, a: &Attr) -> Result<Object, StoreError> {
        let meta: Option<ObjectMeta> = self.meta(a.ino, META_XATTR)?;
        let (etag, headers) = match meta {
            Some(m) if a.kind == NodeType::Dir => (EMPTY_ETAG.to_string(), m.headers),
            Some(m) if m.size == a.size && m.mtime_ns == a.mtime_ns => (m.etag, m.headers),
            Some(m) => (derived_etag(a), m.headers),
            None => (derived_etag(a), Headers::default()),
        };
        Ok(Object {
            ino: a.ino,
            size: if a.kind == NodeType::Dir { 0 } else { a.size },
            mtime_ns: a.mtime_ns,
            etag,
            headers,
        })
    }

    /// The object at `key`: a regular file, or a directory carrying a marker.
    pub fn head(&self, key: &str) -> Result<Object, StoreError> {
        let p = match parse_key(key) {
            Ok(p) => p,
            Err(StoreError::InvalidKey(_)) => return Err(StoreError::NoSuchKey),
            Err(e) => return Err(e),
        };
        let dir = self.find_dir(&p.dirs)?.ok_or(StoreError::NoSuchKey)?;
        match p.name {
            Some(name) => match self.lookup(dir, name)? {
                Some(a) if a.kind == NodeType::File => self.object(&a),
                _ => Err(StoreError::NoSuchKey),
            },
            None => {
                if !self.is_marker(dir)? {
                    return Err(StoreError::NoSuchKey);
                }
                self.object(&self.ops.getattr(dir).map_err(io)?)
            }
        }
    }

    pub fn read(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, StoreError> {
        match self.ops.read(ino, offset, len) {
            Err(libc::EISDIR) => Ok(Vec::new()),
            r => r.map_err(io),
        }
    }

    /// Where an object at `key` goes: its directory (created) and file name. A marker key makes
    /// its directories and marks the last one.
    pub fn prepare_put(
        &self,
        key: &str,
        headers: &Headers,
    ) -> Result<Option<(u64, String)>, StoreError> {
        self.writable()?;
        let p = parse_key(key)?;
        let dir = self.make_dirs(&p.dirs)?;
        match p.name {
            Some(name) => {
                if let Some(a) = self.lookup(dir, name)? {
                    if a.kind == NodeType::Dir {
                        return Err(StoreError::Conflict(format!(
                            "`{key}` is a prefix of other keys (a directory), so it cannot be an object"
                        )));
                    }
                }
                Ok(Some((dir, name.to_string())))
            }
            None => {
                let a = self.ops.getattr(dir).map_err(io)?;
                let meta = ObjectMeta {
                    etag: EMPTY_ETAG.into(),
                    size: 0,
                    mtime_ns: a.mtime_ns,
                    headers: headers.clone(),
                };
                self.set_meta(dir, META_XATTR, &meta)?;
                Ok(None)
            }
        }
    }

    fn set_meta<T: Serialize>(&self, ino: u64, xattr: &str, v: &T) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(v).map_err(|_| StoreError::Io(libc::EINVAL))?;
        self.ops
            .setxattr(ino, xattr, &bytes, false, false)
            .map_err(io)
    }

    /// A hidden temporary file in `parent` to write an object or part into.
    pub fn writer(&self, parent: u64) -> Result<Writer, StoreError> {
        self.writable()?;
        let tmp = format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple());
        let a = self
            .ops
            .mknode(
                parent,
                &tmp,
                NodeType::File,
                None,
                0o644,
                self.uid,
                self.gid,
            )
            .map_err(io)?;
        Ok(Writer {
            parent,
            ino: a.ino,
            tmp,
            offset: 0,
            md5: Md5::new(),
        })
    }

    pub fn write(&self, w: &mut Writer, data: &[u8]) -> Result<(), StoreError> {
        if data.is_empty() {
            return Ok(());
        }
        self.ops.write(w.ino, w.offset, data).map_err(io)?;
        w.md5.update(data);
        w.offset += data.len() as u64;
        Ok(())
    }

    /// Removes a temporary file that will not be committed.
    pub fn discard(&self, w: Writer) {
        let _ = self.ops.flush(w.ino);
        if let Err(e) = self.ops.unlink(w.parent, &w.tmp) {
            tracing::warn!(bucket = %self.name, errno = e, "could not remove a temporary file");
        }
    }

    /// Makes the written bytes the object `name` in their directory, replacing any object there.
    /// `expected_md5` is the request's Content-MD5; `etag` overrides the MD5 ETag (multipart).
    pub fn commit(
        &self,
        w: Writer,
        name: &str,
        expected_md5: Option<[u8; 16]>,
        etag: Option<String>,
        headers: Headers,
    ) -> Result<Object, StoreError> {
        let Writer {
            parent,
            ino,
            tmp,
            md5,
            ..
        } = w;
        let digest = md5.finalize();
        let result = (|| {
            self.ops.flush(ino).map_err(io)?;
            if expected_md5.is_some_and(|m| m != digest) {
                return Err(StoreError::BadDigest);
            }
            let etag = etag.unwrap_or_else(|| hex(&digest));
            let a = self.ops.getattr(ino).map_err(io)?;
            let meta = ObjectMeta {
                etag: etag.clone(),
                size: a.size,
                mtime_ns: a.mtime_ns,
                headers: headers.clone(),
            };
            self.set_meta(ino, META_XATTR, &meta)?;
            if let Some(t) = self.lookup(parent, name)? {
                if t.kind == NodeType::Dir {
                    return Err(StoreError::Conflict(format!(
                        "`{name}` is a directory (a prefix of other keys), so it cannot be an object"
                    )));
                }
                self.refuse_if_retained(t.ino)?;
            }
            self.ops
                .rename(parent, &tmp, parent, name)
                .map_err(|e| match e {
                    libc::EISDIR | libc::ENOTEMPTY | libc::EEXIST => StoreError::Conflict(format!(
                        "`{name}` is a directory (a prefix of other keys), so it cannot be an object"
                    )),
                    e => io(e),
                })?;
            Ok(Object {
                ino,
                size: a.size,
                mtime_ns: a.mtime_ns,
                etag,
                headers,
            })
        })();
        if result.is_err() {
            if let Err(e) = self.ops.unlink(parent, &tmp) {
                tracing::warn!(bucket = %self.name, errno = e, "could not remove a temporary file");
            }
        }
        result
    }

    /// Replaces the stored headers of an existing object (a copy onto itself).
    pub fn set_headers(&self, key: &str, headers: Headers) -> Result<Object, StoreError> {
        self.writable()?;
        let mut o = self.head(key)?;
        let a = self.ops.getattr(o.ino).map_err(io)?;
        let meta = ObjectMeta {
            etag: o.etag.clone(),
            size: if a.kind == NodeType::Dir { 0 } else { a.size },
            mtime_ns: a.mtime_ns,
            headers: headers.clone(),
        };
        self.set_meta(o.ino, META_XATTR, &meta)?;
        o.headers = headers;
        Ok(o)
    }

    /// Appends `len` bytes of `src`'s file `ino` from `offset` to `w`.
    pub fn copy_from(
        &self,
        src: &Bucket,
        ino: u64,
        offset: u64,
        len: u64,
        w: &mut Writer,
    ) -> Result<(), StoreError> {
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(CHUNK as u64) as usize;
            let data = src.read(ino, offset + done, n)?;
            if data.is_empty() {
                return Err(StoreError::Invalid(
                    "the source object changed while it was copied".into(),
                ));
            }
            self.write(w, &data)?;
            done += data.len() as u64;
        }
        Ok(())
    }

    /// Deletes the object at `key` (nothing if there is none) and the directories it leaves
    /// empty, up to the first one that is a marker or still holds something.
    pub fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.writable()?;
        let p = match parse_key(key) {
            Ok(p) => p,
            Err(StoreError::InvalidKey(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut chain = vec![ROOT_INO];
        for d in &p.dirs {
            match self.lookup(*chain.last().unwrap_or(&ROOT_INO), d)? {
                Some(a) if a.kind == NodeType::Dir => chain.push(a.ino),
                _ => return Ok(()),
            }
        }
        let dir = *chain.last().unwrap_or(&ROOT_INO);
        match p.name {
            Some(name) => match self.lookup(dir, name)? {
                Some(a) if a.kind != NodeType::Dir => {
                    self.refuse_if_retained(a.ino)?;
                    match self.ops.unlink(dir, name) {
                        Ok(()) | Err(libc::ENOENT) => {}
                        Err(e) => return Err(io(e)),
                    }
                }
                _ => return Ok(()),
            },
            None => match self.ops.removexattr(dir, META_XATTR) {
                Ok(()) | Err(libc::ENODATA) | Err(libc::ENOENT) => {}
                Err(e) => return Err(io(e)),
            },
        }
        for i in (0..p.dirs.len()).rev() {
            if self.is_marker(chain[i + 1]).unwrap_or(true) {
                break;
            }
            if self.ops.rmdir(chain[i], p.dirs[i]).is_err() {
                break;
            }
        }
        Ok(())
    }

    /// Up to `max` keys and common prefixes after `after`, in key order, and whether more follow.
    pub fn list(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        after: Option<&str>,
        max: usize,
    ) -> Result<(Vec<Listed>, bool), StoreError> {
        if max == 0 {
            return Ok((Vec::new(), false));
        }
        let (dir_path, rest) = match prefix.rfind('/') {
            Some(i) => (&prefix[..=i], &prefix[i + 1..]),
            None => ("", prefix),
        };
        let dirs: Vec<&str> = dir_path.split('/').filter(|s| !s.is_empty()).collect();
        if dirs.len() != dir_path.matches('/').count()
            || dirs
                .iter()
                .any(|d| *d == "." || *d == ".." || d.starts_with(RESERVED_PREFIX))
        {
            return Ok((Vec::new(), false));
        }
        let Some(start) = self.find_dir(&dirs)? else {
            return Ok((Vec::new(), false));
        };
        let mut walk = Walk {
            bucket: self,
            prefix,
            delimiter: delimiter.filter(|d| !d.is_empty()),
            after,
            max,
            found: Vec::new(),
            last_prefix: None,
        };
        if !dir_path.is_empty()
            && rest.is_empty()
            && after.is_none_or(|a| dir_path > a)
            && self.is_marker(start)?
        {
            walk.found.push(Found::Object(dir_path.to_string(), start));
        }
        walk.dir(start, dir_path, Some(rest))?;
        let truncated = walk.found.len() > max;
        walk.found.truncate(max);
        Ok((self.resolve(walk.found)?, truncated))
    }

    /// Fetches the objects' attributes, several at once; ones removed meanwhile are left out.
    fn resolve(&self, found: Vec<Found>) -> Result<Vec<Listed>, StoreError> {
        let mut slots: Vec<Option<Result<Option<Listed>, StoreError>>> =
            found.iter().map(|_| None).collect();
        let chunk = found.len().div_ceil(LIST_PARALLELISM).max(1);
        std::thread::scope(|s| {
            for (items, out) in found.chunks(chunk).zip(slots.chunks_mut(chunk)) {
                s.spawn(move || {
                    for (f, slot) in items.iter().zip(out.iter_mut()) {
                        *slot = Some(match f {
                            Found::Prefix(p) => Ok(Some(Listed::Prefix(p.clone()))),
                            Found::Object(key, ino) => match self.ops.getattr(*ino) {
                                Ok(a) => match self.object(&a) {
                                    Ok(object) => Ok(Some(Listed::Object {
                                        key: key.clone(),
                                        object,
                                    })),
                                    Err(StoreError::NoSuchKey) => Ok(None),
                                    Err(e) => Err(e),
                                },
                                Err(libc::ENOENT) => Ok(None),
                                Err(e) => Err(io(e)),
                            },
                        });
                    }
                });
            }
        });
        let mut out = Vec::with_capacity(found.len());
        for slot in slots.into_iter().flatten() {
            if let Some(l) = slot? {
                out.push(l);
            }
        }
        Ok(out)
    }

    fn upload_dir(&self, id: &str, key: &str) -> Result<(u64, UploadMeta), StoreError> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::NoSuchUpload);
        }
        let dir = self
            .find_dir(&[UPLOADS_DIR, id])?
            .ok_or(StoreError::NoSuchUpload)?;
        match self.meta::<UploadMeta>(dir, UPLOAD_XATTR) {
            Ok(Some(m)) if m.key == key => Ok((dir, m)),
            Ok(_) | Err(StoreError::NoSuchKey) => Err(StoreError::NoSuchUpload),
            Err(e) => Err(e),
        }
    }

    pub fn create_upload(&self, key: &str, headers: Headers) -> Result<String, StoreError> {
        self.writable()?;
        if parse_key(key)?.name.is_none() {
            return Err(StoreError::InvalidKey(
                "a directory marker cannot be uploaded in parts".into(),
            ));
        }
        let uploads = self.make_dirs(&[UPLOADS_DIR])?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let a = self
            .ops
            .mknode(uploads, &id, NodeType::Dir, None, 0o700, self.uid, self.gid)
            .map_err(io)?;
        let meta = UploadMeta {
            key: key.to_string(),
            initiated_ns: a.ctime_ns,
            headers,
        };
        self.set_meta(a.ino, UPLOAD_XATTR, &meta)?;
        Ok(id)
    }

    /// A writer for part `number` of upload `id`.
    pub fn part_writer(&self, id: &str, key: &str, number: i32) -> Result<Writer, StoreError> {
        self.writable()?;
        if !(1..=MAX_PART_NUMBER).contains(&number) {
            return Err(StoreError::Invalid(format!(
                "part numbers run from 1 to {MAX_PART_NUMBER}"
            )));
        }
        let (dir, _) = self.upload_dir(id, key)?;
        self.writer(dir)
    }

    pub fn commit_part(
        &self,
        w: Writer,
        number: i32,
        expected_md5: Option<[u8; 16]>,
    ) -> Result<String, StoreError> {
        let name = format!("{number:05}");
        Ok(self
            .commit(w, &name, expected_md5, None, Headers::default())?
            .etag)
    }

    pub fn list_parts(&self, id: &str, key: &str) -> Result<Vec<Part>, StoreError> {
        let (dir, _) = self.upload_dir(id, key)?;
        let mut parts = Vec::new();
        for e in self.ops.readdir(dir).map_err(io)? {
            let Ok(number) = e.name.parse::<i32>() else {
                continue;
            };
            if e.kind != NodeType::File || e.name.len() != 5 {
                continue;
            }
            let a = match self.ops.getattr(e.ino) {
                Ok(a) => a,
                Err(libc::ENOENT) => continue,
                Err(e) => return Err(io(e)),
            };
            let o = self.object(&a)?;
            parts.push(Part {
                number,
                size: o.size,
                mtime_ns: o.mtime_ns,
                etag: o.etag,
            });
        }
        parts.sort_by_key(|p| p.number);
        Ok(parts)
    }

    /// Assembles the parts (in ascending order, each but the last at least 5 MiB, ETags
    /// matching) into the object, then removes the upload.
    pub fn complete_upload(
        &self,
        id: &str,
        key: &str,
        parts: &[(i32, String)],
    ) -> Result<Object, StoreError> {
        self.writable()?;
        let (dir, upload) = self.upload_dir(id, key)?;
        if parts.is_empty() {
            return Err(StoreError::InvalidPart(
                "at least one part is required".into(),
            ));
        }
        if parts.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(StoreError::InvalidPart(
                "parts must be listed in ascending order".into(),
            ));
        }
        let stored: BTreeMap<i32, Part> = self
            .list_parts(id, key)?
            .into_iter()
            .map(|p| (p.number, p))
            .collect();
        let mut digests = Vec::with_capacity(parts.len() * 16);
        let mut sources = Vec::with_capacity(parts.len());
        for (i, (number, etag)) in parts.iter().enumerate() {
            let part = stored.get(number).ok_or_else(|| {
                StoreError::InvalidPart(format!("part {number} was not uploaded"))
            })?;
            if part.etag != etag.trim_matches('"') {
                return Err(StoreError::InvalidPart(format!(
                    "part {number}'s ETag does not match"
                )));
            }
            if i + 1 < parts.len() && part.size < MIN_PART_BYTES {
                return Err(StoreError::TooSmall(*number));
            }
            digests.extend_from_slice(&unhex(&part.etag).unwrap_or_default());
            let ino = self
                .lookup(dir, &format!("{number:05}"))?
                .ok_or_else(|| StoreError::InvalidPart(format!("part {number} was not uploaded")))?
                .ino;
            sources.push((ino, part.size));
        }
        let etag = format!("{}-{}", hex(&Md5::checksum(&digests)), parts.len());
        let (parent, name) = self
            .prepare_put(key, &upload.headers)?
            .ok_or_else(|| StoreError::InvalidKey("not an object key".into()))?;
        let mut w = self.writer(parent)?;
        for (ino, size) in sources {
            if let Err(e) = self.copy_from(self, ino, 0, size, &mut w) {
                self.discard(w);
                return Err(e);
            }
        }
        let object = self.commit(w, &name, None, Some(etag), upload.headers)?;
        self.remove_upload(dir, id);
        Ok(object)
    }

    pub fn abort_upload(&self, id: &str, key: &str) -> Result<(), StoreError> {
        self.writable()?;
        let (dir, _) = self.upload_dir(id, key)?;
        self.remove_upload(dir, id);
        Ok(())
    }

    fn remove_upload(&self, dir: u64, id: &str) {
        if let Ok(entries) = self.ops.readdir(dir) {
            for e in entries {
                let _ = self.ops.unlink(dir, &e.name);
            }
        }
        if let Ok(Some(uploads)) = self.find_dir(&[UPLOADS_DIR]) {
            if let Err(e) = self.ops.rmdir(uploads, id) {
                tracing::warn!(bucket = %self.name, errno = e, "could not remove a finished upload");
            }
        }
    }

    pub fn list_uploads(&self, prefix: &str) -> Result<Vec<Upload>, StoreError> {
        let Some(uploads) = self.find_dir(&[UPLOADS_DIR])? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for e in self.ops.readdir(uploads).map_err(io)? {
            if e.kind != NodeType::Dir {
                continue;
            }
            match self.meta::<UploadMeta>(e.ino, UPLOAD_XATTR) {
                Ok(Some(m)) if m.key.starts_with(prefix) => out.push(Upload {
                    key: m.key,
                    id: e.name,
                    initiated_ns: m.initiated_ns,
                }),
                Ok(_) | Err(StoreError::NoSuchKey) => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| (&a.key, a.initiated_ns).cmp(&(&b.key, b.initiated_ns)));
        Ok(out)
    }
}

/// ETag of a file without a valid recorded one: stable while its content is unchanged.
fn derived_etag(a: &Attr) -> String {
    hex(&Md5::checksum(
        format!("{}:{}:{}", a.ino, a.size, a.mtime_ns).as_bytes(),
    ))
}

enum Found {
    Object(String, u64),
    Prefix(String),
}

/// A depth-first walk in key order: siblings sort by name, with `/` appended to directories, so
/// the walk visits keys in the same order as a sorted list of them.
struct Walk<'a> {
    bucket: &'a Bucket,
    prefix: &'a str,
    delimiter: Option<&'a str>,
    after: Option<&'a str>,
    max: usize,
    /// Up to `max + 1` results: one more than asked tells whether the listing is truncated.
    found: Vec<Found>,
    last_prefix: Option<String>,
}

impl Walk<'_> {
    fn full(&self) -> bool {
        self.found.len() > self.max
    }

    fn past(&self, key: &str) -> bool {
        self.after.is_none_or(|a| key > a)
    }

    /// Walks directory `ino` at key `path`; `filter` keeps only names starting with it (the part
    /// of the prefix after its last `/`). Returns false once enough results are found.
    fn dir(&mut self, ino: u64, path: &str, filter: Option<&str>) -> Result<bool, StoreError> {
        let mut entries: Vec<(String, u64, bool)> = self
            .bucket
            .ops
            .readdir(ino)
            .map_err(io)?
            .into_iter()
            .filter(|e| !e.name.starts_with(RESERVED_PREFIX))
            .filter(|e| filter.is_none_or(|f| e.name.starts_with(f)))
            .filter_map(|e| match e.kind {
                NodeType::File => Some((e.name, e.ino, false)),
                NodeType::Dir => Some((format!("{}/", e.name), e.ino, true)),
                _ => None,
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, child, is_dir) in entries {
            if self.full() {
                return Ok(false);
            }
            let key = format!("{path}{rel}");
            if let Some(d) = self.delimiter {
                if let Some(pos) = key[self.prefix.len()..].find(d) {
                    let cp = &key[..self.prefix.len() + pos + d.len()];
                    if self.last_prefix.as_deref() != Some(cp) {
                        self.last_prefix = Some(cp.to_string());
                        if self.past(cp) {
                            self.found.push(Found::Prefix(cp.to_string()));
                        }
                    }
                    // Keys below a directory whose own key already holds the delimiter all share
                    // this prefix.
                    continue;
                }
            }
            if is_dir {
                // A resume point past this whole subtree skips it.
                if let Some(a) = self.after {
                    if a > key.as_str() && !a.starts_with(&key) {
                        continue;
                    }
                }
                if self.past(&key) && self.bucket.is_marker(child)? {
                    self.found.push(Found::Object(key.clone(), child));
                }
                if !self.dir(child, &key, None)? {
                    return Ok(false);
                }
            } else if self.past(&key) {
                self.found.push(Found::Object(key, child));
            }
        }
        Ok(!self.full())
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_paths_or_are_refused() {
        assert_eq!(
            parse_key("a/b/c.txt").unwrap(),
            KeyPath {
                dirs: vec!["a", "b"],
                name: Some("c.txt")
            }
        );
        assert_eq!(
            parse_key("a/b/").unwrap(),
            KeyPath {
                dirs: vec!["a", "b"],
                name: None
            }
        );
        assert_eq!(
            parse_key("top").unwrap(),
            KeyPath {
                dirs: vec![],
                name: Some("top")
            }
        );
        for bad in [
            "",
            "/",
            "/lead",
            "a//b",
            "a/./b",
            "../x",
            "a/..",
            ".atlas_s3_uploads/x",
            "d/.atlas_hidden_1",
            "nul\0",
        ] {
            assert!(
                matches!(parse_key(bad), Err(StoreError::InvalidKey(_))),
                "{bad:?}"
            );
        }
        assert!(parse_key(&"x".repeat(256)).is_err());
        assert!(parse_key(&format!("{}/y", "x".repeat(255))).is_ok());
        assert!(parse_key(&"a/".repeat(513)).is_err());
    }

    #[test]
    fn hex_round_trips() {
        let d = Md5::checksum(b"abc");
        assert_eq!(hex(&d), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(unhex(&hex(&d)), Some(d));
        assert_eq!(unhex("zz"), None);
        assert_eq!(hex(&Md5::checksum(b"")), EMPTY_ETAG);
    }
}
