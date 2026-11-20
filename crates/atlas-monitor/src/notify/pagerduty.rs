// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! PagerDuty Events API v2 — triggers an incident per open alert and auto-resolves it once the
//! underlying condition clears, so PagerDuty's own incident state tracks Atlas's rather than
//! needing an operator to close it there by hand too.

use std::time::Duration;

use anyhow::Result;
use atlas_api_types::AlertRecord;
use serde_json::json;
use sqlx::AnyPool;

use super::PagerDutyConfig;

const ENQUEUE_URL: &str = "https://events.pagerduty.com/v2/enqueue";
const SINK: &str = "pagerduty";

fn severity(a: &AlertRecord) -> &'static str {
    match a.severity.as_str() {
        "critical" => "critical",
        "warning" => "warning",
        _ => "info",
    }
}

/// Trigger incidents for not-yet-notified open alerts and resolve ones whose Atlas alert has
/// since cleared. Returns `(triggered, resolved)`. Failures are logged and retried next tick.
pub async fn dispatch(pool: &AnyPool, cfg: &PagerDutyConfig) -> Result<(usize, usize)> {
    dispatch_to(pool, cfg, ENQUEUE_URL).await
}

/// Same as [`dispatch`], but posting to `enqueue_url` instead of PagerDuty's real endpoint —
/// exposed for tests to point at a local mock server.
pub async fn dispatch_to(
    pool: &AnyPool,
    cfg: &PagerDutyConfig,
    enqueue_url: &str,
) -> Result<(usize, usize)> {
    let client = reqwest::Client::new();
    let mut triggered = 0;
    for a in atlas_inventory::alert_notifications::pending_triggers(pool, SINK).await? {
        match send(&client, enqueue_url, cfg, &a, "trigger").await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "trigger")
                    .await?;
                triggered += 1;
            }
            Err(e) => tracing::warn!("pagerduty trigger failed for {}: {e:#}", a.id),
        }
    }
    let mut resolved = 0;
    for a in atlas_inventory::alert_notifications::pending_resolves(pool, SINK).await? {
        match send(&client, enqueue_url, cfg, &a, "resolve").await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "resolve")
                    .await?;
                resolved += 1;
            }
            Err(e) => tracing::warn!("pagerduty resolve failed for {}: {e:#}", a.id),
        }
    }
    Ok((triggered, resolved))
}

async fn send(
    client: &reqwest::Client,
    enqueue_url: &str,
    cfg: &PagerDutyConfig,
    a: &AlertRecord,
    action: &str,
) -> Result<()> {
    // `payload` is only meaningful (and only required) for `trigger`; PagerDuty ignores it for
    // `resolve`/`acknowledge`, so omit it there rather than resending stale alert details.
    let mut body = json!({
        "routing_key": cfg.routing_key,
        "event_action": action,
        "dedup_key": a.id,
    });
    if action == "trigger" {
        body["payload"] = json!({
            "summary": format!("{} — {}", a.title, a.description),
            "source": "atlas",
            "severity": severity(a),
            "custom_details": {
                "resource_type": a.resource_type,
                "resource_id": a.resource_id,
                "source": a.source,
            },
        });
    }
    let resp = client
        .post(enqueue_url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("pagerduty returned HTTP {}", resp.status());
    }
    Ok(())
}
