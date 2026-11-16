//! Shared utility functions for the simple API

use anyhow::{Result, anyhow};
#[cfg(feature = "plugin-loaders")]
use querymt::dynamic::PluginRegistryDynamicExt;
use querymt::plugin::host::PluginRegistry;
use std::path::PathBuf;

/// Converts a path to an absolute path, canonicalizing if it exists
pub(super) fn to_absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        path.canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize path {:?}: {}", path, e))
    } else {
        std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current directory: {}", e))?
            .join(&path)
            .canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize path {:?}: {}", path, e))
    }
}

pub(super) async fn default_registry() -> Result<PluginRegistry> {
    // Best-effort bootstrap of provider metadata used by plugin initialization paths.
    if let Err(err) = querymt::providers::update_providers_if_stale().await {
        log::warn!("Failed to update providers metadata cache: {}", err);
    }

    let cfg_path = querymt_utils::providers::get_providers_config(None).await?;
    #[cfg(feature = "plugin-loaders")]
    {
        PluginRegistry::from_path(&cfg_path)
            .map(|registry| registry.with_dynamic_loaders())
            .map_err(|e| anyhow!("Failed to load plugin registry: {}", e))
    }

    #[cfg(not(feature = "plugin-loaders"))]
    {
        PluginRegistry::from_path(&cfg_path)
            .map_err(|e| anyhow!("Failed to load plugin registry: {}", e))
    }
}

pub(super) use crate::agent::utils::latest_assistant_message;
