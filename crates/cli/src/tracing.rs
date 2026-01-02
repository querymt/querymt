use opentelemetry::{KeyValue, trace::TracerProvider};
use opentelemetry_sdk::{
    Resource,
    trace::{RandomIdGenerator, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::{SCHEMA_URL, resource::SERVICE_VERSION};
use tracing_log::LogTracer;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

/// Build a resource with service name & version
fn resource() -> Resource {
    Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .with_schema_url(
            [KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION"))],
            SCHEMA_URL,
        )
        .build()
}

/// Initialize an OTLP tracer provider
fn init_tracer_provider() -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .expect("OTLP exporter init failed");

    SdkTracerProvider::builder()
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource())
        .with_batch_exporter(exporter)
        .build()
}

/// Setup tracing + log integration
pub fn setup_logging() {
    LogTracer::init().expect("Failed to set LogTracer");
    let tp = init_tracer_provider();
    let tracer = tp.tracer("qmt-tracer");
    let fmt_layer = fmt::layer().with_target(true);
    let filter = EnvFilter::from_default_env();
    let subscriber = Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(OpenTelemetryLayer::new(tracer));
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set tracing subscriber");
}
