use crate::{
    error::LLMError,
    plugin::{
        adapters::HTTPFactoryAdapter,
        host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin},
        FactoryCtor, HTTPFactoryCtor, HTTPLLMProviderFactory, LLMProviderFactory,
        PluginInitLoggingFn,
    },
};
use async_trait::async_trait;
use libloading::Library;
use std::ffi::CStr;
use std::path::Path;
use std::sync::Arc;
use tracing::instrument;

struct NativeFactoryWrapper {
    factory_impl: Box<dyn LLMProviderFactory>,
    _library: Arc<Library>, // The underscore indicates we hold it just for its lifetime
}

// Manually implement the trait for your wrapper
impl LLMProviderFactory for NativeFactoryWrapper {
    fn name(&self) -> &str {
        self.factory_impl.name()
    }

    fn supports_custom_models(&self) -> bool {
        self.factory_impl.supports_custom_models()
    }

    fn config_schema(&self) -> String {
        self.factory_impl.config_schema()
    }
    fn from_config(&self, cfg: &str) -> Result<Box<dyn crate::LLMProvider>, LLMError> {
        self.factory_impl.from_config(cfg)
    }

    fn list_models<'a>(
        &'a self,
        cfg: &str,
    ) -> crate::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
        self.factory_impl.list_models(cfg)
    }
}

/// Host-side logging callback that forwards plugin log calls to the host's logger.
///
/// This function is passed to native plugins via their `plugin_init_logging` export.
/// It receives log calls from the plugin and forwards them to the host's `log` crate,
/// which is bridged to `tracing` and respects `RUST_LOG` filtering.
///
/// # Safety
///
/// This function is `unsafe extern "C"` because it's called across FFI from the plugin.
/// The `target` and `message` pointers must be valid null-terminated C strings.
unsafe extern "C" fn host_log_callback(
    level: usize,
    target: *const std::ffi::c_char,
    message: *const std::ffi::c_char,
) {
    // Convert C strings to Rust strings, with fallbacks for invalid pointers
    let target_str = if target.is_null() {
        "plugin"
    } else {
        CStr::from_ptr(target).to_str().unwrap_or("plugin")
    };

    let message_str = if message.is_null() {
        ""
    } else {
        CStr::from_ptr(message).to_str().unwrap_or("")
    };

    // Convert usize level to log::Level
    let log_level = match level {
        1 => log::Level::Error,
        2 => log::Level::Warn,
        3 => log::Level::Info,
        4 => log::Level::Debug,
        5 => log::Level::Trace,
        _ => return, // Invalid level, ignore
    };

    // Forward to the host's logger with the plugin's target
    log::log!(target: target_str, log_level, "{}", message_str);
}

pub struct NativeLoader;

#[async_trait]
impl PluginLoader for NativeLoader {
    fn supported_type(&self) -> PluginType {
        PluginType::Native
    }

    #[instrument(name = "native_loader.load_plugin", skip_all, fields(plugin = %plugin.file_path.display(), name = %plugin_cfg.name))]
    async fn load_plugin(
        &self,
        plugin: ProviderPlugin,
        plugin_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        log::info!(
            "Loading native plugin '{}' from {}",
            plugin_cfg.name,
            plugin.file_path.display()
        );

        let provider = self.load_library(&plugin_cfg.name, &plugin.file_path)?;
        Ok(provider)
    }
}

impl NativeLoader {
    fn load_library(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        let lib = unsafe {
            Arc::new(Library::new(path).map_err(|e| LLMError::PluginError(format!("{:#}", e)))?)
        };

        let factory: Box<dyn LLMProviderFactory> = unsafe {
            if let Ok(async_ctor) = lib.get::<FactoryCtor>(b"plugin_factory") {
                let raw = async_ctor();
                if raw.is_null() {
                    return Err(LLMError::PluginError(format!(
                        "plugin_factory returned null in {}",
                        path.display()
                    )));
                }
                Box::from_raw(raw)
            } else if let Ok(sync_ctor) = lib.get::<HTTPFactoryCtor>(b"plugin_http_factory") {
                let raw: *mut dyn HTTPLLMProviderFactory = sync_ctor();
                if raw.is_null() {
                    return Err(LLMError::PluginError(format!(
                        "plugin_http_factory returned null in {}",
                        path.display()
                    )));
                }
                let sync_fact: Box<dyn HTTPLLMProviderFactory> = Box::from_raw(raw);
                let async_fact = HTTPFactoryAdapter::new(Arc::from(sync_fact));
                Box::new(async_fact)
            } else {
                return Err(LLMError::PluginError(format!(
                    "no plugin_factory or plugin_http_factory in {}",
                    path.display()
                )));
            }
        };

        let factory_name = factory.name();
        if factory_name != name {
            log::warn!(
                "Plugin name mismatch in {}: config name is '{}', but plugin reports '{}'. Using config name.",
                path.display(),
                name,
                factory_name
            );
        }

        // Optionally initialize logging for the plugin
        // If the plugin doesn't export `plugin_init_logging`, this is a no-op
        unsafe {
            if let Ok(init_logging) = lib.get::<PluginInitLoggingFn>(b"plugin_init_logging") {
                let max_level = log::max_level() as usize;
                init_logging(host_log_callback, max_level);
                log::debug!(
                    "Initialized logging for native plugin '{}' with max_level={:?}",
                    name,
                    log::max_level()
                );
            }
        }

        Ok(Arc::new(NativeFactoryWrapper {
            factory_impl: factory,
            _library: Arc::clone(&lib),
        }))
    }
}
