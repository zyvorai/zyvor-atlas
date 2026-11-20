// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Opsgenie Alert API (<https://docs.opsgenie.com/docs/alert-api>) — creates an alert per open
//! Atlas alert (keyed by `alias` = Atlas's alert id, so re-firing the same condition doesn't
//! duplicate) and closes it once the condition clears.

use std::time::Duration;

use anyhow::Result;
use atlas_api_types::AlertRecord;
use serde_json::json;
use sqlx::AnyPool;

use super::OpsgenieConfig;

const SINK: &str = "opsgenie";

fn base_url(cfg: &OpsgenieConfig) -> &'static str {
    if cfg.region.eq_ignore_ascii_case("eu") {
        "https://api.eu.opsgenie.com"
    } else {
        "https://api.opsgenie.com"
    }
}

fn priority(a: &AlertRecord) -> &'static str {
    match a.severity.as_str() {
        "critical" => "P1",
        "warning" => "P3",
        _ => "P5",
    }
}

/// Create alerts for not-yet-notified open alerts and close ones whose Atlas alert has since
/// cleared. Returns `(triggered, resolved)`. Failures are logged and retried next tick.
pub async fn dispatch(pool: &AnyPool, cfg: &OpsgenieConfig) -> Result<(usize, usize)> {
    dispatch_to(pool, cfg, base_url(cfg)).await
}

/// Same as [`dispatch`], but against `base` instead of the real Opsgenie API host — exposed for
/// tests to point at a local mock server.
pub async fn dispatch_to(
    pool: &AnyPool,
    cfg: &OpsgenieConfig,
    base: &str,
) -> Result<(usize, usize)> {
    let client = reqwest::Client::new();
    let mut triggered = 0;
    for a in atlas_inventory::alert_notifications::pending_triggers(pool, SINK).await? {
        match create(&client, base, cfg, &a).await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "trigger")
                    .await?;
                triggered += 1;
            }
            Err(e) => tracing::warn!("opsgenie create failed for {}: {e:#}", a.id),
        }
    }
    let mut resolved = 0;
    for a in atlas_inventory::alert_notifications::pending_resolves(pool, SINK).await? {
        match close(&client, base, cfg, &a).await {
            Ok(()) => {
                atlas_inventory::alert_notifications::mark_sent(pool, &a.id, SINK, "resolve")
                    .await?;
                resolved += 1;
            }
            Err(e) => tracing::warn!("opsgenie close failed for {}: {e:#}", a.id),
        }
    }
    Ok((triggered, resolved))
}

async fn create(
    client: &reqwest::Client,
    base: &str,
    cfg: &OpsgenieConfig,
    a: &AlertRecord,
) -> Result<()> {
    let url = format!("{base}/v2/alerts");
    let body = json!({
        "message": a.title,
        "alias": a.id,
        "description": a.description,
        "priority": priority(a),
        "details": {
            "resource_type": a.resource_type,
            "resource_id": a.resource_id,
            "source": a.source,
        },
    });
    let resp = client
        .post(&url)
        .header("Authorization", format!("GenieKey {}", cfg.api_key))
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("opsgenie create returned HTTP {}", resp.status());
    }
    Ok(())
}

async fn close(
    client: &reqwest::Client,
    base: &str,
    cfg: &OpsgenieConfig,
    a: &AlertRecord,
) -> Result<()> {
    let url = format!("{base}/v2/alerts/{}/close?identifierType=alias", a.id);
    let resp = client
        .post(&url)
        .header("Authorization", format!("GenieKey {}", cfg.api_key))
        .json(&json!({}))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("opsgenie close returned HTTP {}", resp.status());
    }
    Ok(())
}
