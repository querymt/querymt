//! Message handlers for UI client requests.
//!
//! The dispatch entry-point `handle_ui_message` lives here; each domain of
//! functionality is split into its own submodule:
//!
//! - [`models`]      — model listing, selection, and API-key resolution
//! - [`oauth`]       — OAuth flow, callback listener, credential persistence
//! - [`session_ops`] — session list/load/cancel/subscribe/undo/redo/elicitation/mode
//! - [`remote`]      — remote node and session management

mod audio;
mod knowledge;
mod models;
mod oauth;
mod plugins;
mod remote;
mod schedules;
mod session_ops;

// ── Re-exports consumed by sibling modules ────────────────────────────────────

pub use models::handle_add_custom_model_from_file;
pub use models::handle_add_custom_model_from_hf;
pub use models::handle_delete_custom_model;
pub use models::handle_list_all_models;
pub use models::handle_list_auth_providers;
pub use oauth::handle_complete_oauth_login;
pub use oauth::handle_disconnect_oauth;
pub use oauth::handle_start_oauth_login;
pub(crate) use oauth::stop_oauth_callback_listener_for_connection;
pub use plugins::handle_update_plugins;
pub use remote::handle_attach_remote_session;
pub use remote::handle_create_mesh_invite;
pub use remote::handle_create_remote_session;
pub use remote::handle_dismiss_remote_session;
pub use remote::handle_list_mesh_invites;
pub use remote::handle_list_remote_nodes;
pub use remote::handle_list_remote_sessions;
pub use remote::handle_revoke_mesh_invite;
pub use schedules::handle_create_schedule;
pub use schedules::handle_delete_schedule;
pub use schedules::handle_list_schedules;
pub use schedules::handle_pause_schedule;
pub use schedules::handle_resume_schedule;
pub use schedules::handle_trigger_schedule;
pub use session_ops::handle_cancel_session;
pub use session_ops::handle_delete_session;
pub use session_ops::handle_elicitation_response;
pub use session_ops::handle_fork_session;
pub use session_ops::handle_get_agent_mode;
pub use session_ops::handle_get_file_index;
pub use session_ops::handle_get_llm_config;
pub use session_ops::handle_get_reasoning_effort;
pub use session_ops::handle_list_sessions;
pub use session_ops::handle_load_session;
pub use session_ops::handle_redo;
pub use session_ops::handle_set_agent_mode;
pub use session_ops::handle_set_reasoning_effort;
pub use session_ops::handle_subscribe_session;
pub use session_ops::handle_undo;
pub use session_ops::handle_unsubscribe_session;
#[cfg(all(test, feature = "remote"))]
pub(crate) use session_ops::refresh_attached_remote_summary;

use super::ServerState;
use super::connection::{send_error, send_state};
use super::messages::{UiClientMessage, UiPromptBlock};
use super::session::{ensure_sessions_for_mode, prompt_for_mode, resolve_cwd};
use models::{handle_get_recent_models, handle_set_session_model};
use std::time::Instant;

use crate::session::projection::SessionScope;
use tokio::sync::mpsc;

// ── Main dispatch ─────────────────────────────────────────────────────────────

