use std::fmt;

use opentelemetry::trace::TracerProvider as _;
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
use tracing_subscriber::fmt as ts_fmt;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry};

/// Default OTLP endpoint for telemetry
pub const DEFAULT_OTLP_ENDPOINT: &str = "http://otel.query.mt:4317";

/// Returns the default OTLP endpoint. Useful for callers that need the value
/// without duplicating the constant.
pub fn default_otlp_endpoint() -> &'static str {
    DEFAULT_OTLP_ENDPOINT
}

/// Build a resource with service name & version
pub fn resource(service_name: &str, service_version: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_string())
        .with_schema_url(
            [opentelemetry::KeyValue::new(
                SERVICE_VERSION,
                service_version.to_string(),
            )],
            SCHEMA_URL,
        )
        .build()
}

// ─── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur during telemetry initialization.
#[derive(Debug)]
pub enum TelemetryInitError {
    /// Failed to install the `log` → `tracing` bridge (`LogTracer`).
    LogTracer,
    /// Failed to create the OTLP tracer provider (span exporter).
    TracerProvider(String),
    /// Failed to create the OTLP logger provider (log exporter).
    LoggerProvider(String),
    /// Failed to install the global tracing subscriber
    /// (typically because one was already installed).
    Subscriber,
}

impl fmt::Display for TelemetryInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LogTracer => write!(f, "failed to install LogTracer"),
            Self::TracerProvider(ctx) => write!(f, "failed to init OTLP tracer provider: {ctx}"),
            Self::LoggerProvider(ctx) => write!(f, "failed to init OTLP logger provider: {ctx}"),
            Self::Subscriber => write!(f, "failed to set global tracing subscriber"),
        }
    }
}

impl std::error::Error for TelemetryInitError {}

// ─── Config ────────────────────────────────────────────────────────────────────

/// Configuration for telemetry setup.
///
/// Use this together with [`try_setup_telemetry`] or [`try_setup_telemetry_with_layers`].
pub struct TelemetryConfig<'a> {
    /// Service name used in the OTLP resource (e.g. `"querymt-cli"`).
    pub service_name: &'a str,
    /// Service version used in the OTLP resource.
    pub service_version: &'a str,
    /// If `true`, the console `fmt` layer writes to stderr instead of stdout.
    pub use_stderr: bool,
    /// Whether to enable OTLP export. When `false`, only the console fmt layer
    /// is installed (plus any extra layers supplied by the caller).
    pub enable_otlp: bool,
    /// Override the OTLP endpoint. When `None`, falls back to the
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` env var, then [`DEFAULT_OTLP_ENDPOINT`].
    pub endpoint: Option<String>,
}

impl<'a> TelemetryConfig<'a> {
    /// Resolve the effective OTLP endpoint.
    ///
    /// Priority: explicit override → `OTEL_EXPORTER_OTLP_ENDPOINT` env → default.
    fn resolve_endpoint(&self) -> String {
        self.endpoint
            .clone()
            .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
            .unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.to_string())
    }
}

// ─── Provider construction (fallible) ──────────────────────────────────────────

/// Initialize an OTLP tracer provider.
///
/// Returns an error instead of panicking when the exporter cannot be built.
pub fn init_tracer_provider(
    service_name: &str,
    service_version: &str,
    endpoint: &str,
) -> Result<SdkTracerProvider, TelemetryInitError> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryInitError::TracerProvider(e.to_string()))?;

    Ok(SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource(service_name, service_version))
        .with_batch_exporter(exporter)
        .build())
}

/// Initialize an OTLP logger provider.
///
/// Returns an error instead of panicking when the exporter cannot be built.
pub fn init_logger_provider(
    service_name: &str,
    service_version: &str,
    endpoint: &str,
) -> Result<SdkLoggerProvider, TelemetryInitError> {
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryInitError::LoggerProvider(e.to_string()))?;

    Ok(SdkLoggerProvider::builder()
        .with_resource(resource(service_name, service_version))
        .with_batch_exporter(exporter)
        .build())
}

// ─── Boxed layer alias ─────────────────────────────────────────────────────────

/// A type-erased tracing layer suitable for dynamic subscriber composition.
///
/// Callers that need to inject custom layers (e.g. an FFI callback layer)
/// should box them with `.boxed()` and pass them via
/// [`try_setup_telemetry_with_layers`].
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

// ─── Core setup (fallible, no extra layers) ────────────────────────────────────

/// Try to set up telemetry with the given configuration.
///
/// On success, returns `Ok(Some(endpoint))` when OTLP was enabled, or
/// `Ok(None)` when OTLP was disabled but the console subscriber was installed.
///
/// This is the fallible counterpart to [`setup_telemetry`].
pub fn try_setup_telemetry(
    config: TelemetryConfig<'_>,
) -> Result<Option<String>, TelemetryInitError> {
    try_setup_telemetry_with_layers(config, Vec::new())
}

// ─── Core setup (fallible, with extra layers) ──────────────────────────────────

/// Try to set up telemetry, attaching extra [`Layer`]s to the subscriber.
///
/// On success, returns `Ok(Some(endpoint))` when OTLP was enabled, or
/// `Ok(None)` when OTLP was disabled but the console subscriber was installed.
///
/// Extra layers are appended **after** the fmt and OTLP layers, so they
/// receive every event that passes the earlier filters.
///
/// # Example
///
/// ```ignore
/// use tracing_subscriber::prelude::*;
///
/// let my_layer = MyCallbackLayer.with_filter(EnvFilter::new("info")).boxed();
///
/// let config = querymt_utils::telemetry::TelemetryConfig { /* ... */ };
/// querymt_utils::telemetry::try_setup_telemetry_with_layers(config, vec![my_layer])?;
/// ```
pub fn try_setup_telemetry_with_layers(
    config: TelemetryConfig<'_>,
    extra_layers: Vec<BoxedLayer>,
) -> Result<Option<String>, TelemetryInitError> {
    // Bridge log → tracing so `log::info!` etc. also flow through.
    LogTracer::init().map_err(|_| TelemetryInitError::LogTracer)?;

    // Console filter: default ERROR, overridden by RUST_LOG.
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));
    let writer: BoxMakeWriter = if config.use_stderr {
        BoxMakeWriter::new(std::io::stderr)
    } else {
        BoxMakeWriter::new(std::io::stdout)
    };

    let mut layers: Vec<BoxedLayer> = Vec::new();

    // Always add the console fmt layer.
    layers.push(
        ts_fmt::layer()
            .with_writer(writer)
            .with_target(true)
            .with_filter(console_filter)
            .boxed(),
    );

    if config.enable_otlp {
        // OTLP telemetry filter: default INFO, overridden by QMT_TELEMETRY_LEVEL.
        // Two separate instances because EnvFilter is not Clone.
        let telemetry_level =
            std::env::var("QMT_TELEMETRY_LEVEL").unwrap_or_else(|_| "info".into());
        let trace_filter = EnvFilter::new(&telemetry_level);
        let log_filter = EnvFilter::new(&telemetry_level);

        let endpoint = config.resolve_endpoint();

        let tracer_provider =
            init_tracer_provider(config.service_name, config.service_version, &endpoint)?;
        let tracer = tracer_provider.tracer("qmt-tracer");

        let logger_provider =
            init_logger_provider(config.service_name, config.service_version, &endpoint)?;
        let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

        layers.push(
            OpenTelemetryLayer::new(tracer)
                .with_filter(trace_filter)
                .boxed(),
        );
        layers.push(otel_log_layer.with_filter(log_filter).boxed());

        // Append caller-provided layers.
        layers.extend(extra_layers);

        let subscriber = Registry::default().with(layers);
        tracing::subscriber::set_global_default(subscriber)
            .map_err(|_| TelemetryInitError::Subscriber)?;

        return Ok(Some(endpoint));
    }

    // OTLP disabled — just console + any caller layers.
    layers.extend(extra_layers);

    let subscriber = Registry::default().with(layers);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| TelemetryInitError::Subscriber)?;

    Ok(None)
}

