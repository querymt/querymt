use crate::{HTTPLLMProvider, error::LLMError};
use http::{Request, Response};

pub trait HTTPLLMProviderFactory: Send + Sync {
    fn name(&self) -> &str;

    /// Whether this provider supports user-managed custom models.
    fn supports_custom_models(&self) -> bool {
        false
    }

    fn api_key_name(&self) -> Option<String> {
        None
    }

    /// Schema for plugin config
    fn config_schema(&self) -> String;

    /// Build the HTTP request that lists models.
    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError>;

    /// Turn the raw HTTP response into a Vec<String>.
    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError>;

    /// Given a chosen model name, build a sync `HttpLLMProvider`
    // FIXME: refactor to follow rust standards
    #[allow(clippy::wrong_self_convention)]
    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError>;
}

#[allow(improper_ctypes_definitions)]
pub type HTTPFactoryCtor = unsafe extern "C" fn() -> *mut dyn HTTPLLMProviderFactory;

#[macro_export]
macro_rules! handle_http_error {
    ($resp:expr) => {{
        if !$resp.status().is_success() {
            let status_code = $resp.status().as_u16();
            let headers = $resp.headers().clone();
            let error_body = $resp.into_body();
            return Err($crate::error::classify_http_status(
                status_code,
                &headers,
                &error_body,
            ));
        }
    }};
}
