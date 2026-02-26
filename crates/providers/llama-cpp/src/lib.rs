mod backend;
mod config;
mod context;
mod generation;
mod memory;
mod messages;
mod multimodal;
mod provider;
mod response;
mod tools;

pub use config::LlamaCppConfig;
use provider::LlamaCppProvider;

/// Create a provider directly from a config struct (useful for testing and embedding).
pub fn create_provider(
    cfg: LlamaCppConfig,
) -> Result<Box<dyn querymt::LLMProvider>, querymt::error::LLMError> {
    Ok(Box::new(LlamaCppProvider::new(cfg)?))
}

use provider::CachedModel;
use querymt::LLMProvider;
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use schemars::schema_for;

struct LlamaCppFactory {
    /// Single-slot model cache. Stores the most recently loaded model
    /// (`Arc<LlamaModel>` + `Arc<MultimodalContext>`) keyed on hardware
    /// params (model path, n_gpu_layers).
    ///
    /// Capacity = 1 matches the common case: one model on the peer,
    /// multiple delegates sharing it with different system prompts.
    /// If a request arrives for a different model, the old one is evicted.
    model_cache: std::sync::Mutex<Option<CachedModel>>,
}

impl LLMProviderFactory for LlamaCppFactory {
    fn name(&self) -> &str {
        "llama_cpp"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(LlamaCppConfig);
        serde_json::to_string(&schema.schema)
            .expect("LlamaCppConfig schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg: LlamaCppConfig = serde_json::from_str(cfg)?;
        let provider = LlamaCppProvider::new_with_cache(cfg, &self.model_cache)?;
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        let cfg = cfg.to_string();
        Box::pin(async move {
            let cfg: LlamaCppConfig = serde_json::from_str(&cfg).map_err(|err| {
                LLMError::InvalidRequest(format!(
                    "Invalid llama_cpp config for list_models: {}. Expected JSON with at least a 'model' field.",
                    err
                ))
            })?;
            Ok(vec![cfg.model])
        })
    }

    fn supports_custom_models(&self) -> bool {
        true
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
// SAFETY: While trait objects aren't technically FFI-safe, this is a well-established
// plugin pattern where both sides of the FFI boundary are Rust code compiled with the
// same ABI. The host process will cast this back to `Box<dyn LLMProviderFactory>` using
// the same vtable layout. This pattern is used throughout the plugin system.
#[allow(improper_ctypes_definitions)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(LlamaCppFactory {
        model_cache: std::sync::Mutex::new(None),
    })) as *mut _
}

/// Initialize logging from the host process.
///
/// This function is called by the host after loading the plugin via dlopen.
/// It sets up a logger that forwards all `log` crate calls from this plugin
/// back to the host's logger, enabling `RUST_LOG` filtering to work for the plugin.
///
/// # Safety
///
/// This function is unsafe because:
/// - The `callback` function pointer must remain valid for the lifetime of the plugin
/// - This should only be called once per plugin load (the host ensures this)
/// - The callback must be thread-safe
#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init_logging(
    callback: querymt::plugin::LogCallbackFn,
    max_level: usize,
) {
    unsafe {
        querymt::plugin::plugin_log::init_from_host(callback, max_level);
    }
}
