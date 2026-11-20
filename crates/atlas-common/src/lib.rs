// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Shared primitives for the Atlas storage control plane: configuration, the application error
//! type, tracing setup, and resource-id helpers.

pub mod config;
pub mod device;
pub mod error;
pub mod ids;

pub use config::Config;
pub use error::{AppError, AppResult};

/// Initialize `tracing` once, honoring `RUST_LOG`. Safe to call multiple times. When
/// `ATLAS_OTEL_EXPORTER_ENDPOINT` is set, also exports spans via OTLP/HTTP to that collector
/// (e.g. Jaeger, Tempo, Grafana Cloud, any OTel-compatible backend) — otherwise a no-op, so
/// tracing behaves exactly as before for every deployment that doesn't opt in. Must be called
/// from within a Tokio runtime (the batch exporter spawns a background flush task) — every
/// caller today is `#[tokio::main]`'s `async fn main`, which already guarantees this.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let otel_layer = otel_layer_from_env();
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(otel_layer)
        .try_init();
}

/// Builds the OpenTelemetry tracing layer from `ATLAS_OTEL_EXPORTER_ENDPOINT` (presence gates the
/// feature, mirroring `alert_webhook_url`/`ceph_prometheus_url`'s auto-gating) and the optional
/// `ATLAS_OTEL_SAMPLE_RATIO` (default `1.0` — export every span; lower for high-traffic
/// deployments where 100% sampling would be wasteful). `None` on any setup failure, logged as a
/// warning rather than failing startup — tracing export is an operational nicety, not something
/// that should take the gateway down if the collector endpoint is unreachable at boot.
fn otel_layer_from_env<S>() -> Option<tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let mut endpoint = std::env::var("ATLAS_OTEL_EXPORTER_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    // `SpanExporter::builder().with_endpoint(..)` is the *signal-specific* OTLP endpoint (no
    // auto-appended path) — verified live against a mock collector: given just a collector base
    // URL, the exporter posted straight to "/" instead of "/v1/traces" and every real collector
    // 404'd. Append the standard OTLP/HTTP traces path unless the caller already gave a specific
    // one, so `ATLAS_OTEL_EXPORTER_ENDPOINT` can just be "http://collector:4318" as most operators
    // expect from the general (non-signal-specific) OTEL_EXPORTER_OTLP_ENDPOINT convention.
    if !endpoint.trim_end_matches('/').ends_with("/v1/traces") {
        if !endpoint.ends_with('/') {
            endpoint.push('/');
        }
        endpoint.push_str("v1/traces");
    }
    let sample_ratio: f64 = std::env::var("ATLAS_OTEL_SAMPLE_RATIO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

    use opentelemetry_otlp::WithExportConfig;
    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ATLAS_OTEL_EXPORTER_ENDPOINT set but exporter setup failed, tracing export disabled: {e}");
            return None;
        }
    };
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name("atlas-gateway")
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(
            sample_ratio,
        ))
        .with_resource(resource)
        .build();
    let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, "atlas-gateway");
    opentelemetry::global::set_tracer_provider(provider);
    Some(tracing_opentelemetry::layer().with_tracer(tracer))
}

#[cfg(test)]
mod otel_tests {
    use super::otel_layer_from_env;

    // Combined into one test (rather than two `#[test]` fns) since both mutate the same
    // process-global env var and cargo runs tests in a crate on separate threads by default —
    // two separate tests here would race on ATLAS_OTEL_EXPORTER_ENDPOINT.
    #[test]
    fn otel_layer_gates_on_endpoint_env_var() {
        std::env::remove_var("ATLAS_OTEL_EXPORTER_ENDPOINT");
        assert!(
            otel_layer_from_env::<tracing_subscriber::Registry>().is_none(),
            "no endpoint set -> tracing export must stay off"
        );

        std::env::set_var("ATLAS_OTEL_EXPORTER_ENDPOINT", "http://127.0.0.1:4318");
        assert!(
            otel_layer_from_env::<tracing_subscriber::Registry>().is_some(),
            "endpoint set -> layer should build (exporter construction doesn't require the \
             collector to actually be reachable yet)"
        );
        std::env::remove_var("ATLAS_OTEL_EXPORTER_ENDPOINT");
    }
}
