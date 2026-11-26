//! OpenTelemetry export, modelled on QueryMT's `querymt-utils::telemetry`:
//! the same crates and versions, the same resource attributes, the same
//! environment variables and the same subscriber shape (console `fmt` layer,
//! then OTLP traces, logs and metrics).
//!
//! Differences:
//! - **Off by default.** QueryMT exports unless `QMT_NO_TELEMETRY` is set; this
//!   UI only exports when `OTEL_EXPORTER_OTLP_ENDPOINT` names a collector.
//!   `QMT_NO_TELEMETRY` still turns it off.
//! - **A dedicated one-thread Tokio runtime** hosts the gRPC (tonic) channels,
//!   since the UI does not run on Tokio. Export is OTLP/gRPC as in QueryMT.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset: off)* | Collector base URL, e.g. `http://localhost:4317` (OTLP/gRPC). |
//! | `QMT_NO_TELEMETRY` | *(unset)* | Any value disables export. |
//! | `QMT_TELEMETRY_LEVEL` | `info` | Filter for exported spans and logs; `info,querymt_gpui=debug` adds per-frame scroll samples. |
//! | `OTEL_METRIC_EXPORT_INTERVAL` | `60000` | Metric export interval in ms (SDK standard). |
//! | `RUST_LOG` | `error` | Console filter, independent of export. |

#![cfg_attr(target_family = "wasm", allow(dead_code))]

#[cfg(not(target_family = "wasm"))]
pub use native::*;

/// The browser build has no exporter: instrumentation compiles to no-ops.
#[cfg(target_family = "wasm")]
mod web {
    pub fn enabled() -> bool {
        false
    }
    pub fn record_step(_: f64) {}
    pub fn record_shift(_: f64) {}
    pub fn record_jump(_: &'static str) {}
    pub fn record_follow_engaged() {}
    pub fn record_remeasure() {}
    pub fn record_history_load(_: &'static str) {}
    pub fn record_frame(_: f64, _: usize) {}
    pub fn record_row(_: f64, _: &'static str, _: usize) {}
}
#[cfg(target_family = "wasm")]
pub use web::*;

/// Session open in flight: id and start time (ms since epoch).
static SESSION_OPEN: std::sync::Mutex<Option<(String, f64)>> = std::sync::Mutex::new(None);

/// A session open started (click → `session/load`).
pub fn mark_session_open(session_id: &str) {
    if let Ok(mut open) = SESSION_OPEN.lock() {
        *open = Some((session_id.to_string(), crate::util::now_ms()));
    }
}

/// The transcript of `session_id` rendered with `items` rows after loading:
/// logs time from the open once per open.
pub fn session_rendered(session_id: &str, items: usize) {
    let Ok(mut open) = SESSION_OPEN.lock() else {
        return;
    };
    if let Some((id, started)) = open.as_ref()
        && id == session_id
    {
        let elapsed = crate::util::now_ms() - started;
        tracing::info!(
            session.open_to_render_ms = elapsed,
            session.items = items,
            "session first render"
        );
        *open = None;
    }
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator, SdkTracerProvider};
    use opentelemetry_semantic_conventions::{SCHEMA_URL, resource::SERVICE_VERSION};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Layer, Registry};

    pub const SERVICE_NAME: &str = "querymt-gpui";

    /// Exporter internals must not feed their own spans and logs back into the
    /// exporter; QueryMT's `iroh=warn` default serves the same purpose.
    const EXPORTER_QUIET: &str = "hyper=off,hyper_util=off,h2=off,tonic=off,tower=off,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_otlp=off,opentelemetry_http=off";

    static ENABLED: AtomicBool = AtomicBool::new(false);
    struct Providers {
        tracer: SdkTracerProvider,
        logger: SdkLoggerProvider,
        meter: SdkMeterProvider,
        // Keep the tonic channel workers alive through provider shutdown.
        _runtime: tokio::runtime::Runtime,
    }

    static PROVIDERS: Mutex<Option<Providers>> = Mutex::new(None);

    /// Whether OTLP export is on; instrumentation that does real work per frame
    /// checks this first, so the default build pays nothing.
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }

