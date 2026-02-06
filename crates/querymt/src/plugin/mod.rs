use crate::{error::LLMError, LLMProvider};
use futures::future::BoxFuture;
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

pub mod plugin_log;

pub type Fut<'a, T> = BoxFuture<'a, T>;

/// FFI-safe logging callback that native plugins can use to forward log messages
/// to the host process logger.
///
/// Parameters:
/// - level: log level as usize (Error=1, Warn=2, Info=3, Debug=4, Trace=5)
/// - target: null-terminated C string for the log target (e.g. "qmt_llama_cpp")
/// - message: null-terminated C string for the log message
#[allow(improper_ctypes_definitions)]
pub type LogCallbackFn = unsafe extern "C" fn(
    level: usize,
    target: *const std::ffi::c_char,
    message: *const std::ffi::c_char,
);

/// Type for the optional `plugin_init_logging` symbol in native plugins.
///
/// Parameters:
/// - callback: the logging function pointer
/// - max_level: maximum log level filter as usize (Off=0, Error=1, ..., Trace=5)
#[allow(improper_ctypes_definitions)]
pub type PluginInitLoggingFn = unsafe extern "C" fn(callback: LogCallbackFn, max_level: usize);

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
    fn config_schema(&self) -> String;
    // FIXME: refactor
    #[allow(clippy::wrong_self_convention)]
    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError>;

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>>;

    fn as_http(&self) -> Option<&dyn http::HTTPLLMProviderFactory> {
        None
    }
}

#[allow(improper_ctypes_definitions)]
pub type FactoryCtor = unsafe extern "C" fn() -> *mut dyn LLMProviderFactory;
