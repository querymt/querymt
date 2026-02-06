mod backend;
mod config;
mod context;
mod generation;
mod memory;
mod provider;
mod response;
mod tools;

pub use config::LlamaCppConfig;
use provider::LlamaCppProvider;

use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::LLMProvider;
use schemars::schema_for;
use std::path::Path;

struct LlamaCppFactory;

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
        let provider = LlamaCppProvider::new(cfg)?;
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        let cfg = cfg.to_string();
        Box::pin(async move {
            let cfg: LlamaCppConfig = serde_json::from_str(&cfg)?;
            let model_name = cfg.model.clone().or_else(|| {
                Path::new(&cfg.model_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            });
            Ok(vec![model_name.unwrap_or(cfg.model_path)])
        })
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(LlamaCppFactory)) as *mut _
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
