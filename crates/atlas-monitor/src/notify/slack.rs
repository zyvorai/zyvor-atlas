// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Native Slack alerting via an incoming webhook URL. Independent from the generic `webhook` sink
//! (which also happens to be Slack-payload-compatible) so a deployment can run both at once — a
//! generic webhook feeding a SIEM/automation pipeline, and this one posting human-readable
//! trigger/resolve messages to an operator-facing channel.

use std::time::Duration;

use anyhow::Result;
use atlas_api_types::AlertRecord;
use serde_json::json;
use sqlx::AnyPool;

const SINK: &str = "slack";

fn emoji(a: &AlertRecord) -> &'static str {
    match a.severity.as_str() {
        "critical" => "🔴",
        "warning" => "🟠",
        _ => "🔵",
    }
}

/// Post trigger messages for not-yet-notified open alerts and resolve messages for ones whose
/// Atlas alert has since cleared. Returns `(triggered, resolved)`.
pub async fn dispatch(pool: &AnyPool, webhook_url: &str) -> Result<(usize, usize)> {
    let client = reqwest::Client::new();
    let mut triggered = 0;
    for a in atlas_inventory::alert_notifications::pending_triggers(pool, SINK).await? {
        match send(&client, webhook_url, &a, false).await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "trigger")
                    .await?;
                triggered += 1;
            }
            Err(e) => tracing::warn!("slack trigger post failed for {}: {e:#}", a.id),
        }
    }
    let mut resolved = 0;
    for a in atlas_inventory::alert_notifications::pending_resolves(pool, SINK).await? {
        match send(&client, webhook_url, &a, true).await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "resolve")
                    .await?;
                resolved += 1;
            }
            Err(e) => tracing::warn!("slack resolve post failed for {}: {e:#}", a.id),
        }
    }
    Ok((triggered, resolved))
}

async fn send(
    client: &reqwest::Client,
    url: &str,
    a: &AlertRecord,
    resolved: bool,
) -> Result<()> {
    let text = if resolved {
        format!(
            "✅ *[resolved]* {} ({}/{})",
            a.title, a.resource_type, a.resource_id
        )
    } else {
        format!(
            "{} *[{}]* {} — {} ({}/{})",
            emoji(a),
            a.severity,
            a.title,
            a.description,
            a.resource_type,
            a.resource_id
        )
    };
    let resp = client
        .post(url)
        .json(&json!({ "text": text }))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("slack webhook returned HTTP {}", resp.status());
    }
    Ok(())
}
