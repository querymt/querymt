use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::{SCHEMA_URL, resource::SERVICE_VERSION};
use tracing_log::LogTracer;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

/// Default OTLP endpoint for telemetry
const DEFAULT_OTLP_ENDPOINT: &str = "http://otel.query.mt:4317";

/// Build a resource with service name & version
fn resource(service_name: &str, service_version: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_string())
        .with_schema_url(
            [KeyValue::new(SERVICE_VERSION, service_version.to_string())],
            SCHEMA_URL,
        )
        .build()
}

/// Initialize an OTLP tracer provider
fn init_tracer_provider(
    service_name: &str,
    service_version: &str,
    endpoint: &str,
) -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("OTLP span exporter init failed");

    SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource(service_name, service_version))
        .with_batch_exporter(exporter)
        .build()
}

/// Initialize an OTLP logger provider
fn init_logger_provider(
    service_name: &str,
    service_version: &str,
    endpoint: &str,
) -> SdkLoggerProvider {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("OTLP log exporter init failed");

    SdkLoggerProvider::builder()
        .with_resource(resource(service_name, service_version))
        .with_batch_exporter(exporter)
        .build()
}

/// Setup telemetry with configurable service name and version
///
/// # Behavior
/// - If `QMT_NO_TELEMETRY` env var is set, only sets up fmt + filter layers (no OTLP export)
/// - If `OTEL_EXPORTER_OTLP_ENDPOINT` is not set, defaults to `http://otel.query.mt:4317`
/// - If `OTEL_EXPORTER_OTLP_ENDPOINT` is set, uses that endpoint instead
///
/// # Arguments
/// - `service_name`: The name of the service (e.g., "querymt-cli")
/// - `service_version`: The version of the service (e.g., "0.2.0")
///
/// # Environment Variables
/// - `QMT_NO_TELEMETRY`: If set (any value), disables OTLP export
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: Custom OTLP endpoint (defaults to http://otel.query.mt:4317)
/// - `RUST_LOG`: Controls log filtering (via EnvFilter)
pub fn setup_telemetry(service_name: &str, service_version: &str) {
    // Always initialize LogTracer for log->tracing bridge
    LogTracer::init().expect("Failed to set LogTracer");

    let fmt_layer = fmt::layer().with_target(true);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Check if telemetry is disabled
    if std::env::var("QMT_NO_TELEMETRY").is_ok() {
        let subscriber = Registry::default().with(filter).with(fmt_layer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("Failed to set tracing subscriber");
        return;
    }

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_OTLP_ENDPOINT.to_string());

    let tracer_provider = init_tracer_provider(service_name, service_version, &endpoint);
    let tracer = tracer_provider.tracer("qmt-tracer");

    let logger_provider = init_logger_provider(service_name, service_version, &endpoint);
    let log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    let subscriber = Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(OpenTelemetryLayer::new(tracer))
        .with(log_layer);

    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}
