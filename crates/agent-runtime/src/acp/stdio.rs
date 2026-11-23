//! SDK-based stdio server with bidirectional client bridge.
//!
//! This module implements the full bidirectional communication pattern:
//! - Client → Agent: via `AgentSideConnection` calling `Agent` trait methods
//! - Agent → Client: via channel bridge forwarding to `AgentSideConnection` as `Client`
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────┐         ┌─────────────────────────────────────┐
//! │   ACP Connection    │◄────────│             Bridge Task             │
//! │  - Client requests  │         │  - Receives ClientBridgeMessage    │
//! │  - Agent responses  │         │  - Uses ACP-managed spawned tasks  │
//! └──────────┬──────────┘         └──────────────────▲──────────────────┘
//!            │                                        │
//!            │ calls Agent                            │ mpsc::channel
//!            ▼                                        │
//! ┌─────────────────────┐                ┌─────────────┴─────────────┐
//! │  ApcAgentAdapter    │───────────────►│ QueryMTAgent (Send+Sync) │
//! └─────────────────────┘                │ - Holds bridge sender    │
//!                                        └───────────────────────────┘
//! ```

use crate::acp::client_bridge::{
    ClientBridgeMessage, ClientBridgeSender, NotificationForwardingState,
};
use crate::acp::protocol::{
    AgentRequest, AuthenticateRequest, CancelNotification, ClientNotification, ClientRequest,
    CloseSessionRequest, DeleteSessionRequest, ExtRequest, ForkSessionRequest, InitializeRequest,
    ListSessionsRequest, LoadSessionRequest, LoadSessionResponse, NewSessionRequest, PromptRequest,
    ResumeSessionRequest, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, SetSessionModelRequest,
};
use crate::acp::shared::{
    AcpLiveEventTranslator, QMT_NOTIFICATION_DELEGATION_UPDATE, QMT_NOTIFICATION_INPUT_STATE,
    convert_elicitation_response, create_elicitation_request, input_state_from_event,
};
use crate::acp::shutdown;
use crate::send_agent::SendAgent;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{self as acp, ByteStreams};
use querymt_agent_services::event_fanout::EventFanout;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{Instrument, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Result of the native ACP session/load core and history replay preparation.
#[derive(Debug)]
pub struct AcpSessionLoadOutcome {
    pub response: LoadSessionResponse,
    pub notifications: Vec<SessionNotification>,
    pub event_count: usize,
    /// The last event the answer (snapshot or replay) contains.
    #[doc(hidden)]
    pub cut: Option<crate::acp::delivery::LoadCut>,
}

fn stored_user_prompt_blocks(
    records: Vec<crate::session::load_snapshot::UserPromptRecord>,
) -> HashMap<String, Vec<crate::acp::protocol::ContentBlock>> {
    records
        .into_iter()
        .map(|record| (record.message_id, record.blocks))
        .collect()
}

fn session_load_span(req: &LoadSessionRequest) -> tracing::Span {
    let session_id = req.session_id.to_string();
    let span = info_span!(
        "acp.load_session",
        rpc.system.name = "jsonrpc",
        rpc.method = "session/load",
        session.id = %session_id,
        session.load.event_count = tracing::field::Empty,
        session.load.replay_notification_count = tracing::field::Empty,
    );
    if let Some(meta) = req.meta.as_ref()
        && let Some(parent) = crate::acp::trace_context::extract_acp_trace_context_from_meta(meta)
    {
        let _ = span.set_parent(parent);
    }

    span
}

/// Journal events and stored prompt blocks of a local session that has no
/// loaded actor. `None` when the session is not stored on this node.
async fn actorless_replay_sources(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
) -> Result<
    Option<(
        Vec<querymt_agent_core::events::AgentEvent>,
        HashMap<String, Vec<crate::acp::protocol::ContentBlock>>,
    )>,
    acp::Error,
> {
    let internal = |what: &str, err: &dyn std::fmt::Display| {
        tracing::warn!(session.id = %session_id, error = %err, "failed to load actor-less ACP replay {what}");
        acp::Error::internal_error().data(serde_json::json!({
            "message": format!("Failed to replay session {what}: {err}"),
            "sessionId": session_id,
        }))
    };
    let stored = agent
        .config
        .storage
        .session_store()
        .get_session(session_id)
        .await
        .map_err(|err| internal("lookup", &err))?;
    if stored.is_none() {
        return Ok(None);
    }
    // The hosting runtime owns the journal this session's events went to.
    let config = agent
        .hosting_config(session_id)
        .await
        .unwrap_or_else(|| agent.config.clone());
    let events = config
        .event_sink
        .journal()
        .load_session_stream(session_id, None, None)
        .instrument(info_span!("session.event_stream.load", session.id = %session_id))
        .await
        .map_err(|err| internal("events", &err))?
        .into_iter()
        .map(querymt_agent_core::events::AgentEvent::from)
        .collect();
    let history = config
        .provider
        .history_store()
        .get_user_prompts(session_id, None)
        .instrument(info_span!("session.history.load", session.id = %session_id))
        .await
        .map_err(|err| internal("history", &err))?;
    Ok(Some((events, stored_user_prompt_blocks(history))))
}

#[doc(hidden)]
pub async fn replay_loaded_session(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    capabilities: &crate::acp::shared::AcpSessionUpdateCapabilities,
) -> Result<(Vec<SessionNotification>, usize), acp::Error> {
    replay_loaded_session_with_cut(agent, session_id, capabilities)
        .await
        .map(|(notifications, event_count, _)| (notifications, event_count))
}

/// [`replay_loaded_session`] that also reports the last event it replays.
#[doc(hidden)]
pub async fn replay_loaded_session_with_cut(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    capabilities: &crate::acp::shared::AcpSessionUpdateCapabilities,
) -> Result<
    (
        Vec<SessionNotification>,
        usize,
        crate::acp::delivery::LoadCut,
    ),
    acp::Error,
> {
    // Profile-bound sessions live in their owning runtime's registry.
    let session_ref = agent.owned_session_ref(session_id).await;
    let session_ref_missing = session_ref.is_none();
    let (events, prompt_blocks) = match session_ref {
        Some(session_ref) => {
            match crate::acp::shared::session_events(agent, session_id, &session_ref)
                .instrument(info_span!("session.event_stream.load", session.id = %session_id))
                .await
            {
                Ok(events) => {
                    let history = session_ref
                    .get_user_prompts(None)
                    .instrument(info_span!("session.history.load", session.id = %session_id))
                    .await
                    .map_err(|err| {
                        tracing::warn!(session.id = %session_id, error = %err, "failed to load ACP replay history");
                        acp::Error::internal_error().data(serde_json::json!({
                            "message": format!("Failed to replay session history: {err}"),
                            "sessionId": session_id,
                        }))
                    })?;
                    (events, stored_user_prompt_blocks(history))
                }
                Err(err) => {
                    tracing::warn!(session.id = %session_id, error = %err, "failed to load ACP replay events");
                    return Err(err);
                }
            }
        }
        // A read-only previous owner has no actor: read the journal and the
        // history store directly, as the actor handlers do.
        // Only a read-only session (transferred owner, archived workspace) may
        // replay without an actor; for any other session a missing ref is a
        // real materialization bug.
        None => {
            let sources = if agent
                .session_materializer
                .read_only_reason(session_id)
                .await?
                .is_some()
            {
                actorless_replay_sources(agent, session_id).await?
            } else {
                None
            };
            match sources {
                Some(sources) => sources,
                None => {
                    tracing::warn!(session.id = %session_id, "loaded session has no runtime ref for ACP replay");
                    return Err(acp::Error::internal_error().data(serde_json::json!({
                        "message": "Loaded session has no runtime ref for ACP replay",
                        "sessionId": session_id,
                    })));
                }
            }
        }
    };
    let actorless = session_ref_missing;
    let event_count = events.len();
    let cut = crate::acp::delivery::LoadCut::from_events(&events);
    let mut notifications = async {
        crate::acp::shared::replay_agent_events_materialized(
            session_id,
            events,
            &prompt_blocks,
            capabilities,
        )
    }
    .instrument(info_span!("acp.replay.translate", session.id = %session_id))
    .await;
    if actorless {
        // No actor means no live configuration to report.
        return Ok((notifications, event_count, cut));
    }
    // Historical events do not carry profile selectors. Replay the owner's full
    // current configuration, not a partial historical replacement.
    let options = agent
        .session_config_options(Some(session_id), crate::agent::core::AgentMode::Build, None)
        .await?;
    notifications.retain(|notification| {
        !matches!(notification.update, SessionUpdate::ConfigOptionUpdate(_))
    });
    let config_index = notifications
        .iter()
        .position(|notification| matches!(notification.update, SessionUpdate::CurrentModeUpdate(_)))
        .map_or(0, |index| index + 1);
    notifications.insert(
        config_index,
        SessionNotification::new(
            SessionId::from(session_id.to_string()),
            SessionUpdate::ConfigOptionUpdate(crate::acp::protocol::ConfigOptionUpdate::new(
                options,
            )),
        ),
    );
    Ok((notifications, event_count, cut))
}

async fn prepare_session_load(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    req: LoadSessionRequest,
    capabilities: &crate::acp::shared::AcpSessionUpdateCapabilities,
    preferences: &crate::acp::shared::AcpSessionLoadPreferences,
) -> Result<AcpSessionLoadOutcome, acp::Error> {
    let session_id = req.session_id.to_string();
    let response = agent
        .load_session(req)
        .instrument(info_span!("agent.session_load", session.id = %session_id))
        .await?;
    // Snapshot-capable clients hydrate history from the load-response snapshot.
    // Skipping replay here also avoids the redundant event-stream and message
    // reads that only exist to produce historical notifications.
    let (notifications, event_count, cut) = if preferences.replays_history() {
        let (notifications, event_count, cut) =
            replay_loaded_session_with_cut(agent.as_ref(), &session_id, capabilities).await?;
        (notifications, event_count, Some(cut))
    } else {
        (
            Vec::new(),
            0,
            crate::acp::delivery::LoadCut::from_snapshot_meta(response.meta.as_ref()),
        )
    };
    tracing::Span::current().record("session.load.event_count", event_count as i64);
    tracing::Span::current().record(
        "session.load.replay_notification_count",
        notifications.len() as i64,
    );
    Ok(AcpSessionLoadOutcome {
        response,
        notifications,
        event_count,
        cut,
    })
}

/// Execute the same load and replay preparation used by the native ACP stdio route.
pub async fn load_session_with_replay(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    req: LoadSessionRequest,
) -> Result<AcpSessionLoadOutcome, acp::Error> {
    let span = session_load_span(&req);
    prepare_session_load(
        agent,
        req,
        &crate::acp::shared::AcpSessionUpdateCapabilities::default(),
        &crate::acp::shared::AcpSessionLoadPreferences::default(),
    )
    .instrument(span)
    .await
}

async fn advertise_available_commands(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    bridge_sender: &ClientBridgeSender,
    session_id: &str,
) {
    let Some(notification) = agent.available_slash_commands(session_id).await else {
        return;
    };
    if let Err(e) = bridge_sender.notify(notification).await {
        log::debug!("Failed to advertise slash commands for session {session_id}: {e}");
    }
}

async fn load_session_and_enqueue_replay(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    bridge_sender: &ClientBridgeSender,
    req: LoadSessionRequest,
    translator: &std::sync::Arc<std::sync::Mutex<crate::acp::shared::AcpLiveEventTranslator>>,
    session_load: &std::sync::Arc<std::sync::Mutex<crate::acp::shared::AcpSessionLoadPreferences>>,
    delivery: &Arc<crate::acp::delivery::ConnectionDelivery>,
) -> Result<
    (
        LoadSessionResponse,
        crate::acp::delivery::LoadHold,
        Option<crate::acp::delivery::LoadCut>,
    ),
    acp::Error,
> {
    let span = session_load_span(&req);
    async {
        let capabilities = {
            let translator = translator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            translator.capabilities()
        };
        let preferences = {
            *session_load
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
        let session_id = req.session_id.to_string();
        // Live events of the session wait from here until the response is
        // out: none is delivered before it, none is lost between the read
        // and the answer. The caller releases the hold after responding.
        let hold = delivery.begin_load(&session_id);
        let outcome = prepare_session_load(agent, req, &capabilities, &preferences).await?;
        crate::acp::shared::seed_loaded_run_state(
            agent,
            &session_id,
            outcome.response.meta.as_ref(),
            translator,
        )
        .await;
        let recovered = {
            let mut translator = translator
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(options) = outcome.response.config_options.as_ref() {
                translator.seed_config_options(&session_id, options);
            }
            translator.seed_history_from_meta(&session_id, outcome.response.meta.as_ref())
        };
        let recovered = match recovered {
            Some(updates) => updates,
            None => {
                crate::acp::shared::restore_session_history(agent, &session_id, translator).await
            }
        };
        let enqueue_span = info_span!(
            "acp.replay.enqueue",
            event_count = outcome.event_count,
            notification_count = outcome.notifications.len(),
        );
        async {
            for notification in outcome.notifications {
                bridge_sender.notify(notification).await?;
            }
            crate::acp::shared::send_recovered_delegations(bridge_sender, recovered).await?;
            bridge_sender
                .flush()
                .instrument(info_span!("acp.replay.flush"))
                .await?;
            Ok::<_, acp::Error>(())
        }
        .instrument(enqueue_span)
        .await?;
        // More live events than the connection can buffer arrived: answering
        // would leave a gap, so the load fails and can be retried.
        hold.check().map_err(|_| {
            acp::Error::internal_error().data(serde_json::json!({
                "message": "Too many live events arrived while loading the session; retry the load",
                "sessionId": session_id,
            }))
        })?;
        Ok((outcome.response, hold, outcome.cut))
    }
    .instrument(span)
    .await
}

fn agent_ext_request(
    method: &'static str,
    params: impl serde::Serialize,
) -> Result<AgentRequest, acp::Error> {
    let params_json = serde_json::to_string(&params).map_err(acp::Error::into_internal_error)?;
    let raw_value = serde_json::value::RawValue::from_string(params_json)
        .map_err(acp::Error::into_internal_error)?;
    Ok(AgentRequest::ExtMethodRequest(ExtRequest::new(
        method,
        Arc::from(raw_value),
    )))
}

/// Run the client bridge task for the active ACP connection.
///
/// This task receives messages from agent tasks via an mpsc channel and forwards
/// them through the connection. Long-lived response waits use `ConnectionTo::spawn`
/// so the bridge can continue handling other messages.
///
/// # Message Handling
///
/// - **Notification**: Fire-and-forget, just call `connection.session_notification()`
/// - **RequestPermission**: Request-response, send result back via oneshot channel
async fn run_bridge_task(
    mut rx: mpsc::Receiver<ClientBridgeMessage>,
    connection: ConnectionTo<acp::Client>,
) {
    log::info!("Client bridge task started");
    let mut notification_forwarding = NotificationForwardingState::default();

    while let Some(msg) = rx.recv().await {
        match msg {
            ClientBridgeMessage::Notification(notif) => {
                let _span = info_span!("acp.notification").entered();
                log::debug!("Bridge: forwarding session notification");
                let result = connection.send_notification(notif);
                if let Err(e) = &result {
                    log::error!("Bridge: session_notification failed: {:?}", e);
                }
                notification_forwarding.record_result(result);
            }
            ClientBridgeMessage::Flush { response_tx } => {
                let _ = response_tx.send(notification_forwarding.take_flush_result());
            }
            ClientBridgeMessage::ExtNotification(notif) => {
                let _span = info_span!("acp.ext_notification").entered();
                log::debug!("Bridge: forwarding ext notification");
                if let Err(e) =
                    connection.send_notification(ClientNotification::ExtNotification(notif))
                {
                    log::error!("Bridge: ext_notification failed: {:?}", e);
                }
            }
            ClientBridgeMessage::RequestPermission {
                request,
                response_tx,
            } => {
                let _span = info_span!("acp.permission_request").entered();
                log::debug!("Bridge: forwarding permission request");
                let result = connection.send_request(request).block_task().await;

                if response_tx.send(result).is_err() {
                    log::error!("Bridge: failed to send permission response (receiver dropped)");
                }
            }
            ClientBridgeMessage::Elicit {
                elicitation_id,
                session_id,
                message,
                requested_schema,
                source,
                response_tx,
            } => {
                log::debug!(
                    "Bridge: forwarding native ACP elicitation request: {}",
                    elicitation_id
                );
                let task_connection = connection.clone();
                let response_tx = Arc::new(Mutex::new(Some(response_tx)));
                let task_response_tx = response_tx.clone();
                let spawn_result = connection.spawn(async move {
                    let result = match create_elicitation_request(
                        elicitation_id,
                        session_id,
                        message,
                        requested_schema,
                        source,
                    ) {
                        Ok(request) => task_connection
                            .send_request(request)
                            .block_task()
                            .await
                            .and_then(convert_elicitation_response),
                        Err(err) => Err(err),
                    };

                    let response_tx = task_response_tx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if response_tx.is_some_and(|tx| tx.send(result).is_err()) {
                        log::error!(
                            "Bridge: failed to send elicitation response (receiver dropped)"
                        );
                    }
                    Ok(())
                });

                if let Err(err) = spawn_result {
                    let response_tx = response_tx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if response_tx.is_some_and(|tx| tx.send(Err(err)).is_err()) {
                        log::error!(
                            "Bridge: failed to report elicitation task spawn failure (receiver dropped)"
                        );
                    }
                }
            }
            ClientBridgeMessage::WorkspaceQuery { query, response_tx } => {
                let _span = info_span!("acp.workspace_query").entered();
                log::debug!("Bridge: forwarding workspace query: {:?}", query);

                // Send a wire-form ACP extension request to the client.
                let result = match agent_ext_request("_workspace/query", &query) {
                    Ok(ext_req) => connection.send_request(ext_req).block_task().await,
                    Err(err) => Err(err),
                };

                let parsed = match result {
                    Ok(ext_resp) => serde_json::from_value(ext_resp).map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to parse workspace query response: {}", e))
                    }),
                    Err(e) => Err(e),
                };

                if response_tx.send(parsed).is_err() {
                    log::error!(
                        "Bridge: failed to send workspace query response (receiver dropped)"
                    );
                }
            }
        }
    }

    log::info!("Client bridge task ended (channel closed)");
}

