//! Shared OCI plugin update logic.
//!
//! This module is always compiled (no feature gates) so that both the ACP
//! `querymt/updatePlugins` ext-method and the dashboard WebSocket handler can
//! share the same core update loop.

use querymt::plugin::host::{OciProgressCallback, PluginRegistry};
use serde::{Deserialize, Serialize};

/// A factory that creates a per-plugin [`OciProgressCallback`].
///
/// Called once per OCI plugin with `(plugin_name, image_reference)` so that
/// callers can build transport-specific progress reporters (e.g. WebSocket
/// status messages). Pass `None` to [`update_all_plugins`] when no streaming
/// progress is needed.
pub type ProgressCallbackFactory = dyn Fn(&str, &str) -> OciProgressCallback + Send + Sync;

/// Result of updating a single OCI plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUpdateResult {
    pub plugin_name: String,
    pub success: bool,
    pub message: Option<String>,
}

/// Force-update every OCI-based provider plugin in `registry`.
///
/// Returns per-plugin results. An optional `progress_factory` can be
/// supplied for streaming progress to the caller; when `None`, downloads run
/// without progress reporting.
pub async fn update_all_plugins(
    registry: &PluginRegistry,
    progress_factory: Option<&ProgressCallbackFactory>,
) -> Vec<PluginUpdateResult> {
    let mut results: Vec<PluginUpdateResult> = Vec::new();

    for provider_cfg in &registry.config.providers {
        let Some(image_ref) = provider_cfg.path.strip_prefix("oci://") else {
            continue;
        };

        let name = provider_cfg.name.clone();
        let image_ref_owned = image_ref.to_string();

        let progress = progress_factory.map(|f| f(&name, &image_ref_owned));

        let result = registry
            .oci_downloader
            .pull_and_extract(&image_ref_owned, None, &registry.cache_path, true, progress)
            .await;

        results.push(PluginUpdateResult {
            plugin_name: name,
            success: result.is_ok(),
            message: result.err().map(|e| e.to_string()),
        });
    }

    results
}