/// Main message dispatch handler.
pub async fn handle_ui_message(
    state: &ServerState,
    conn_id: &str,
    tx: &mpsc::Sender<String>,
    bin_tx: &mpsc::Sender<Vec<u8>>,
    msg: UiClientMessage,
) {
    match msg {
        UiClientMessage::Init => {
            let started = Instant::now();
            send_state(state, conn_id, tx).await;
            let send_state_ms = started.elapsed().as_millis() as u64;

            handle_list_sessions(
                state,
                tx,
                None,
                None,
                None,
                None,
                None,
                Some(SessionScope::Root),
            )
            .await;
            audio::handle_audio_capabilities(state, tx).await;
            tracing::info!(
                target: "querymt_agent::ui::handlers",
                operation = "ui.init",
                send_state_ms,
                total_ms = started.elapsed().as_millis() as u64,
                "ui init completed"
            );
        }
        UiClientMessage::SetActiveAgent { agent_id } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.active_agent_id = agent_id;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::SetRoutingMode { mode } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.routing_mode = mode;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::NewSession { cwd, request_id } => {
            let cwd = resolve_cwd(cwd).or_else(|| state.default_cwd.clone());

            // Clear existing sessions for this connection to start fresh
            {
                let mut connections = state.connections.lock().await;
                if let Some(conn) = connections.get_mut(conn_id) {
                    let session_ids: Vec<String> = conn.sessions.values().cloned().collect();
                    conn.sessions.clear();

                    drop(connections);

                    let mut agents = state.session_agents.lock().await;
                    let mut cwds = state.session_cwds.lock().await;
                    for sid in session_ids {
                        agents.remove(&sid);
                        cwds.remove(&sid);
                    }
                }
            }

            if let Err(err) =
                ensure_sessions_for_mode(state, conn_id, cwd.as_ref(), tx, request_id.as_deref())
                    .await
            {
                let _ = send_error(tx, err).await;
            }

            handle_list_sessions(
                state,
                tx,
                None,
                None,
                None,
                None,
                None,
                Some(SessionScope::Root),
            )
            .await;
        }
        UiClientMessage::Prompt { prompt } => {
            let has_user_text = prompt.iter().any(|block| match block {
                UiPromptBlock::Text { text } => !text.trim().is_empty(),
                _ => false,
            });
            if !has_user_text {
                return;
            }
            // Spawn prompt execution on a separate task so the WebSocket receive
            // loop continues processing messages (crucially, CancelSession).
            let state = state.clone();
            let conn_id = conn_id.to_string();
            let tx = tx.clone();
            tokio::spawn(async move {
                let cwd = resolve_cwd(None);
                if let Err(err) =
                    prompt_for_mode(&state, &conn_id, &prompt, cwd.as_ref(), &tx).await
                {
                    log::error!("prompt_for_mode failed: {}", err);
                    let _ = super::connection::send_error(&tx, err).await;
                }
                handle_list_sessions(
                    &state,
                    &tx,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(SessionScope::Root),
                )
                .await;
            });
        }
        UiClientMessage::ListSessions {
            mode,
            cursor,
            limit,
            cwd,
            query,
            session_scope,
        } => {
            handle_list_sessions(state, tx, mode, cursor, limit, cwd, query, session_scope).await;
        }
        UiClientMessage::LoadSession { session_id } => {
            handle_load_session(state, conn_id, &session_id, tx).await;
        }
        UiClientMessage::DeleteSession { session_id } => {
            handle_delete_session(state, conn_id, &session_id, tx).await;
        }
        UiClientMessage::ListAllModels { refresh } => {
            handle_list_all_models(state, refresh, tx).await;
        }
        UiClientMessage::GetRecentModels {
            limit_per_workspace,
        } => {
            let limit = limit_per_workspace.unwrap_or(10) as usize;
            handle_get_recent_models(state, limit, tx).await;
        }
        UiClientMessage::SetSessionModel {
            session_id,
            model_id,
            node_id,
        } => {
            if let Err(err) =
                handle_set_session_model(state, &session_id, &model_id, node_id.as_deref()).await
            {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::GetFileIndex => {
            handle_get_file_index(state, conn_id, tx).await;
        }
        UiClientMessage::GetLlmConfig { config_id } => {
            handle_get_llm_config(state, config_id, tx).await;
        }
        UiClientMessage::CancelSession => {
            handle_cancel_session(state, conn_id, tx).await;
        }
        UiClientMessage::Undo { message_id } => {
            handle_undo(state, conn_id, &message_id, tx).await;
        }
        UiClientMessage::Redo => {
            handle_redo(state, conn_id, tx).await;
        }
        UiClientMessage::ForkSession { message_id } => {
            handle_fork_session(state, conn_id, &message_id, tx).await;
        }
        UiClientMessage::SubscribeSession {
            session_id,
            agent_id,
        } => {
            handle_subscribe_session(state, conn_id, &session_id, agent_id.as_deref(), tx).await;
        }
        UiClientMessage::UnsubscribeSession { session_id } => {
            handle_unsubscribe_session(state, conn_id, &session_id).await;
        }
        UiClientMessage::ElicitationResponse {
            elicitation_id,
            action,
            content,
        } => {
            handle_elicitation_response(state, &elicitation_id, &action, content.as_ref()).await;
        }
        UiClientMessage::ListAuthProviders => {
            handle_list_auth_providers(state, tx).await;
        }
        UiClientMessage::StartOAuthLogin { provider } => {
            handle_start_oauth_login(state, conn_id, &provider, tx).await;
        }
        UiClientMessage::CompleteOAuthLogin { flow_id, response } => {
            handle_complete_oauth_login(state, conn_id, &flow_id, &response, tx).await;
        }
        UiClientMessage::DisconnectOAuth { provider } => {
            handle_disconnect_oauth(state, conn_id, &provider, tx).await;
        }
        UiClientMessage::SetAgentMode { mode } => {
            handle_set_agent_mode(state, conn_id, &mode, tx).await;
        }
        UiClientMessage::GetAgentMode => {
            handle_get_agent_mode(state, conn_id, tx).await;
        }
        UiClientMessage::SetReasoningEffort { reasoning_effort } => {
            handle_set_reasoning_effort(state, conn_id, &reasoning_effort, tx).await;
        }
        UiClientMessage::GetReasoningEffort => {
            handle_get_reasoning_effort(state, conn_id, tx).await;
        }
        UiClientMessage::ListRemoteNodes => {
            handle_list_remote_nodes(state, tx).await;
        }
        UiClientMessage::ListRemoteSessions { node_id } => {
            handle_list_remote_sessions(state, &node_id, tx).await;
        }
        UiClientMessage::CreateRemoteSession {
            node_id,
            cwd,
            request_id,
        } => {
            handle_create_remote_session(
                state,
                conn_id,
                &node_id,
                cwd.as_deref(),
                request_id.as_deref(),
                tx,
            )
            .await;
        }
        UiClientMessage::AttachRemoteSession {
            node_id,
            session_id,
        } => {
            handle_attach_remote_session(state, conn_id, &node_id, &session_id, tx).await;
        }
        UiClientMessage::DismissRemoteSession { session_id } => {
            handle_dismiss_remote_session(state, &session_id, tx).await;
        }
        UiClientMessage::CreateMeshInvite {
            mesh_name,
            ttl,
            max_uses,
        } => {
            handle_create_mesh_invite(state, mesh_name, ttl, max_uses, tx).await;
        }
        UiClientMessage::ListMeshInvites => {
            handle_list_mesh_invites(state, tx).await;
        }
        UiClientMessage::RevokeMeshInvite { invite_id } => {
            handle_revoke_mesh_invite(state, &invite_id, tx).await;
        }
        UiClientMessage::AddCustomModelFromHf {
            provider,
            repo,
            filename,
            display_name,
        } => {
            if let Err(err) = handle_add_custom_model_from_hf(
                state,
                &provider,
                &repo,
                &filename,
                display_name,
                tx,
            )
            .await
            {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::AddCustomModelFromFile {
            provider,
            file_path,
            display_name,
        } => {
            if let Err(err) =
                handle_add_custom_model_from_file(state, &provider, &file_path, display_name).await
            {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::DeleteCustomModel { provider, model_id } => {
            if let Err(err) = handle_delete_custom_model(state, &provider, &model_id).await {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::SetApiToken { provider, api_key } => {
            models::handle_set_api_token(state, &provider, &api_key, tx).await;
        }
        UiClientMessage::ClearApiToken { provider } => {
            models::handle_clear_api_token(state, &provider, tx).await;
        }
        UiClientMessage::SetAuthMethod { provider, method } => {
            models::handle_set_auth_method(state, &provider, &method, tx).await;
        }
        UiClientMessage::UpdatePlugins => {
            handle_update_plugins(state, tx).await;
        }
        UiClientMessage::CreateSchedule {
            session_id,
            prompt,
            trigger,
            max_steps,
            max_cost_usd,
            max_runs,
        } => {
            let params = schedules::CreateScheduleParams {
                session_id: &session_id,
                prompt: &prompt,
                trigger_json: &trigger,
                max_steps,
                max_cost_usd,
                max_runs,
            };
            handle_create_schedule(state, &params, tx).await;
        }
        UiClientMessage::ListSchedules { session_id } => {
            handle_list_schedules(state, session_id.as_deref(), tx).await;
        }
        UiClientMessage::PauseSchedule { schedule_public_id } => {
            handle_pause_schedule(state, &schedule_public_id, tx).await;
        }
        UiClientMessage::ResumeSchedule { schedule_public_id } => {
            handle_resume_schedule(state, &schedule_public_id, tx).await;
        }
        UiClientMessage::TriggerSchedule { schedule_public_id } => {
            handle_trigger_schedule(state, &schedule_public_id, tx).await;
        }
        UiClientMessage::DeleteSchedule { schedule_public_id } => {
            handle_delete_schedule(state, &schedule_public_id, tx).await;
        }
        UiClientMessage::QueryKnowledge {
            scope,
            question,
            limit,
        } => {
            knowledge::handle_query_knowledge(state, &scope, &question, limit, tx).await;
        }
        UiClientMessage::ListKnowledge { scope, filter } => {
            knowledge::handle_list_knowledge(state, &scope, filter.as_ref(), tx).await;
        }
        UiClientMessage::KnowledgeStats { scope } => {
            knowledge::handle_knowledge_stats(state, &scope, tx).await;
        }
        UiClientMessage::Transcribe { .. } => {
            // Transcribe arrives as a binary frame — handled by handle_binary_message.
            // If it arrives as a text frame (no audio payload), return an error.
            let _ = send_error(
                tx,
                "Transcribe must be sent as a binary WebSocket frame".to_string(),
            )
            .await;
        }
        UiClientMessage::Speech {
            provider,
            model,
            text,
            voice,
            format,
        } => {
            audio::handle_speech(
                state,
                &audio::SpeechParams {
                    provider_name: &provider,
                    model: &model,
                    text: &text,
                    voice: voice.as_deref(),
                    format: format.as_deref(),
                },
                tx,
                bin_tx,
            )
            .await;
        }
    }
}

/// Dispatch handler for binary WebSocket frames.
///
/// The binary frame has already been parsed into a JSON header (`msg`) and a
/// raw binary `payload`. Only audio messages are expected here; all other
/// message types are rejected with an error.
pub async fn handle_binary_message(
    state: &ServerState,
    _conn_id: &str,
    tx: &mpsc::Sender<String>,
    _bin_tx: &mpsc::Sender<Vec<u8>>,
    msg: UiClientMessage,
    payload: Vec<u8>,
) {
    match msg {
        UiClientMessage::Transcribe {
            provider,
            model,
            mime_type,
        } => {
            audio::handle_transcribe(state, &provider, &model, payload, mime_type.as_deref(), tx)
                .await;
        }
        other => {
            let type_name = match &other {
                UiClientMessage::Speech { .. } => "speech",
                _ => "unknown",
            };
            let _ = send_error(
                tx,
                format!("Message type '{type_name}' should not be sent as a binary frame"),
            )
            .await;
        }
    }
}