    /// Resolved settings: `Some(endpoint)` when export is requested.
    fn requested_endpoint() -> Option<String> {
        if std::env::var_os("QMT_NO_TELEMETRY").is_some() {
            return None;
        }
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|e| !e.trim().is_empty())
    }

    /// Same identity attributes as QueryMT's `resource()`.
    fn resource() -> Resource {
        Resource::builder()
            .with_attributes([
                KeyValue::new(
                    "service.instance.id",
                    RandomIdGenerator::default().new_trace_id().to_string(),
                ),
                KeyValue::new("process.pid", i64::from(std::process::id())),
            ])
            .with_service_name(SERVICE_NAME)
            .with_schema_url(
                [KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION"))],
                SCHEMA_URL,
            )
            .build()
    }

    /// Install the global subscriber. Returns the collector endpoint when OTLP
    /// export started, `None` when only the console layer was installed.
    /// Failures leave the app running without export.
    pub fn init() -> Option<String> {
        let console = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")),
            )
            .boxed();
        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![console];

        let endpoint = requested_endpoint();
        let providers = endpoint.as_ref().map(|_| build_providers());
        if let Some(Ok(Providers { tracer, logger, .. })) = &providers {
            let level = std::env::var("QMT_TELEMETRY_LEVEL").unwrap_or_else(|_| "info".into());
            let filter = || EnvFilter::new(format!("{level},{EXPORTER_QUIET}"));
            layers.push(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer.tracer("querymt-gpui"))
                    .with_filter(filter())
                    .boxed(),
            );
            layers.push(
                OpenTelemetryTracingBridge::new(logger)
                    .with_filter(filter())
                    .boxed(),
            );
        }
        if tracing::subscriber::set_global_default(Registry::default().with(layers)).is_err() {
            return None;
        }
        let (Some(endpoint), Some(providers)) = (endpoint, providers) else {
            return None;
        };
        let providers = match providers {
            Ok(providers) => providers,
            Err(error) => {
                tracing::error!(%error, "OTLP telemetry initialization failed");
                return None;
            }
        };
        let mut slot = PROVIDERS.lock().ok()?;
        opentelemetry::global::set_meter_provider(providers.meter.clone());
        *slot = Some(providers);
        ENABLED.store(true, Ordering::Relaxed);
        tracing::info!(endpoint = %endpoint, "OTLP telemetry initialized");
        Some(endpoint)
    }

    /// Exporters read `OTEL_EXPORTER_OTLP_ENDPOINT` themselves.
    fn build_providers() -> Result<Providers, String> {
        use opentelemetry_otlp::WithTonicConfig as _;
        // tonic spawns each channel's connection worker on the runtime that is
        // current when the exporter is built; the SDK's export threads then
        // only hand requests to that worker, so they need no runtime of their own.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("otlp-export")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let guard = runtime.enter();
        let tls = || tonic::transport::ClientTlsConfig::new().with_enabled_roots();
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_tls_config(tls())
            .build()
            .map_err(|e| e.to_string())?;
        drop(guard);
        let resource = resource();
        Ok(Providers {
            tracer: SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(spans)
                .build(),
            logger: SdkLoggerProvider::builder()
                .with_resource(resource.clone())
                // Normalize before the batch processor clones the record.
                .with_log_processor(EventTimestampProcessor)
                .with_batch_exporter(logs)
                .build(),
            meter: SdkMeterProvider::builder()
                .with_resource(resource)
                .with_periodic_exporter(metrics)
                .build(),
            _runtime: runtime,
        })
    }

    /// As in QueryMT: `tracing` events carry no event timestamp, so use the
    /// observed time (otherwise collectors show 1970).
    #[derive(Debug)]
    struct EventTimestampProcessor;

    impl opentelemetry_sdk::logs::LogProcessor for EventTimestampProcessor {
        fn emit(
            &self,
            record: &mut opentelemetry_sdk::logs::SdkLogRecord,
            _: &opentelemetry::InstrumentationScope,
        ) {
            use opentelemetry::logs::LogRecord as _;
            if record.timestamp().is_none()
                && let Some(observed) = record.observed_timestamp()
            {
                record.set_timestamp(observed);
            }
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn event_enabled(
            &self,
            _: opentelemetry::logs::Severity,
            _: &str,
            _: Option<&str>,
        ) -> bool {
            // A transform alone must not enable logging without an exporter.
            false
        }
    }

    /// Flush and shut the providers down; call once on exit. Best effort.
    pub fn shutdown() {
        let Some(providers) = PROVIDERS.lock().ok().and_then(|mut p| p.take()) else {
            return;
        };
        ENABLED.store(false, Ordering::Relaxed);
        let _ = providers.tracer.shutdown();
        let _ = providers.logger.shutdown();
        let _ = providers.meter.shutdown();
    }

    /// Scroll-path metric instruments (no-ops until a meter provider is set).
    pub mod scroll {
        use std::sync::OnceLock;

        use opentelemetry::metrics::{Counter, Histogram};

        pub struct Instruments {
            /// Distance of each injected smooth-scroll step, px (signed: + is up).
            pub step: Histogram<f64>,
            /// Visible content movement per frame, px.
            pub shift: Histogram<f64>,
            /// Frames where content moved against, or far past, the scroll input.
            pub jumps: Counter<u64>,
            /// Tail-follow turning back on while the user scrolls.
            pub follow_engaged: Counter<u64>,
            /// Transcript rows remeasured (height change, same identity).
            pub remeasure: Counter<u64>,
            /// Older-history page loads, by `outcome`.
            pub history_loads: Counter<u64>,
            /// Transcript build, layout and prepaint per frame, ms.
            pub frame_ms: Histogram<f64>,
            /// Transcript rows built per frame.
            pub frame_rows: Histogram<u64>,
            /// Time to build one transcript row, ms, by `kind` and `entries`.
            pub row_ms: Histogram<f64>,
        }

        pub fn instruments() -> &'static Instruments {
            static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
            INSTRUMENTS.get_or_init(|| {
                let meter = opentelemetry::global::meter(super::SERVICE_NAME);
                Instruments {
                    step: meter
                        .f64_histogram("querymt_gpui.scroll.step")
                        .with_unit("px")
                        .with_description("Injected smooth-scroll step")
                        .build(),
                    shift: meter
                        .f64_histogram("querymt_gpui.transcript.frame_shift")
                        .with_unit("px")
                        .with_description("Visible transcript movement per frame")
                        .build(),
                    jumps: meter
                        .u64_counter("querymt_gpui.transcript.scroll_jump")
                        .with_description("Frames moving against or past the scroll input")
                        .build(),
                    follow_engaged: meter
                        .u64_counter("querymt_gpui.transcript.follow_tail_engaged")
                        .with_description("Tail-follow re-engaged")
                        .build(),
                    remeasure: meter
                        .u64_counter("querymt_gpui.transcript.remeasure")
                        .with_description("Rows remeasured after a height change")
                        .build(),
                    history_loads: meter
                        .u64_counter("querymt_gpui.transcript.history_load")
                        .with_description("Older history pages requested")
                        .build(),
                    frame_ms: meter
                        .f64_histogram("querymt_gpui.transcript.frame_ms")
                        .with_unit("ms")
                        .with_description("Transcript build, layout and prepaint per frame")
                        .with_boundaries(vec![
                            2., 4., 8., 12., 16., 24., 33., 50., 100., 200., 500.,
                        ])
                        .build(),
                    frame_rows: meter
                        .u64_histogram("querymt_gpui.transcript.frame_rows")
                        .with_description("Transcript rows built per frame")
                        .with_boundaries(vec![5., 10., 20., 40., 80., 160., 320.])
                        .build(),
                    row_ms: meter
                        .f64_histogram("querymt_gpui.transcript.row_ms")
                        .with_unit("ms")
                        .with_description("Time to build one transcript row")
                        .with_boundaries(vec![0.1, 0.25, 0.5, 1., 2., 4., 8., 16., 33., 100.])
                        .build(),
                }
            })
        }
    }

    fn record(f: impl FnOnce(&scroll::Instruments)) {
        if enabled() {
            f(scroll::instruments());
        }
    }
    pub fn record_step(px: f64) {
        record(|m| m.step.record(px, &[]));
    }
    pub fn record_shift(px: f64) {
        record(|m| m.shift.record(px, &[]));
    }
    pub fn record_jump(kind: &'static str) {
        record(|m| m.jumps.add(1, &[KeyValue::new("kind", kind)]));
    }
    pub fn record_follow_engaged() {
        record(|m| m.follow_engaged.add(1, &[]));
    }
    pub fn record_remeasure() {
        record(|m| m.remeasure.add(1, &[]));
    }
    pub fn record_history_load(outcome: &'static str) {
        record(|m| m.history_loads.add(1, &[KeyValue::new("outcome", outcome)]));
    }
    pub fn record_frame(ms: f64, rows: usize) {
        record(|m| {
            m.frame_ms.record(ms, &[]);
            m.frame_rows.record(rows as u64, &[]);
        });
    }
    /// `entries`: work entries the row holds (0 for other rows), bucketed so
    /// a slow kind of row stands out without unbounded attribute values.
    pub fn record_row(ms: f64, kind: &'static str, entries: usize) {
        let size = match entries {
            0 => "0",
            1..50 => "<50",
            50..500 => "<500",
            _ => "500+",
        };
        record(|m| {
            m.row_ms.record(
                ms,
                &[KeyValue::new("kind", kind), KeyValue::new("entries", size)],
            )
        });
    }
}
