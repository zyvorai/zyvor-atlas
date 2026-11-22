// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The gateway's bucket list (`buckets.json`) and access keys (`credentials.json`, from a
//! Secret). Secret keys never appear in errors, logs or `Debug` output.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
};

use serde::Deserialize;

use crate::service::Grant;

/// One bucket: a filesystem, or a snapshot of one (always read-only).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketSpec {
    pub bucket: String,
    pub fs: String,
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

impl BucketSpec {
    /// The filesystem as the node API names it: `<fs>` or `<fs>@<snapshot>`.
    pub fn target(&self) -> String {
        match &self.snapshot {
            Some(s) => format!("{}@{s}", self.fs),
            None => self.fs.clone(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSpec {
    pub access_key: String,
    pub secret_key: String,
    /// Buckets this key may use; all of them when absent.
    #[serde(default)]
    pub buckets: Option<Vec<String>>,
    #[serde(default)]
    pub read_only: bool,
}

impl fmt::Debug for CredentialSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialSpec")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("buckets", &self.buckets)
            .field("read_only", &self.read_only)
            .finish()
    }
}

fn id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn validate_buckets(buckets: &[BucketSpec]) -> Result<(), String> {
    if buckets.is_empty() {
        return Err("at least one bucket is required".into());
    }
    let mut names = BTreeSet::new();
    for b in buckets {
        if !s3s::path::check_bucket_name(&b.bucket) {
            return Err(format!(
                "bucket {:?}: not a valid S3 bucket name (3-63 lowercase letters, digits, `.` and `-`)",
                b.bucket
            ));
        }
        if !id_ok(&b.fs) {
            return Err(format!(
                "bucket {}: filesystem id {:?} is not valid",
                b.bucket, b.fs
            ));
        }
        if b.snapshot.as_deref().is_some_and(|s| !id_ok(s)) {
            return Err(format!("bucket {}: snapshot name is not valid", b.bucket));
        }
        if !names.insert(b.bucket.as_str()) {
            return Err(format!("bucket {} is listed twice", b.bucket));
        }
    }
    Ok(())
}

/// Checks the access keys and turns them into grants (access key → grant).
pub fn grants(
    creds: &[CredentialSpec],
    buckets: &[BucketSpec],
) -> Result<HashMap<String, Grant>, String> {
    if creds.is_empty() {
        return Err("at least one access key is required".into());
    }
    let known: BTreeSet<&str> = buckets.iter().map(|b| b.bucket.as_str()).collect();
    let mut out = HashMap::new();
    for (i, c) in creds.iter().enumerate() {
        let ak = &c.access_key;
        if ak.len() < 3
            || ak.len() > 128
            || !ak
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(format!(
                "credential {i}: access keys are 3-128 letters, digits, `.`, `_` or `-`"
            ));
        }
        if c.secret_key.len() < 16 || c.secret_key.len() > 256 {
            return Err(format!("credential {ak}: secret keys are 16-256 bytes"));
        }
        if let Some(unknown) = c
            .buckets
            .iter()
            .flatten()
            .find(|b| !known.contains(b.as_str()))
        {
            return Err(format!(
                "credential {ak}: bucket {unknown} is not configured"
            ));
        }
        let grant = Grant {
            buckets: c.buckets.as_ref().map(|b| b.iter().cloned().collect()),
            read_only: c.read_only,
        };
        if out.insert(ak.clone(), grant).is_some() {
            return Err(format!("access key {ak} is listed twice"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(name: &str, fs: &str) -> BucketSpec {
        BucketSpec {
            bucket: name.into(),
            fs: fs.into(),
            snapshot: None,
            read_only: false,
        }
    }

    #[test]
    fn validates_buckets() {
        validate_buckets(&[
            bucket("team-share", "team-share"),
            bucket("logs", "fs.logs_1"),
        ])
        .unwrap();
        assert!(validate_buckets(&[]).is_err());
        assert!(validate_buckets(&[bucket("Upper", "fs")]).is_err());
        assert!(validate_buckets(&[bucket("ok-name", "../etc")]).is_err());
        assert!(validate_buckets(&[bucket("dup", "a"), bucket("dup", "b")]).is_err());
        let mut snap = bucket("snap", "fs");
        snap.snapshot = Some("bad/name".into());
        assert!(validate_buckets(&[snap]).is_err());
        let mut snap = bucket("snap", "fs");
        snap.snapshot = Some("nightly".into());
        assert_eq!(snap.target(), "fs@nightly");
    }

    #[test]
    fn checks_credentials_without_revealing_secrets() {
        let buckets = [bucket("a", "fs-a"), bucket("bbb", "fs-b")];
        let parse = |j: &str| serde_json::from_str::<Vec<CredentialSpec>>(j).unwrap();
        let ok = parse(
            r#"[{"access_key":"reader","secret_key":"0123456789abcdef","buckets":["bbb"],"read_only":true},
                {"access_key":"admin","secret_key":"fedcba9876543210"}]"#,
        );
        let g = grants(&ok, &buckets).unwrap();
        assert!(g["reader"].read_only);
        assert!(g["admin"].buckets.is_none());
        let secret = "short-secret";
        let err = grants(
            &parse(&format!(
                r#"[{{"access_key":"k1x","secret_key":"{secret}"}}]"#
            )),
            &buckets,
        )
        .unwrap_err();
        assert!(!err.contains(secret), "{err}");
        assert!(grants(
            &parse(r#"[{"access_key":"k1x","secret_key":"0123456789abcdef","buckets":["nope"]}]"#),
            &buckets
        )
        .is_err());
        assert!(!format!("{:?}", ok[0]).contains("0123456789abcdef"));
        assert!(serde_json::from_str::<Vec<CredentialSpec>>(
            r#"[{"access_key":"a","secret_key":"b","extra":1}]"#
        )
        .is_err());
    }
}
