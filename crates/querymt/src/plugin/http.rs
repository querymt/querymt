use crate::{error::LLMError, HTTPLLMProvider};
use http::{Request, Response};
use serde_json::Value;
use std::error::Error;

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
    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, Box<dyn Error>>;

    /// Given a chosen model name, build a sync `HttpLLMProvider`
    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn Error>>;
}

pub type HTTPFactoryCtor = unsafe extern "C" fn() -> *mut dyn HTTPLLMProviderFactory;
