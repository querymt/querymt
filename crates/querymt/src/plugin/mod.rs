use crate::{error::LLMError, LLMProvider};
use futures::future::BoxFuture;
use serde_json::Value;

#[cfg(feature = "http-client")]
pub mod adapters;

pub mod http;
pub use http::HTTPFactoryCtor;
pub use http::HTTPLLMProviderFactory;

#[cfg(any(feature = "extism_host", feature = "native"))]
pub mod host;

#[cfg(any(feature = "extism_host", feature = "extism_plugin"))]
pub mod extism_impl;

pub type Fut<'a, T> = BoxFuture<'a, T>;

pub trait LLMProviderFactory: Send + Sync {
    fn name(&self) -> &str;
    fn config_schema(&self) -> Value;
    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError>;

    fn list_models<'a>(&'a self, cfg: &Value) -> Fut<'a, Result<Vec<String>, LLMError>>;

    fn as_http(&self) -> Option<&dyn http::HTTPLLMProviderFactory> {
        None
    }
}

pub type FactoryCtor = unsafe extern "C" fn() -> *mut dyn LLMProviderFactory;
