use http::{Method, Request, Response, header::CONTENT_TYPE};
use querymt::{
    HTTPLLMProvider, error::LLMError, handle_http_error, plugin::HTTPLLMProviderFactory,
};
use schemars::schema_for;
use serde_json::Value;
use url::Url;

use crate::{Anthropic, detect_auth_type};

struct AnthropicFactory;

impl HTTPLLMProviderFactory for AnthropicFactory {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("ANTHROPIC_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Anthropic::default_base_url(),
        };

        match cfg.get("api_key").and_then(Value::as_str) {
            Some(api_key) => {
                let url = base_url.join("models")?;

                // Determine auth type using the shared detection function
                let explicit_auth_type = cfg
                    .get("auth_type")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let auth_type = detect_auth_type(api_key, explicit_auth_type);

                let builder = Request::builder()
                    .method(Method::GET)
                    .header(CONTENT_TYPE, "application/json")
                    .uri(url.as_str());

                let builder = match auth_type {
                    crate::AuthType::OAuth => builder
                        .header("Authorization", format!("Bearer {}", api_key))
                        .header("anthropic-beta", "oauth-2025-04-20"),
                    crate::AuthType::ApiKey => builder.header("x-api-key", api_key),
                };

                let builder = builder.header("anthropic-version", "2023-06-01");

                Ok(builder.body(Vec::new())?)
            }
            None => Err(LLMError::AuthError("Missing Anthropic API key".into())),
        }
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        handle_http_error!(resp);

        let resp_json: Value = serde_json::from_slice(resp.body())?;
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

    fn config_schema(&self) -> String {
        let schema = schema_for!(Anthropic);
        serde_json::to_string(&schema.schema)
            .expect("Anthropic JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Anthropic = serde_json::from_str(cfg)?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
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
