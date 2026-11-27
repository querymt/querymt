use crate::{HTTPLLMProvider, error::LLMError, plugin::account::ProviderAccount};
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

    /// Return model names without network access when provider has a static/model-local catalog.
    ///
    /// Returning `Some` short-circuits HTTP list-models flow.
    fn list_models_static(&self, _cfg: &str) -> Option<Result<Vec<String>, LLMError>> {
        None
    }

    /// Build the HTTP request that lists models.
    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError>;

    /// Turn the raw HTTP response into a Vec<String>.
    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError>;

    /// Given a chosen model name, build a sync `HttpLLMProvider`
    // FIXME: refactor to follow rust standards
    #[allow(clippy::wrong_self_convention)]
    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError>;

    /// Build the request for the account snapshot of the credential in `cfg`,
    /// or `None` when this provider or this credential has nothing to report
    /// (most inference API keys, local providers).
    ///
    /// # Errors
    ///
    /// Returns an error when `cfg` cannot be read or the credential is unusable.
    fn account_request(&self, _cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
        Ok(None)
    }

    /// Turn the account endpoint's success response into a snapshot.
    ///
    /// The host classifies non-success statuses before calling this.
    ///
    /// # Errors
    ///
    /// Returns [`LLMError::ResponseFormatError`] for a body this provider cannot read.
    fn parse_account(&self, _resp: Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
        Err(LLMError::NotImplemented("account snapshot".into()))
    }

    /// Hosts other than the base-URL host that [`account_request`](Self::account_request)
    /// may target. A Wasm host reads this once at load and allows these hosts for
    /// account requests only.
    fn account_hosts(&self) -> Vec<String> {
        Vec::new()
    }
}

#[allow(improper_ctypes_definitions)]
pub type HTTPFactoryCtor = unsafe extern "C" fn() -> *mut dyn HTTPLLMProviderFactory;

/// Classify a non-success HTTP response with the **generic** status classifier
/// (legacy [`crate::error::LLMError`] variants).
///
/// Use this on paths the HTTP adapter does **not** pre-classify — e.g.
/// completions, embeddings, STT/TTS, list-models.
///
/// **Do not** use on chat `parse_chat` / stream parsers. Chat non-success is
/// owned by [`crate::chat::http::HTTPChatProvider::classify_chat_error`].
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
