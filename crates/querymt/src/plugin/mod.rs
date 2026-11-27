use crate::{LLMProvider, error::LLMError};
use futures::future::BoxFuture;

pub mod account;
pub use account::{AccountReport, CreditBalance, ProviderAccount, UsageLimit};

#[cfg(feature = "http-client")]
pub mod adapters;

pub mod http;
pub use http::HTTPFactoryCtor;
pub use http::HTTPLLMProviderFactory;

#[cfg(feature = "plugin_host")]
pub mod host;

#[cfg(any(feature = "extism_host", feature = "extism_plugin"))]
pub mod extism_impl;

pub mod plugin_log;

pub type Fut<'a, T> = BoxFuture<'a, T>;

/// Native Rust plugin ABI version required by this QueryMT release.
///
/// Version 2 includes item-aware chat response and streaming contracts.
/// Version 3 adds the account methods to the factory traits.
pub const NATIVE_PLUGIN_API: u32 = 3;

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

    /// Whether this provider supports user-managed custom models.
    /// Examples: llama_cpp (GGUF files), ollama (pulled models), mrs (local models)
    fn supports_custom_models(&self) -> bool {
        false
    }

    /// Report plan, usage limits and credits for the credential in `cfg`.
    ///
    /// `cfg` is the same resolved configuration [`from_config`](Self::from_config)
    /// accepts. This is the dispatch point for HTTP factories, whose adapters
    /// implement it over
    /// [`HTTPLLMProviderFactory::account_request`](http::HTTPLLMProviderFactory::account_request);
    /// other factories inherit [`AccountReport::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns the classified HTTP error for a non-success account response, or
    /// the parser's error for a body it cannot read.
    fn account<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<AccountReport, LLMError>> {
        Box::pin(async { Ok(AccountReport::Unsupported) })
    }
}

#[allow(improper_ctypes_definitions)]
pub type FactoryCtor = unsafe extern "C" fn() -> *mut dyn LLMProviderFactory;
