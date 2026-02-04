use querymt::error::LLMError;
use querymt::plugin::LLMProviderFactory;
use schemars::schema_for;
use serde_json::Value;

use crate::config::MistralRSConfig;
use crate::model::MistralRS;

pub(crate) struct MistralRSFactory;

impl LLMProviderFactory for MistralRSFactory {
    fn name(&self) -> &str {
        "mistralrs"
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(MistralRSConfig);
        serde_json::to_string(&schema.schema)
            .expect("OpenRouter JSON Schema should always serialize")
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

        let provider = match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(MistralRS::new(cfg)))?,
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;
                runtime.block_on(MistralRS::new(cfg))?
            }
        };
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(MistralRSFactory)) as *mut _
}
