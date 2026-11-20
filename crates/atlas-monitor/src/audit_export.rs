// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Audit-log export to an external sink (SIEM webhook / generic HTTP collector) before retention
//! pruning deletes rows. Without this, `storage_audit_logs` retention is silent data loss with no
//! external record — exactly what a compliance review (e.g. APRA CPS 234) flags first. Mirrors
//! `notify::dispatch`'s webhook pattern.

use anyhow::Result;
use sqlx::AnyPool;

/// Export audit rows older than `keep_days` to `url` as one batched JSON POST, then delete only
/// the rows that were actually exported. If the sink is unreachable, nothing is deleted — rows
/// stay in SQLite and are retried on the next tick, so a SIEM outage can never silently lose
/// audit history the way an unconditional `prune()` would.
pub async fn export_and_prune(pool: &AnyPool, keep_days: i64, url: &str) -> Result<u64> {
    // Audit rows are compliance-sensitive (APRA CPS 234, see module docs) — refuse to ship them
    // over plaintext HTTP even if an operator misconfigures ATLAS_AUDIT_EXPORT_URL. Nothing is
    // pruned when this check fails, same fail-closed behavior as an unreachable sink below.
    // Loopback is exempt: that traffic never leaves the machine, so there's no third party to
    // intercept it — this is what lets the in-process mock-sink tests use a plain-HTTP listener.
    if !url.starts_with("https://") && !is_loopback_url(url) {
        anyhow::bail!(
            "ATLAS_AUDIT_EXPORT_URL must use https:// (compliance-sensitive audit data); got: {url}"
        );
    }
    let rows = atlas_inventory::audit::rows_older_than(pool, keep_days).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .json(&serde_json::json!({ "audit_logs": rows }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("audit export webhook returned HTTP {}", resp.status());
    }
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .collect();
    atlas_inventory::audit::delete_ids(pool, &ids).await
}

/// True when `url`'s host is `127.0.0.1`, `::1`, or `localhost` — used to exempt loopback-only
/// destinations from the HTTPS requirement above (test mock servers, not real sinks).
fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(h) => {
            // Url::host_str() keeps the brackets on an IPv6 literal (e.g. "[::1]").
            let bare = h
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(h);
            bare == "localhost"
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_url("http://127.0.0.1:8080/"));
        assert!(is_loopback_url("http://localhost/"));
        assert!(is_loopback_url("http://[::1]:9000/"));
        assert!(!is_loopback_url("http://212.8.248.187:30557/"));
        assert!(!is_loopback_url("http://example.com/"));
        assert!(!is_loopback_url("not a url"));
    }
}
