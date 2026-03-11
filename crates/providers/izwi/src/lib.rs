mod config;
mod provider;

pub use config::IzwiConfig;

use provider::{CachedRuntime, IzwiProvider};
use querymt::LLMProvider;
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use schemars::schema_for;

/// Create a provider directly from a config struct (useful for testing and embedding).
pub fn create_provider(cfg: IzwiConfig) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
    Ok(Box::new(IzwiProvider::new(cfg)?))
}

struct IzwiFactory {
    /// Single-slot runtime cache. Stores the most recently constructed
    /// `Arc<RuntimeService>` keyed on engine-level config.
    ///
    /// Multiple provider instances (different models/voices) share the
    /// same runtime when their engine config is identical.  If a request
    /// arrives for a different engine config, the old runtime is evicted.
    runtime_cache: std::sync::Mutex<Option<CachedRuntime>>,
}

impl LLMProviderFactory for IzwiFactory {
    fn name(&self) -> &str {
        "izwi"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(IzwiConfig);
        serde_json::to_string(&schema).expect("IzwiConfig schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg: IzwiConfig = serde_json::from_str(cfg)?;
        let provider = IzwiProvider::new_with_cache(cfg, &self.runtime_cache)?;
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async move { Ok(IzwiProvider::list_models()) })
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
    Box::into_raw(Box::new(IzwiFactory {
        runtime_cache: std::sync::Mutex::new(None),
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
