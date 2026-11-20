// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Alert notification sinks. `webhook` is the original generic (Slack-payload-shaped) single-URL
//! notifier, tracked via `storage_alerts.notified_at`. `pagerduty`/`opsgenie`/`slack` are native
//! integrations that fan out independently — a deployment can run several at once (e.g. PagerDuty
//! for paging plus a human-readable Slack channel) — tracked per-sink via the
//! `alert_notifications` table (`migrations/0030_alert_notifications.sql`) rather than
//! `notified_at`, since that column was already spoken for by `webhook` and only tracks one sink.

pub mod opsgenie;
pub mod pagerduty;
pub mod slack;
pub mod webhook;

pub use webhook::dispatch;

/// PagerDuty Events API v2 configuration (<https://developer.pagerduty.com/docs/events-api-v2/overview/>).
#[derive(Clone)]
pub struct PagerDutyConfig {
    pub routing_key: String,
}

/// Opsgenie Alert API configuration (<https://docs.opsgenie.com/docs/alert-api>).
#[derive(Clone)]
pub struct OpsgenieConfig {
    pub api_key: String,
    /// "us" (default) or "eu" — Opsgenie's EU tenants are served from a separate API host.
    pub region: String,
}
