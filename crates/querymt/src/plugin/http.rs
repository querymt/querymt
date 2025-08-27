use crate::{error::LLMError, HTTPLLMProvider};
use http::{Request, Response};
use serde_json::Value;

pub trait HTTPLLMProviderFactory: Send + Sync {
    fn name(&self) -> &str;

    fn api_key_name(&self) -> Option<String> {
        None
    }

    /// Schema for plugin config
    fn config_schema(&self) -> Value;

    /// Build the HTTP request that lists models.
    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError>;

    /// Turn the raw HTTP response into a Vec<String>.
    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError>;

    /// Given a chosen model name, build a sync `HttpLLMProvider`
    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError>;
}

pub type HTTPFactoryCtor = unsafe extern "C" fn() -> *mut dyn HTTPLLMProviderFactory;

#[macro_export]
macro_rules! handle_http_error {
    ($key:expr) => {{
        if !$key.status().is_success() {
            let status = $key.status();
            let error_text: String = String::from_utf8($key.into_body())?;
            return Err(LLMError::ResponseFormatError {
                message: format!("API returned error status: {}", status),
                raw_response: error_text,
            });
        }
    }};
}
