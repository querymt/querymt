use crate::{error::LLMError, LLMProvider};
use futures::future::BoxFuture;
use serde_json::Value;
use std::path::PathBuf;

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

#[cfg(feature = "extism_host")]
pub fn default_providers_path() -> PathBuf {
    if let Some(home_dir) = dirs::home_dir() {
        return home_dir.join(".qmt").join("providers.toml");
    }
    if let Some(config_dir) = dirs::config_dir() {
        return config_dir.join("qmt").join("providers.toml");
    }
    PathBuf::from(".qmt").join("providers.toml")
}

#[cfg(not(feature = "extism_host"))]
pub fn default_providers_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        return PathBuf::from(home).join(".qmt").join("providers.toml");
    }
    PathBuf::from(".qmt").join("providers.toml")
}

pub trait LLMProviderFactory: Send + Sync {
    fn name(&self) -> &str;
    fn config_schema(&self) -> Value;
    // FIXME: refactor
    #[allow(clippy::wrong_self_convention)]
    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError>;

    fn list_models<'a>(&'a self, cfg: &Value) -> Fut<'a, Result<Vec<String>, LLMError>>;

    fn as_http(&self) -> Option<&dyn http::HTTPLLMProviderFactory> {
        None
    }
}

#[allow(improper_ctypes_definitions)]
pub type FactoryCtor = unsafe extern "C" fn() -> *mut dyn LLMProviderFactory;