/// Deliver one pending permission interaction to the stdio client as a
/// standard `session/request_permission` request.
///
/// Only an explicitly selected option resolves the interaction. A transport
/// error, a client-side `cancelled` outcome or a malformed answer leaves it
/// pending so it can be delivered again (never a grant, never a rejection).
async fn deliver_stdio_permission(
    agent: Arc<crate::agent::LocalAgentHandle>,
    bridge: ClientBridgeSender,
    session_id: String,
    permission_id: String,
    request: crate::acp::protocol::RequestPermissionRequest,
) {
    const CONNECTION_ID: &str = "stdio";
    let Some(claim) = crate::elicitation::claim_live_elicitation_delivery(
        agent.as_ref(),
        &session_id,
        &permission_id,
        CONNECTION_ID,
    )
    .await
    else {
        return;
    };
    let response = match bridge.request_permission(request).await {
        Ok(response) => response,
        Err(err) => {
            log::warn!(
                "Native ACP permission {permission_id} ended without an answer, staying pending: {err}"
            );
            return;
        }
    };
    let answer = match response.outcome {
        crate::acp::protocol::RequestPermissionOutcome::Selected(selected) => {
            crate::elicitation::ElicitationResponse::permission_selected(
                selected.option_id.0.as_ref(),
            )
        }
        _ => {
            log::debug!("Native ACP permission {permission_id} dismissed, staying pending");
            return;
        }
    };
    match crate::elicitation::resolve_claimed_elicitation(
        agent.as_ref(),
        &session_id,
        &permission_id,
        CONNECTION_ID,
        claim.delivery_generation,
        answer,
    )
    .await
    {
        Ok(crate::elicitation::ElicitationResolution::Resolved) => {}
        Ok(resolution) => log::warn!(
            "Permission answer not applied ({resolution:?}): session_id={session_id} permission_id={permission_id}"
        ),
        Err(error) => log::warn!(
            "Permission answer rejected, request stays pending: session_id={session_id} permission_id={permission_id} error={}",
            error.message
        ),
    }
}

/// Outcome of handling one live event in the stdio forwarder.
enum StdioFlow {
    /// The event was delivered to the client.
    Forwarded,
    /// The event produced nothing for this client.
    Skipped,
    /// The bridge is gone; stop forwarding.
    Stop,
}

/// Send one `querymt/session/stateUpdate` notification.
pub(crate) async fn notify_state_update(
    bridge: &ClientBridgeSender,
    update: &crate::acp::shared::SessionStateUpdate,
) -> Result<(), acp::Error> {
    let params = serde_json::value::RawValue::from_string(
        serde_json::to_string(update).map_err(acp::Error::into_internal_error)?,
    )
    .map_err(acp::Error::into_internal_error)?;
    bridge
        .notify_ext(crate::acp::protocol::ExtNotification::new(
            crate::acp::shared::SESSION_STATE_UPDATE_METHOD,
            Arc::from(params),
        ))
        .await
}

