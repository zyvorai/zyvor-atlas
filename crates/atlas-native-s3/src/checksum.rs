// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `x-amz-checksum-*` verification for uploads: the algorithms AWS clients send (CRC32, CRC32C,
//! CRC64NVME, SHA-1, SHA-256), from headers or from aws-chunked trailers.

use s3s::{checksum::ChecksumHasher, dto::Checksum, s3_error, S3Result, TrailingHeaders};

/// A hasher for every checksum the request carries, plus `algorithm` (the client's
/// `x-amz-checksum-algorithm`, whose value may only arrive in a trailer).
pub fn hasher(expected: &Checksum, algorithm: Option<&str>) -> S3Result<ChecksumHasher> {
    let mut h = ChecksumHasher::default();
    if expected.checksum_crc32.is_some() {
        h.crc32 = Some(Default::default());
    }
    if expected.checksum_crc32c.is_some() {
        h.crc32c = Some(Default::default());
    }
    if expected.checksum_crc64nvme.is_some() {
        h.crc64nvme = Some(Default::default());
    }
    if expected.checksum_sha1.is_some() {
        h.sha1 = Some(Default::default());
    }
    if expected.checksum_sha256.is_some() {
        h.sha256 = Some(Default::default());
    }
    match algorithm {
        None => {}
        Some("CRC32") => h.crc32 = Some(Default::default()),
        Some("CRC32C") => h.crc32c = Some(Default::default()),
        Some("CRC64NVME") => h.crc64nvme = Some(Default::default()),
        Some("SHA1") => h.sha1 = Some(Default::default()),
        Some("SHA256") => h.sha256 = Some(Default::default()),
        Some(other) => {
            return Err(s3_error!(
                NotImplemented,
                "checksum algorithm {other} is not supported"
            ))
        }
    }
    Ok(h)
}

/// Adds checksums sent as trailers (after the body) to `expected`.
pub fn add_trailers(expected: &mut Checksum, trailers: Option<&TrailingHeaders>) -> S3Result<()> {
    let Some(map) = trailers.and_then(TrailingHeaders::take) else {
        return Ok(());
    };
    for (name, field) in [
        ("x-amz-checksum-crc32", &mut expected.checksum_crc32),
        ("x-amz-checksum-crc32c", &mut expected.checksum_crc32c),
        ("x-amz-checksum-crc64nvme", &mut expected.checksum_crc64nvme),
        ("x-amz-checksum-sha1", &mut expected.checksum_sha1),
        ("x-amz-checksum-sha256", &mut expected.checksum_sha256),
    ] {
        if let Some(v) = map.get(name) {
            let v = v
                .to_str()
                .map_err(|_| s3_error!(InvalidArgument, "{name} is not text"))?;
            *field = Some(v.to_owned());
        }
    }
    Ok(())
}

/// Fails with `BadDigest` if any checksum the client sent differs from the body's.
pub fn verify(actual: &Checksum, expected: &Checksum) -> S3Result<()> {
    for (name, a, e) in [
        ("CRC32", &actual.checksum_crc32, &expected.checksum_crc32),
        ("CRC32C", &actual.checksum_crc32c, &expected.checksum_crc32c),
        (
            "CRC64NVME",
            &actual.checksum_crc64nvme,
            &expected.checksum_crc64nvme,
        ),
        ("SHA1", &actual.checksum_sha1, &expected.checksum_sha1),
        ("SHA256", &actual.checksum_sha256, &expected.checksum_sha256),
    ] {
        if e.is_some() && a != e {
            return Err(s3_error!(
                BadDigest,
                "the {name} checksum does not match the body"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_what_the_client_sent() {
        let mut h = hasher(&Checksum::default(), Some("CRC32")).unwrap();
        h.update(b"hello");
        let actual = h.finalize();
        let crc = actual.checksum_crc32.clone().unwrap();
        let ok = Checksum {
            checksum_crc32: Some(crc),
            ..Default::default()
        };
        verify(&actual, &ok).unwrap();
        let bad = Checksum {
            checksum_crc32: Some("AAAAAA==".into()),
            ..Default::default()
        };
        assert!(verify(&actual, &bad).is_err());
        verify(&actual, &Checksum::default()).unwrap();
        assert!(hasher(&Checksum::default(), Some("BOGUS")).is_err());
    }
}
