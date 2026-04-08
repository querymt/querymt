use querymt::error::LLMError;
use querymt::plugin::LLMProviderFactory;
use schemars::schema_for;
use serde_json::Value;

use crate::config::MistralRSConfig;
use crate::model::{CachedModel, MistralRS};

/// Creates a mistral.rs factory for direct static registration.
pub fn create_factory() -> std::sync::Arc<dyn LLMProviderFactory> {
    std::sync::Arc::new(MistralRSFactory {
        model_cache: std::sync::Mutex::new(None),
    })
}

pub(crate) struct MistralRSFactory {
    /// Single-slot model cache. Stores the most recently loaded model
    /// keyed on hardware params (model path, kind, dtype, force_cpu).
    ///
    /// If a request arrives for a different model, the old one is evicted.
    model_cache: std::sync::Mutex<Option<CachedModel>>,
}

impl LLMProviderFactory for MistralRSFactory {
    fn name(&self) -> &str {
        "mistralrs"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(MistralRSConfig);
        serde_json::to_string(&schema).expect("OpenRouter JSON Schema should always serialize")
    }

    fn list_models<'a>(
        &'a self,
        cfg: &str,
    ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
        let cfg = cfg.to_string();
        Box::pin(async move {
            let cfg: Value = serde_json::from_str(&cfg)?;
            let model = cfg
                .get("model")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            Ok(model.into_iter().collect())
        })
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
        let cfg: MistralRSConfig = serde_json::from_str(cfg)
            .map_err(|e| LLMError::PluginError(format!("mistral.rs config error: {}", e)))?;

        let provider = MistralRS::new_with_cache(cfg, &self.model_cache)?;
        Ok(Box::new(provider))
    }

    fn supports_custom_models(&self) -> bool {
        true
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(MistralRSFactory {
        model_cache: std::sync::Mutex::new(None),
    })) as *mut _
}
