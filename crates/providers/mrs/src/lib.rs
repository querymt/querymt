mod chat;
mod config;
mod factory;
mod messages;
mod model;
mod streaming;
mod tools;

pub use config::{
    MistralRSConfig, MistralRSDeviceMap, MistralRSModelKind, MistralRSPagedCacheType,
};
pub use factory::create_factory;
pub use model::MistralRS;

/// Initialize logging from the host process.
///
/// This function is called by the host after loading the plugin via dlopen.
/// It sets up a logger that forwards all `log` crate calls from this plugin
/// back to the host's logger, enabling `RUST_LOG` filtering to work for the plugin.
///
/// # Safety
///
/// The `callback` function pointer must remain valid for the lifetime of the plugin.
/// This should only be called once per plugin load (the host ensures this).
/// The callback must be thread-safe.
#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_init_logging(
    callback: querymt::plugin::LogCallbackFn,
    max_level: usize,
) {
    unsafe {
        querymt::plugin::plugin_log::init_from_host(callback, max_level);
    }
}

#[cfg(test)]
mod tests;
