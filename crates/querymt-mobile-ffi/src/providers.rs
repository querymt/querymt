//! Provider registry helpers for mobile static provider registration.
//!
//! This module is intentionally thin — the heavy lifting is done in `agent.rs`
//! via `PluginRegistry::register_static_http()`. These helpers exist so the FFI
//! can iterate known providers and list their available models.

use querymt::plugin::host::PluginRegistry;

/// Return the list of provider names from the registry.
#[allow(dead_code)]
pub fn list_providers(registry: &PluginRegistry) -> Vec<String> {
    registry
        .list()
        .iter()
        .map(|f| f.name().to_string())
        .collect()
}
