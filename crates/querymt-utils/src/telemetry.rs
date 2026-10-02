use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use opentelemetry::InstrumentationScope;
use opentelemetry::logs::{LogRecord, Severity};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    error::OTelSdkResult,
    logs::{LogExporter, LogProcessor, SdkLogRecord, SdkLoggerProvider},
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
pub const DEFAULT_OTLP_ENDPOINT: &str = "https://otel.query.mt";

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
    use opentelemetry_otlp::WithTonicConfig;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())
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
    use opentelemetry_otlp::WithTonicConfig;
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())
        .build()
        .map_err(|e| TelemetryInitError::LoggerProvider(e.to_string()))?;

    Ok(build_logger_provider(
        service_name,
        service_version,
        exporter,
    ))
}

#[derive(Debug)]
struct EventTimestampProcessor;

impl LogProcessor for EventTimestampProcessor {
    fn emit(&self, record: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        // The SDK sets observed time before processors run. Use it only when
        // the source did not supply an event timestamp.
        if record.timestamp().is_none()
            && let Some(observed) = record.observed_timestamp()
        {
            record.set_timestamp(observed);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn event_enabled(&self, _level: Severity, _target: &str, _name: Option<&str>) -> bool {
        // A transform alone must not enable logging without an exporting processor.
        false
    }
}

fn build_logger_provider(
    service_name: &str,
    service_version: &str,
    exporter: impl LogExporter + 'static,
) -> SdkLoggerProvider {
    SdkLoggerProvider::builder()
        .with_resource(resource(service_name, service_version))
        // Normalize before the batch processor clones the record into its queue.
        .with_log_processor(EventTimestampProcessor)
        .with_batch_exporter(exporter)
        .build()
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

        // Store the providers so [`flush_telemetry`] can flush them on shutdown.
        // The providers are kept alive for the entire process lifetime.
        store_providers(tracer_provider, logger_provider);

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
// ─── Provider storage for shutdown flush ──────────────────────────────────────

/// Global storage for the OTLP providers so they can be flushed on shutdown.
static PROVIDERS: Mutex<Option<(SdkTracerProvider, SdkLoggerProvider)>> = Mutex::new(None);

fn store_providers(tracer: SdkTracerProvider, logger: SdkLoggerProvider) {
    match PROVIDERS.lock() {
        Ok(mut guard) => *guard = Some((tracer, logger)),
        Err(e) => {
            // Mutex poisoned — unlikely, but don't panic during init.
            log::warn!("Failed to store OTLP providers for later flush: {e}");
        }
    }
}

/// Flush pending OTLP spans and logs to the collector.
///
/// Call this during graceful shutdown (after all instrumented work is done)
/// to ensure the batch exporters have sent any buffered telemetry.
///
/// This is a best-effort operation: errors are logged but not propagated.
pub fn flush_telemetry() {
    let providers = PROVIDERS.lock().ok().and_then(|mut g| g.take());
    match providers {
        Some((tracer_provider, logger_provider)) => {
            log::info!("Flushing OTLP telemetry providers…");
            if let Err(e) = tracer_provider.force_flush() {
                log::warn!("OTLP tracer provider force_flush failed: {e}");
            }
            if let Err(e) = logger_provider.force_flush() {
                log::warn!("OTLP logger provider force_flush failed: {e}");
            }
            // shutdown() internally flushes then releases resources.
            if let Err(e) = tracer_provider.shutdown() {
                log::warn!("OTLP tracer provider shutdown failed: {e}");
            }
            if let Err(e) = logger_provider.shutdown() {
                eprintln!("OTLP logger provider shutdown failed: {e}");
            }
        }
        None => {
            // Either OTLP was never enabled or already flushed.
        }
    }
}

// ─── Convenience wrapper (panics on failure) ───────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    use opentelemetry::Context;
    use opentelemetry::logs::{AnyValue, Logger, LoggerProvider};
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry_sdk::logs::InMemoryLogExporter;

    fn log_record() -> SdkLogRecord {
        SdkLoggerProvider::builder()
            .build()
            .logger("timestamp-test")
            .create_log_record()
    }

    #[test]
    fn fills_missing_event_timestamp_without_changing_other_fields() {
        let observed = UNIX_EPOCH + Duration::from_secs(123);
        let mut record = log_record();
        record.set_observed_timestamp(observed);
        record.set_body("timestamp check".into());
        record.set_event_name("timestamp-test");
        record.set_target("querymt.timestamp_test");
        record.set_severity_number(Severity::Info);
        record.set_severity_text("INFO");
        record.add_attribute("user_id", 42_i64);
        record.set_trace_context(TraceId::from(1), SpanId::from(2), Some(TraceFlags::SAMPLED));
        let mut expected = record.clone();
        expected.set_timestamp(observed);

        EventTimestampProcessor.emit(
            &mut record,
            &InstrumentationScope::builder("timestamp-test").build(),
        );

        assert_eq!(record, expected);
    }

    #[test]
    fn preserves_explicit_event_timestamp() {
        let mut record = log_record();
        record.set_timestamp(UNIX_EPOCH + Duration::from_secs(123));
        record.set_observed_timestamp(UNIX_EPOCH + Duration::from_secs(456));
        let expected = record.clone();

        EventTimestampProcessor.emit(
            &mut record,
            &InstrumentationScope::builder("timestamp-test").build(),
        );

        assert_eq!(record, expected);
    }

    #[test]
    fn does_not_invent_timestamp_when_observed_time_is_missing() {
        let mut record = log_record();
        let expected = record.clone();

        EventTimestampProcessor.emit(
            &mut record,
            &InstrumentationScope::builder("timestamp-test").build(),
        );

        assert_eq!(record, expected);
    }

    #[test]
    fn timestamp_processor_alone_does_not_enable_events() {
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(EventTimestampProcessor)
            .build();

        assert!(!provider.logger("timestamp-test").event_enabled(
            Severity::Info,
            "querymt.timestamp_test",
            Some("timestamp-test"),
        ));
        provider.force_flush().unwrap();
        provider.shutdown().unwrap();
    }

    #[test]
    fn normalizes_upstream_bridge_logs_with_simple_exporter() {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource("timestamp-test", "1"))
            .with_log_processor(EventTimestampProcessor)
            .with_simple_exporter(exporter.clone())
            .build();

        assert_bridge_pipeline(provider, exporter);
    }

    #[test]
    fn normalizes_upstream_bridge_logs_before_batch_export() {
        let exporter = InMemoryLogExporter::default();
        let provider = build_logger_provider("timestamp-test", "1", exporter.clone());

        assert_bridge_pipeline(provider, exporter);
    }

    fn assert_bridge_pipeline(provider: SdkLoggerProvider, exporter: InMemoryLogExporter) {
        let trace_id = TraceId::from(1);
        let span_id = SpanId::from(2);
        let span_context = SpanContext::new(
            trace_id,
            span_id,
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        let context = Context::new().with_remote_span_context(span_context);
        let layer = OpenTelemetryTracingBridge::new(&provider)
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
        let subscriber = Registry::default().with(layer);
        let before = SystemTime::now();
        tracing::subscriber::with_default(subscriber, || {
            let _guard = context.attach();
            tracing::info!(target: "querymt.timestamp_test", user_id = 42_i64, active = true, "timestamp pipeline");
        });
        let after = SystemTime::now();

        provider.force_flush().unwrap();
        let logs = exporter.get_emitted_logs().unwrap();
        assert_eq!(logs.len(), 1);
        let log = &logs[0];
        let timestamp = log.record.timestamp().expect("event timestamp must be set");
        assert!((before..=after).contains(&timestamp));
        assert_eq!(log.record.observed_timestamp(), Some(timestamp));
        assert_eq!(log.record.severity_number(), Some(Severity::Info));
        assert_eq!(log.record.severity_text(), Some("INFO"));
        assert_eq!(
            log.record.target().map(|target| target.as_ref()),
            Some("querymt.timestamp_test"),
        );
        assert!(log.record.event_name().is_some());
        assert_eq!(
            log.record.body(),
            Some(&AnyValue::String("timestamp pipeline".into())),
        );
        let attributes: Vec<_> = log.record.attributes_iter().cloned().collect();
        assert_eq!(
            attributes,
            vec![
                ("user_id".into(), AnyValue::Int(42)),
                ("active".into(), AnyValue::Boolean(true)),
            ],
        );
        let trace_context = log.record.trace_context().expect("trace context preserved");
        assert_eq!(trace_context.trace_id, trace_id);
        assert_eq!(trace_context.span_id, span_id);
        assert_eq!(trace_context.trace_flags, Some(TraceFlags::SAMPLED));
        assert_eq!(log.instrumentation.name(), "");
        assert_eq!(
            log.resource.get(&opentelemetry::Key::new("service.name")),
            Some("timestamp-test".into()),
        );
        assert_eq!(
            log.resource.get(&opentelemetry::Key::new(SERVICE_VERSION)),
            Some("1".into()),
        );
        provider.shutdown().unwrap();
    }
}
