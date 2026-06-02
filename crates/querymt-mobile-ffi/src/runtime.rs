//! Global Tokio runtime for blocking FFI calls.
//!
//! FFI functions from C/Swift/Kotlin cannot use `.await` directly. Instead, each
//! FFI entry point spawns work on this global multi-threaded runtime and blocks
//! the calling thread until the future completes.

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

const FFI_RUNTIME_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;
const FFI_BLOCK_ON_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Global multi-threaded Tokio runtime for FFI calls.
static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(FFI_RUNTIME_THREAD_STACK_BYTES)
        .build()
        .expect("Failed to create Tokio runtime for FFI")
});

/// Return a reference to the global runtime.
pub fn global_runtime() -> &'static Runtime {
    &RUNTIME
}

/// Run a future on a dedicated OS thread with a larger stack than the RN
/// TurboModule dispatch queue to avoid stack overflows during deep async
/// serde/RMCP parsing paths.
pub fn block_on_ffi<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("qmt-ffi-block-on".to_string())
        .stack_size(FFI_BLOCK_ON_STACK_BYTES)
        .spawn(move || global_runtime().block_on(future))
        .expect("failed to spawn qmt-ffi-block-on thread")
        .join()
        .expect("qmt-ffi-block-on thread panicked")
}
