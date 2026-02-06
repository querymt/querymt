//! Shared logging infrastructure for native plugins.
//!
//! This module provides a common `PluginLogger` implementation that native plugins
//! can use to forward their `log` crate calls back to the host process's logger.
//!
//! Since native plugins are compiled as `cdylib` and loaded via `dlopen`, each plugin
//! gets its own copy of the `log` crate's global statics. This means `RUST_LOG` and
//! the host's logger initialization don't affect the plugin. This module bridges that
//! gap by storing a function pointer callback and implementing `log::Log` to forward
//! all log calls through the callback to the host.

use crate::plugin::LogCallbackFn;
use std::ffi::CString;
use std::sync::atomic::{AtomicPtr, Ordering};

/// Global storage for the host's log callback function pointer.
/// This is set once when `init_from_host()` is called from the plugin's
/// `plugin_init_logging` export.
static LOG_CALLBACK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// A `log::Log` implementation that forwards all log calls through a function
/// pointer callback to the host process's logger.
///
/// This logger is designed for use in native plugins (cdylib) that need to
/// integrate with the host's logging infrastructure. It does not filter or
/// process log calls itself — all filtering is expected to happen on the host
/// side.
struct PluginLogger;

impl log::Log for PluginLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // If the callback is set, we're enabled. The host will do the actual filtering.
        !LOG_CALLBACK.load(Ordering::Relaxed).is_null()
    }

    fn log(&self, record: &log::Record) {
        // Load the callback pointer
        let cb = LOG_CALLBACK.load(Ordering::Relaxed);
        if cb.is_null() {
            return;
        }

        // Transmute the raw pointer back to the function pointer type
        let callback: LogCallbackFn = unsafe { std::mem::transmute(cb) };

        // Convert target and message to C strings
        let target = CString::new(record.target()).unwrap_or_default();
        let message = CString::new(format!("{}", record.args())).unwrap_or_default();

        // Call the host's logging callback
        unsafe {
            callback(record.level() as usize, target.as_ptr(), message.as_ptr());
        }
    }

    fn flush(&self) {
        // No-op — the host's logger handles flushing
    }
}

/// Global static instance of the PluginLogger.
static PLUGIN_LOGGER: PluginLogger = PluginLogger;

/// Initialize the plugin's logger from the host's callback.
///
/// This function should be called from the plugin's `plugin_init_logging` export.
/// It stores the callback function pointer, sets the plugin's logger to `PLUGIN_LOGGER`,
/// and configures the maximum log level.
///
/// # Safety
///
/// This function is unsafe because:
/// - The `callback` function pointer must remain valid for the lifetime of the plugin
/// - This function should only be called once per plugin load
/// - The callback must be thread-safe (it will be called from multiple threads)
///
/// # Parameters
///
/// - `callback`: The host's logging function pointer
/// - `max_level`: The maximum log level as a usize (Off=0, Error=1, Warn=2, Info=3, Debug=4, Trace=5)
///
/// # Example
///
/// ```ignore
/// #[cfg(feature = "native")]
/// #[unsafe(no_mangle)]
/// pub unsafe extern "C" fn plugin_init_logging(
///     callback: querymt::plugin::LogCallbackFn,
///     max_level: usize,
/// ) {
///     querymt::plugin::plugin_log::init_from_host(callback, max_level);
/// }
/// ```
pub unsafe fn init_from_host(callback: LogCallbackFn, max_level: usize) {
    // Store the callback as a raw pointer
    LOG_CALLBACK.store(callback as *mut (), Ordering::Relaxed);

    // Set the logger (ignore errors if already set)
    let _ = log::set_logger(&PLUGIN_LOGGER);

    // Convert max_level to LevelFilter
    let level = match max_level {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        5 => log::LevelFilter::Trace,
        _ => log::LevelFilter::Trace, // Default to Trace for unknown values
    };

    log::set_max_level(level);
}
