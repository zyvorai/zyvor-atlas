// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Oracle connect strings. Always compiled: the OCI driver is behind the `oracle` feature, but
//! the descriptor is pure text and is what `OracleSourceConnector` dials when that feature is on.
//!
//! `tcps`, `require`, `verify-ca` and `verify-full` select the TCPS protocol. Wallet location and
//! DN matching stay in the client's `sqlnet.ora`; this string only names the protocol, host, port
//! and service. An unknown mode does not fall back to plaintext.

use anyhow::{anyhow, Result};

/// Easy Connect for plaintext, or a DESCRIPTION with `PROTOCOL=TCPS`.
pub fn connect_string(host: &str, port: i64, service: &str, tls_mode: &str) -> Result<String> {
    validate_token("host", host)?;
    validate_token("service", service)?;
    if !(1..=65535).contains(&port) {
        return Err(anyhow!("oracle port {port} is outside 1..=65535"));
    }
    if uses_tcps(tls_mode) {
        Ok(format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST={host})(PORT={port}))(CONNECT_DATA=(SERVICE_NAME={service})))"
        ))
    } else {
        Ok(format!("//{host}:{port}/{service}"))
    }
}

fn uses_tcps(tls_mode: &str) -> bool {
    !matches!(
        tls_mode.trim().to_ascii_lowercase().as_str(),
        "" | "disable" | "off" | "plaintext" | "allow"
    )
}

fn validate_token(what: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.bytes().any(|b| {
            b.is_ascii_whitespace() || matches!(b, b'(' | b')' | b'=' | b'/' | b'\\' | b'\0')
        })
    {
        return Err(anyhow!("oracle {what} is empty or not a single token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_stays_easy_connect() {
        let s = connect_string("db.example", 1521, "FREEPDB1", "disable").unwrap();
        assert_eq!(s, "//db.example:1521/FREEPDB1");
        assert!(!s.contains("TCPS"));
    }

    #[test]
    fn tcps_modes_build_a_description_and_unknown_does_not_go_plaintext() {
        for mode in ["tcps", "require", "verify-full", "verify-ca", "TCPS", "something-else"] {
            let s = connect_string("db.example", 2484, "FREEPDB1", mode).unwrap();
            assert!(s.contains("PROTOCOL=TCPS"), "{mode}: {s}");
            assert!(s.contains("HOST=db.example") && s.contains("SERVICE_NAME=FREEPDB1"), "{s}");
        }
    }

    #[test]
    fn hostile_host_is_refused() {
        assert!(connect_string("db.example)(PORT=1", 1521, "svc", "tcps").is_err());
        assert!(connect_string("db.example", 0, "svc", "disable").is_err());
    }
}
