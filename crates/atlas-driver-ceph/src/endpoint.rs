// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Connection to a Ceph cluster that is not the gateway host's default `/etc/ceph/ceph.conf`.
//!
//! The driver passes these as argv to `ceph` and `rbd` (`--conf`, `--keyring`, `--mon-host`,
//! `--name`). The keyring file is never read into Atlas and never stored in inventory; only its
//! path is kept, as a reference.

use std::path::{Component, Path};

/// Monitors, config file and keyring path for one external Ceph cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CephEndpoint {
    pub mon_host: Option<String>,
    pub conf_path: Option<String>,
    pub keyring_path: Option<String>,
    /// Ceph entity name, always `client.<id>` when set.
    pub user: Option<String>,
}

impl CephEndpoint {
    /// Build an endpoint from operator input. At least `mon_host` or `conf_path` is required.
    /// Empty strings are treated as unset. This checks the shape of the values, not that the
    /// files exist.
    pub fn new(
        mon_host: Option<String>,
        conf_path: Option<String>,
        keyring_path: Option<String>,
        user: Option<String>,
    ) -> Result<Self, String> {
        let endpoint = Self {
            mon_host: nonempty(mon_host)
                .as_deref()
                .map(normalize_mon_host)
                .transpose()?,
            conf_path: nonempty(conf_path)
                .as_deref()
                .map(normalize_path)
                .transpose()?,
            keyring_path: nonempty(keyring_path)
                .as_deref()
                .map(normalize_path)
                .transpose()?,
            user: nonempty(user).as_deref().map(normalize_user).transpose()?,
        };
        endpoint.validate()?;
        Ok(endpoint)
    }

    pub fn is_configured(&self) -> bool {
        self.mon_host.is_some() || self.conf_path.is_some()
    }

    fn validate(&self) -> Result<(), String> {
        if !self.is_configured() {
            return Err(
                "invalid: external Ceph needs mon_host or conf_path (a keyring path alone cannot find the monitors)"
                    .into(),
            );
        }
        Ok(())
    }

    /// Reject a path that is not a regular file. Does not open it, so the key material stays on disk.
    pub fn validate_readable(&self) -> Result<(), String> {
        for path in [&self.conf_path, &self.keyring_path].into_iter().flatten() {
            if !Path::new(path).is_file() {
                return Err(format!("invalid: ceph file {path} is not a file"));
            }
        }
        Ok(())
    }

    /// Global `ceph`/`rbd` arguments. Empty for the default cluster (`RealCephDriver::new`).
    pub fn cli_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(conf) = &self.conf_path {
            args.push("--conf".into());
            args.push(conf.clone());
        }
        if let Some(keyring) = &self.keyring_path {
            args.push("--keyring".into());
            args.push(keyring.clone());
        }
        if let Some(mon) = &self.mon_host {
            args.push("--mon-host".into());
            args.push(mon.clone());
        }
        if let Some(user) = &self.user {
            args.push("--name".into());
            args.push(user.clone());
        }
        args
    }

    /// Inventory reference. Paths and monitor addresses only — never key material.
    pub fn connection_ref(&self) -> String {
        let mut parts = Vec::new();
        if let Some(mon) = &self.mon_host {
            parts.push(format!("mon_host={mon}"));
        }
        if let Some(conf) = &self.conf_path {
            parts.push(format!("conf={conf}"));
        }
        if let Some(keyring) = &self.keyring_path {
            parts.push(format!("keyring={keyring}"));
        }
        if let Some(user) = &self.user {
            parts.push(format!("name={user}"));
        }
        format!("ceph:{}", parts.join(";"))
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_mon_host(raw: &str) -> Result<String, String> {
    if raw
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, ';' | '\'' | '"' | '$' | '`' | '\\' | '\0'))
    {
        return Err(format!(
            "invalid: mon_host {raw:?} contains whitespace or a shell metacharacter"
        ));
    }
    if raw.len() > 2048 {
        return Err("invalid: mon_host is too long".into());
    }
    let mut depth = 0i32;
    let mut current = String::new();
    let mut tokens = Vec::new();
    for ch in raw.chars() {
        match ch {
            '[' => {
                depth += 1;
                current.push(ch);
            }
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("invalid: mon_host {raw:?} has an unmatched ']'"));
                }
                current.push(ch);
            }
            ',' if depth == 0 => {
                if current.is_empty() {
                    return Err("invalid: mon_host has an empty monitor address".into());
                }
                tokens.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if depth != 0 {
        return Err(format!("invalid: mon_host {raw:?} has an unmatched '['"));
    }
    if current.is_empty() {
        return Err("invalid: mon_host has an empty monitor address".into());
    }
    tokens.push(current);
    if tokens.iter().any(|token| !mon_token_ok(token)) {
        return Err(format!(
            "invalid: mon_host {raw:?} must be a comma-separated list of host, host:port, v1:/v2: addresses, or [ipv6]"
        ));
    }
    Ok(tokens.join(","))
}

