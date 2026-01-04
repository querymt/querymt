use http::{Method, Request, Response, header::CONTENT_TYPE};
use querymt::{HTTPLLMProvider, error::LLMError, plugin::HTTPLLMProviderFactory};
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

                // Determine auth type using the same logic as the main implementation
                let auth_type = cfg
                    .get("auth_type")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_else(|| {
                        // Auto-detect based on api_key format
                        // Check for OAuth token pattern: sk-ant-oat<digits>-
                        if api_key.starts_with("sk-ant-oat")
                            && let Some(rest) = api_key.strip_prefix("sk-ant-oat")
                                && rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                                    return crate::AuthType::OAuth;
                                }

                        // Check for API key pattern: sk-ant-api<digits>-
                        if api_key.starts_with("sk-ant-api")
                            && let Some(rest) = api_key.strip_prefix("sk-ant-api")
                                && rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                                    return crate::AuthType::ApiKey;
                                }

                        // Fallback: Check for generic sk-ant- prefix (backward compatibility)
                        if api_key.starts_with("sk-ant-") {
                            eprintln!(
                                "Warning: Anthropic token format not recognized (expected 'sk-ant-oat<N>-' or 'sk-ant-api<N>-'). \
                                Defaulting to API key authentication. Consider setting 'auth_type' explicitly."
                            );
                            return crate::AuthType::ApiKey;
                        }

                        // Token doesn't match Anthropic format at all
                        eprintln!(
                            "Warning: Token does not match expected Anthropic format (should start with 'sk-ant-'). \
                            Defaulting to API key authentication. This may cause authentication failures."
                        );
                        crate::AuthType::ApiKey
                    });

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

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Anthropic);
        serde_json::to_value(&schema.schema).expect("Anthropic JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Anthropic = serde_json::from_value(cfg.clone())?;
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
