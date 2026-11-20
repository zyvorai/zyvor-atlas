-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Per-sink delivery tracking for the native PagerDuty/Opsgenie/Slack alerting integrations
-- (crates/atlas-monitor/src/notify/). Deliberately separate from storage_alerts.notified_at
-- (the pre-existing generic webhook's own tracking column, left untouched) since a single alert
-- can now fan out to several independently-configured sinks, each needing its own trigger/resolve
-- delivery state — a fixed set of extra columns doesn't scale the way this table does.
CREATE TABLE IF NOT EXISTS alert_notifications (
    id       BIGSERIAL PRIMARY KEY,
    alert_id TEXT NOT NULL REFERENCES storage_alerts(id) ON DELETE CASCADE,
    sink     TEXT NOT NULL,   -- 'pagerduty' | 'opsgenie' | 'slack'
    event    TEXT NOT NULL,   -- 'trigger' | 'resolve'
    sent_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    UNIQUE(alert_id, sink, event)
);
CREATE INDEX IF NOT EXISTS idx_alert_notifications_sink ON alert_notifications(sink, event);
