mod config;
mod provider;

pub use config::IzwiConfig;

use provider::IzwiProvider;
use querymt::LLMProvider;
use querymt::error::LLMError;
use querymt::plugin::{Fut, LLMProviderFactory};
use schemars::schema_for;

pub fn create_provider(cfg: IzwiConfig) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
    Ok(Box::new(IzwiProvider::new(cfg)?))
}

struct IzwiFactory;

impl LLMProviderFactory for IzwiFactory {
    fn name(&self) -> &str {
        "izwi"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(IzwiConfig);
        serde_json::to_string(&schema.schema).expect("IzwiConfig schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg: IzwiConfig = serde_json::from_str(cfg)?;
        let provider = IzwiProvider::new(cfg)?;
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async move { Ok(IzwiProvider::list_models()) })
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(IzwiFactory)) as *mut _
}

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