/// Handle one live event: elicitations, permissions and session updates.
async fn forward_stdio_event(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    bridge: &ClientBridgeSender,
    forwarded_elicitations: &Arc<Mutex<HashSet<(String, String)>>>,
    translator: &Arc<Mutex<AcpLiveEventTranslator>>,
    event: &querymt_agent_core::events::EventEnvelope,
) -> StdioFlow {
    // Run state first: it is derived from every event (including the waits
    // handled below) and orders with the session updates of the same run.
    let state_update = translator
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .translate_state_update(event);
    if let Some(update) = state_update
        && let Err(error) = notify_state_update(bridge, &update).await
    {
        log::info!("Bridge closed, stopping forwarder: {}", error);
        return StdioFlow::Stop;
    }

    if let querymt_agent_core::events::AgentEventKind::ElicitationRequested {
        elicitation_id,
        session_id,
        message,
        requested_schema,
        source,
    } = event.kind()
    {
        let key = (session_id.clone(), elicitation_id.clone());
        let should_forward = forwarded_elicitations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key);
        if should_forward {
            let agent = agent.clone();
            let bridge = bridge.clone();
            let elicitation_id = elicitation_id.clone();
            let session_id = session_id.clone();
            let message = message.clone();
            let requested_schema = requested_schema.clone();
            let source = source.clone();
            tokio::spawn(async move {
                let response = match bridge
                    .elicit(
                        elicitation_id.clone(),
                        session_id.clone(),
                        message,
                        requested_schema,
                        source,
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(err) => {
                        log::warn!(
                            "Native ACP elicitation {} ended without a user response: {}",
                            elicitation_id,
                            err
                        );
                        // Transport failure is not user cancellation. Leave the
                        // shared waiter available for an authorized re-delivery.
                        return;
                    }
                };

                match crate::elicitation::resolve_elicitation_from_connection(
                    &agent,
                    Some(&session_id),
                    &elicitation_id,
                    "stdio",
                    response,
                )
                .await
                {
                    Ok(crate::elicitation::ElicitationResolution::Resolved) => {
                        log::debug!(
                            "Elicitation response delivered: session_id={} elicitation_id={}",
                            session_id,
                            elicitation_id
                        );
                    }
                    Ok(resolution) => {
                        log::warn!(
                            "Elicitation response not applied ({:?}): session_id={} elicitation_id={}",
                            resolution,
                            session_id,
                            elicitation_id
                        );
                    }
                    Err(error) => {
                        log::warn!(
                            "Elicitation response rejected: session_id={} elicitation_id={} error={}",
                            session_id,
                            elicitation_id,
                            error.message
                        );
                    }
                }
            });
        }
        return StdioFlow::Skipped;
    }

    if let querymt_agent_core::events::AgentEventKind::PermissionRequested {
        permission_id,
        request: Some(request),
        ..
    } = event.kind()
    {
        let session_id = event.session_id().to_string();
        let key = (session_id.clone(), permission_id.clone());
        let should_forward = forwarded_elicitations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key);
        if should_forward {
            let agent = agent.clone();
            let bridge = bridge.clone();
            let permission_id = permission_id.clone();
            let request = request.clone();
            tokio::spawn(async move {
                deliver_stdio_permission(agent, bridge, session_id, permission_id, request).await;
            });
        }
        return StdioFlow::Skipped;
    }

    let input_state = input_state_from_event(event);
    let (delegation_update, session_updates) = {
        let mut translator = translator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let delegation_update = translator.translate_delegation_update(event);
        let session_updates = if delegation_update.is_none() {
            translator.translate_updates(event)
        } else {
            Vec::new()
        };
        (delegation_update, session_updates)
    };
    let result = if let querymt_agent_core::events::AgentEventKind::RuntimeChanged { runtime } =
        event.kind()
    {
        match serde_json::value::RawValue::from_string(
            serde_json::json!({
                "version": 1, "session_id": event.session_id(), "runtime": runtime
            })
            .to_string(),
        ) {
            Ok(params) => {
                bridge
                    .notify_ext(crate::acp::protocol::ExtNotification::new(
                        crate::acp::shared::QMT_NOTIFICATION_RUNTIME_CHANGED,
                        Arc::from(params),
                    ))
                    .await
            }
            Err(error) => Err(acp::Error::into_internal_error(error)),
        }
    } else if let querymt_agent_core::events::AgentEventKind::DelegateModelsChanged { revision } =
        event.kind()
    {
        let notification =
            crate::acp::shared::delegate_models_changed_notification(event.session_id(), *revision);
        let params = serde_json::value::RawValue::from_string(notification["params"].to_string())
            .map(Arc::from)
            .map_err(acp::Error::into_internal_error);
        match params {
            Ok(params) => {
                bridge
                    .notify_ext(crate::acp::protocol::ExtNotification::new(
                        crate::acp::shared::QMT_NOTIFICATION_DELEGATE_MODELS_CHANGED,
                        params,
                    ))
                    .await
            }
            Err(error) => Err(error),
        }
    } else if let querymt_agent_core::events::AgentEventKind::McpServerAuthRequired {
        server_name,
        provider_id,
        error,
    } = event.kind()
    {
        let notification = crate::acp::shared::mcp_auth_required_notification(
            event.session_id(),
            server_name,
            provider_id,
            error.as_deref(),
        );
        match serde_json::value::RawValue::from_string(notification["params"].to_string()) {
            Ok(params) => {
                bridge
                    .notify_ext(crate::acp::protocol::ExtNotification::new(
                        crate::acp::shared::QMT_NOTIFICATION_MCP_AUTH_REQUIRED,
                        Arc::from(params),
                    ))
                    .await
            }
            Err(error) => Err(acp::Error::into_internal_error(error)),
        }
    } else if let Some(update) = input_state {
        let params = serde_json::value::RawValue::from_string(
            serde_json::to_string(&update).unwrap_or_else(|_| "null".to_string()),
        )
        .map(Arc::from)
        .map_err(acp::Error::into_internal_error);
        match params {
            Ok(params) => {
                bridge
                    .notify_ext(crate::acp::protocol::ExtNotification::new(
                        QMT_NOTIFICATION_INPUT_STATE,
                        params,
                    ))
                    .await
            }
            Err(err) => Err(err),
        }
    } else if let Some(update) = delegation_update {
        let params = serde_json::value::RawValue::from_string(
            serde_json::to_string(&update).unwrap_or_else(|_| "null".to_string()),
        )
        .map(Arc::from)
        .map_err(acp::Error::into_internal_error);
        match params {
            Ok(params) => {
                bridge
                    .notify_ext(crate::acp::protocol::ExtNotification::new(
                        QMT_NOTIFICATION_DELEGATION_UPDATE,
                        params,
                    ))
                    .await
            }
            Err(err) => Err(err),
        }
    } else if !session_updates.is_empty() {
        let mut result = Ok(());
        for update in session_updates {
            result = bridge
                .notify(SessionNotification::new(
                    SessionId::from(event.session_id().to_owned()),
                    update,
                ))
                .await;
            if result.is_err() {
                break;
            }
        }
        result
    } else {
        return StdioFlow::Skipped;
    };

    if let Err(e) = result {
        log::info!("Bridge closed, stopping forwarder: {}", e);
        return StdioFlow::Stop;
    }

    StdioFlow::Forwarded
}

/// Handle admitted events in order. The delivered position of each event
/// advances once it was handled.
async fn forward_stdio_admitted(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    bridge: &ClientBridgeSender,
    forwarded_elicitations: &Arc<Mutex<HashSet<(String, String)>>>,
    translator: &Arc<Mutex<AcpLiveEventTranslator>>,
    delivery: &Arc<crate::acp::delivery::ConnectionDelivery>,
    batch: Vec<crate::acp::delivery::Admitted>,
    forwarded_count: &mut u64,
) -> bool {
    for admitted in batch {
        // A load in progress holds the event; a finished load's answer may
        // already contain it.
        let Some(admitted) = delivery.intercept(admitted) else {
            continue;
        };
        let crate::acp::delivery::Admitted { event, guard } = admitted;
        let flow =
            forward_stdio_event(agent, bridge, forwarded_elicitations, translator, &event).await;
        drop(guard);
        match flow {
            StdioFlow::Stop => return false,
            StdioFlow::Skipped => {}
            StdioFlow::Forwarded => {
                *forwarded_count += 1;
                log::trace!(
                    "Forwarded event {} for session {}",
                    forwarded_count,
                    event.session_id()
                );
            }
        }
    }
    true
}

/// Deliver what finished loads held back: the current run state and the
/// in-flight text first, then the held events the load's answer does not
/// contain, in order.
async fn forward_stdio_released(
    agent: &Arc<crate::agent::LocalAgentHandle>,
    bridge: &ClientBridgeSender,
    forwarded_elicitations: &Arc<Mutex<HashSet<(String, String)>>>,
    translator: &Arc<Mutex<AcpLiveEventTranslator>>,
    delivery: &Arc<crate::acp::delivery::ConnectionDelivery>,
    forwarded_count: &mut u64,
) -> bool {
    for released in delivery.take_released() {
        let (state, updates) = translator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .released_load_prelude(&released.session_id, released.inflight.as_ref());
        if let Some(state) = state
            && notify_state_update(bridge, &state).await.is_err()
        {
            return false;
        }
        for update in updates {
            let notification =
                SessionNotification::new(SessionId::from(released.session_id.clone()), update);
            if bridge.notify(notification).await.is_err() {
                return false;
            }
        }
        if !forward_stdio_admitted(
            agent,
            bridge,
            forwarded_elicitations,
            translator,
            delivery,
            released.events,
            forwarded_count,
        )
        .await
        {
            return false;
        }
    }
    true
}

/// Forwards events from EventFanout to the client bridge.
///
/// This task subscribes to the EventFanout and forwards all events to the ACP client
/// via the bridge. Unlike the WebSocket server, this does not filter events by
/// session ownership because the SDK stdio server serves a single client (Zed).
///
/// The forwarder automatically stops when:
/// - The EventFanout is shut down (recv returns error)
/// - The bridge channel closes (notify returns error)
fn spawn_event_bridge_forwarder(
    event_fanout: Arc<EventFanout>,
    bridge: ClientBridgeSender,
    agent: Arc<crate::agent::LocalAgentHandle>,
    forwarded_elicitations: Arc<Mutex<HashSet<(String, String)>>>,
    translator: Arc<Mutex<AcpLiveEventTranslator>>,
    delivery: Arc<crate::acp::delivery::ConnectionDelivery>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        log::info!("Event bridge forwarder started");
        let mut events = event_fanout.subscribe();

        let mut forwarded_count = 0u64;

        loop {
            let received = tokio::select! {
                _ = delivery.released_load() => {
                    if forward_stdio_released(
                        &agent,
                        &bridge,
                        &forwarded_elicitations,
                        &translator,
                        &delivery,
                        &mut forwarded_count,
                    )
                    .await
                    {
                        continue;
                    }
                    break;
                }
                received = events.recv() => received,
            };
            match received {
                Ok(event) => {
                    let batch = delivery.admit(event).await;
                    if !forward_stdio_admitted(
                        &agent,
                        &bridge,
                        &forwarded_elicitations,
                        &translator,
                        &delivery,
                        batch,
                        &mut forwarded_count,
                    )
                    .await
                    {
                        log::info!(
                            "Bridge closed after forwarding {} events, stopping forwarder",
                            forwarded_count
                        );
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!(
                        "EventFanout closed after forwarding {} events, stopping forwarder",
                        forwarded_count
                    );
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(
                        "Event forwarder lagged, skipped {} events (total forwarded: {})",
                        skipped,
                        forwarded_count
                    );
                    // Recover the missed durable events from the journal and
                    // keep forwarding; one lag must not drop session updates.
                    let recovered = delivery.recover_after_lag().await;
                    let keep_going = forward_stdio_admitted(
                        &agent,
                        &bridge,
                        &forwarded_elicitations,
                        &translator,
                        &delivery,
                        recovered,
                        &mut forwarded_count,
                    )
                    .await;
                    delivery.finish_lag_recovery();
                    if !keep_going {
                        break;
                    }
                    let ids = delivery.observed_sessions();
                    let (mut notifications, updates) =
                        crate::acp::shared::current_session_observations(
                            &agent,
                            &ids,
                            agent.config.event_sink.fanout(),
                        )
                        .await;
                    translator
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .restore_delegation_observations(&updates);
                    notifications.extend(
                        updates
                            .into_iter()
                            .map(crate::acp::shared::delegation_update_notification),
                    );
                    let mut closed = false;
                    for notification in notifications {
                        let Ok(notification) = serde_json::from_value::<
                            crate::acp::protocol::ExtNotification,
                        >(serde_json::json!({
                            "method": notification["method"], "params": notification["params"]
                        })) else {
                            continue;
                        };
                        if bridge.notify_ext(notification).await.is_err() {
                            closed = true;
                            break;
                        }
                    }
                    if closed {
                        break;
                    }
                }
            }
        }

        // Signal that this forwarder has stopped (for graceful shutdown coordination)
        let _ = shutdown_tx.send(()).await;
        log::info!(
            "Event bridge forwarder stopped (forwarded {} total events)",
            forwarded_count
        );
    })
}

