//! OAuth flow handlers — thin UI adapters over [`crate::auth::service`].
//!
//! All core OAuth logic (flow start, code exchange, credential persistence,
//! callback listener) lives in the shared service module. These handlers
//! translate between UI WebSocket messages and service calls.

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::UiServerMessage;
use super::models::{handle_list_all_models, handle_list_auth_providers};
use crate::auth::service;
use tokio::sync::mpsc;

// ── Public-facing handlers ────────────────────────────────────────────────────

/// Start OAuth login flow for a provider.
pub async fn handle_start_oauth_login(
    state: &ServerState,
    conn_id: &str,
    provider: &str,
    tx: &mpsc::Sender<String>,
) {
    let state_for_notify = state.clone();
    let tx_for_notify = tx.clone();
    let notifier: service::AutoCompleteNotifier = std::sync::Arc::new(move |result| {
        let state = state_for_notify.clone();
        let tx = tx_for_notify.clone();
        tokio::spawn(async move {
            let _ = send_message(
                &tx,
                UiServerMessage::OAuthResult {
                    provider: result.provider.clone(),
                    success: result.success,
                    message: result.message,
                },
            )
            .await;

            if result.success {
                handle_list_auth_providers(&state, &tx).await;
                handle_list_all_models(&state, true, &tx).await;
            }
        });
    });

    match state
        .oauth_service
        .start_flow(conn_id, provider, Some(notifier))
        .await
    {
        Ok(result) => {
            let flow_kind = match result.flow_kind.as_str() {
                "redirect_code" => querymt_utils::OAuthFlowKind::RedirectCode,
                _ => querymt_utils::OAuthFlowKind::DevicePoll,
            };
            let _ = send_message(
                tx,
                UiServerMessage::OAuthFlowStarted {
                    flow_id: result.flow_id,
                    provider: result.provider,
                    authorization_url: result.authorization_url,
                    flow_kind,
                },
            )
            .await;
        }
        Err(err) => {
            let _ = send_error(tx, err).await;
        }
    }
}

/// Complete OAuth login flow using callback URL or auth code pasted by user.
pub async fn handle_complete_oauth_login(
    state: &ServerState,
    conn_id: &str,
    flow_id: &str,
    response: &str,
    tx: &mpsc::Sender<String>,
) {
    let result = state
        .oauth_service
        .complete_flow(conn_id, flow_id, response)
        .await;

    let _ = send_message(
        tx,
        UiServerMessage::OAuthResult {
            provider: result.provider,
            success: result.success,
            message: result.message,
        },
    )
    .await;

    if result.success {
        handle_list_auth_providers(state, tx).await;
        handle_list_all_models(state, true, tx).await;
    }
}

/// Disconnect OAuth credentials for a provider.
pub async fn handle_disconnect_oauth(
    state: &ServerState,
    conn_id: &str,
    provider: &str,
    tx: &mpsc::Sender<String>,
) {
    let result = state.oauth_service.logout(conn_id, provider).await;

    let _ = send_message(
        tx,
        UiServerMessage::OAuthResult {
            provider: result.provider,
            success: result.success,
            message: result.message,
        },
    )
    .await;

    if result.success {
        handle_list_auth_providers(state, tx).await;
        handle_list_all_models(state, true, tx).await;
    }
}

/// Stop the active OAuth callback listener for a given connection (called on disconnect).
pub(crate) async fn stop_oauth_callback_listener_for_connection(
    state: &ServerState,
    conn_id: &str,
) {
    state.oauth_service.cleanup_owner(conn_id).await;
}
