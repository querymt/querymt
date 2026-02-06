use llama_cpp_2::llama_backend::LlamaBackend;
use querymt::error::LLMError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Get the global llama backend instance.
///
/// The backend is initialized once and shared across all provider instances.
pub(crate) fn llama_backend() -> Result<std::sync::MutexGuard<'static, LlamaBackend>, LLMError> {
    static BACKEND: OnceLock<Result<Mutex<LlamaBackend>, String>> = OnceLock::new();
    let backend = BACKEND
        .get_or_init(|| {
            LlamaBackend::init()
                .map(Mutex::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| LLMError::ProviderError(e.clone()))?;
    backend
        .lock()
        .map_err(|_| LLMError::ProviderError("Llama backend lock poisoned".to_string()))
}

// ---------------------------------------------------------------------------
// GGML abort callback — capture fatal error messages before abort()
// ---------------------------------------------------------------------------

/// FFI declaration for the ggml abort callback API.
///
/// This is not re-exported by the llama-cpp-2 crate, so we declare it ourselves.
/// The function has a stable C ABI and is always linked via llama-cpp-sys-2.
type GgmlAbortCallbackT = Option<unsafe extern "C" fn(*const std::os::raw::c_char)>;

unsafe extern "C" {
    fn ggml_set_abort_callback(callback: GgmlAbortCallbackT) -> GgmlAbortCallbackT;
}

/// Whether the ggml abort callback has been installed.
static ABORT_CALLBACK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// C callback that ggml calls right before `abort()` on a fatal error.
///
/// This cannot prevent the abort — it is strictly for logging purposes so that
/// the user sees a meaningful error message (e.g. Metal "out of memory") instead
/// of only a raw stack trace.
unsafe extern "C" fn ggml_fatal_error_callback(error_message: *const std::os::raw::c_char) {
    if !error_message.is_null() {
        let msg = unsafe { std::ffi::CStr::from_ptr(error_message) }.to_string_lossy();
        // Use eprintln! directly since we're about to abort — log infrastructure
        // may not flush in time.
        eprintln!("\n[qmt-llama-cpp] FATAL: ggml/llama.cpp abort: {}", msg);
        eprintln!(
            "[qmt-llama-cpp] This is typically caused by GPU out-of-memory.\n\
             Suggestions:\n\
             - Reduce n_ctx (context window size)\n\
             - Enable KV cache quantization: kv_cache_type_k=q4_0 kv_cache_type_v=q4_0\n\
             - Enable flash attention: flash_attention=enabled\n\
             - Use a smaller model quantization (e.g. Q4_K_M instead of Q8_0)\n\
             - Set n_gpu_layers to offload fewer layers to GPU"
        );
        // Also log through the log crate in case it flushes
        log::error!("FATAL: ggml/llama.cpp abort: {}", msg);
    }
}

/// Install the ggml abort callback (idempotent — only installs once).
pub(crate) fn install_abort_callback() {
    if ABORT_CALLBACK_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        unsafe {
            ggml_set_abort_callback(Some(ggml_fatal_error_callback));
        }
    }
}