// ─── Convenience wrapper (panics on failure) ───────────────────────────────────

/// Setup telemetry with configurable service name and version.
///
/// Uses **per-layer filtering** so that console output and OTLP telemetry
/// can operate at independent log levels:
///
/// - Console (`fmt` layer): defaults to `ERROR`, overridden by `RUST_LOG`
/// - OTLP layers (traces + logs): defaults to `INFO`, overridden by `QMT_TELEMETRY_LEVEL`
///
/// # Behavior
/// - If `QMT_NO_TELEMETRY` env var is set, only sets up fmt layer (no OTLP export)
/// - If `OTEL_EXPORTER_OTLP_ENDPOINT` is not set, defaults to `http://otel.query.mt:4317`
/// - If `OTEL_EXPORTER_OTLP_ENDPOINT` is set, uses that endpoint instead
///
/// # Arguments
/// - `service_name`: The name of the service (e.g., "querymt-cli")
/// - `service_version`: The version of the service (e.g., "0.2.0")
/// - `use_stderr`: If `true`, the console `fmt` layer writes to stderr instead of stdout.
///   Useful for ACP/stdio mode where stdout is reserved for JSON-RPC.
///
/// # Environment Variables
/// - `QMT_NO_TELEMETRY`: If set (any value), disables OTLP export
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: Custom OTLP endpoint (defaults to http://otel.query.mt:4317)
/// - `RUST_LOG`: Controls console output filtering (defaults to `error`)
/// - `QMT_TELEMETRY_LEVEL`: Controls OTLP telemetry filtering (defaults to `info`)
///
/// # Panics
/// Panics if the global subscriber or `LogTracer` could not be installed
/// (e.g. because another caller already set one).
pub fn setup_telemetry(service_name: &str, service_version: &str, use_stderr: bool) {
    let config = TelemetryConfig {
        service_name,
        service_version,
        use_stderr,
        enable_otlp: std::env::var("QMT_NO_TELEMETRY").is_err(),
        endpoint: None,
    };

    try_setup_telemetry(config).expect("Failed to set up telemetry");
}
