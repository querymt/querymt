use http::{header::CONTENT_TYPE, Method, Request, Response};
use querymt::{error::LLMError, plugin::HTTPLLMProviderFactory, HTTPLLMProvider};
use schemars::schema_for;
use serde_json::Value;
use url::Url;

use crate::Anthropic;

struct AnthropicFactory;

impl HTTPLLMProviderFactory for AnthropicFactory {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("ANTHROPIC_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Anthropic::default_base_url(),
        };

        match cfg.get("api_key").and_then(Value::as_str) {
            Some(api_key) => {
                let url = base_url.join("models")?;

                Ok(Request::builder()
                    .method(Method::GET)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .uri(url.as_str())
                    .body(Vec::new())?)
            }
            None => Err(LLMError::AuthError("Missing Anthropic API key".into())),
        }
    }

    fn parse_list_models(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let resp_json: Value = serde_json::from_slice(&resp.body())?;
        let arr = resp_json
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| LLMError::InvalidRequest("`data` missing or not an array".into()))?;

        let names = arr
            .iter()
            .filter_map(|m| m.get("id"))
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        Ok(names)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Anthropic);
        serde_json::to_value(&schema.schema).expect("Anthropic JSON Schema should always serialize")
    }

    fn from_config(
        &self,
        cfg: &Value,
    ) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn std::error::Error>> {
        let provider: Anthropic = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("Anthropic config error: {}", e)))?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(AnthropicFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Anthropic, AnthropicFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Anthropic,
        factory = AnthropicFactory,
        name   = "anthropic",
    }
}
