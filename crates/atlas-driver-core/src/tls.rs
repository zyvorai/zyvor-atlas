// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! A `reqwest::Client` that additionally trusts a CA certificate from `ATLAS_S3_CA_CERT`, for
//! talking to an S3-compatible endpoint (Ceph RGW, MinIO, Garage, ...) whose TLS certificate was
//! not issued by a CA already in the system trust store — a private CA, or a self-signed lab cert.
//! The system/OS root store stays trusted regardless (reqwest's own default); this only adds one
//! more root, it never removes the others and never disables verification. Used by
//! `atlas-driver-rgw` (`S3Target`) so every S3 client speaks the same TLS trust policy. The Helm
//! chart sets this variable from `s3.caSecretName`.
use std::time::Duration;

use rustls_pki_types::{pem::PemObject, CertificateDer};

use crate::DriverError;

pub fn trusted_http_client(timeout: Option<Duration>) -> Result<reqwest::Client, DriverError> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    if let Ok(path) = std::env::var("ATLAS_S3_CA_CERT") {
        if !path.trim().is_empty() {
            let pem = std::fs::read(&path)
                .map_err(|e| DriverError::Backend(format!("ATLAS_S3_CA_CERT {path}: {e}")))?;
            // reqwest's own `Certificate::from_pem` defers actual PEM parsing to `.build()` time
            // with the rustls backend (it just wraps the raw bytes), and its parser silently treats
            // content with no `-----BEGIN CERTIFICATE-----` blocks at all as "zero certificates
            // found" rather than an error — so a garbage/misconfigured file would otherwise be
            // ignored with no error at startup. Validate eagerly here instead: reject both a parse
            // failure of a recognized block and a file with no certificate blocks at all.
            let mut certs_found = 0usize;
            for result in CertificateDer::pem_slice_iter(&pem) {
                result.map_err(|e| {
                    DriverError::Backend(format!(
                        "ATLAS_S3_CA_CERT {path} is not a valid PEM certificate: {e}"
                    ))
                })?;
                certs_found += 1;
            }
            if certs_found == 0 {
                return Err(DriverError::Backend(format!(
                    "ATLAS_S3_CA_CERT {path} contains no PEM certificate blocks"
                )));
            }
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| {
                DriverError::Backend(format!(
                    "ATLAS_S3_CA_CERT {path} is not a valid PEM certificate: {e}"
                ))
            })?;
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| DriverError::Backend(format!("http client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // cargo test runs these in parallel threads of the same process, and all three mutate the same
    // process-global ATLAS_S3_CA_CERT — without this lock one test's remove_var could race
    // another's set_var. (invalid_pem_errors_not_panics also failed in CI on 2026-09-28, but that
    // turned out to be a real, fully deterministic bug in trusted_http_client itself — reqwest's
    // Certificate::from_pem defers parsing to build()-time and silently accepts zero-certificate
    // input — now fixed above with an eager rustls_pki_types check; this lock guards a separate,
    // latent hazard, not what caused that failure.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn no_env_var_builds_a_plain_client() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::remove_var("ATLAS_S3_CA_CERT") };
        assert!(trusted_http_client(Some(Duration::from_secs(1))).is_ok());
    }

    #[test]
    fn missing_ca_cert_file_errors_not_panics() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::set_var("ATLAS_S3_CA_CERT", "/nonexistent/path/ca.pem") };
        let result = trusted_http_client(None);
        unsafe { std::env::remove_var("ATLAS_S3_CA_CERT") };
        assert!(result.is_err());
    }

    #[test]
    fn invalid_pem_errors_not_panics() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("atlas-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.pem");
        std::fs::write(&path, b"not a certificate").unwrap();
        // SAFETY: test-only env mutation; serialized by ENV_LOCK.
        unsafe { std::env::set_var("ATLAS_S3_CA_CERT", path.to_str().unwrap()) };
        let result = trusted_http_client(None);
        unsafe { std::env::remove_var("ATLAS_S3_CA_CERT") };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.is_err());
    }
}
