<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Native alerting integrations

The monitor worker (`crates/atlas-monitor`) evaluates alert rules every `ATLAS_MONITOR_INTERVAL_SECS`
and can push each one to any combination of these sinks — they're independent, not a single
either/or choice, so e.g. PagerDuty for paging and a human-readable Slack channel can both be on
at once.

| Sink | Env var(s) | Notes |
|---|---|---|
| Generic webhook | `ATLAS_ALERT_WEBHOOK_URL` | Slack-payload-shaped JSON POST; pre-existing, unchanged. Good for SIEM/automation pipelines. |
| PagerDuty | `ATLAS_PAGERDUTY_ROUTING_KEY` | Events API v2. Triggers an incident per alert (deduped by Atlas's alert id) and auto-resolves it when the condition clears. |
| Opsgenie | `ATLAS_OPSGENIE_API_KEY`, `ATLAS_OPSGENIE_REGION` (`us` default or `eu`) | Alert API. Creates an alert (aliased to Atlas's alert id) and closes it on resolution. |
| Slack (native) | `ATLAS_SLACK_WEBHOOK_URL` | A Slack incoming webhook, posting a human-readable trigger message and a follow-up "resolved" message — distinct from the generic webhook above so both can run simultaneously. |

## Delivery tracking

The generic webhook keeps its original single-sink tracking (`storage_alerts.notified_at`).
PagerDuty/Opsgenie/Slack are tracked per-sink in the `alert_notifications` table
(`migrations/0030_alert_notifications.sql`), so each sink independently knows which alerts it has
already triggered and resolved — enabling/disabling a sink after alerts have already fired won't
cause a flood of stale notifications or miss the resolve event for alerts it never saw trigger.

## Setting these via the Helm chart

`deploy/helm/atlas/values.yaml`'s `alerting.*` block wires all four sinks, referencing secrets
(never inlining values) for the PagerDuty routing key, Opsgenie API key, and Slack webhook URL:

```yaml
alerting:
  pagerduty:
    enabled: true
    existingSecret: atlas-alerting
    routingKeySecretKey: pagerduty-routing-key
```

## Severity mapping

| Atlas severity | PagerDuty | Opsgenie |
|---|---|---|
| `critical` | `critical` | `P1` |
| `warning` | `warning` | `P3` |
| other | `info` | `P5` |
