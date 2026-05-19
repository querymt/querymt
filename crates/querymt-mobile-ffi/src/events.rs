//! Event callback and log handler infrastructure.
//!
//! The FFI layer exposes callbacks that fire from Rust's event system and log
//! framework. Callbacks must be non-blocking from Rust's perspective; native
//! layers should dispatch heavy work to their own queues.
//!
//! ## Telemetry
//!
//! Telemetry is controlled via environment variables:
//! - `QMT_MOBILE_TELEMETRY=1` or `OTEL_EXPORTER_OTLP_ENDPOINT` — enables OTLP
//!   traces + logs export via gRPC.
//! - `QMT_TELEMETRY_LEVEL` — log level for telemetry (default: `info`).
//!
//! When telemetry is not enabled via env, only the FFI callback logger is
//! installed (using the simpler `log` crate path).

use crate::types::FfiErrorCode;
use std::sync::Mutex;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

// ─── Callback Function Pointer Types ────────────────────────────────────────

/// C callback for agent events.
///
/// `user_data` is the opaque pointer supplied at registration time.
/// Use `Option<EventHandlerFn>` for nullable parameters.
pub type EventHandlerFn = unsafe extern "C" fn(
    agent_handle: u64,
    session_handle: u64,
    event_json: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
);

/// C callback for log messages.
/// Use `Option<LogHandlerFn>` for nullable parameters.
pub type LogHandlerFn = unsafe extern "C" fn(
    level: i32,
    message: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
);

// ─── Registered Callback State ──────────────────────────────────────────────

/// Holds the event handler callback configuration for a single agent.
pub struct EventCallbacks {
    pub handler: EventHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}

// Safety: EventCallbacks is only ever used from the FFI thread that registered it.
// The user_data pointer is opaque to Rust and managed by the native layer.
unsafe impl Send for EventCallbacks {}
unsafe impl Sync for EventCallbacks {}

// ─── Global Log Handler ─────────────────────────────────────────────────────

/// Global log handler state.
static GLOBAL_LOG_HANDLER: Mutex<Option<LogHandlerState>> = Mutex::new(None);

struct LogHandlerState {
    handler: LogHandlerFn,
    user_data: *mut std::ffi::c_void,
}

// Safety: same reasoning as EventCallbacks.
unsafe impl Send for LogHandlerState {}
unsafe impl Sync for LogHandlerState {}

/// Register the global log handler.
pub fn set_log_handler(
    handler: Option<LogHandlerFn>,
    user_data: *mut std::ffi::c_void,
) -> Result<(), FfiErrorCode> {
    if handler.is_none() && !user_data.is_null() {
        return Err(FfiErrorCode::InvalidArgument);
    }

    let mut log = GLOBAL_LOG_HANDLER.lock().unwrap();
    match handler {
        Some(h) => {
            *log = Some(LogHandlerState {
                handler: h,
                user_data,
            });
        }
        None => {
            *log = None;
        }
    }
    Ok(())
}

/// Invoke the global log handler if one is registered.
fn invoke_log_handler(level: log::Level, message: &str) {
    let state = GLOBAL_LOG_HANDLER.lock().unwrap();
    if let Some(ref state) = *state {
        let c_message = std::ffi::CString::new(message).unwrap_or_default();
        let level_code = match level {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 4,
        };
        unsafe {
            (state.handler)(level_code, c_message.as_ptr(), state.user_data);
        }
    }
}

// ─── Tracing Layer for FFI Callback ─────────────────────────────────────────

/// A `tracing_subscriber::Layer` that forwards every event to the FFI log
/// callback registered via `set_log_handler`. This works whether or not OTLP
/// telemetry is active.
struct FfiCallbackLayer;

impl<S> tracing_subscriber::Layer<S> for FfiCallbackLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // Convert the tracing event to a string and forward to FFI callback.
        let mut visitor = FfiEventVisitor(String::new());
        event.record(&mut visitor);

        let level = match *event.metadata().level() {
            tracing::Level::ERROR => log::Level::Error,
            tracing::Level::WARN => log::Level::Warn,
            tracing::Level::INFO => log::Level::Info,
            tracing::Level::DEBUG => log::Level::Debug,
            tracing::Level::TRACE => log::Level::Trace,
        };
        invoke_log_handler(level, &visitor.0);
    }
}

/// Simple visitor that concatenates the event message + fields.
struct FfiEventVisitor(String);

impl tracing::field::Visit for FfiEventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut self.0, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else {
            use std::fmt::Write;
            let _ = write!(&mut self.0, " {}={}", field.name(), value);
        }
    }
}

// ─── Fallback: log-crate-only logger (no telemetry) ─────────────────────────

/// Log callback for the `log` crate. Registered via `log::set_boxed_logger`
/// when telemetry is disabled.
struct FfiLogger;

