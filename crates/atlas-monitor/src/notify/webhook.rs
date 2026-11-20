// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Alert webhook notifier. Pushes each open alert to a configured HTTP endpoint exactly once per
//! firing (tracked via `storage_alerts.notified_at`), re-firing when a resolved alert re-opens.
//! The payload is Slack-compatible (`text`) with a structured `alert` object for generic sinks.

use anyhow::Result;
use atlas_api_types::AlertRecord;
use serde_json::json;
use sqlx::AnyPool;

/// Post any not-yet-notified open alerts to `url`, marking each notified on success.
/// Returns the number successfully delivered. Failures are logged and retried next tick.
pub async fn dispatch(pool: &AnyPool, url: &str) -> Result<usize> {
    let pending = atlas_inventory::alerts::list_unnotified_open(pool).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let client = reqwest::Client::new();
    let mut sent = 0;
    for a in pending {
        match post(&client, url, &a).await {
            Ok(()) => {
                atlas_inventory::alerts::mark_notified(pool, &a.id).await?;
                sent += 1;
            }
            Err(e) => tracing::warn!("alert webhook post failed for {}: {e:#}", a.id),
        }
    }
    Ok(sent)
}

async fn post(client: &reqwest::Client, url: &str, a: &AlertRecord) -> Result<()> {
    let emoji = match a.severity.as_str() {
        "critical" => "🔴",
        "warning" => "🟠",
        _ => "🔵",
    };
    let text = format!(
        "{emoji} *[{}]* {} — {} ({}/{})",
        a.severity, a.title, a.description, a.resource_type, a.resource_id
    );
    let body = json!({
        "text": text,
        "alert": {
            "id": a.id,
            "severity": a.severity,
            "title": a.title,
            "description": a.description,
            "resource_type": a.resource_type,
            "resource_id": a.resource_id,
            "source": a.source,
            "created_at": a.created_at,
        }
    });
    let resp = client
        .post(url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("webhook returned HTTP {}", resp.status());
    }
    Ok(())
}
