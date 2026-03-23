//! Handler for OCI plugin update requests.
//!
//! The core update loop lives in [`crate::plugin_update::update_all_plugins`]
//! (always compiled, no feature gate). This module provides the WebSocket-
//! specific wrapper that streams progress messages to the UI client.

use super::super::ServerState;
use super::super::connection::send_message;
use super::super::messages::UiServerMessage;
use crate::plugin_update::update_all_plugins;
use querymt::plugin::host::{OciDownloadPhase, OciDownloadProgress, OciProgressCallback};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Handle an `UpdatePlugins` WebSocket message.
///
/// Builds a progress callback factory that streams `PluginUpdateStatus`
/// messages for each download phase, delegates to [`update_all_plugins`],
/// then sends a single `PluginUpdateComplete` message with the results.
pub async fn handle_update_plugins(state: &ServerState, tx: &mpsc::Sender<String>) {
    let registry = state.agent.config.provider.plugin_registry();

    let tx_outer = tx.clone();
    let progress_factory = move |plugin_name: &str, image_reference: &str| -> OciProgressCallback {
        let tx = tx_outer.clone();
        let plugin_name = plugin_name.to_owned();
        let image_reference = image_reference.to_owned();
        Arc::new(move |p: OciDownloadProgress| {
            let tx = tx.clone();
            let plugin_name = plugin_name.clone();
            let image_reference = image_reference.clone();
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
        })
    };

    let results = update_all_plugins(&registry, Some(&progress_factory)).await;

    // Convert shared PluginUpdateResult → UI PluginUpdateResult for the WS message.
    let ui_results = results
        .into_iter()
        .map(|r| super::super::messages::PluginUpdateResult {
            plugin_name: r.plugin_name,
            success: r.success,
            message: r.message,
        })
        .collect();

    let _ = send_message(
        tx,
        UiServerMessage::PluginUpdateComplete {
            results: ui_results,
        },
    )
    .await;
}