impl log::Log for FfiLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // Always enabled; filtering is the callback's responsibility.
        true
    }

    fn log(&self, record: &log::Record) {
        invoke_log_handler(record.level(), &record.args().to_string());
    }

    fn flush(&self) {}
}

// ─── Initialization ─────────────────────────────────────────────────────────

static LOGGER_INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The OTLP endpoint that was configured (if telemetry is enabled).
/// Stored so mesh_status can report it.
static ACTIVE_OTLP_ENDPOINT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Return the active OTLP endpoint, if telemetry was enabled.
pub fn active_otlp_endpoint() -> Option<String> {
    ACTIVE_OTLP_ENDPOINT.lock().unwrap().clone()
}

/// Initialize the logging/telemetry subsystem.
///
/// Checks environment variables to decide whether to enable OTLP telemetry:
/// - `QMT_MOBILE_TELEMETRY=1` or a non-empty `OTEL_EXPORTER_OTLP_ENDPOINT`
///   enables full telemetry (tracing subscriber with OTLP + FFI callback).
/// - Otherwise falls back to a simple `log`-crate logger that only forwards
///   to the FFI callback.
///
/// This function is idempotent; subsequent calls are no-ops.
pub fn setup_mobile_telemetry() {
    if LOGGER_INITIALIZED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        // Already initialized.
        return;
    }

    let telemetry_enabled = std::env::var("QMT_MOBILE_TELEMETRY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .map(|v| !v.is_empty())
            .unwrap_or(false);

    if telemetry_enabled {
        init_telemetry();
    } else {
        init_fallback_logger();
    }
}

/// Full telemetry init: tracing subscriber with OTLP + FFI callback layers.
fn init_telemetry() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
    use tracing_opentelemetry::OpenTelemetryLayer;
    use tracing_subscriber::EnvFilter;

    // Resolve endpoint: env var → querymt-utils default.
    let default_endpoint = querymt_utils::telemetry::default_otlp_endpoint();
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| default_endpoint.to_string());

    // Store for mesh_status diagnostics.
    {
        let mut active = ACTIVE_OTLP_ENDPOINT.lock().unwrap();
        *active = Some(endpoint.clone());
    }

    // Bridge log -> tracing so log::info! etc. also flow through.
    let _ = tracing_log::LogTracer::init();

    // Console filter
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_filter(console_filter);

    // OTLP telemetry filter — read QMT_TELEMETRY_LEVEL, default info.
    let telemetry_level = std::env::var("QMT_TELEMETRY_LEVEL").unwrap_or_else(|_| "info".into());
    let trace_filter = EnvFilter::new(&telemetry_level);
    let log_filter = EnvFilter::new(&telemetry_level);

    // FFI callback filter — forward info+ to native side.
    let ffi_filter = EnvFilter::new("info");

    // Build OTLP providers using querymt-utils helpers.
    let tracer_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        querymt_utils::telemetry::init_tracer_provider(
            "querymt-mobile",
            env!("CARGO_PKG_VERSION"),
            &endpoint,
        )
    }));
    let logger_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        querymt_utils::telemetry::init_logger_provider(
            "querymt-mobile",
            env!("CARGO_PKG_VERSION"),
            &endpoint,
        )
    }));

    match (tracer_result, logger_result) {
        (Ok(tracer_provider), Ok(logger_provider)) => {
            let tracer = tracer_provider.tracer("qmt-mobile-tracer");

            let subscriber = tracing_subscriber::Registry::default()
                .with(fmt_layer)
                .with(OpenTelemetryLayer::new(tracer).with_filter(trace_filter))
                .with(OpenTelemetryTracingBridge::new(&logger_provider).with_filter(log_filter))
                .with(FfiCallbackLayer.with_filter(ffi_filter));

            match tracing::subscriber::set_global_default(subscriber) {
                Ok(()) => {
                    log::info!("OTLP telemetry initialized, endpoint={}", endpoint);
                }
                Err(e) => {
                    log::warn!("Failed to set global subscriber: {e}; OTLP will not be active");
                }
            }
        }
        (tracer_err, logger_err) => {
            if tracer_err.is_err() {
                log::warn!("OTLP tracer provider init failed for endpoint {endpoint}");
            }
            if logger_err.is_err() {
                log::warn!("OTLP logger provider init failed for endpoint {endpoint}");
            }
            init_fallback_logger();
        }
    }
}

/// Fallback: simple log-crate logger that only forwards to FFI callback.
fn init_fallback_logger() {
    log::set_max_level(log::LevelFilter::Debug);
    let _ = log::set_boxed_logger(Box::new(FfiLogger));
}

/// Backwards-compatible entry point. Equivalent to `setup_mobile_telemetry()`.
pub fn ensure_logger() {
    setup_mobile_telemetry();
}