fn mon_token_ok(token: &str) -> bool {
    let rest = token
        .strip_prefix("v1:")
        .or_else(|| token.strip_prefix("v2:"))
        .unwrap_or(token);
    if let Some(inner) = rest.strip_prefix('[') {
        let Some((addr, after)) = inner.split_once(']') else {
            return false;
        };
        if addr.is_empty()
            || !addr
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return false;
        }
        return after.is_empty() || port_suffix_ok(after);
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            host_ok(host) && port_ok(port)
        }
        _ => host_ok(rest),
    }
}

fn port_suffix_ok(after: &str) -> bool {
    after.strip_prefix(':').is_some_and(port_ok)
}

fn port_ok(port: &str) -> bool {
    port.parse::<u16>().ok().is_some_and(|n| n > 0)
}

fn host_ok(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let ipv4 = host.split('.').collect::<Vec<_>>();
    if ipv4.len() == 4
        && ipv4
            .iter()
            .all(|octet| octet.parse::<u8>().is_ok() && !octet.is_empty())
    {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn normalize_path(raw: &str) -> Result<String, String> {
    if raw.contains('\0') || raw.contains('\n') || raw.len() > 4096 {
        return Err(format!("invalid: ceph path {raw:?} is empty or unsafe"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!("invalid: ceph path {raw:?} must be absolute"));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("invalid: ceph path {raw:?} must not contain '..'"));
    }
    Ok(raw.to_string())
}

fn normalize_user(raw: &str) -> Result<String, String> {
    let name = raw.strip_prefix("client.").unwrap_or(raw);
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !ok {
        return Err(format!("invalid: ceph user {raw:?} must be a client name"));
    }
    Ok(format!("client.{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_args_are_flags_and_the_reference_omits_key_material() {
        let endpoint = CephEndpoint::new(
            Some("10.1.0.1:6789,v2:10.1.0.2:3300".into()),
            Some("/etc/ceph/external.conf".into()),
            Some("/etc/ceph/ceph.client.admin.keyring".into()),
            Some("admin".into()),
        )
        .unwrap();
        assert_eq!(
            endpoint.cli_args(),
            vec![
                "--conf",
                "/etc/ceph/external.conf",
                "--keyring",
                "/etc/ceph/ceph.client.admin.keyring",
                "--mon-host",
                "10.1.0.1:6789,v2:10.1.0.2:3300",
                "--name",
                "client.admin",
            ]
        );
        let reference = endpoint.connection_ref();
        assert!(reference.starts_with("ceph:"));
        assert!(reference.contains("mon_host=10.1.0.1:6789"));
        assert!(reference.contains("keyring=/etc/ceph/ceph.client.admin.keyring"));
        assert!(!reference.contains("AQ"));
        assert_eq!(
            CephEndpoint::new(None, None, Some("/etc/ceph/keyring".into()), None).unwrap_err(),
            "invalid: external Ceph needs mon_host or conf_path (a keyring path alone cannot find the monitors)"
        );
    }

    #[test]
    fn rejects_unsafe_monitors_paths_and_users() {
        assert!(CephEndpoint::new(Some("10.0.0.1;id".into()), None, None, None).is_err());
        assert!(CephEndpoint::new(Some("10.0.0.1 6789".into()), None, None, None).is_err());
        assert!(CephEndpoint::new(Some("[2001:db8::1]:6789".into()), None, None, None).is_ok());
        assert!(CephEndpoint::new(None, Some("ceph.conf".into()), None, None).is_err());
        assert!(
            CephEndpoint::new(None, Some("/etc/ceph/../ceph.conf".into()), None, None).is_err()
        );
        assert!(CephEndpoint::new(
            Some("mon.example:6789".into()),
            None,
            None,
            Some("client admin".into())
        )
        .is_err());
        let named = CephEndpoint::new(
            Some("mon.example".into()),
            None,
            None,
            Some("client.atlas".into()),
        )
        .unwrap();
        assert_eq!(named.user.as_deref(), Some("client.atlas"));
        assert!(CephEndpoint::default().cli_args().is_empty());
    }
}
