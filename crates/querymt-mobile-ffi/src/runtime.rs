//! Global Tokio runtime for blocking FFI calls.
//!
//! FFI functions from C/Swift/Kotlin cannot use `.await` directly. Instead, each
//! FFI entry point spawns work on this global multi-threaded runtime and blocks
//! the calling thread until the future completes.

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

/// Global multi-threaded Tokio runtime for FFI calls.
static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Failed to create Tokio runtime for FFI"));

/// Return a reference to the global runtime.
pub fn global_runtime() -> &'static Runtime {
    &RUNTIME
}
