//! Handler for OCI plugin update requests.

use super::super::ServerState;
use super::super::connection::send_message;
use super::super::messages::{PluginUpdateResult, UiServerMessage};
use querymt::plugin::host::{OciDownloadPhase, OciDownloadProgress, OciProgressCallback};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Handle an `UpdatePlugins` WebSocket message.
///
/// Force-updates every OCI-based provider plugin in the registry, streaming
/// `PluginUpdateStatus` messages for each download phase and finishing with a
/// single `PluginUpdateComplete` message.
pub async fn handle_update_plugins(state: &ServerState, tx: &mpsc::Sender<String>) {
    let registry = state.agent.config.provider.plugin_registry();
    let mut results: Vec<PluginUpdateResult> = Vec::new();

    for provider_cfg in &registry.config.providers {
        let Some(image_ref) = provider_cfg.path.strip_prefix("oci://") else {
            continue;
        };

        let name = provider_cfg.name.clone();
        let image_ref_owned = image_ref.to_string();
        let tx_clone = tx.clone();
        let name_for_cb = name.clone();
        let image_ref_for_cb = image_ref_owned.clone();

        let progress: OciProgressCallback = Arc::new(move |p: OciDownloadProgress| {
            let tx = tx_clone.clone();
            let plugin_name = name_for_cb.clone();
            let image_reference = image_ref_for_cb.clone();
            let phase = match &p.phase {
                OciDownloadPhase::Resolving => "resolving",
                OciDownloadPhase::VerifyingSignature => "verifying_signature",
                OciDownloadPhase::Downloading => "downloading",
                OciDownloadPhase::Extracting => "extracting",
                OciDownloadPhase::Persisting => "persisting",
                OciDownloadPhase::Completed => "completed",
                OciDownloadPhase::Failed(_) => "failed",
            }
            .to_string();
            let message = match &p.phase {
                OciDownloadPhase::Failed(msg) => Some(msg.clone()),
                _ => None,
            };
            tokio::spawn(async move {
                let _ = send_message(
                    &tx,
                    UiServerMessage::PluginUpdateStatus {
                        plugin_name,
                        image_reference,
                        phase,
                        bytes_downloaded: p.bytes_downloaded,
                        bytes_total: p.bytes_total,
                        percent: p.percent,
                        message,
                    },
                )
                .await;
            });
        });

        let result = registry
            .oci_downloader
            .pull_and_extract(
                &image_ref_owned,
                None,
                &registry.cache_path,
                true,
                Some(progress),
            )
            .await;

        results.push(PluginUpdateResult {
            plugin_name: name,
            success: result.is_ok(),
            message: result.err().map(|e| e.to_string()),
        });
    }

    let _ = send_message(tx, UiServerMessage::PluginUpdateComplete { results }).await;
}
