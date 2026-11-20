// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Precise CDC offset-lag measurement via an embedded Kafka client.
//!
//! A JDBC-sink `KafkaConnector` reports RUNNING but not *how far behind* it is. The true replication
//! lag is, per topic-partition, `high_watermark − committed_offset` of the sink's consumer group
//! (Kafka Connect names it `connect-<sink-connector>`). Summed over all of a stream's Debezium
//! topics, that's the number of change events not yet applied to the edge.
//!
//! Behind the `kafka-lag` cargo feature because it links `rdkafka` (vendored librdkafka via cmake).
//! Without the feature, `measure` returns `0` and the reconciler keeps its liveness-only behaviour.

/// The sink consumer group Kafka Connect assigns a sink connector.
pub fn sink_consumer_group(sink_connector: &str) -> String {
    format!("connect-{sink_connector}")
}

#[cfg(feature = "kafka-lag")]
mod imp {
    use std::time::Duration;

    use anyhow::{Context, Result};
    use rdkafka::config::ClientConfig;
    use rdkafka::consumer::{BaseConsumer, Consumer};
    use rdkafka::topic_partition_list::{Offset, TopicPartitionList};

    /// Sum `high_watermark − committed_offset` over every partition of the topics whose name starts
    /// with `topic_prefix`, for consumer group `group`. Blocking (librdkafka).
    fn offset_lag(
        bootstrap: &str,
        group: &str,
        topic_prefix: &str,
        timeout_ms: u64,
    ) -> Result<i64> {
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", bootstrap)
            .set("group.id", group)
            .set("enable.auto.commit", "false")
            .create()
            .context("build kafka consumer")?;
        let timeout = Duration::from_millis(timeout_ms);

        let metadata = consumer
            .fetch_metadata(None, timeout)
            .context("fetch kafka metadata")?;
        let mut tpl = TopicPartitionList::new();
        for topic in metadata.topics() {
            if topic.name().starts_with(topic_prefix) {
                for p in topic.partitions() {
                    tpl.add_partition(topic.name(), p.id());
                }
            }
        }
        if tpl.count() == 0 {
            return Ok(0);
        }

        let committed = consumer
            .committed_offsets(tpl, timeout)
            .context("read committed offsets")?;
        let mut lag = 0i64;
        for elem in committed.elements() {
            let (low, high) = consumer
                .fetch_watermarks(elem.topic(), elem.partition(), timeout)
                .context("fetch watermarks")?;
            let committed_off = match elem.offset() {
                Offset::Offset(o) => o,
                // no commit yet — everything from the low watermark is pending
                _ => low,
            };
            lag += (high - committed_off).max(0);
        }
        Ok(lag)
    }

    /// Async wrapper — runs the blocking measurement on a blocking thread. Returns 0 on any error so
    /// the reconciler never fails a tick over a transient Kafka hiccup.
    pub async fn measure(bootstrap: String, group: String, topic_prefix: String) -> i64 {
        tokio::task::spawn_blocking(move || offset_lag(&bootstrap, &group, &topic_prefix, 5000))
            .await
            .ok()
            .and_then(|r| {
                r.map_err(|e| tracing::warn!("kafka lag measure failed: {e:#}"))
                    .ok()
            })
            .unwrap_or(0)
    }
}

#[cfg(not(feature = "kafka-lag"))]
mod imp {
    /// Stub when the `kafka-lag` feature is off — the reconciler reports caught-up (0) once healthy.
    pub async fn measure(_bootstrap: String, _group: String, _topic_prefix: String) -> i64 {
        0
    }
}

pub use imp::measure;