async fn forward_mesh_access_updates(
    mut updates: tokio::sync::broadcast::Receiver<crate::acp::protocol::ExtNotification>,
    bridge: ClientBridgeSender,
) {
    loop {
        match updates.recv().await {
            Ok(notification)
                if notification.method.as_ref()
                    == crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED =>
            {
                if bridge.notify_ext(notification).await.is_err() {
                    break;
                }
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Run an ACP stdio server with a QueryMTAgent.
///
/// This is a convenience function for running QueryMTAgent over stdio with the ACP protocol.
/// It sets up the bidirectional bridge, configures the agent, and handles graceful shutdown.
///
/// # Example
///
/// ```rust,ignore
/// // `Agent` is defined in the `querymt-agent` facade crate.
/// use querymt_agent::api::Agent;
/// use querymt_agent::acp::serve_stdio;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let agent = Agent::builder()
///         .provider("anthropic", "claude-sonnet-4-20250514")
///         .cwd("/tmp")
///         .build()
///         .await?;
///     
///     serve_stdio(agent.inner()).await
/// }
/// ```
///
/// # Graceful Shutdown
///
/// The server handles SIGTERM and SIGINT (Ctrl+C) for graceful shutdown.
/// Current operations are allowed to complete before exit.
pub async fn serve_stdio(agent: Arc<crate::agent::LocalAgentHandle>) -> anyhow::Result<()> {
    log::info!("Starting ACP stdio server for QueryMTAgent");

    // 1. Create the channel for agent→client communication
    let (tx, rx) = mpsc::channel::<ClientBridgeMessage>(100);
    let bridge_sender = ClientBridgeSender::new(tx);
    log::debug!("Created bridge channel");

    // 2. Set the bridge on the agent
    agent.set_bridge(bridge_sender.clone()).await;
    log::debug!("Set bridge on agent");

    // 3. Every member is built on the root's event stream, so the root's
    // fanout is the only source to forward.
    let event_fanout = agent.config.event_sink.fanout().clone();

    // 4. Create shutdown coordination channel
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // 5. Spawn the event bridge forwarder
    let forwarded_elicitations = Arc::new(Mutex::new(HashSet::new()));
    let translator = Arc::new(Mutex::new(AcpLiveEventTranslator::new()));
    // One session-load preference per connection: parsed from the client's
    // advertised capabilities at initialize and consulted by session/load.
    let session_load = Arc::new(Mutex::new(
        crate::acp::shared::AcpSessionLoadPreferences::default(),
    ));
    let session_load_forwarders = session_load.clone();
    // One delivery position per connection, shared by every forwarder: it
    // recovers lag/gaps from the journal and backs the prompt-response wait.
    let delivery = crate::acp::delivery::ConnectionDelivery::new(
        Some(agent.config.event_sink.journal().clone()),
        tokio_util::sync::CancellationToken::new(),
    );
    let forwarder_handle = spawn_event_bridge_forwarder(
        event_fanout,
        bridge_sender.clone(),
        agent.clone(),
        forwarded_elicitations.clone(),
        translator.clone(),
        delivery.clone(),
        shutdown_tx,
    );
    log::info!("Spawned event forwarder");
    let mesh_access_forwarder = tokio::spawn(forward_mesh_access_updates(
        agent.subscribe_ext_notifications(),
        bridge_sender.clone(),
    ));

    // 6. Set up stdio streams and connect ACP agent role.
    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    let serve_fut = acp::Agent
        .v1()
        .on_receive_request(
            {
                let agent = agent.clone();
                let translator = translator.clone();
                let session_load = session_load.clone();
                async move |req: InitializeRequest, responder, _cx| {
                    let capabilities =
                        crate::acp::shared::AcpSessionUpdateCapabilities::from_client_capabilities(
                            &req.client_capabilities,
                        );
                    let preferences =
                        crate::acp::shared::AcpSessionLoadPreferences::from_client_capabilities(
                            &req.client_capabilities,
                        );
                    let result = agent.initialize(req).await;
                    if result.is_ok()
                        && let Ok(mut translator) = translator.lock()
                    {
                        translator.set_capabilities(capabilities);
                    }
                    if result.is_ok()
                        && let Ok(mut current) = session_load.lock()
                    {
                        *current = preferences;
                    }
                    responder.respond_with_result(result)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: AuthenticateRequest, responder, _cx| {
                    responder.respond_with_result(agent.authenticate(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                let bridge_sender = bridge_sender.clone();
                let translator = translator.clone();
                async move |req: NewSessionRequest, responder, _cx| {
                    let result = agent.new_session(req).await;
                    if let Ok(response) = &result
                        && let Some(options) = response.config_options.as_ref() {
                        translator.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).seed_config_options(&response.session_id.to_string(), options);
                    }
                    let session_id = result.as_ref().ok().map(|r| r.session_id.to_string());
                    let send_result = responder.respond_with_result(result);
                    if let Some(session_id) = session_id {
                        advertise_available_commands(&agent, &bridge_sender, &session_id).await;
                    }
                    send_result
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                let delivery = delivery.clone();
                let bridge_sender = bridge_sender.clone();
                async move |req: PromptRequest, responder, cx| {
                    // Prompt execution can run for a long time, so spawn it and
                    // return immediately to keep ACP able to process cancel.
                    let agent = agent.clone();
                    let delivery = delivery.clone();
                    let bridge_sender = bridge_sender.clone();
                    let session_id = req.session_id.to_string();
                    cx.spawn(async move {
                        let result = agent.prompt(req).await.map(|mut response| {
                            let tip = crate::acp::delivery::take_event_tip(&mut response);
                            (response, tip)
                        });
                        let result = match result {
                            Ok((response, Some(tip))) => {
                                // Hold the response until this connection has
                                // delivered the prompt's updates, then flush the
                                // bridge so they are on the wire first.
                                delivery.hold_response_for_updates(&session_id, tip).await;
                                if let Err(error) = bridge_sender.flush().await {
                                    log::warn!(
                                        "session {session_id}: bridge flush before prompt response failed: {error:?}"
                                    );
                                }
                                Ok(response)
                            }
                            Ok((response, None)) => Ok(response),
                            Err(error) => Err(error),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = agent.clone();
                async move |notif: CancelNotification, _cx| agent.cancel(notif).await
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                let bridge_sender = bridge_sender.clone();
                let translator = translator.clone();
                let session_load = session_load_forwarders.clone();
                let delivery = delivery.clone();
                async move |req: LoadSessionRequest, responder, _cx| {
                    let session_id = req.session_id.to_string();
                    let result = load_session_and_enqueue_replay(
                        &agent,
                        &bridge_sender,
                        req,
                        &translator,
                        &session_load,
                        &delivery,
                    )
                    .await;
                    let (result, release) = match result {
                        Ok((response, hold, cut)) => (Ok(response), Some((hold, cut))),
                        Err(error) => (Err(error), None),
                    };
                    let ok = result.is_ok();
                    let send_result = responder.respond_with_result(result);
                    // The response is queued: the held live events follow it.
                    if let Some((hold, cut)) = release {
                        hold.release_with(cut, agent.config.event_sink.fanout());
                    }
                    if ok {
                        advertise_available_commands(&agent, &bridge_sender, &session_id).await;
                    }
                    send_result
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: ListSessionsRequest, responder, _cx| {
                    responder.respond_with_result(agent.list_sessions(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                let bridge_sender = bridge_sender.clone();
                async move |req: ForkSessionRequest, responder, _cx| {
                    let result = agent.fork_session(req).await;
                    let fork_id = result.as_ref().ok().map(|r| r.session_id.to_string());
                    let send_result = responder.respond_with_result(result);
                    if let Some(fork_id) = fork_id {
                        advertise_available_commands(&agent, &bridge_sender, &fork_id).await;
                    }
                    send_result
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                let bridge_sender = bridge_sender.clone();
                async move |req: ResumeSessionRequest, responder, _cx| {
                    let session_id = req.session_id.to_string();
                    let result = agent.resume_session(req).await;
                    let ok = result.is_ok();
                    let send_result = responder.respond_with_result(result);
                    if ok {
                        advertise_available_commands(&agent, &bridge_sender, &session_id).await;
                    }
                    send_result
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: CloseSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.close_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: DeleteSessionRequest, responder, _cx| {
                    responder.respond_with_result(agent.delete_session(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionModelRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_model(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionModeRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_mode(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |req: SetSessionConfigOptionRequest, responder, _cx| {
                    responder.respond_with_result(agent.set_session_config_option(req).await)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_dispatch(
            {
                let agent = agent.clone();
                let bridge_sender = bridge_sender.clone();
                async move |dispatch: acp::v1::Dispatch<ClientRequest, ClientNotification>, _cx| {
                    match dispatch {
                        acp::v1::Dispatch::Request(ClientRequest::ExtMethodRequest(req), responder) => {
                            let method = req.method.to_string();
                            let params: serde_json::Value =
                                serde_json::from_str(req.params.get()).unwrap_or_default();
                            let ext_value: Result<serde_json::Value, acp::Error> =
                                agent.ext_method(req).await.and_then(|resp| {
                                    serde_json::from_str(resp.0.get()).map_err(|e| {
                                        acp::Error::internal_error().data(e.to_string())
                                    })
                                });
                            // A remote attach binds a session to this connection
                            // without session/new|load|resume, so advertise its
                            // slash-command catalog here, after the response.
                            let attached = ext_value.as_ref().ok().and_then(|value| {
                                crate::acp::shared::remote_attached_session_id(
                                    &method, &params, value,
                                )
                            });
                            let send_result = responder.respond_with_result(ext_value);
                            if let Some(session_id) = attached {
                                advertise_available_commands(&agent, &bridge_sender, &session_id)
                                    .await;
                            }
                            send_result?;
                            Ok(acp::Handled::Yes)
                        }
                        acp::v1::Dispatch::Notification(ClientNotification::ExtNotification(notif)) => {
                            agent.ext_notification(notif).await?;
                            Ok(acp::Handled::Yes)
                        }
                        acp::v1::Dispatch::Request(request, responder) => Ok(acp::Handled::No {
                            message: acp::v1::Dispatch::Request(request, responder),
                            retry: false,
                        }),
                        acp::v1::Dispatch::Notification(notification) => Ok(acp::Handled::No {
                            message: acp::v1::Dispatch::Notification(notification),
                            retry: false,
                        }),
                        acp::v1::Dispatch::Response(result, router) => Ok(acp::Handled::No {
                            message: acp::v1::Dispatch::Response(result, router),
                            retry: false,
                        }),
                    }
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_with(ByteStreams::new(stdout, stdin), async move |cx| {
            run_bridge_task(rx, cx).await;
            Ok(())
        });

    log::info!("ACP stdio server ready, listening on stdin...");

    let shutdown_triggered = tokio::select! {
        result = serve_fut => {
            match result {
                Ok(()) => log::info!("ACP stdio connection finished (likely due to stdin close)"),
                Err(e) => log::error!("ACP stdio connection error: {}", e),
            }
            true
        }
        _ = shutdown::signal() => {
            log::info!("Shutdown signal received, stopping server...");
            true
        }
    };

    mesh_access_forwarder.abort();
    if shutdown_triggered {
        log::info!("Initiating graceful shutdown...");
        log::info!("Shutting down agent...");
        agent.shutdown().await;
        log::info!("Agent shutdown complete");

        log::info!("Waiting for the event forwarder to stop...");
        let forwarder_wait = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while shutdown_rx.recv().await.is_some() {
                log::debug!("Event forwarder stopped");
            }
        });

        match forwarder_wait.await {
            Ok(_) => log::info!("Event forwarder stopped gracefully"),
            Err(_) => {
                log::warn!("Event forwarder did not stop within 5s, aborting...");
                forwarder_handle.abort();
            }
        }
    }

    log::info!("ACP stdio server shutdown complete");
    Ok(())
}

#[cfg(test)]
mod stdio_tests {
    use super::{
        AcpLiveEventTranslator, CancelNotification, ClientBridgeMessage, PromptRequest,
        agent_ext_request, create_elicitation_request, run_bridge_task,
        spawn_event_bridge_forwarder, stored_user_prompt_blocks,
    };
    use crate::acp::client_bridge::ClientBridgeSender;
    use crate::acp::protocol::{
        AgentRequest, ContentBlock, CreateElicitationRequest, ElicitationAcceptAction,
        ElicitationAction as AcpElicitationAction, ElicitationContentValue, ImageContent,
        PromptResponse, SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
    };
    use crate::acp::shared::replay_agent_events_with_user_prompts;
    use crate::elicitation::ElicitationAction;
    use agent_client_protocol::{Agent, ByteStreams, Client, JsonRpcMessage};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};
    use tokio::sync::{Mutex, Notify, mpsc};
    use tokio::time::{Duration, timeout};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[tokio::test]
    async fn stdio_forwards_mesh_invalidation_after_lag_without_forwarding_unrelated_updates() {
        use crate::acp::protocol::ExtNotification;
        use crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED;
        use serde_json::{json, value::to_raw_value};

        let (updates, subscription) = tokio::sync::broadcast::channel(2);
        let payload = json!({"version": 1, "direction": "outgoing", "reason": "policy"});
        for (method, params) in [
            (
                QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED,
                json!({"reason": "superseded"}),
            ),
            ("querymt/session/runtimeChanged", json!({})),
            (QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED, payload.clone()),
        ] {
            updates
                .send(ExtNotification::new(
                    method,
                    to_raw_value(&params).unwrap().into(),
                ))
                .unwrap();
        }
        drop(updates);

        let (bridge, mut messages) = mpsc::channel(4);
        timeout(
            Duration::from_secs(2),
            super::forward_mesh_access_updates(subscription, ClientBridgeSender::new(bridge)),
        )
        .await
        .expect("forwarder drains the subscription and stops when it closes");
        let Some(ClientBridgeMessage::ExtNotification(notification)) = messages.recv().await else {
            panic!("expected the latest mesh access invalidation");
        };
        assert_eq!(
            notification.method.as_ref(),
            QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(notification.params.get()).unwrap(),
            payload
        );
        assert!(messages.recv().await.is_none());
    }

    fn assert_structured_user_message_replay(prompt_part: querymt_agent_core::model::MessagePart) {
        let event = querymt_agent_core::events::AgentEvent {
            session_seq: 0,
            seq: 1,
            timestamp: 1,
            session_id: "s-1".to_string(),
            origin: querymt_agent_core::events::EventOrigin::Local,
            source_node: None,
            kind: querymt_agent_core::events::AgentEventKind::PromptReceived {
                content: "flattened summary".to_string(),
                message_id: Some("stored-message-id".to_string()),
            },
        };
        let message = querymt_agent_core::model::AgentMessage {
            id: "stored-message-id".to_string(),
            session_id: "s-1".to_string(),
            role: querymt::chat::ChatRole::User,
            parts: vec![prompt_part],
            created_at: 1,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let prompt_blocks = stored_user_prompt_blocks(
            querymt_agent_core::session::prompt_records::user_prompt_records(&[message]),
        );

        let notifications =
            replay_agent_events_with_user_prompts("s-1", vec![event], &prompt_blocks);

        assert_eq!(notifications.len(), 3);
        assert!(matches!(
            &notifications[0].update,
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "before")
        ));
        assert!(matches!(
            &notifications[1].update,
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Image(image) if image.data == "AQID")
        ));
        assert!(matches!(
            &notifications[2].update,
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "after")
        ));
        for notification in notifications {
            let SessionUpdate::UserMessageChunk(chunk) = notification.update else {
                panic!("expected structured user message chunk");
            };
            assert_eq!(
                chunk.message_id.as_ref().map(|id| id.0.as_ref()),
                Some("stored-message-id")
            );
        }
    }

    fn structured_blocks() -> Vec<ContentBlock> {
        vec![
            ContentBlock::Text(TextContent::new("before")),
            ContentBlock::Image(ImageContent::new("AQID", "image/png")),
            ContentBlock::Text(TextContent::new("after")),
        ]
    }

    #[test]
    fn session_load_replay_preserves_structured_prompt_order_and_message_id() {
        assert_structured_user_message_replay(querymt_agent_core::model::MessagePart::Prompt {
            blocks: structured_blocks(),
        });
    }

    #[test]
    fn session_load_replay_preserves_structured_steering_order_and_message_id() {
        assert_structured_user_message_replay(querymt_agent_core::model::MessagePart::Steering {
            run_id: "run-1".to_string(),
            client_input_id: Some("input-1".to_string()),
            blocks: structured_blocks(),
        });
    }

    #[tokio::test]
    async fn corrupt_tool_result_does_not_fail_session_update_replay() {
        use crate::session::backend::StorageBackend;
        use kameo::actor::Spawn;
        use querymt_agent_core::model::{AgentMessage, MessagePart};

        let fixture = crate::test_utils::TestAgent::new().await;
        let session_id = fixture.create_session().await;
        let store = fixture.storage.session_store();
        let message = |id: &str, part: MessagePart, created_at: i64| AgentMessage {
            id: id.to_string(),
            session_id: session_id.clone(),
            role: querymt::chat::ChatRole::User,
            parts: vec![part],
            created_at,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        store
            .add_message(
                &session_id,
                message(
                    "stored-message-id",
                    MessagePart::Prompt {
                        blocks: structured_blocks(),
                    },
                    1,
                ),
            )
            .await
            .unwrap();
        store
            .add_message(
                &session_id,
                message(
                    "tool-message",
                    MessagePart::ToolResult {
                        call_id: "call-1".to_string(),
                        content: vec![querymt::chat::ToolResultPart::text("output")],
                        is_error: false,
                        tool_name: Some("read".to_string()),
                        tool_arguments: None,
                        compacted_at: None,
                    },
                    2,
                ),
            )
            .await
            .unwrap();
        let changed = fixture
            .storage
            .conn_for_test()
            .lock()
            .execute(
                "UPDATE message_parts SET content_json = '{not json' WHERE part_type = 'tool_result'",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
        assert!(store.get_history(&session_id).await.is_err());

        // Journal the prompt event that the replay pairs with the stored blocks.
        fixture
            .config
            .emit_event_persisted(
                &session_id,
                querymt_agent_core::events::AgentEventKind::PromptReceived {
                    content: "flattened summary".to_string(),
                    message_id: Some("stored-message-id".to_string()),
                },
            )
            .await
            .unwrap();

        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            std::collections::HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = crate::agent::session_actor::SessionActor::new(
            fixture.config.clone(),
            session_id.clone(),
            runtime,
        );
        let actor_ref = crate::agent::session_actor::SessionActor::spawn(actor);
        fixture
            .handle
            .registry
            .lock()
            .await
            .insert(session_id.clone(), actor_ref);

        let (notifications, _count) = super::replay_loaded_session(
            &fixture.handle,
            &session_id,
            &crate::acp::shared::AcpSessionUpdateCapabilities::default(),
        )
        .await
        .expect("replay survives the corrupt tool result");
        let chunks: Vec<&ContentBlock> = notifications
            .iter()
            .filter_map(|n| match &n.update {
                SessionUpdate::UserMessageChunk(chunk) => Some(&chunk.content),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3, "structured blocks replayed");
        assert!(matches!(chunks[1], ContentBlock::Image(image) if image.data == "AQID"));
    }

    #[tokio::test]
    async fn delegate_assignment_invalidation_uses_typed_stdio_bridge() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;
        fixture.config.event_sink.fanout().publish(
            querymt_agent_core::events::EventEnvelope::Durable(
                querymt_agent_core::events::DurableEvent {
                    session_seq: 0,
                    event_id: "assignment-event".into(),
                    stream_seq: 1,
                    session_id: "parent".into(),
                    timestamp: 0,
                    origin: querymt_agent_core::events::EventOrigin::Local,
                    source_node: None,
                    kind: querymt_agent_core::events::AgentEventKind::DelegateModelsChanged {
                        revision: Some(7),
                    },
                },
            ),
        );
        let message = timeout(Duration::from_secs(2), bridge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let ClientBridgeMessage::ExtNotification(notification) = message else {
            panic!("expected extension notification");
        };
        assert_eq!(
            notification.method.as_ref(),
            crate::acp::shared::QMT_NOTIFICATION_DELEGATE_MODELS_CHANGED
        );
        let params: serde_json::Value = serde_json::from_str(notification.params.get()).unwrap();
        assert_eq!(
            params,
            serde_json::json!({ "version": 1, "session_id": "parent", "revision": 7 })
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn input_state_uses_typed_ext_notification_bridge() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;
        fixture.config.event_sink.fanout().publish(
            querymt_agent_core::events::EventEnvelope::Durable(
                querymt_agent_core::events::DurableEvent {
                    session_seq: 0,
                    event_id: "input-event".into(),
                    stream_seq: 1,
                    session_id: "parent".into(),
                    timestamp: 0,
                    origin: querymt_agent_core::events::EventOrigin::Local,
                    source_node: None,
                    kind: querymt_agent_core::events::AgentEventKind::SteeringApplied {
                        run_id: "run-1".into(),
                        input_id: "input-1".into(),
                        boundary: "after_tools".into(),
                        latency_ms: 12,
                    },
                },
            ),
        );
        let message = timeout(Duration::from_secs(2), bridge_rx.recv())
            .await
            .expect("notification should arrive")
            .expect("bridge should remain open");
        let ClientBridgeMessage::ExtNotification(notification) = message else {
            panic!("expected extension notification");
        };
        assert_eq!(
            notification.method.as_ref(),
            crate::acp::shared::QMT_NOTIFICATION_INPUT_STATE
        );
        let params: serde_json::Value =
            serde_json::from_str(notification.params.get()).expect("valid params");
        assert_eq!(params["session_id"], "parent");
        assert_eq!(params["input_id"], "input-1");
        assert_eq!(params["state"], "applied");
        assert_eq!(params["boundary"], "after_tools");
        fixture.config.event_sink.emit_ephemeral(
            "parent",
            querymt_agent_core::events::AgentEventKind::RuntimeChanged {
                runtime: crate::agent::messages::SessionRuntimeStatus::Waiting,
            },
        );
        let message = timeout(Duration::from_secs(2), bridge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let ClientBridgeMessage::ExtNotification(notification) = message else {
            panic!("expected runtime extension");
        };
        assert_eq!(
            notification.method.as_ref(),
            crate::acp::shared::QMT_NOTIFICATION_RUNTIME_CHANGED
        );
        let params: serde_json::Value = serde_json::from_str(notification.params.get()).unwrap();
        assert_eq!(params["runtime"]["phase"], "waiting");
        forwarder.abort();
    }

    // ── Loads that overlap live traffic ────────────────────────────────────

    /// A connection with a running forwarder, started after the events a
    /// load is meant to have missed.
    struct LoadRig {
        fixture: crate::test_utils::TestAgent,
        session_id: String,
        bridge: ClientBridgeSender,
        bridge_rx: mpsc::Receiver<ClientBridgeMessage>,
        /// Messages read while serving a load's flush barrier.
        stashed: std::collections::VecDeque<ClientBridgeMessage>,
        translator: Arc<std::sync::Mutex<AcpLiveEventTranslator>>,
        session_load: Arc<std::sync::Mutex<crate::acp::shared::AcpSessionLoadPreferences>>,
        delivery: Arc<crate::acp::delivery::ConnectionDelivery>,
        forwarder: Option<tokio::task::JoinHandle<()>>,
    }

    impl LoadRig {
        async fn new() -> Self {
            let fixture = crate::test_utils::TestAgent::new().await;
            let session_id = fixture.create_session().await;
            let (bridge_tx, bridge_rx) = mpsc::channel(256);
            let delivery = crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            );
            Self {
                fixture,
                session_id,
                bridge: ClientBridgeSender::new(bridge_tx),
                bridge_rx,
                stashed: Default::default(),
                translator: Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
                session_load: Arc::new(std::sync::Mutex::new(Default::default())),
                delivery,
                forwarder: None,
            }
        }

        fn publish(
            &self,
            seq: i64,
            durable: bool,
            kind: querymt_agent_core::events::AgentEventKind,
        ) {
            use querymt_agent_core::events::{
                DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
            };
            let envelope = if durable {
                EventEnvelope::Durable(DurableEvent {
                    session_seq: seq,
                    event_id: format!("e{seq}"),
                    stream_seq: 1_000 + seq,
                    session_id: self.session_id.clone(),
                    timestamp: 0,
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind,
                })
            } else {
                EventEnvelope::Ephemeral(EphemeralEvent {
                    session_seq: seq,
                    session_id: self.session_id.clone(),
                    timestamp: 0,
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind,
                })
            };
            self.fixture.config.event_sink.fanout().publish(envelope);
        }

        fn delta(&self, seq: i64, message_id: &str, text: &str) {
            self.publish(
                seq,
                false,
                querymt_agent_core::events::AgentEventKind::AssistantContentDelta {
                    content: text.to_string(),
                    message_id: message_id.to_string(),
                },
            );
        }

        fn start_forwarder(&mut self) {
            let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
            self.forwarder = Some(spawn_event_bridge_forwarder(
                self.fixture.config.event_sink.fanout().clone(),
                self.bridge.clone(),
                self.fixture.handle.clone(),
                Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
                self.translator.clone(),
                self.delivery.clone(),
                shutdown_tx,
            ));
        }

        /// Everything the client receives until a title update `marker`
        /// shows up: state updates (as `state:<state>`), message and thought
        /// chunk texts (as `message:<text>` / `thought:<text>`).
        async fn received_until(&mut self, marker: &str) -> Vec<String> {
            let mut seen = Vec::new();
            loop {
                let message = match self.stashed.pop_front() {
                    Some(message) => message,
                    None => timeout(Duration::from_secs(5), self.bridge_rx.recv())
                        .await
                        .expect("the marker should arrive")
                        .expect("bridge stays open"),
                };
                match message {
                    ClientBridgeMessage::ExtNotification(notification)
                        if notification.method.as_ref()
                            == crate::acp::shared::SESSION_STATE_UPDATE_METHOD =>
                    {
                        let params: serde_json::Value =
                            serde_json::from_str(notification.params.get()).unwrap();
                        seen.push(format!("state:{}", params["state"].as_str().unwrap()));
                    }
                    ClientBridgeMessage::Notification(notification) => match notification.update {
                        SessionUpdate::AgentMessageChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                seen.push(format!("message:{}", text.text));
                            }
                        }
                        SessionUpdate::AgentThoughtChunk(chunk) => {
                            if let ContentBlock::Text(text) = chunk.content {
                                seen.push(format!("thought:{}", text.text));
                            }
                        }
                        SessionUpdate::SessionInfoUpdate(info)
                            if info.title
                                == crate::acp::protocol::MaybeUndefined::Value(
                                    marker.to_string(),
                                ) =>
                        {
                            return seen;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }

        fn marker(&self, seq: i64, title: &str) {
            self.publish(
                seq,
                true,
                querymt_agent_core::events::AgentEventKind::SessionMetadataUpdated {
                    title: Some(Some(title.to_string())),
                    updated_at: None,
                },
            );
        }

        async fn load(
            &mut self,
        ) -> (
            crate::acp::protocol::LoadSessionResponse,
            crate::acp::delivery::LoadHold,
            Option<crate::acp::delivery::LoadCut>,
        ) {
            let load = super::load_session_and_enqueue_replay(
                &self.fixture.handle,
                &self.bridge,
                crate::acp::protocol::LoadSessionRequest::new(
                    self.session_id.clone(),
                    std::path::PathBuf::new(),
                ),
                &self.translator,
                &self.session_load,
                &self.delivery,
            );
            tokio::pin!(load);
            // The load flushes the bridge before answering: serve the barrier.
            loop {
                tokio::select! {
                    result = &mut load => return result.expect("the load answers"),
                    message = self.bridge_rx.recv() => match message {
                        Some(ClientBridgeMessage::Flush { response_tx }) => {
                            let _ = response_tx.send(Ok(()));
                        }
                        Some(other) => self.stashed.push_back(other),
                        None => {}
                    },
                }
            }
        }
    }

    impl Drop for LoadRig {
        fn drop(&mut self) {
            if let Some(forwarder) = self.forwarder.take() {
                forwarder.abort();
            }
        }
    }

    #[tokio::test]
    async fn message_streaming_at_the_load_arrives_in_full() {
        let mut rig = LoadRig::new().await;
        // 400 characters streamed before this connection looked at the session.
        for (seq, chunk) in [(2, "a"), (3, "b"), (4, "c"), (5, "d")] {
            rig.delta(seq, "m1", &chunk.repeat(100));
        }
        rig.start_forwarder();
        tokio::task::yield_now().await;

        let (_response, hold, cut) = rig.load().await;
        // More text streams while the load is being answered.
        rig.delta(6, "m1", &"e".repeat(50));
        hold.release_with(cut, rig.fixture.config.event_sink.fanout());
        // The rest of the message, then its stored form.
        rig.delta(7, "m1", &"f".repeat(300));
        rig.publish(
            8,
            true,
            querymt_agent_core::events::AgentEventKind::AssistantMessageStored {
                content: format!(
                    "{}{}{}{}{}{}",
                    "a".repeat(100),
                    "b".repeat(100),
                    "c".repeat(100),
                    "d".repeat(100),
                    "e".repeat(50),
                    "f".repeat(300)
                ),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("m1".into()),
            },
        );
        rig.marker(9, "after-stored");

        let seen = rig.received_until("after-stored").await;
        let messages: Vec<&str> = seen
            .iter()
            .filter_map(|entry| entry.strip_prefix("message:"))
            .collect();
        // The text up to the answer is one chunk, the rest follows, and the
        // stored message adds nothing: the client holds the message once.
        assert_eq!(messages.len(), 2, "{seen:?}");
        assert_eq!(messages[0].len(), 450);
        assert!(messages[0].starts_with(&"a".repeat(100)));
        assert_eq!(messages[1], "f".repeat(300));
    }

    #[tokio::test]
    async fn reasoning_streaming_at_the_load_arrives_in_full() {
        let mut rig = LoadRig::new().await;
        rig.publish(
            2,
            false,
            querymt_agent_core::events::AgentEventKind::AssistantThinkingDelta {
                content: "considering ".into(),
                message_id: "m1".into(),
                part_id: Some("p1".into()),
            },
        );
        rig.publish(
            3,
            false,
            querymt_agent_core::events::AgentEventKind::AssistantThinkingDelta {
                content: "options".into(),
                message_id: "m1".into(),
                part_id: Some("p1".into()),
            },
        );
        rig.start_forwarder();
        tokio::task::yield_now().await;

        let (_response, hold, cut) = rig.load().await;
        hold.release_with(cut, rig.fixture.config.event_sink.fanout());
        rig.publish(
            4,
            false,
            querymt_agent_core::events::AgentEventKind::AssistantThinkingDelta {
                content: " and more".into(),
                message_id: "m1".into(),
                part_id: Some("p1".into()),
            },
        );
        rig.marker(5, "after-reasoning");

        let seen = rig.received_until("after-reasoning").await;
        let thoughts: Vec<&str> = seen
            .iter()
            .filter_map(|entry| entry.strip_prefix("thought:"))
            .collect();
        assert_eq!(thoughts, ["considering options", " and more"], "{seen:?}");
    }

    impl LoadRig {
        /// Publish one generated envelope through the agent's event sink:
        /// durable events are journaled, ephemeral ones only streamed.
        async fn emit_envelope(&self, envelope: &querymt_agent_core::events::EventEnvelope) {
            let mut kind = envelope.kind().clone();
            // Attempts are charged to the session that journals them.
            querymt_agent_session_core::testing::charge_to_session(&mut kind, &self.session_id);
            let sink = &self.fixture.config.event_sink;
            match envelope {
                querymt_agent_core::events::EventEnvelope::Durable(_) => {
                    sink.emit_durable(&self.session_id, kind)
                        .await
                        .expect("the event is journaled");
                }
                querymt_agent_core::events::EventEnvelope::Ephemeral(_) => {
                    sink.emit_ephemeral(&self.session_id, kind);
                }
            }
        }

        /// What the client received, as reducer inputs, until the session
        /// reports `idle` for `run_id`.
        async fn received_inputs_until_idle(
            &mut self,
            run_id: &str,
        ) -> Vec<querymt_agent_session_core::input::SessionInput> {
            use querymt_agent_session_core::input::SessionInput;
            let mut inputs = Vec::new();
            loop {
                let message = match self.stashed.pop_front() {
                    Some(message) => message,
                    None => timeout(Duration::from_secs(5), self.bridge_rx.recv())
                        .await
                        .expect("the run should report idle")
                        .expect("bridge stays open"),
                };
                match message {
                    ClientBridgeMessage::ExtNotification(notification)
                        if notification.method.as_ref()
                            == crate::acp::shared::SESSION_STATE_UPDATE_METHOD =>
                    {
                        let params: serde_json::Value =
                            serde_json::from_str(notification.params.get()).unwrap();
                        let done = params["state"] == "idle" && params["runId"] == run_id;
                        inputs.push(SessionInput::StateUpdate {
                            notification: params,
                            at_ms: 0,
                        });
                        if done {
                            return inputs;
                        }
                    }
                    ClientBridgeMessage::Notification(notification) => {
                        inputs.push(SessionInput::AcpNotification {
                            notification: serde_json::to_value(&notification).unwrap(),
                            replayed: false,
                            at_ms: 0,
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    /// A client attaches while an assistant message is streaming (spec:
    /// Load during a run). The snapshot holds everything journaled, the
    /// load hands over the text streamed so far, and the rest arrives live;
    /// the view must equal the one of a client that saw the whole stream.
    #[tokio::test]
    async fn load_during_a_run_continues_seamlessly() {
        use crate::acp::session_core_conformance::{
            comparable, gpui_capabilities, journal, live_envelopes, live_stream,
        };
        use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
        use querymt_agent_session_core::SessionCore;
        use querymt_agent_session_core::input::SessionInput;
        use querymt_agent_session_core::testing::TraceSpec;

        let events = journal(TraceSpec::small(3));
        let reference = {
            let mut core = SessionCore::new("s1");
            let report = core.apply(&live_stream(&events));
            assert!(report.errors.is_empty(), "{report:?}");
            comparable(&core)
        };

        // The envelopes the agent publishes, and the cut: inside the third
        // reply, after its first streamed piece.
        let mut blocked = std::collections::HashSet::new();
        let envelopes: Vec<EventEnvelope> = events
            .iter()
            .flat_map(|event| live_envelopes(event, &mut blocked))
            .collect();
        let first_piece = envelopes
            .iter()
            .position(|envelope| {
                matches!(
                    envelope.kind(),
                    AgentEventKind::AssistantContentDelta { message_id, .. } if message_id == "a2"
                )
            })
            .expect("the third reply streams");
        let (before_cut, after_cut) = envelopes.split_at(first_piece + 1);
        let (during_load, rest) = after_cut.split_at(1);
        assert!(
            matches!(
                during_load[0].kind(),
                AgentEventKind::AssistantContentDelta { .. }
            ),
            "the load answers between two pieces of the message"
        );
        let last_run = events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                AgentEventKind::RunCompleted { run_id, .. } => Some(run_id.clone()),
                _ => None,
            })
            .expect("the trace ends a run");

        let mut rig = LoadRig::new().await;
        rig.session_load.lock().unwrap().snapshot_history = true;
        rig.translator
            .lock()
            .unwrap()
            .set_capabilities(gpui_capabilities());
        for envelope in before_cut {
            rig.emit_envelope(envelope).await;
        }
        rig.start_forwarder();
        tokio::task::yield_now().await;

        let (response, hold, cut) = rig.load().await;
        // What the load found: the run in progress (the test agent has no
        // actor running it, so it is set as the runtime status would).
        rig.translator
            .lock()
            .unwrap()
            .seed_run_state(&rig.session_id, Some("r2".into()), false);
        for envelope in during_load {
            rig.emit_envelope(envelope).await;
        }
        hold.release_with(cut, rig.fixture.config.event_sink.fanout());
        for envelope in rest {
            rig.emit_envelope(envelope).await;
        }

        let mut inputs = vec![SessionInput::SnapshotLoaded {
            response: serde_json::to_value(&response).unwrap(),
        }];
        inputs.extend(rig.received_inputs_until_idle(&last_run).await);
        let mut core = SessionCore::new(rig.session_id.clone());
        let report = core.apply(&inputs);
        assert!(report.errors.is_empty(), "{report:?}");
        crate::acp::session_core_conformance::assert_same_view(
            "load during a run",
            &comparable(&core),
            &reference,
        );
    }

    #[tokio::test]
    async fn current_state_after_a_load() {
        let mut rig = LoadRig::new().await;
        rig.start_forwarder();
        tokio::task::yield_now().await;

        // The load finds a run in progress; a delta arrives while it answers.
        let hold = rig.delivery.begin_load(&rig.session_id);
        rig.translator
            .lock()
            .unwrap()
            .seed_run_state(&rig.session_id, Some("run-7".into()), false);
        rig.delta(3, "m1", "live");
        hold.release(crate::acp::delivery::LoadCut::default(), None);
        rig.marker(4, "after-load");

        let seen = rig.received_until("after-load").await;
        // One state update first, then the held update; no historical state.
        assert_eq!(seen, ["state:running", "message:live"]);
    }

    #[tokio::test]
    async fn a_held_event_before_the_cut_is_not_sent() {
        let mut rig = LoadRig::new().await;
        rig.start_forwarder();
        tokio::task::yield_now().await;

        let hold = rig.delivery.begin_load(&rig.session_id);
        rig.delta(2, "m1", "inside the snapshot");
        rig.delta(5, "m2", "after the cut");
        // The answer contains everything up to session_seq 3.
        hold.release(
            crate::acp::delivery::LoadCut {
                session_seq: 3,
                local_stream_seq: 0,
            },
            None,
        );
        rig.marker(6, "after-cut");

        let seen = rig.received_until("after-cut").await;
        assert_eq!(seen, ["message:after the cut"]);
    }

    #[tokio::test]
    async fn run_state_updates_arrive_in_order_through_the_bridge() {
        use querymt_agent_core::events::{
            AgentEventKind, DurableEvent, EventEnvelope, EventOrigin,
        };
        let fixture = crate::test_utils::TestAgent::new().await;
        let (bridge_tx, mut bridge_rx) = mpsc::channel(16);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;
        let event = |seq: i64, kind: AgentEventKind| {
            EventEnvelope::Durable(DurableEvent {
                session_seq: seq,
                event_id: format!("e{seq}"),
                stream_seq: seq,
                session_id: "parent".into(),
                timestamp: 0,
                origin: EventOrigin::Local,
                source_node: None,
                kind,
            })
        };
        // A run that starts and ends back to back, with a wait in between.
        let fanout = fixture.config.event_sink.fanout();
        fanout.publish(event(
            1,
            AgentEventKind::RunStarted {
                run_id: "run-1".into(),
                origin: "interactive".into(),
            },
        ));
        fanout.publish(event(
            2,
            AgentEventKind::PermissionRequested {
                permission_id: "perm-1".into(),
                task_id: None,
                tool_name: "shell".into(),
                reason: "approval".into(),
                tool_call_id: "call-1".into(),
                request: None,
            },
        ));
        fanout.publish(event(
            3,
            AgentEventKind::PermissionGranted {
                permission_id: "perm-1".into(),
                granted: true,
            },
        ));
        fanout.publish(event(
            4,
            AgentEventKind::RunCompleted {
                run_id: "run-1".into(),
                outcome: "completed".into(),
                stop_type: None,
                reason: None,
            },
        ));

        let mut seen = Vec::new();
        while seen.len() < 4 {
            let message = timeout(Duration::from_secs(2), bridge_rx.recv())
                .await
                .expect("state update should arrive")
                .expect("bridge should remain open");
            let ClientBridgeMessage::ExtNotification(notification) = message else {
                continue;
            };
            if notification.method.as_ref() != crate::acp::shared::SESSION_STATE_UPDATE_METHOD {
                continue;
            }
            let params: serde_json::Value =
                serde_json::from_str(notification.params.get()).expect("valid params");
            assert_eq!(params["sessionId"], "parent");
            assert_eq!(params["runId"], "run-1");
            seen.push(params["state"].as_str().unwrap().to_string());
        }
        assert_eq!(seen, ["running", "requires_action", "running", "idle"]);
        forwarder.abort();
    }

    #[tokio::test]
    async fn delegation_update_uses_typed_ext_notification_bridge() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;
        let delegation = crate::session::domain::Delegation {
            isolation: Default::default(),
            id: 0,
            public_id: "delegation-1".into(),
            session_id: 0,
            task_id: None,
            target_agent_id: "coder".into(),
            objective: "Implement it".into(),
            objective_hash: querymt_agent_core::hash::RapidHash::default(),
            context: None,
            constraints: None,
            expected_output: None,
            verification_spec: None,
            planning_summary: None,
            status: crate::session::domain::DelegationStatus::Requested,
            retry_count: 0,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            completed_at: None,
        };
        fixture.config.event_sink.fanout().publish(
            querymt_agent_core::events::EventEnvelope::Durable(
                querymt_agent_core::events::DurableEvent {
                    session_seq: 0,
                    event_id: "event-1".into(),
                    stream_seq: 1,
                    session_id: "parent-1".into(),
                    timestamp: 10,
                    origin: querymt_agent_core::events::EventOrigin::Local,
                    source_node: None,
                    kind: querymt_agent_core::events::AgentEventKind::DelegationRequested {
                        delegation,
                        tool_call_id: Some("call-1".into()),
                        batch_id: None,
                        cycle_id: None,
                    },
                },
            ),
        );

        let message = timeout(Duration::from_secs(2), bridge_rx.recv())
            .await
            .expect("notification should arrive")
            .expect("bridge should remain open");
        match message {
            ClientBridgeMessage::ExtNotification(notification) => {
                assert_eq!(
                    notification.method.as_ref(),
                    crate::acp::shared::QMT_NOTIFICATION_DELEGATION_UPDATE
                );
                let params: serde_json::Value =
                    serde_json::from_str(notification.params.get()).expect("valid params");
                assert_eq!(params["delegationId"], "delegation-1");
                assert_eq!(params["toolCallId"], "call-1");
            }
            _ => panic!("expected extension notification"),
        }
        forwarder.abort();
    }

    #[test]
    fn outbound_workspace_query_preserves_wire_extension_method() {
        let request = agent_ext_request(
            "_workspace/query",
            serde_json::json!({ "sessionId": "s-1" }),
        )
        .expect("agent ext request");

        assert_eq!(request.method(), "_workspace/query");
        match &request {
            AgentRequest::ExtMethodRequest(ext) => {
                assert_eq!(ext.method.as_ref(), "_workspace/query");
            }
            other => panic!("expected ext request, got {other:?}"),
        }

        let untyped = request.to_untyped_message().expect("untyped ext request");
        assert_eq!(untyped.method, "_workspace/query");
    }

    #[test]
    fn native_elicitation_request_preserves_scope_schema_and_metadata() {
        let request = create_elicitation_request(
            "e-1".to_string(),
            "s-1".to_string(),
            "Choose one".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "selection": {
                        "type": "string",
                        "oneOf": [{"const": "A", "title": "A"}]
                    }
                },
                "required": ["selection"]
            }),
            "builtin:question".to_string(),
        )
        .expect("native elicitation request");

        let value = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(value["mode"], "form");
        assert_eq!(value["sessionId"], "s-1");
        assert_eq!(value["message"], "Choose one");
        assert_eq!(
            value["requestedSchema"]["properties"]["selection"]["oneOf"][0]["const"],
            "A"
        );
        assert_eq!(value["_meta"]["querymt"]["elicitation_id"], "e-1");
        assert_eq!(value["_meta"]["querymt"]["source"], "builtin:question");
        assert_eq!(request.method(), "elicitation/create");
    }

    #[test]
    fn native_elicitation_rejects_invalid_schema() {
        let result = create_elicitation_request(
            "e-1".to_string(),
            "s-1".to_string(),
            "Invalid".to_string(),
            serde_json::json!({"type": "object", "properties": {"nested": {"type": "object"}}}),
            "test".to_string(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn native_elicitation_preserves_decline_and_cancel_actions() {
        let declined = super::convert_elicitation_response(
            crate::acp::protocol::CreateElicitationResponse::new(AcpElicitationAction::Decline),
        )
        .expect("decline response");
        let cancelled = super::convert_elicitation_response(
            crate::acp::protocol::CreateElicitationResponse::new(AcpElicitationAction::Cancel),
        )
        .expect("cancel response");

        assert_eq!(declined.action, ElicitationAction::Decline);
        assert_eq!(cancelled.action, ElicitationAction::Cancel);
        assert_eq!(declined.content, None);
        assert_eq!(cancelled.content, None);
    }

    /// A stdio forwarder that falls behind the live buffer recovers the missed
    /// durable updates from the journal and keeps forwarding, in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stdio_forwarder_recovers_from_broadcast_lag_in_order() {
        const TOTAL: usize = 1_400; // more than the 1024-slot live buffer

        let fixture = crate::test_utils::TestAgent::new().await;
        let session_id = "stdio-lagging-session";
        // A one-slot bridge that nobody drains yet stalls the forwarder while
        // the session keeps producing events.
        let (bridge_tx, mut bridge_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;

        let stream = fixture.config.event_sink.session_stream(session_id);
        let stored =
            |content: String| querymt_agent_core::events::AgentEventKind::AssistantMessageStored {
                content,
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: None,
            };
        for n in 1..=TOTAL {
            stream.emit(stored(format!("m{n}")));
        }
        timeout(Duration::from_secs(20), async {
            while stream.published_tip() < TOTAL as i64 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all events published");

        async fn next_text(
            bridge_rx: &mut mpsc::Receiver<ClientBridgeMessage>,
            wait: Duration,
        ) -> Option<String> {
            loop {
                let message = timeout(wait, bridge_rx.recv()).await.ok()??;
                if let ClientBridgeMessage::Notification(notification) = message {
                    let json = serde_json::to_value(&notification).unwrap();
                    return json["update"]["content"]["text"]
                        .as_str()
                        .map(str::to_string);
                }
            }
        }

        let mut received = Vec::with_capacity(TOTAL);
        while received.len() < TOTAL {
            let text = next_text(&mut bridge_rx, Duration::from_secs(20))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "forwarder stopped after {} of {TOTAL} updates",
                        received.len()
                    )
                });
            received.push(text);
        }
        let expected: Vec<String> = (1..=TOTAL).map(|n| format!("m{n}")).collect();
        assert_eq!(
            received, expected,
            "every durable update arrives once, in production order"
        );

        stream.emit(stored("after-lag".to_string()));
        let text = next_text(&mut bridge_rx, Duration::from_secs(5))
            .await
            .expect("update after recovery");
        assert_eq!(text, "after-lag");
        forwarder.abort();
    }

    #[tokio::test]
    async fn stdio_transport_error_keeps_original_elicitation_waiter_pending() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let session_id = "stdio-recoverable-session";
        let elicitation_id = "stdio-recoverable-elicitation";
        let (waiter_tx, mut waiter_rx) = tokio::sync::oneshot::channel();
        crate::elicitation::insert_pending_elicitation(
            &fixture.handle.pending_elicitations(),
            elicitation_id.to_string(),
            session_id.to_string(),
            waiter_tx,
        )
        .await;

        let (bridge_tx, mut bridge_rx) = mpsc::channel(4);
        let (shutdown_tx, _shutdown_rx) = mpsc::channel(1);
        let forwarder = spawn_event_bridge_forwarder(
            fixture.config.event_sink.fanout().clone(),
            ClientBridgeSender::new(bridge_tx),
            fixture.handle.clone(),
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new())),
            crate::acp::delivery::ConnectionDelivery::new(
                Some(fixture.config.event_sink.journal().clone()),
                tokio_util::sync::CancellationToken::new(),
            ),
            shutdown_tx,
        );
        tokio::task::yield_now().await;
        fixture.config.event_sink.fanout().publish(
            querymt_agent_core::events::EventEnvelope::Ephemeral(
                querymt_agent_core::events::EphemeralEvent {
                    session_seq: 0,
                    session_id: session_id.to_string(),
                    timestamp: 0,
                    origin: querymt_agent_core::events::EventOrigin::Local,
                    source_node: None,
                    kind: querymt_agent_core::events::AgentEventKind::ElicitationRequested {
                        elicitation_id: elicitation_id.to_string(),
                        session_id: session_id.to_string(),
                        message: "Choose".to_string(),
                        requested_schema: serde_json::json!({"type": "object"}),
                        source: "builtin:question".to_string(),
                    },
                },
            ),
        );

        let message = timeout(Duration::from_secs(2), bridge_rx.recv())
            .await
            .expect("elicitation should reach stdio bridge")
            .expect("bridge channel should remain open");
        let ClientBridgeMessage::Elicit { response_tx, .. } = message else {
            panic!("expected elicitation bridge message");
        };
        response_tx
            .send(Err(
                agent_client_protocol::Error::internal_error().data("disconnected")
            ))
            .expect("forwarder should await the result");
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        assert!(
            fixture
                .handle
                .pending_elicitations()
                .lock()
                .await
                .contains_key(elicitation_id)
        );
        assert!(matches!(
            waiter_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        forwarder.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn native_elicitation_round_trip_uses_typed_acp_request() {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let (client_read, client_write) = split(client_stream);
        let (server_read, server_write) = split(server_stream);
        let seen_request = Arc::new(Mutex::new(None::<CreateElicitationRequest>));
        let seen_request_for_handler = seen_request.clone();

        let client_task = tokio::spawn(async move {
            Client
                .v1()
                .on_receive_request(
                    async move |request: CreateElicitationRequest, responder, _cx| {
                        *seen_request_for_handler.lock().await = Some(request);
                        let content = BTreeMap::from([(
                            "selection".to_string(),
                            ElicitationContentValue::String("Production".to_string()),
                        )]);
                        responder.respond(crate::acp::protocol::CreateElicitationResponse::new(
                            AcpElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(content),
                            ),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(ByteStreams::new(
                    client_write.compat_write(),
                    client_read.compat(),
                ))
                .await
                .expect("client should run")
        });

        let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(4);
        let bridge = ClientBridgeSender::new(bridge_tx);
        let server = Agent.v1().connect_with(
            ByteStreams::new(server_write.compat_write(), server_read.compat()),
            async move |connection| {
                run_bridge_task(bridge_rx, connection).await;
                Ok(())
            },
        );
        let interaction = async move {
            let response = bridge
                .elicit(
                    "e-2".to_string(),
                    "s-2".to_string(),
                    "Pick a target".to_string(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "selection": {
                                "type": "string",
                                "oneOf": [{"const": "Production", "title": "Production"}]
                            }
                        },
                        "required": ["selection"]
                    }),
                    "builtin:question".to_string(),
                )
                .await
                .expect("elicitation should succeed");

            assert_eq!(response.action, ElicitationAction::Accept);
            assert_eq!(
                response.content,
                Some(serde_json::json!({"selection": "Production"}))
            );

            let invalid_result = bridge
                .elicit(
                    "e-invalid".to_string(),
                    "s-2".to_string(),
                    "Invalid schema".to_string(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {"nested": {"type": "object"}}
                    }),
                    "test".to_string(),
                )
                .await;
            assert!(invalid_result.is_err());

            bridge
                .notify(SessionNotification::new(
                    SessionId::from("s-2"),
                    crate::acp::protocol::SessionUpdate::AgentMessageChunk(
                        crate::acp::protocol::ContentChunk::new(
                            crate::acp::protocol::ContentBlock::Text(
                                crate::acp::protocol::TextContent::new("still alive"),
                            ),
                        ),
                    ),
                ))
                .await
                .expect("bridge should remain alive after elicitation failure");
        };

        let (server_result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(server, interaction)
        })
        .await
        .expect("ordinary-runtime elicitation round trip should finish");
        server_result.expect("server should stop cleanly after bridge closes");

        let request = seen_request
            .lock()
            .await
            .clone()
            .expect("client should receive request");
        let request_value = serde_json::to_value(request).expect("serialize captured request");
        assert_eq!(request_value["sessionId"], "s-2");
        assert_eq!(request_value["_meta"]["querymt"]["elicitation_id"], "e-2");

        client_task.abort();
        let _ = client_task.await;
    }

    #[tokio::test]
    async fn spawned_prompt_handler_keeps_cancel_unblocked() {
        let prompt_started = Arc::new(Notify::new());
        let release_prompt = Arc::new(Notify::new());
        let cancel_seen = Arc::new(Notify::new());
        let cancel_flag = Arc::new(Mutex::new(false));

        let prompt_started_for_handler = prompt_started.clone();
        let release_prompt_for_handler = release_prompt.clone();
        let cancel_seen_for_handler = cancel_seen.clone();
        let cancel_flag_for_handler = cancel_flag.clone();

        let (client, server) = tokio::io::duplex(4096);
        let (client_read, mut client_write) = split(client);
        let (server_read, server_write) = split(server);

        let server_task = tokio::spawn(async move {
            Agent
                .v1()
                .on_receive_request(
                    async move |_req: PromptRequest, responder, cx| {
                        let prompt_started = prompt_started_for_handler.clone();
                        let release_prompt = release_prompt_for_handler.clone();
                        cx.spawn(async move {
                            prompt_started.notify_one();
                            release_prompt.notified().await;
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |_notif: CancelNotification, _cx| {
                        *cancel_flag_for_handler.lock().await = true;
                        cancel_seen_for_handler.notify_one();
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_to(ByteStreams::new(
                    server_write.compat_write(),
                    server_read.compat(),
                ))
                .await
                .expect("server should run")
        });

        let mut reader = BufReader::new(client_read);

        client_write
            .write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s-1","prompt":[{"type":"text","text":"long task"}]}}
{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s-1"}}
"#,
            )
            .await
            .expect("requests should write");

        timeout(Duration::from_secs(2), prompt_started.notified())
            .await
            .expect("prompt handler should start");
        timeout(Duration::from_secs(2), cancel_seen.notified())
            .await
            .expect("cancel should be processed while prompt is blocked");
        assert!(
            *cancel_flag.lock().await,
            "cancel should be processed before the prompt is released"
        );

        release_prompt.notify_one();

        let mut response = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut response))
            .await
            .expect("response should arrive")
            .expect("response should read");
        assert!(response.contains("\"stopReason\":\"cancelled\""));

        drop(reader);
        drop(client_write);
        server_task.abort();
        let _ = server_task.await;
    }
}
