//! Event callback and log handler infrastructure.
//!
//! The FFI layer exposes callbacks that fire from Rust's event system and log
//! framework. Callbacks must be non-blocking from Rust's perspective; native
//! layers should dispatch heavy work to their own queues.

use crate::types::FfiErrorCode;
use std::sync::Mutex;

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

/// C free function for MCP response strings.
/// Use `Option<McpFreeFn>` for nullable parameters.
pub type McpFreeFn =
    unsafe extern "C" fn(ptr: *mut std::ffi::c_char, user_data: *mut std::ffi::c_void);

/// C handler function for MCP requests.
/// Use `Option<McpHandlerFn>` for nullable parameters.
pub type McpHandlerFn = unsafe extern "C" fn(
    request_json: *const std::ffi::c_char,
    user_data: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char;

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
pub fn invoke_log_handler(level: log::Level, message: &str) {
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

/// Log callback for the `log` crate. Registered via `log::set_boxed_logger`.
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

static LOGGER_INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Initialize the FFI log logger (call once at startup).
pub fn ensure_logger() {
    if LOGGER_INITIALIZED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        // Use the lowest level so we see everything; the callback can filter.
        log::set_max_level(log::LevelFilter::Debug);
        // Ignore errors if a logger was already set.
        let _ = log::set_boxed_logger(Box::new(FfiLogger));
    }
}
