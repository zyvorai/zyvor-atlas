// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Object tags: AWS's limits, the `x-amz-tagging` header, and where they are stored.
//!
//! Tags are one extended attribute on the object's file, apart from the one holding its ETag and
//! headers: changing tags never changes an object's ETag or fails a conditional write.

use std::collections::BTreeSet;

use crate::store::StoreError;

/// On an object (and a multipart upload's state): its tags, a JSON list of `[key, value]`.
pub(crate) const TAGS_XATTR: &str = "user.atlas.s3.tags";
pub const MAX_TAGS: usize = 10;
pub const MAX_TAG_KEY_CHARS: usize = 128;
pub const MAX_TAG_VALUE_CHARS: usize = 256;

/// An object's tags, in the order they were given.
pub type Tags = Vec<(String, String)>;

fn invalid(why: impl Into<String>) -> StoreError {
    StoreError::InvalidTag(why.into())
}

/// Letters, numbers, spaces and `_ . : / = + - @`.
fn allowed(c: char) -> bool {
    c.is_alphanumeric() || (c.is_whitespace() && !c.is_control()) || "_.:/=+-@".contains(c)
}

/// At most [`MAX_TAGS`] tags with distinct keys of 1 to 128 characters and values of at most 256,
/// in AWS's character set; keys starting with `aws:` are reserved.
pub fn validate(tags: &[(String, String)]) -> Result<(), StoreError> {
    if tags.len() > MAX_TAGS {
        return Err(invalid(format!(
            "an object can have at most {MAX_TAGS} tags"
        )));
    }
    let mut keys = BTreeSet::new();
    for (k, v) in tags {
        let len = k.chars().count();
        if len == 0 || len > MAX_TAG_KEY_CHARS {
            return Err(invalid(format!(
                "a tag key has 1 to {MAX_TAG_KEY_CHARS} characters"
            )));
        }
        if v.chars().count() > MAX_TAG_VALUE_CHARS {
            return Err(invalid(format!(
                "a tag value has at most {MAX_TAG_VALUE_CHARS} characters"
            )));
        }
        if k.starts_with("aws:") {
            return Err(invalid("tag keys starting with `aws:` are reserved"));
        }
        if !k.chars().chain(v.chars()).all(allowed) {
            return Err(invalid(
                "tags may hold letters, numbers, spaces and _ . : / = + - @",
            ));
        }
        if !keys.insert(k.as_str()) {
            return Err(invalid("tag keys must be unique"));
        }
    }
    Ok(())
}

/// Decodes one `application/x-www-form-urlencoded` component.
fn form_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'+' => b' ',
            b'%' => {
                let digit = |c: Option<u8>| char::from(c?).to_digit(16);
                (digit(bytes.next())? * 16 + digit(bytes.next())?) as u8
            }
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

/// The tags of an `x-amz-tagging` header (`key=value&...`, URL encoded), validated.
pub fn parse_header(header: &str) -> Result<Tags, StoreError> {
    let tags = header
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            form_decode(k)
                .zip(form_decode(v))
                .ok_or_else(|| invalid("x-amz-tagging is not URL encoded UTF-8"))
        })
        .collect::<Result<Tags, _>>()?;
    validate(&tags)?;
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[(&str, &str)]) -> Tags {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_the_header() {
        assert_eq!(parse_header("").unwrap(), tags(&[]));
        assert_eq!(
            parse_header("a=1&b=&c&d+e=f%2Bg&%C3%A9=%3D").unwrap(),
            tags(&[("a", "1"), ("b", ""), ("c", ""), ("d e", "f+g"), ("é", "=")])
        );
        for bad in ["a=%", "a=%2", "a=%zz", "a=%ff", "=x", "a=1&a=2", "a=%00"] {
            assert!(
                matches!(parse_header(bad), Err(StoreError::InvalidTag(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn enforces_aws_limits() {
        let n = |n: usize| -> Tags { (0..n).map(|i| (format!("k{i}"), "v".into())).collect() };
        assert!(validate(&n(MAX_TAGS)).is_ok());
        assert!(validate(&n(MAX_TAGS + 1)).is_err());
        // Limits count characters, not bytes.
        let (k, v) = ("é".repeat(128), "é".repeat(256));
        assert!(validate(&tags(&[(&k, &v)])).is_ok());
        assert!(validate(&tags(&[(&format!("{k}x"), "")])).is_err());
        assert!(validate(&tags(&[("k", &format!("{v}x"))])).is_err());
        assert!(validate(&tags(&[("Az 09_.:/=+-@", "日本 ½")])).is_ok());
        for bad in ["aws:x", "a*b", "a\tb", "a\nb", "a,b", ""] {
            assert!(validate(&tags(&[(bad, "v")])).is_err(), "{bad:?}");
        }
        assert!(validate(&tags(&[("k", "<")])).is_err());
        // Keys are case sensitive.
        assert!(validate(&tags(&[("a", "1"), ("A", "2")])).is_ok());
    }
}
