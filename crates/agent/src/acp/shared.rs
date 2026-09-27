//! Shared types and functions for ACP server implementations.
//!
//! This module provides common types and utilities used by both stdio and WebSocket
//! ACP server implementations, including JSON-RPC types, event translation, and
//! RPC message handling.

use crate::acp::protocol::AGENT_METHOD_NAMES;
use crate::acp::protocol::{
    ClientCapabilities, CompactionId, CompactionStatus, CompactionSummaryChunk, CompactionUpdate,
    ConfigOptionUpdate, Content, ContentBlock, ContentChunk, CreateElicitationRequest,
    CreateElicitationResponse, CurrentModeUpdate, ElicitationAction as AcpElicitationAction,
    ElicitationFormMode, ElicitationSchema, ElicitationSessionScope, Error, MaybeUndefined,
    MessageId, Notice, NoticeSeverity, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PlanId,
    PlanRemoved, PlanUpdate, PlanUpdateContent, RequestPermissionOutcome, SessionId,
    SessionInfoUpdate, SessionModeId, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, UsageUpdate,
};
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::control::remote::{AttachRemoteSessionRequest, RemoteSessionAttachInfo};
use crate::event_fanout::EventFanout;
use crate::events::{AgentEvent, AgentEventKind, EventEnvelope, ReasoningPartStored};
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Session subscriptions keyed by session id.
///
/// Multiple connections may subscribe to the same session so browser tabs do not
/// steal live updates from one another.
type SessionRequestLocks = Arc<Mutex<HashMap<(String, String), Weak<Mutex<()>>>>>;

#[derive(Clone, Default)]
pub struct SessionOwnerMap {
    owners: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    request_locks: SessionRequestLocks,
}

impl SessionOwnerMap {
    /// Lock the session subscription map.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, HashMap<String, HashSet<String>>> {
        self.owners.lock().await
    }

    async fn request_lock(&self, session_id: &str, conn_id: &str) -> Arc<Mutex<()>> {
        let key = (session_id.to_string(), conn_id.to_string());
        let mut request_locks = self.request_locks.lock().await;
        request_locks.retain(|_, request_lock| request_lock.strong_count() > 0);
        if let Some(request_lock) = request_locks.get(&key).and_then(Weak::upgrade) {
            return request_lock;
        }

        let request_lock = Arc::new(Mutex::new(()));
        request_locks.insert(key, Arc::downgrade(&request_lock));
        request_lock
    }
}

async fn subscribe_connection(
    session_owners: &SessionOwnerMap,
    session_id: String,
    conn_id: &str,
) -> bool {
    session_owners
        .lock()
        .await
        .entry(session_id)
        .or_default()
        .insert(conn_id.to_string())
}

async fn unsubscribe_connection(session_owners: &SessionOwnerMap, session_id: &str, conn_id: &str) {
    let mut owners = session_owners.lock().await;
    let remove_session = owners.get_mut(session_id).is_some_and(|subscribers| {
        subscribers.remove(conn_id);
        subscribers.is_empty()
    });
    if remove_session {
        owners.remove(session_id);
    }
}

/// Type alias for pending permission requests (tool_call_id -> response sender)
pub type PermissionMap = Arc<Mutex<HashMap<String, oneshot::Sender<RequestPermissionOutcome>>>>;

/// Type alias for pending elicitation requests (elicitation_id -> response sender)
pub type PendingElicitationMap = crate::elicitation::PendingElicitationMap;

pub(crate) fn create_elicitation_request(
    elicitation_id: String,
    session_id: String,
    message: String,
    requested_schema: serde_json::Value,
    source: String,
) -> Result<CreateElicitationRequest, Error> {
    let schema = serde_json::from_value::<ElicitationSchema>(requested_schema).map_err(|err| {
        Error::invalid_params().data(format!("Invalid elicitation schema: {err}"))
    })?;

    for (name, property) in &schema.properties {
        if let crate::acp::protocol::ElicitationPropertySchema::Other(other) = property {
            return Err(Error::invalid_params().data(format!(
                "Unsupported elicitation property type `{}` for property `{name}`",
                other.type_
            )));
        }
    }

    let scope = ElicitationSessionScope::new(SessionId::from(session_id));
    let mode = ElicitationFormMode::new(scope, schema);
    let mut meta = serde_json::Map::new();
    meta.insert(
        "querymt".to_string(),
        serde_json::json!({
            "elicitation_id": elicitation_id,
            "source": source,
        }),
    );

    Ok(CreateElicitationRequest::new(mode, message).meta(meta))
}

pub(crate) fn convert_elicitation_response(
    response: CreateElicitationResponse,
) -> Result<crate::elicitation::ElicitationResponse, Error> {
    let (action, content) = match response.action {
        AcpElicitationAction::Accept(accepted) => {
            let content = accepted
                .content
                .map(serde_json::to_value)
                .transpose()
                .map_err(Error::into_internal_error)?;
            (crate::elicitation::ElicitationAction::Accept, content)
        }
        AcpElicitationAction::Decline => (crate::elicitation::ElicitationAction::Decline, None),
        AcpElicitationAction::Cancel => (crate::elicitation::ElicitationAction::Cancel, None),
        _ => {
            return Err(Error::invalid_params().data("Unsupported elicitation response action"));
        }
    };

    Ok(crate::elicitation::ElicitationResponse { action, content })
}

pub(crate) fn convert_elicitation_response_value(
    value: serde_json::Value,
) -> Result<crate::elicitation::ElicitationResponse, Error> {
    let response = serde_json::from_value(value).map_err(|err| {
        Error::invalid_params().data(format!("Invalid elicitation response: {err}"))
    })?;
    convert_elicitation_response(response)
}

/// JSON-RPC 2.0 method call envelope for both requests and notifications.
#[derive(Deserialize)]
pub struct RpcMessage {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
    pub id: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 response structure
#[derive(Serialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
    pub id: serde_json::Value,
}

pub struct RpcDispatchOutput {
    pub notifications: Vec<serde_json::Value>,
    pub response: Option<RpcResponse>,
}

/// Dispatch one JSON-RPC method call and forward all resulting wire messages.
///
/// Transports spawn this helper for each inbound call so a long-running request
/// cannot prevent a later notification, such as `session/cancel`, from running.
/// The dispatched work intentionally outlives a client connection; sessions are
/// owned by the agent runtime rather than a transport connection.
pub(crate) async fn dispatch_rpc_message<S: SendAgent>(
    agent: Arc<S>,
    session_owners: SessionOwnerMap,
    pending_permissions: PermissionMap,
    pending_elicitations: PendingElicitationMap,
    conn_id: String,
    request: RpcMessage,
    tx: mpsc::Sender<String>,
) {
    dispatch_rpc_message_with_context(
        RpcDispatchState {
            agent,
            session_owners,
            pending_permissions,
            pending_elicitations,
            conn_id,
            tx,
        },
        request,
        RpcDispatchContext::default(),
    )
    .await;
}

#[derive(Clone)]
pub(crate) struct RpcDispatchState<S> {
    pub agent: Arc<S>,
    pub session_owners: SessionOwnerMap,
    pub pending_permissions: PermissionMap,
    pub pending_elicitations: PendingElicitationMap,
    pub conn_id: String,
    pub tx: mpsc::Sender<String>,
}

pub(crate) async fn dispatch_rpc_message_with_context<S: SendAgent>(
    state: RpcDispatchState<S>,
    request: RpcMessage,
    context: RpcDispatchContext,
) {
    let RpcDispatchState {
        agent,
        session_owners,
        pending_permissions,
        pending_elicitations,
        conn_id,
        tx,
    } = state;
    let output = handle_rpc_message_with_context(
        agent.as_ref(),
        &session_owners,
        &pending_permissions,
        &pending_elicitations,
        &conn_id,
        request,
        context,
    )
    .await;

    // Reply first so the client can bind the session before catalog updates arrive.
    if let Some(response) = output.response {
        match serde_json::to_string(&response) {
            Ok(json) => {
                if tx.send(json).await.is_err() {
                    return;
                }
            }
            Err(err) => log::warn!("Failed to serialize JSON-RPC response: {}", err),
        }
    }

    for notification in output.notifications {
        let json = match serde_json::to_string(&notification) {
            Ok(json) => json,
            Err(err) => {
                log::warn!("Failed to serialize JSON-RPC notification: {}", err);
                continue;
            }
        };
        if tx.send(json).await.is_err() {
            return;
        }
    }
}

#[derive(Clone, Default)]
pub struct RpcDispatchContext {
    pub session_hooks: Option<Arc<dyn AcpSessionHooks>>,
    pub session_bridge: Option<crate::acp::client_bridge::ClientBridgeSender>,
    pub elicitation_recovery:
        Option<crate::control::elicitation_recovery::ElicitationRecoveryRegistry>,
    /// The connection's live translator. On successful initialization the
    /// negotiated client capabilities are parsed into it so Preview session
    /// updates stay scoped to this connection.
    pub translator: Option<Arc<std::sync::Mutex<AcpLiveEventTranslator>>>,
}

async fn attach_rpc_session<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    conn_id: &str,
    context: &RpcDispatchContext,
    session_id: &str,
    bridge_required: bool,
) -> Result<bool, Error> {
    let request_lock = session_owners.request_lock(session_id, conn_id).await;
    let _request_guard = request_lock.lock().await;
    attach_rpc_session_locked(
        agent,
        session_owners,
        conn_id,
        context,
        session_id,
        bridge_required,
    )
    .await
}

async fn attach_rpc_session_locked<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    conn_id: &str,
    context: &RpcDispatchContext,
    session_id: &str,
    bridge_required: bool,
) -> Result<bool, Error> {
    let connection_state = context
        .session_bridge
        .as_ref()
        .and_then(crate::acp::client_bridge::ClientBridgeSender::connection_state);
    let _attachment_guard = match connection_state.as_ref() {
        Some(state) => Some(state.lock_attachment().await),
        None => None,
    };
    if connection_state
        .as_ref()
        .is_some_and(|state| !state.is_active())
    {
        return if bridge_required {
            Err(Error::from(crate::error::AgentError::ClientBridgeClosed))
        } else {
            Ok(false)
        };
    }

    let ownership_inserted =
        subscribe_connection(session_owners, session_id.to_string(), conn_id).await;
    let Some(local_agent) = agent.as_any().downcast_ref::<AgentHandle>() else {
        return Ok(ownership_inserted);
    };
    if let Some(bridge) = context.session_bridge.as_ref()
        && let Err(error) = local_agent
            .set_session_bridge(session_id, bridge.clone())
            .await
    {
        if bridge_required {
            if ownership_inserted {
                unsubscribe_connection(session_owners, session_id, conn_id).await;
            }
            return Err(error);
        }
        log::warn!("Failed to attach ACP bridge for session {session_id}: {error}");
    }
    if let Some(hooks) = context.session_hooks.as_ref()
        && let Err(error) = hooks.on_session_attached(local_agent, session_id).await
    {
        if ownership_inserted {
            unsubscribe_connection(session_owners, session_id, conn_id).await;
        }
        return Err(error);
    }
    Ok(ownership_inserted)
}

#[async_trait::async_trait]
pub trait AcpSessionHooks: Send + Sync {
    async fn on_session_attached(
        &self,
        _agent: &AgentHandle,
        _session_id: &str,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn on_session_loaded(
        &self,
        _agent: &AgentHandle,
        _session_id: &str,
        _response: &mut serde_json::Value,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn on_remote_session_attached(
        &self,
        _agent: &AgentHandle,
        _session_id: &str,
        _response: &mut serde_json::Value,
    ) -> Result<(), Error> {
        Ok(())
    }
}

pub const QMT_NOTIFICATION_MESH_NODES_CHANGED: &str = "querymt/mesh/nodesChanged";
pub const QMT_NOTIFICATION_MESH_JOINED: &str = "querymt/mesh/joined";
pub const QMT_NOTIFICATION_MESH_PEER_EXPIRED: &str = "querymt/mesh/peerExpired";
pub const QMT_NOTIFICATION_MODELS_CHANGED: &str = "querymt/models/changed";
pub const QMT_NOTIFICATION_SCHEDULES_CHANGED: &str = "querymt/schedules/changed";
pub const QMT_NOTIFICATION_DELEGATION_UPDATE: &str = "querymt/session/delegationUpdate";
pub const QMT_NOTIFICATION_DELEGATE_MODELS_CHANGED: &str = "querymt/session/delegateModelsChanged";
pub const QMT_NOTIFICATION_INPUT_STATE: &str = "querymt/session/inputState";

fn ext_notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

pub fn mesh_nodes_changed_notification(peer_id: &str, change: &str) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_MESH_NODES_CHANGED,
        serde_json::to_value(
            crate::control::notifications::MeshNodesChangedNotification {
                peer_id: peer_id.to_string(),
                change: change.to_string(),
            },
        )
        .expect("serialize mesh nodes changed notification"),
    )
}

pub fn mesh_joined_notification(peer_id: &str, transport: &str) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_MESH_JOINED,
        serde_json::to_value(crate::control::notifications::MeshJoinedNotification {
            peer_id: peer_id.to_string(),
            transport: transport.to_string(),
        })
        .expect("serialize mesh joined notification"),
    )
}

pub fn mesh_peer_expired_notification(peer_id: &str) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_MESH_PEER_EXPIRED,
        serde_json::to_value(crate::control::notifications::MeshPeerExpiredNotification {
            peer_id: peer_id.to_string(),
        })
        .expect("serialize mesh peer expired notification"),
    )
}

pub fn models_changed_notification(reason: &str) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_MODELS_CHANGED,
        serde_json::to_value(crate::control::notifications::ModelsChangedNotification {
            reason: reason.to_string(),
        })
        .expect("serialize models changed notification"),
    )
}

pub fn schedules_changed_notification(
    payload: crate::control::notifications::SchedulesChangedNotification,
) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_SCHEDULES_CHANGED,
        serde_json::to_value(payload).expect("serialize schedules changed notification"),
    )
}

pub fn delegate_models_changed_notification(
    session_id: &str,
    revision: Option<u64>,
) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_DELEGATE_MODELS_CHANGED,
        serde_json::to_value(
            crate::control::delegate_models::DelegateModelsChangedNotification {
                version: crate::control::delegate_models::DELEGATE_MODELS_VERSION,
                session_id: session_id.to_owned(),
                revision: revision.into(),
            },
        )
        .expect("serialize delegate models changed notification"),
    )
}

pub fn delegation_update_notification(
    payload: crate::control::delegation_notifications::DelegationUpdateNotification,
) -> serde_json::Value {
    ext_notification(
        QMT_NOTIFICATION_DELEGATION_UPDATE,
        serde_json::to_value(payload).expect("serialize delegation update notification"),
    )
}

pub fn input_state_from_event(
    event: &EventEnvelope,
) -> Option<crate::control::notifications::SessionInputStateNotification> {
    use crate::control::notifications::{
        SESSION_INPUT_STATE_VERSION, SessionInputDelivery, SessionInputState,
        SessionInputStateNotification,
    };

    let mut notification = SessionInputStateNotification {
        version: SESSION_INPUT_STATE_VERSION,
        session_id: event.session_id().to_owned(),
        input_id: String::new(),
        delivery: SessionInputDelivery::Steer,
        state: SessionInputState::Accepted,
        run_id: None,
        position: None,
        boundary: None,
        reason: None,
        latency_ms: None,
    };
    match event.kind() {
        AgentEventKind::SteeringAccepted {
            run_id,
            input_id,
            position,
            ..
        } => {
            notification.input_id.clone_from(input_id);
            notification.run_id = Some(run_id.clone());
            notification.position = Some(*position);
        }
        AgentEventKind::SteeringApplied {
            run_id,
            input_id,
            boundary,
            latency_ms,
        } => {
            notification.input_id.clone_from(input_id);
            notification.state = SessionInputState::Applied;
            notification.run_id = Some(run_id.clone());
            notification.boundary = Some(boundary.clone());
            notification.latency_ms = Some(*latency_ms);
        }
        AgentEventKind::SteeringDiscarded {
            run_id,
            input_id,
            reason,
        } => {
            notification.input_id.clone_from(input_id);
            notification.state = SessionInputState::Discarded;
            notification.run_id = Some(run_id.clone());
            notification.reason = Some(reason.clone());
        }
        AgentEventKind::InputQueued {
            input_id, position, ..
        } => {
            notification.input_id.clone_from(input_id);
            notification.delivery = SessionInputDelivery::Queue;
            notification.state = SessionInputState::Queued;
            notification.position = Some(*position);
        }
        AgentEventKind::QueuedInputStarted { input_id, run_id } => {
            notification.input_id.clone_from(input_id);
            notification.delivery = SessionInputDelivery::Queue;
            notification.state = SessionInputState::Started;
            notification.run_id = Some(run_id.clone());
        }
        AgentEventKind::QueuedInputDiscarded { input_id, reason } => {
            notification.input_id.clone_from(input_id);
            notification.delivery = SessionInputDelivery::Queue;
            notification.state = SessionInputState::Discarded;
            notification.reason = Some(reason.clone());
        }
        _ => return None,
    }
    Some(notification)
}

pub fn input_state_notification(event: &EventEnvelope) -> Option<serde_json::Value> {
    let payload = input_state_from_event(event)?;
    Some(ext_notification(
        QMT_NOTIFICATION_INPUT_STATE,
        serde_json::to_value(payload).expect("serialize session input state notification"),
    ))
}

fn normalize_querymt_ext_method(method: &str) -> &str {
    method.strip_prefix('_').unwrap_or(method)
}

fn attach_before_querymt_ext_method(method: &str) -> bool {
    matches!(
        normalize_querymt_ext_method(method),
        "querymt/session/steer" | "querymt/session/queue" | "querymt/session/discardQueuedInput"
    )
}

fn querymt_session_id_from_request(method: &str, params: &serde_json::Value) -> Option<String> {
    match normalize_querymt_ext_method(method) {
        "querymt/session/delegateModels"
        | "querymt/session/setDelegateModel"
        | "querymt/session/steer"
        | "querymt/session/queue"
        | "querymt/session/discardQueuedInput"
        | "querymt/session/runtimeState" => params
            .get("session_id")
            .or_else(|| params.get("sessionId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "querymt/remote/attachSession" => {
            serde_json::from_value::<AttachRemoteSessionRequest>(params.clone())
                .ok()
                .map(|req| req.session_id)
        }
        _ => None,
    }
}

fn querymt_session_id_from_response(method: &str, value: &serde_json::Value) -> Option<String> {
    match normalize_querymt_ext_method(method) {
        "querymt/remote/createSession" => {
            serde_json::from_value::<RemoteSessionAttachInfo>(value.clone())
                .ok()
                .map(|resp| resp.session_id)
        }
        _ => None,
    }
}

pub fn event_envelopes_to_notifications<I>(events: I) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = EventEnvelope>,
{
    events
        .into_iter()
        .filter_map(|event| translate_replay_event_to_notification(&event))
        .collect()
}

pub fn replay_agent_events_to_session_notifications<I>(
    session_id: &str,
    events: I,
) -> Vec<crate::acp::protocol::SessionNotification>
where
    I: IntoIterator<Item = AgentEvent>,
{
    replay_agent_events_with_user_prompts(session_id, events, &HashMap::new())
}

fn thought_chunk(content: &str, message_id: &str, part_id: Option<&str>) -> ContentChunk {
    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(content.to_string())))
        .message_id(MessageId::from(message_id.to_string()));
    match part_id.filter(|part_id| !part_id.is_empty()) {
        Some(part_id) => chunk.meta(serde_json::Map::from_iter([(
            "querymt".to_string(),
            serde_json::json!({ "reasoning_part_id": part_id }),
        )])),
        None => chunk,
    }
}

fn stored_summary_updates(
    content: &str,
    message_id: Option<&str>,
    reasoning_parts: &[ReasoningPartStored],
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(message_id) = message_id {
        for part in reasoning_parts {
            if part.text.is_empty() {
                continue;
            }
            updates.push(SessionUpdate::AgentThoughtChunk(thought_chunk(
                &part.text,
                message_id,
                Some(&part.id),
            )));
        }
    }
    if !content.is_empty() {
        updates.push(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(content.to_string())))
                .message_id(message_id.map(|id| MessageId::from(id.to_string()))),
        ));
    }
    updates
}

fn user_prompt_chunk(
    message_id: &str,
    client_prompt_id: Option<&str>,
    block: ContentBlock,
) -> ContentChunk {
    let chunk = ContentChunk::new(block).message_id(MessageId::from(message_id.to_string()));
    match client_prompt_id {
        Some(client_prompt_id) => chunk.meta(serde_json::Map::from_iter([(
            "querymt".to_string(),
            serde_json::json!({"client_prompt_id": client_prompt_id}),
        )])),
        None => chunk,
    }
}

pub fn replay_agent_events_with_user_prompts<I>(
    session_id: &str,
    events: I,
    user_prompts: &HashMap<String, Vec<ContentBlock>>,
) -> Vec<crate::acp::protocol::SessionNotification>
where
    I: IntoIterator<Item = AgentEvent>,
{
    replay_agent_events_materialized(
        session_id,
        events,
        user_prompts,
        &AcpSessionUpdateCapabilities::default(),
    )
}

/// Projects replayed session history with materialized state.
///
/// Durable session-update state (configuration, metadata, usage, the current
/// todo plan, and terminal compactions) is folded before notifications are
/// created, so replay emits only current state with stable identities. Notices,
/// historical compaction summary chunks, and obsolete in-progress compaction
/// states are never replayed; Preview variants are gated by the connection's
/// capability snapshot.
pub fn replay_agent_events_materialized<I>(
    session_id: &str,
    events: I,
    user_prompts: &HashMap<String, Vec<ContentBlock>>,
    capabilities: &AcpSessionUpdateCapabilities,
) -> Vec<crate::acp::protocol::SessionNotification>
where
    I: IntoIterator<Item = AgentEvent>,
{
    let events: Vec<AgentEvent> = events.into_iter().collect();
    let materialized = materialize_replay_state(&events, capabilities);
    let mut notifications = Vec::new();
    let session = crate::acp::protocol::SessionId::from(session_id.to_string());
    let push = |update: SessionUpdate, notifications: &mut Vec<_>| {
        notifications.push(crate::acp::protocol::SessionNotification::new(
            session.clone(),
            update,
        ));
    };

    // 1. Latest current mode and configuration snapshot.
    if let Some(mode) = materialized.mode {
        push(
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(
                mode.as_str().to_string(),
            ))),
            &mut notifications,
        );
        let effort = materialized
            .reasoning_effort
            .as_deref()
            .and_then(parse_reasoning_effort);
        push(
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(session_config_options(
                mode, effort,
            ))),
            &mut notifications,
        );
    }

    // 2. Folded session metadata patch state.
    if materialized.metadata_touched {
        let mut update = SessionInfoUpdate::new();
        if let Some(title) = materialized.metadata_title {
            update.title = match title {
                Some(title) => MaybeUndefined::Value(title),
                None => MaybeUndefined::Null,
            };
        }
        if let Some(updated_at) = materialized.metadata_updated_at {
            update.updated_at = MaybeUndefined::Value(updated_at);
        }
        push(SessionUpdate::SessionInfoUpdate(update), &mut notifications);
    }

    // 3. Latest valid usage snapshot.
    if let Some(used) = materialized.usage_used
        && let Some(update) = materialized.usage.usage_update(used)
    {
        push(SessionUpdate::UsageUpdate(update), &mut notifications);
    }

    // 4. Current todo plan (retained only when non-empty).
    if let Some(entries) = materialized.todo_entries
        && !entries.is_empty()
    {
        if capabilities.plan_operations {
            push(
                SessionUpdate::PlanUpdate(PlanUpdate::new(PlanUpdateContent::items(
                    PlanId::from(QUERYMT_TODO_PLAN_ID.to_string()),
                    entries,
                ))),
                &mut notifications,
            );
        } else {
            push(SessionUpdate::Plan(Plan::new(entries)), &mut notifications);
        }
    }

    // 5. Terminal compactions with their original identities and final
    // summaries; chunks and in-progress states are omitted.
    if capabilities.compaction {
        for terminal in &materialized.compactions {
            let mut update = CompactionUpdate::new(
                CompactionId::from(terminal.id.clone()),
                terminal.status.clone(),
            );
            if let Some(summary) = &terminal.summary {
                update = update.summary(Some(vec![ContentBlock::Text(TextContent::new(
                    summary.clone(),
                ))]));
            }
            if let Some(error) = &terminal.error {
                update = update.error(error.clone());
            }
            push(SessionUpdate::CompactionUpdate(update), &mut notifications);
        }
    }

    // 6. Historical conversation/tool content in order.
    for event in events {
        let structured_updates = match &event.kind {
            AgentEventKind::PromptReceived {
                message_id: Some(message_id),
                ..
            } => user_prompts.get(message_id).map(|blocks| {
                blocks
                    .iter()
                    .cloned()
                    .map(|block| {
                        SessionUpdate::UserMessageChunk(user_prompt_chunk(message_id, None, block))
                    })
                    .collect::<Vec<_>>()
            }),
            _ => None,
        };
        let updates = structured_updates.unwrap_or_else(|| {
            let envelope = EventEnvelope::from(event);
            translate_replay_event_to_updates(&envelope)
        });
        for update in updates {
            push(update, &mut notifications);
        }
    }

    notifications
}

/// Final folded state of one compaction entity.
struct ReplayCompactionTerminal {
    id: String,
    status: CompactionStatus,
    summary: Option<String>,
    error: Option<String>,
}

/// Folds durable events into the current session-update state for replay.
fn materialize_replay_state(
    events: &[AgentEvent],
    capabilities: &AcpSessionUpdateCapabilities,
) -> MaterializedReplayState {
    let _ = capabilities;
    let mut state = MaterializedReplayState::default();
    // Most recent legacy (ID-less) compaction start, for pairing terminals.
    let mut open_legacy_start: Option<String> = None;
    let mut legacy_counter: usize = 0;

    for event in events {
        let envelope = EventEnvelope::from(event.clone());
        match event.kind.clone() {
            AgentEventKind::SessionModeChanged { mode } => {
                state.mode = Some(mode);
            }
            AgentEventKind::SessionConfigChanged {
                mode,
                reasoning_effort,
            } => {
                state.mode = Some(mode);
                state.reasoning_effort = reasoning_effort;
            }
            AgentEventKind::SessionMetadataUpdated { title, updated_at } => {
                state.metadata_touched = true;
                if title.is_some() {
                    state.metadata_title = title;
                }
                if updated_at.is_some() {
                    state.metadata_updated_at = updated_at;
                }
            }
            AgentEventKind::ProviderChanged { context_limit, .. } => {
                state.usage.context_limit = context_limit;
            }
            AgentEventKind::LlmRequestEnd {
                context_tokens,
                cumulative_cost_usd,
                ..
            } => {
                if let Some(cost) = cumulative_cost_usd {
                    state.usage.cumulative_cost_usd = Some(cost);
                }
                if state.usage.context_limit.is_some() {
                    state.usage_used = Some(context_tokens);
                } else {
                    state.usage_used = None;
                }
            }
            AgentEventKind::CompactionStart { compaction_id, .. } => match compaction_id {
                Some(_) => open_legacy_start = None,
                None => {
                    legacy_counter += 1;
                    open_legacy_start = Some(format!(
                        "querymt-compaction-legacy-{}",
                        envelope.seq().max(legacy_counter as i64)
                    ));
                }
            },
            AgentEventKind::CompactionEnd {
                compaction_id,
                summary,
                context_tokens,
                ..
            } => {
                let id = match compaction_id {
                    Some(id) => id,
                    None => open_legacy_start.take().unwrap_or_else(|| {
                        legacy_counter += 1;
                        format!(
                            "querymt-compaction-legacy-{}",
                            envelope.seq().max(legacy_counter as i64)
                        )
                    }),
                };
                state
                    .compactions
                    .retain(|c: &ReplayCompactionTerminal| c.id != id);
                state.compactions.push(ReplayCompactionTerminal {
                    id: id.clone(),
                    status: CompactionStatus::Completed,
                    summary: (!summary.trim().is_empty()).then_some(summary),
                    error: None,
                });
                if let Some(tokens) = context_tokens {
                    state.usage_used = Some(tokens);
                }
                open_legacy_start = None;
            }
            AgentEventKind::CompactionFailed {
                compaction_id,
                reason,
                cancelled,
            } => {
                state.compactions.retain(|c| c.id != compaction_id);
                state.compactions.push(ReplayCompactionTerminal {
                    id: compaction_id.clone(),
                    status: if cancelled {
                        CompactionStatus::Cancelled
                    } else {
                        CompactionStatus::Failed
                    },
                    summary: None,
                    error: (!cancelled).then_some(reason),
                });
                open_legacy_start = None;
            }
            AgentEventKind::CompactionSummaryChunk { .. } | AgentEventKind::HookNotice { .. } => {
                // Live-only content: notices are never replayed and summary
                // chunks would duplicate the materialized terminal summary.
            }
            AgentEventKind::ToolCallStart {
                ref tool_name,
                ref arguments,
                ..
            } if is_todo_write_tool(tool_name) => {
                if let Some(entries) = todo_entries_from_arguments(arguments) {
                    state.todo_entries = Some(entries);
                }
            }
            _ => {}
        }
    }

    state
}

/// Materialized current state folded from durable session events.
#[derive(Default)]
struct MaterializedReplayState {
    mode: Option<crate::agent::core::AgentMode>,
    reasoning_effort: Option<String>,
    metadata_touched: bool,
    metadata_title: Option<Option<String>>,
    metadata_updated_at: Option<String>,
    usage: SessionUsageProjection,
    usage_used: Option<u64>,
    todo_entries: Option<Vec<PlanEntry>>,
    compactions: Vec<ReplayCompactionTerminal>,
}

/// Deterministic plan identity for QueryMT's todo list within a session.
pub const QUERYMT_TODO_PLAN_ID: &str = "querymt-todos";

/// Connection-scoped snapshot of ACP v1 Preview session-update capabilities.
///
/// Parsed once from the client's advertised capabilities after a successful
/// ACP initialization. Omitted and `null` Preview capability fields map to
/// `false`, and the snapshot is immutable for the rest of the connection so
/// one client's Preview support never affects another connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcpSessionUpdateCapabilities {
    /// Client advertised `plan_update` / `plan_removed` support.
    pub plan_operations: bool,
    /// Client advertised advisory `notice` support.
    pub notices: bool,
    /// Client advertised ID-addressed compaction updates.
    pub compaction: bool,
}

impl AcpSessionUpdateCapabilities {
    /// Derives the snapshot from negotiated client capabilities.
    pub fn from_client_capabilities(capabilities: &ClientCapabilities) -> Self {
        let session = capabilities.session.as_ref();
        Self {
            plan_operations: capabilities.plan.is_some(),
            notices: session.is_some_and(|session| session.notices.is_some()),
            compaction: session.is_some_and(|session| session.compaction.is_some()),
        }
    }
}

/// Per-session usage projection state for the live context meter.
#[derive(Debug, Clone, Default, PartialEq)]
struct SessionUsageProjection {
    context_limit: Option<u64>,
    cumulative_cost_usd: Option<f64>,
}

impl SessionUsageProjection {
    /// Builds a `usage_update` for the given current context occupancy, or
    /// `None` when the effective context limit is unknown or meaningless.
    fn usage_update(&self, used: u64) -> Option<UsageUpdate> {
        let size = self.context_limit.filter(|size| *size > 0)?;
        let mut update = UsageUpdate::new(used, size);
        if let Some(amount) = self.cumulative_cost_usd {
            update = update.cost(crate::acp::protocol::Cost::new(amount, "USD"));
        }
        Some(update)
    }
}

pub struct AcpLiveEventTranslator {
    streamed_assistant_messages: HashSet<(String, String)>,
    structured_user_messages: HashSet<(String, String)>,
    delegation_updates: crate::control::delegation_notifications::DelegationUpdateProjector,
    capabilities: AcpSessionUpdateCapabilities,
    usage: HashMap<String, SessionUsageProjection>,
    /// Sessions whose deterministic todo plan is currently announced to a
    /// plan-operations client, so an empty snapshot removes it exactly once.
    announced_todo_plans: HashSet<String>,
}

impl Default for AcpLiveEventTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpLiveEventTranslator {
    pub fn new() -> Self {
        Self {
            streamed_assistant_messages: HashSet::new(),
            structured_user_messages: HashSet::new(),
            delegation_updates:
                crate::control::delegation_notifications::DelegationUpdateProjector::for_live_stream(
                ),
            capabilities: AcpSessionUpdateCapabilities::default(),
            usage: HashMap::new(),
            announced_todo_plans: HashSet::new(),
        }
    }

    /// Applies the connection's negotiated Preview capability snapshot.
    /// Called once after successful ACP initialization.
    pub fn set_capabilities(&mut self, capabilities: AcpSessionUpdateCapabilities) {
        self.capabilities = capabilities;
    }

    /// Currently applied Preview capability snapshot.
    pub fn capabilities(&self) -> AcpSessionUpdateCapabilities {
        self.capabilities
    }

    /// Drops all per-session projection state for a closed session.
    pub fn forget_session(&mut self, session_id: &str) {
        self.usage.remove(session_id);
        self.announced_todo_plans.remove(session_id);
        self.streamed_assistant_messages
            .retain(|(session, _)| session != session_id);
        self.structured_user_messages
            .retain(|(session, _)| session != session_id);
    }

    pub fn translate_delegation_update(
        &mut self,
        event: &EventEnvelope,
    ) -> Option<crate::control::delegation_notifications::DelegationUpdateNotification> {
        self.delegation_updates.project_envelope(event)
    }

    /// Translates one internal event into zero or more ordered JSON-RPC
    /// notifications for the owning connection.
    pub fn translate_notifications(&mut self, event: &EventEnvelope) -> Vec<serde_json::Value> {
        if let AgentEventKind::DelegateModelsChanged { revision } = event.kind() {
            return vec![delegate_models_changed_notification(
                event.session_id(),
                *revision,
            )];
        }
        if let Some(update) = self.translate_delegation_update(event) {
            return vec![delegation_update_notification(update)];
        }
        if let Some(notification) = input_state_notification(event) {
            return vec![notification];
        }

        // Handle ElicitationRequested specially - it's a custom notification, not a session/update
        if let AgentEventKind::ElicitationRequested {
            elicitation_id,
            session_id,
            message,
            requested_schema,
            source,
        } = event.kind()
        {
            return vec![serde_json::json!({
                "jsonrpc": "2.0",
                "method": "elicitation/requested",
                "params": {
                    "elicitationId": elicitation_id,
                    "sessionId": session_id,
                    "message": message,
                    "requestedSchema": requested_schema,
                    "source": source,
                }
            })];
        }

        let session_id = event.session_id().to_owned();
        self.translate_updates(event)
            .into_iter()
            .map(|update| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": update
                    }
                })
            })
            .collect()
    }

    /// Single-notification adapter over [`Self::translate_notifications`].
    ///
    /// Prefer the plural form in delivery loops so no projected update is
    /// dropped when one event produces several ordered notifications.
    pub fn translate_notification(&mut self, event: &EventEnvelope) -> Option<serde_json::Value> {
        self.translate_notifications(event).into_iter().next()
    }

    /// Translates one internal event into zero or more ordered session updates.
    pub fn translate_updates(&mut self, event: &EventEnvelope) -> Vec<SessionUpdate> {
        match event.kind() {
            AgentEventKind::UserPromptBlock {
                message_id,
                client_prompt_id,
                block,
            } => {
                self.structured_user_messages
                    .insert((event.session_id().to_owned(), message_id.clone()));
                vec![SessionUpdate::UserMessageChunk(user_prompt_chunk(
                    message_id,
                    client_prompt_id.as_deref(),
                    block.clone(),
                ))]
            }
            AgentEventKind::PromptReceived {
                message_id: Some(message_id),
                ..
            } => {
                let key = (event.session_id().to_owned(), message_id.clone());
                if self.structured_user_messages.remove(&key) {
                    Vec::new()
                } else {
                    self.translate_live_event(event)
                }
            }
            AgentEventKind::AssistantContentDelta {
                content,
                message_id,
            } => {
                if content.is_empty() {
                    return Vec::new();
                }
                self.streamed_assistant_messages
                    .insert((event.session_id().to_owned(), message_id.clone()));
                vec![SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(content.clone())))
                        .message_id(MessageId::from(message_id.clone())),
                )]
            }
            AgentEventKind::AssistantMessageStored {
                content,
                message_id: Some(message_id),
                ..
            } => {
                if content.is_empty() {
                    return Vec::new();
                }
                let key = (event.session_id().to_owned(), message_id.clone());
                if self.streamed_assistant_messages.remove(&key) {
                    Vec::new()
                } else {
                    vec![SessionUpdate::AgentMessageChunk(
                        ContentChunk::new(ContentBlock::Text(TextContent::new(content.clone())))
                            .message_id(MessageId::from(message_id.clone())),
                    )]
                }
            }
            _ => self.translate_live_event(event),
        }
    }

    /// Single-update adapter over [`Self::translate_updates`].
    ///
    /// Prefer the plural form; one event may produce several ordered updates
    /// (for example a terminal compaction followed by a usage snapshot).
    pub fn translate_update(&mut self, event: &EventEnvelope) -> Option<SessionUpdate> {
        self.translate_updates(event).into_iter().next()
    }

    /// Live-only projection for stateful session updates plus fallthrough to
    /// the shared stateless translation.
    fn translate_live_event(&mut self, event: &EventEnvelope) -> Vec<SessionUpdate> {
        let session_id = event.session_id().to_owned();
        match event.kind() {
            AgentEventKind::SessionModeChanged { mode } => {
                vec![SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                    SessionModeId::from(mode.as_str().to_string()),
                ))]
            }
            AgentEventKind::SessionConfigChanged {
                mode,
                reasoning_effort,
            } => {
                let effort = reasoning_effort.as_deref().and_then(parse_reasoning_effort);
                vec![SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                    session_config_options(*mode, effort),
                ))]
            }
            AgentEventKind::SessionMetadataUpdated { title, updated_at } => {
                let mut update = SessionInfoUpdate::new();
                if let Some(title) = title {
                    update.title = match title {
                        Some(title) => MaybeUndefined::Value(title.clone()),
                        None => MaybeUndefined::Null,
                    };
                }
                if let Some(updated_at) = updated_at {
                    update.updated_at = MaybeUndefined::Value(updated_at.clone());
                }
                vec![SessionUpdate::SessionInfoUpdate(update)]
            }
            AgentEventKind::ProviderChanged { context_limit, .. } => {
                self.usage.entry(session_id).or_default().context_limit = *context_limit;
                Vec::new()
            }
            AgentEventKind::LlmRequestEnd {
                context_tokens,
                cumulative_cost_usd,
                ..
            } => {
                let usage = self.usage.entry(session_id).or_default();
                if let Some(cost) = cumulative_cost_usd {
                    usage.cumulative_cost_usd = Some(*cost);
                }
                usage
                    .usage_update(*context_tokens)
                    .map(|update| vec![SessionUpdate::UsageUpdate(update)])
                    .unwrap_or_default()
            }
            AgentEventKind::HookNotice {
                event_name,
                message,
                is_error,
            } => {
                if !self.capabilities.notices {
                    return Vec::new();
                }
                let severity = if *is_error {
                    NoticeSeverity::Error
                } else {
                    NoticeSeverity::Info
                };
                let title = if event_name.is_empty() {
                    "Hook notice"
                } else {
                    event_name.as_str()
                };
                vec![SessionUpdate::Notice(
                    Notice::new(severity, title).description(message.clone()),
                )]
            }
            AgentEventKind::ToolCallStart {
                tool_name,
                arguments,
                ..
            } if is_todo_write_tool(tool_name) => {
                self.translate_todo_updates(&session_id, arguments)
            }
            AgentEventKind::CompactionStart { compaction_id, .. } => {
                if !self.capabilities.compaction {
                    return Vec::new();
                }
                vec![SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                    compaction_identity(compaction_id.as_deref(), event),
                    CompactionStatus::InProgress,
                ))]
            }
            AgentEventKind::CompactionSummaryChunk {
                compaction_id,
                content,
            } => {
                if !self.capabilities.compaction || content.is_empty() {
                    return Vec::new();
                }
                vec![SessionUpdate::CompactionSummaryChunk(
                    CompactionSummaryChunk::new(
                        CompactionId::from(compaction_id.clone()),
                        ContentBlock::Text(TextContent::new(content.clone())),
                    ),
                )]
            }
            AgentEventKind::CompactionEnd {
                compaction_id,
                context_tokens,
                ..
            } => {
                let mut updates = Vec::new();
                if self.capabilities.compaction {
                    updates.push(SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                        compaction_identity(compaction_id.as_deref(), event),
                        CompactionStatus::Completed,
                    )));
                }
                // Post-compaction usage snapshots are stable, ungated updates.
                if let Some(tokens) = context_tokens
                    && let Some(update) = self
                        .usage
                        .get(&session_id)
                        .and_then(|usage| usage.usage_update(*tokens))
                {
                    updates.push(SessionUpdate::UsageUpdate(update));
                }
                updates
            }
            AgentEventKind::CompactionFailed {
                compaction_id,
                reason,
                cancelled,
            } => {
                if !self.capabilities.compaction {
                    return Vec::new();
                }
                let mut update = CompactionUpdate::new(
                    CompactionId::from(compaction_id.clone()),
                    if *cancelled {
                        CompactionStatus::Cancelled
                    } else {
                        CompactionStatus::Failed
                    },
                );
                if !*cancelled {
                    update = update.error(reason.clone());
                }
                vec![SessionUpdate::CompactionUpdate(update)]
            }
            _ => translate_replayable_updates(event),
        }
    }

    /// Projects a todowrite snapshot to plan updates. Plan-operations clients
    /// receive item-based `plan_update` replacements with one deterministic
    /// per-session plan ID and a single `plan_removed` for dismissal; legacy
    /// clients retain the full-replacement `plan` update (an empty snapshot
    /// stays an empty replacement because legacy v1 has no removal variant).
    fn translate_todo_updates(&mut self, session_id: &str, arguments: &str) -> Vec<SessionUpdate> {
        // Malformed snapshots emit nothing; an empty entry list is a valid,
        // explicitly-dismissed snapshot.
        let Some(entries) = todo_entries_from_arguments(arguments) else {
            return Vec::new();
        };
        if self.capabilities.plan_operations {
            if entries.is_empty() {
                if self.announced_todo_plans.remove(session_id) {
                    return vec![SessionUpdate::PlanRemoved(PlanRemoved::new(PlanId::from(
                        QUERYMT_TODO_PLAN_ID.to_string(),
                    )))];
                }
                return Vec::new();
            }
            self.announced_todo_plans.insert(session_id.to_owned());
            return vec![SessionUpdate::PlanUpdate(PlanUpdate::new(
                PlanUpdateContent::items(PlanId::from(QUERYMT_TODO_PLAN_ID.to_string()), entries),
            ))];
        }
        vec![SessionUpdate::Plan(Plan::new(entries))]
    }
}

/// Translate an internal agent event to a replay JSON-RPC notification.
///
/// Returns `None` if the event should not be sent to the client.
pub fn translate_replay_event_to_notification(event: &EventEnvelope) -> Option<serde_json::Value> {
    // Handle ElicitationRequested specially - it's a custom notification, not a session/update
    if let AgentEventKind::ElicitationRequested {
        elicitation_id,
        session_id,
        message,
        requested_schema,
        source,
    } = event.kind()
    {
        return Some(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "elicitation/requested",
            "params": {
                "elicitationId": elicitation_id,
                "sessionId": session_id,
                "message": message,
                "requestedSchema": requested_schema,
                "source": source,
            }
        }));
    }

    let session_id = event.session_id().to_owned();
    let update = translate_replay_event_to_update(event)?;

    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update
        }
    }))
}

/// Translate an agent event to a single replay SessionUpdate.
///
/// Returns `None` if the event should not be sent to the client.
pub fn translate_replay_event_to_update(event: &EventEnvelope) -> Option<SessionUpdate> {
    translate_replayable_updates(event).into_iter().next()
}

pub fn translate_replay_event_to_updates(event: &EventEnvelope) -> Vec<SessionUpdate> {
    if let AgentEventKind::AssistantMessageStored {
        content,
        message_id,
        reasoning_parts,
        ..
    } = event.kind()
    {
        return stored_summary_updates(content, message_id.as_deref(), reasoning_parts);
    }
    translate_replayable_updates(event)
}

/// Stateless, replay-safe projection shared by replay translation and the live
/// fallthrough for conversation/tool events.
///
/// Streaming text deltas and per-block user prompts stay live-only (replay
/// reconstructs them from persisted messages), and stateful session updates
/// (mode/config, metadata, usage, notices, capability-gated plans, and
/// compaction) are projected by their owners instead.
fn translate_replayable_updates(event: &EventEnvelope) -> Vec<SessionUpdate> {
    match event.kind() {
        AgentEventKind::PromptReceived {
            content,
            message_id,
        } => vec![SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(content.clone())))
                .message_id(message_id.clone().map(MessageId::from)),
        )],
        AgentEventKind::UserPromptBlock { .. } => Vec::new(),
        AgentEventKind::AssistantMessageStored {
            content,
            message_id,
            ..
        } => {
            if content.is_empty() {
                return Vec::new();
            }
            vec![SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(content.clone())))
                    .message_id(message_id.clone().map(MessageId::from)),
            )]
        }
        // Streaming text deltas are live-only; replay uses persisted messages.
        AgentEventKind::AssistantContentDelta { .. } => Vec::new(),
        AgentEventKind::AssistantThinkingDelta {
            content,
            message_id,
            part_id,
        } => {
            if content.is_empty() {
                return Vec::new();
            }
            vec![SessionUpdate::AgentThoughtChunk(thought_chunk(
                content,
                message_id,
                part_id.as_deref(),
            ))]
        }
        AgentEventKind::ToolCallStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            if is_todo_write_tool(tool_name) {
                // Ungated contexts receive the legacy full-replacement plan;
                // plan-operations connections intercept todowrite earlier.
                return todo_entries_from_arguments(arguments)
                    .map(|entries| vec![SessionUpdate::Plan(Plan::new(entries))])
                    .unwrap_or_default();
            }

            let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
            vec![SessionUpdate::ToolCall(
                ToolCall::new(
                    ToolCallId::from(tool_call_id.clone()),
                    format!("Run {}", tool_name),
                )
                .kind(tool_kind_for_tool(tool_name))
                .status(ToolCallStatus::InProgress)
                .raw_input(args),
            )]
        }
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            if is_todo_write_tool(tool_name) {
                return Vec::new();
            }

            let status = if *is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let raw_output = serde_json::from_str(result).ok();
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(tool_call_id.clone()),
                ToolCallUpdateFields::new()
                    .kind(tool_kind_for_tool(tool_name))
                    .status(status)
                    .title(format!("Run {}", tool_name))
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(result.clone())),
                    ))])
                    .raw_output(raw_output),
            ))]
        }
        _ => Vec::new(),
    }
}

fn is_todo_write_tool(tool_name: &str) -> bool {
    matches!(tool_name, "todowrite" | "mcp_todowrite")
}

fn todo_entries_from_arguments(arguments: &str) -> Option<Vec<PlanEntry>> {
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let todos = parsed.get("todos")?.as_array()?;
    let mut entries = Vec::with_capacity(todos.len());

    for todo in todos {
        let content = todo.get("content")?.as_str()?.to_string();
        let Some(status) = todo
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_status_to_plan_status)
        else {
            continue;
        };

        let priority = todo
            .get("priority")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_priority_to_plan_priority)
            .unwrap_or(PlanEntryPriority::Medium);

        entries.push(PlanEntry::new(content, priority, status));
    }

    Some(entries)
}

/// Resolves the protocol compaction identity for a compaction event. Live
/// events always carry an explicit ID; legacy persisted records derive a
/// deterministic identity from their stream position.
fn compaction_identity(explicit: Option<&str>, event: &EventEnvelope) -> CompactionId {
    match explicit {
        Some(id) if !id.is_empty() => CompactionId::from(id.to_string()),
        _ => CompactionId::from(format!("querymt-compaction-legacy-{}", event.seq())),
    }
}

/// Parses a reasoning-effort wire string (`"auto"` maps to no override).
fn parse_reasoning_effort(value: &str) -> Option<querymt::chat::ReasoningEffort> {
    if value == "auto" {
        return None;
    }
    serde_json::from_value(serde_json::json!(value)).ok()
}

fn todo_priority_to_plan_priority(priority: &str) -> Option<PlanEntryPriority> {
    match priority {
        "high" => Some(PlanEntryPriority::High),
        "medium" => Some(PlanEntryPriority::Medium),
        "low" => Some(PlanEntryPriority::Low),
        _ => None,
    }
}

fn todo_status_to_plan_status(status: &str) -> Option<PlanEntryStatus> {
    match status {
        "pending" => Some(PlanEntryStatus::Pending),
        "in_progress" => Some(PlanEntryStatus::InProgress),
        "completed" => Some(PlanEntryStatus::Completed),
        // ACP plans do not support a cancelled state; omit these entries.
        "cancelled" => None,
        _ => None,
    }
}

/// Map tool names to ToolKind enum.
pub use crate::agent::utils::tool_kind_for_tool;

/// Check if an event belongs to a specific connection.
///
/// Also handles session forking (delegation) by propagating ownership to child sessions.
pub async fn is_event_owned(
    session_owners: &SessionOwnerMap,
    conn_id: &str,
    event: &EventEnvelope,
) -> bool {
    // Handle session forking - propagate ownership to child sessions
    if let AgentEventKind::SessionForked {
        parent_session_id,
        child_session_id,
        origin,
        ..
    } = event.kind()
        && matches!(origin, ForkOrigin::Delegation)
    {
        let mut owners = session_owners.lock().await;
        if let Some(subscribers) = owners.get(parent_session_id).cloned() {
            owners.insert(child_session_id.clone(), subscribers);
        }
    }

    // Check if this connection owns the session
    let owners = session_owners.lock().await;
    owners
        .get(event.session_id())
        .is_some_and(|subscribers| subscribers.contains(conn_id))
}

/// Collect EventFanout sources from agent and all delegate agents.
///
/// This function collects the EventFanout from the main agent and recursively
/// collects EventFanouts from all registered delegate agents. Each EventFanout is
/// deduplicated by pointer address to avoid subscribing multiple times.
///
/// # Arguments
/// * `agent` - The main agent to collect EventFanout sources from
///
/// # Returns
/// A vector of unique EventFanout instances
pub fn collect_event_sources(agent: &Arc<AgentHandle>) -> Vec<Arc<EventFanout>> {
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let primary = agent.config.event_sink.fanout().clone();
    if seen.insert(Arc::as_ptr(&primary) as usize) {
        sources.push(primary);
    }

    let registry = agent.agent_registry();
    for info in registry.list_agents() {
        if let Some(handle) = registry.get_handle(&info.id) {
            let fanout = handle.event_fanout().clone();
            if seen.insert(Arc::as_ptr(&fanout) as usize) {
                sources.push(fanout);
            }
        }
    }

    sources
}

/// Re-export from session_registry — the single source of truth for config option shape.
/// Used by the config_option_update projection and by tests in this module.
use crate::agent::session_registry::config_options as session_config_options;

/// Handle an RPC request and return a response.
///
/// This function routes JSON-RPC methods to the appropriate `SendAgent` trait methods.
pub async fn handle_rpc_message<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    pending_permissions: &PermissionMap,
    pending_elicitations: &PendingElicitationMap,
    conn_id: &str,
    req: RpcMessage,
) -> RpcDispatchOutput {
    handle_rpc_message_with_context(
        agent,
        session_owners,
        pending_permissions,
        pending_elicitations,
        conn_id,
        req,
        RpcDispatchContext::default(),
    )
    .await
}

pub async fn handle_rpc_message_with_context<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    pending_permissions: &PermissionMap,
    pending_elicitations: &PendingElicitationMap,
    conn_id: &str,
    req: RpcMessage,
    context: RpcDispatchContext,
) -> RpcDispatchOutput {
    let rpc_method = req.method.clone();
    let rpc_params = req.params.clone();
    let mut notifications = Vec::new();
    let result: Result<serde_json::Value, Error> =
        run_with_acp_span(&rpc_method, &rpc_params, async {
            let method = req.method.clone();
            match method.as_str() {
                m if m == AGENT_METHOD_NAMES.initialize => {
                    match serde_json::from_value::<crate::acp::protocol::InitializeRequest>(
                        req.params,
                    ) {
                        Ok(params) => {
                            let supports_form_elicitation = params
                                .client_capabilities
                                .elicitation
                                .as_ref()
                                .is_some_and(|elicitation| elicitation.form.is_some());
                            let capabilities = AcpSessionUpdateCapabilities::from_client_capabilities(
                                &params.client_capabilities,
                            );
                            let response = agent.initialize(params).await;
                            if response.is_ok() {
                                // Parse Preview capabilities once per connection
                                // so gating stays isolated to this client.
                                if let Some(translator) = context.translator.as_ref()
                                    && let Ok(mut translator) = translator.lock()
                                {
                                    translator.set_capabilities(capabilities);
                                }
                                if let Some(registry) = context.elicitation_recovery.as_ref() {
                                    registry
                                        .record_client_capabilities(
                                            conn_id,
                                            supports_form_elicitation,
                                        )
                                        .await;
                                }
                            }
                            response.map(|r| serde_json::to_value(r).unwrap())
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.authenticate => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .authenticate(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }

                m if m == AGENT_METHOD_NAMES.session_new => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => {
                            // MCP attachments are now resolved internally by the
                            // materializer via the runtime attachment source.
                            let response = agent.new_session(params).await;
                            match response {
                                Ok(r) => {
                                    let session_id = r.session_id.to_string();
                                    attach_rpc_session(
                                        agent,
                                        session_owners,
                                        conn_id,
                                        &context,
                                        &session_id,
                                        false,
                                    )
                                    .await?;
                                    if let Some(notification) =
                                        available_commands_session_update(agent, &session_id).await
                                    {
                                        notifications.push(notification);
                                    }
                                    Ok(serde_json::to_value(r).unwrap())
                                }
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(Error::invalid_params().data(serde_json::json!({
                            "error": e.to_string()
                        }))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_prompt => {
                    match serde_json::from_value::<crate::acp::protocol::PromptRequest>(req.params) {
                        Ok(params) => {
                            let session_id = params.session_id.to_string();
                            attach_rpc_session(
                                agent,
                                session_owners,
                                conn_id,
                                &context,
                                &session_id,
                                true,
                            )
                            .await?;
                            agent
                                .prompt_with_bridge(params, context.session_bridge.clone())
                                .await
                                .map(|r| serde_json::to_value(r).unwrap())
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }

                m if m == AGENT_METHOD_NAMES.session_cancel => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent.cancel(params).await.map(|_| serde_json::Value::Null),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_fork => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .fork_session(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_list => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .list_sessions(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_load => {
                    match serde_json::from_value::<crate::acp::protocol::LoadSessionRequest>(
                        req.params,
                    ) {
                        Ok(params) => {
                            let session_id = params.session_id.to_string();
                            // MCP attachments are now resolved internally by the
                            // materializer via the runtime attachment source.
                            let response = agent.load_session(params).await;
                            match response {
                                Ok(r) => {
                                    let request_lock =
                                        session_owners.request_lock(&session_id, conn_id).await;
                                    let _request_guard = request_lock.lock().await;
                                    let ownership_inserted = attach_rpc_session_locked(
                                        agent,
                                        session_owners,
                                        conn_id,
                                        &context,
                                        &session_id,
                                        false,
                                    )
                                    .await?;
                                    let mut value = serde_json::to_value(r).unwrap();
                                    let local_agent = agent.as_any().downcast_ref::<AgentHandle>();
                                    if let (Some(hooks), Some(local_agent)) =
                                        (context.session_hooks.as_ref(), local_agent)
                                        && let Err(error) = hooks
                                            .on_session_loaded(local_agent, &session_id, &mut value)
                                            .await
                                    {
                                        if ownership_inserted {
                                            unsubscribe_connection(
                                                session_owners,
                                                &session_id,
                                                conn_id,
                                            )
                                            .await;
                                        }
                                        return Err(error);
                                    }
                                    if let Some(local_agent) = local_agent {
                                        let capabilities = context
                                            .translator
                                            .as_ref()
                                            .map(|translator| {
                                                translator
                                                    .lock()
                                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                                    .capabilities()
                                            })
                                            .unwrap_or_default();
                                        let replay =
                                            crate::acp::stdio::replay_loaded_session(
                                                local_agent,
                                                &session_id,
                                                &capabilities,
                                            )
                                            .await;
                                        let (replay, _) = match replay {
                                            Ok(replay) => replay,
                                            Err(error) => {
                                                if ownership_inserted {
                                                    unsubscribe_connection(
                                                        session_owners,
                                                        &session_id,
                                                        conn_id,
                                                    )
                                                    .await;
                                                }
                                                return Err(error);
                                            }
                                        };
                                        notifications.extend(replay.into_iter().map(|params| {
                                            serde_json::json!({
                                                "jsonrpc": "2.0",
                                                "method": "session/update",
                                                "params": params,
                                            })
                                        }));
                                    }
                                    if let Some(notification) =
                                        available_commands_session_update(agent, &session_id).await
                                    {
                                        notifications.push(notification);
                                    }
                                    Ok(value)
                                }

                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_resume => {
                    match serde_json::from_value::<crate::acp::protocol::ResumeSessionRequest>(
                        req.params,
                    ) {
                        Ok(params) => {
                            let session_id = params.session_id.to_string();
                            let response = agent.resume_session(params).await;
                            match response {
                                Ok(r) => {
                                    attach_rpc_session(
                                        agent,
                                        session_owners,
                                        conn_id,
                                        &context,
                                        &session_id,
                                        false,
                                    )
                                    .await?;
                                    if let Some(notification) =
                                        available_commands_session_update(agent, &session_id).await
                                    {
                                        notifications.push(notification);
                                    }
                                    Ok(serde_json::to_value(r).unwrap())
                                }
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_close => {
                    match serde_json::from_value::<crate::acp::protocol::CloseSessionRequest>(req.params)
                    {
                        Ok(params) => {
                            let session_id = params.session_id.to_string();
                            let result = agent.close_session(params).await;
                            if result.is_ok()
                                && let Some(translator) = context.translator.as_ref()
                                && let Ok(mut translator) = translator.lock()
                            {
                                translator.forget_session(&session_id);
                            }
                            result.map(|r| serde_json::to_value(r).unwrap())
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_delete => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .delete_session(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_set_config_option => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .set_session_config_option(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == AGENT_METHOD_NAMES.session_set_mode => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .set_session_mode(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }
                m if m == crate::acp::protocol::SET_SESSION_MODEL_METHOD_NAME => {
                    match serde_json::from_value(req.params) {
                        Ok(params) => agent
                            .set_session_model(params)
                            .await
                            .map(|r| serde_json::to_value(r).unwrap()),
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }

                "permission_result" => {
                    #[derive(Deserialize)]
                    struct PermissionResultParams {
                        tool_call_id: String,
                        outcome: RequestPermissionOutcome,
                    }
                    match serde_json::from_value::<PermissionResultParams>(req.params) {
                        Ok(params) => {
                            let mut pending = pending_permissions.lock().await;
                            if let Some(tx) = pending.remove(&params.tool_call_id) {
                                let _ = tx.send(params.outcome);
                                Ok(serde_json::Value::Null)
                            } else {
                                Err(Error::internal_error()
                                    .data("No pending permission for this tool_call_id"))
                            }
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }

                "elicitation_result" => {
                    #[derive(Deserialize)]
                    struct ElicitationResultParams {
                        elicitation_id: String,
                        #[serde(default, alias = "sessionId")]
                        session_id: Option<String>,
                        action: String,
                        content: Option<serde_json::Value>,
                    }
                    match serde_json::from_value::<ElicitationResultParams>(req.params) {
                        Ok(params) => {
                            // Parse action string to enum
                            let action_result = match params.action.as_str() {
                                "accept" => Ok(crate::elicitation::ElicitationAction::Accept),
                                "decline" => Ok(crate::elicitation::ElicitationAction::Decline),
                                "cancel" => Ok(crate::elicitation::ElicitationAction::Cancel),
                                _ => Err(Error::invalid_params().data(serde_json::json!({
                                    "error": format!("Invalid action: {}", params.action)
                                }))),
                            };

                            match action_result {
                                Ok(action) => {
                                    let response = crate::elicitation::ElicitationResponse {
                                        action,
                                        content: params.content,
                                    };

                                    if let Some(query_agent) =
                                        agent.as_any().downcast_ref::<AgentHandle>()
                                    {
                                        match crate::elicitation::resolve_elicitation_from_connection(
                                            query_agent,
                                            params.session_id.as_deref(),
                                            &params.elicitation_id,
                                            conn_id,
                                            response,
                                        )
                                        .await?
                                        {
                                            crate::elicitation::ElicitationResolution::Resolved => {
                                                if let (Some(registry), Some(session_id)) = (
                                                    context.elicitation_recovery.as_ref(),
                                                    params.session_id.as_deref(),
                                                ) {
                                                    registry
                                                        .retire_session_if_idle(session_id, query_agent)
                                                        .await;
                                                }
                                                Ok(serde_json::Value::Null)
                                            }
                                            crate::elicitation::ElicitationResolution::StaleDelivery => {
                                                Err(crate::control::elicitation_recovery::elicitation_recovery_denied(
                                                    crate::control::elicitation_recovery::ElicitationRecoveryDenialReason::Unauthorized,
                                                ))
                                            }
                                            crate::elicitation::ElicitationResolution::NotFound => {
                                                Err(Error::internal_error().data(serde_json::json!({
                                                    "message": "No pending elicitation for this elicitation_id",
                                                    "elicitationId": params.elicitation_id,
                                                    "sessionId": params.session_id,
                                                })))
                                            }
                                        }
                                    } else {
                                        let sender = pending_elicitations
                                            .lock()
                                            .await
                                            .remove(&params.elicitation_id)
                                            .map(|entry| entry.sender);
                                        if let Some(sender) = sender {
                                            let _ = sender.send(response);
                                            Ok(serde_json::Value::Null)
                                        } else {
                                            Err(Error::internal_error().data(
                                                "No pending elicitation for this elicitation_id",
                                            ))
                                        }
                                    }
                                }
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(Error::invalid_params()
                            .data(serde_json::json!({"error": e.to_string()}))),
                    }
                }

                m if normalize_querymt_ext_method(m)
                    == crate::control::elicitation_recovery::ELICITATION_RECOVERY_LIST_PENDING_METHOD =>
                {
                    let registry = context.elicitation_recovery.as_ref().ok_or_else(|| {
                        crate::control::elicitation_recovery::elicitation_recovery_denied(
                            crate::control::elicitation_recovery::ElicitationRecoveryDenialReason::InsecureTransport,
                        )
                    })?;
                    let request = serde_json::from_value(req.params).map_err(|error| {
                        Error::invalid_params().data(serde_json::json!({"error": error.to_string()}))
                    })?;
                    let local_agent = agent.as_any().downcast_ref::<AgentHandle>().ok_or_else(|| {
                        Error::internal_error().data("Recovery requires a local agent")
                    })?;
                    registry
                        .list_pending_sessions(conn_id, &request, local_agent)
                        .await
                        .and_then(|response| {
                            serde_json::to_value(response).map_err(Error::into_internal_error)
                        })
                }

                m if normalize_querymt_ext_method(m)
                    == crate::control::elicitation_recovery::ELICITATION_RECOVERY_ATTACH_METHOD =>
                {
                    let registry = context.elicitation_recovery.as_ref().ok_or_else(|| {
                        crate::control::elicitation_recovery::elicitation_recovery_denied(
                            crate::control::elicitation_recovery::ElicitationRecoveryDenialReason::InsecureTransport,
                        )
                    })?;
                    let request: crate::control::elicitation_recovery::AttachPendingElicitationSessionRequest =
                        serde_json::from_value(req.params).map_err(|error| {
                            Error::invalid_params().data(serde_json::json!({"error": error.to_string()}))
                        })?;
                    let local_agent = agent.as_any().downcast_ref::<AgentHandle>().ok_or_else(|| {
                        Error::internal_error().data("Recovery requires a local agent")
                    })?;
                    let response = registry
                        .authorize_attach(conn_id, &request, local_agent)
                        .await?;
                    attach_rpc_session(
                        agent,
                        session_owners,
                        conn_id,
                        &context,
                        &request.session_id,
                        true,
                    )
                    .await?;
                    serde_json::to_value(response).map_err(Error::into_internal_error)
                }

                // Forward QueryMT extension methods to the agent's ext_method handler.
                // SDK ACP strips the leading underscore for extension requests;
                // WebSocket clients may send either shape, so normalize here.
                m if m.starts_with("_querymt/") || m.starts_with("querymt/") => {
                    let ext_method = normalize_querymt_ext_method(m);
                    let session_id_for_owner =
                        querymt_session_id_from_request(ext_method, &req.params);
                    let raw_params = serde_json::value::RawValue::from_string(
                        serde_json::to_string(&req.params).unwrap_or_else(|_| "null".to_string()),
                    )
                    .unwrap_or_else(|_| {
                        serde_json::value::RawValue::from_string("null".to_string()).unwrap()
                    });
                    let ext_req = crate::acp::protocol::ExtRequest::new(
                        ext_method,
                        std::sync::Arc::from(raw_params),
                    );
                    let attached_before_request = attach_before_querymt_ext_method(ext_method);
                    let _ownership_request_guard = if attached_before_request {
                        if let Some(session_id) = session_id_for_owner.as_deref() {
                            Some(
                                session_owners
                                    .request_lock(session_id, conn_id)
                                    .await
                                    .lock_owned()
                                    .await,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let ownership_inserted = if attached_before_request {
                        if let Some(session_id) = session_id_for_owner.as_deref() {
                            attach_rpc_session_locked(
                                agent,
                                session_owners,
                                conn_id,
                                &context,
                                session_id,
                                false,
                            )
                            .await?
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    let response = agent.ext_method(ext_req).await.map(|r| {
                        serde_json::from_str(r.0.get()).unwrap_or(serde_json::Value::Null)
                    });
                    match response {
                        Ok(mut value) => {
                            let session_id = session_id_for_owner.clone().or_else(|| {
                                querymt_session_id_from_response(ext_method, &value)
                            });
                            if let Some(session_id) = session_id {
                                let mut post_attach_guard = None;
                                let mut post_attach_inserted = false;
                                if !attached_before_request {
                                    let request_lock =
                                        session_owners.request_lock(&session_id, conn_id).await;
                                    post_attach_guard = Some(request_lock.lock_owned().await);
                                    post_attach_inserted = attach_rpc_session_locked(
                                        agent,
                                        session_owners,
                                        conn_id,
                                        &context,
                                        &session_id,
                                        false,
                                    )
                                    .await?;
                                }

                                if let (Some(hooks), Some(local_agent)) = (
                                    context.session_hooks.as_ref(),
                                    agent.as_any().downcast_ref::<AgentHandle>(),
                                ) && let Err(error) = hooks
                                    .on_remote_session_attached(
                                        local_agent,
                                        &session_id,
                                        &mut value,
                                    )
                                    .await
                                {
                                    if ownership_inserted || post_attach_inserted {
                                        unsubscribe_connection(
                                            session_owners,
                                            &session_id,
                                            conn_id,
                                        )
                                        .await;
                                    }
                                    return Err(error);
                                }
                                drop(post_attach_guard);
                            }
                            Ok(value)
                        }
                        Err(e) => {
                            if ownership_inserted
                                && let Some(session_id) = session_id_for_owner.as_deref()
                            {
                                unsubscribe_connection(session_owners, session_id, conn_id).await;
                            }
                            Err(e)
                        }
                    }
                }

                _ => Err(Error::method_not_found()),
            }
        })
        .await;

    let response = match (req.id, result) {
        (Some(id), Ok(res)) => Some(RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: Some(res),
            error: None,
            id,
        }),
        (Some(id), Err(e)) => Some(RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(serde_json::to_value(e).unwrap()),
            id,
        }),
        (None, Ok(_)) => None,
        (None, Err(e)) => {
            log::warn!("JSON-RPC notification '{}' failed: {}", rpc_method, e);
            None
        }
    };

    RpcDispatchOutput {
        notifications,
        response,
    }
}

async fn available_commands_session_update<S: SendAgent>(
    agent: &S,
    session_id: &str,
) -> Option<serde_json::Value> {
    let notification = agent.available_slash_commands(session_id).await?;
    let params = serde_json::to_value(&notification).ok()?;
    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": params,
    }))
}

/// Create a method-specific ACP span, set remote parent if present, and
/// run the future inside it.
///
/// Core ACP methods get individual span names (e.g. `acp.load_session`);
/// everything else uses the existing `#[instrument]` inside the handler,
/// so we only create a generic context span here.
async fn run_with_acp_span<T, F>(method: &str, params: &serde_json::Value, fut: F) -> T
where
    F: Future<Output = T>,
{
    use tracing::Instrument;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let span = acp_method_span(method);

    if let Some(meta) = params.get("_meta")
        && let Some(parent_cx) = super::trace_context::extract_acp_trace_context(meta)
    {
        let _ = span.set_parent(parent_cx);
    }

    fut.instrument(span).await
}

/// Map an ACP method string to a named tracing span.
///
/// Core ACP methods (defined in `AGENT_METHOD_NAMES`) get individual span
/// names for direct readability in Grafana.  Extension and unknown methods
/// get a single `acp.ext_method` span with the method name as attribute.
fn acp_method_span(method: &str) -> tracing::Span {
    use opentelemetry_semantic_conventions::attribute::{RPC_METHOD, RPC_SYSTEM};

    let names = &AGENT_METHOD_NAMES;

    match method {
        m if m == names.initialize => tracing::info_span!(
            "acp.initialize",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.authenticate => tracing::info_span!(
            "acp.authenticate",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_new => tracing::info_span!(
            "acp.new_session",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_prompt => tracing::info_span!(
            "acp.prompt",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_cancel => tracing::info_span!(
            "acp.cancel",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_load => tracing::info_span!(
            "acp.load_session",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_list => tracing::info_span!(
            "acp.list_sessions",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_close => tracing::info_span!(
            "acp.close_session",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        m if m == names.session_resume => tracing::info_span!(
            "acp.resume_session",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
        // Extension methods and everything else: single acp.ext_method span
        // with the method name as attribute.  The #[instrument] was removed
        // from ext_method so this is the only span for extension requests.
        _ => tracing::info_span!(
            "acp.ext_method",
            { RPC_SYSTEM } = "jsonrpc",
            { RPC_METHOD } = %method,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::core::AgentMode;
    use crate::elicitation::ElicitationAction;
    use crate::events::{AgentEventKind, DurableEvent, EventEnvelope, EventOrigin};
    use crate::session::backend::StorageBackend;
    use crate::session::projection::NewDurableEvent;
    use crate::test_utils::DelegateTestFixture;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use tokio::sync::{Mutex, Notify};
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn session_subscriptions_support_multiple_connections_and_cleanup() {
        let subscriptions = SessionOwnerMap::default();
        subscribe_connection(&subscriptions, "session".to_string(), "conn-a").await;
        subscribe_connection(&subscriptions, "session".to_string(), "conn-b").await;

        let event = EventEnvelope::Durable(DurableEvent {
            event_id: "event".into(),
            stream_seq: 1,
            session_id: "session".into(),
            timestamp: 1,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        });
        assert!(is_event_owned(&subscriptions, "conn-a", &event).await);
        assert!(is_event_owned(&subscriptions, "conn-b", &event).await);

        {
            let mut subscriptions = subscriptions.lock().await;
            subscriptions
                .get_mut("session")
                .expect("session subscriptions")
                .remove("conn-b");
        }
        assert!(is_event_owned(&subscriptions, "conn-a", &event).await);
        assert!(!is_event_owned(&subscriptions, "conn-b", &event).await);
        assert_eq!(
            subscriptions.lock().await.get("session"),
            Some(&HashSet::from(["conn-a".to_string()]))
        );
    }

    #[test]
    fn delegate_assignment_ext_requests_register_session_ownership() {
        for method in [
            "querymt/session/delegateModels",
            "_querymt/session/setDelegateModel",
            "querymt/session/steer",
            "_querymt/session/queue",
            "querymt/session/discardQueuedInput",
            "querymt/session/runtimeState",
        ] {
            assert_eq!(
                querymt_session_id_from_request(
                    method,
                    &serde_json::json!({"sessionId": "parent"})
                ),
                Some("parent".into())
            );
        }
        assert_eq!(
            querymt_session_id_from_request("querymt/capabilities", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn input_lifecycle_events_translate_to_input_state_notifications() {
        let cases = [
            (
                AgentEventKind::SteeringAccepted {
                    run_id: "run-1".into(),
                    input_id: "input-1".into(),
                    position: 2,
                    blocks: vec![ContentBlock::Text(TextContent::new("steer"))],
                    accepted_at_ms: Some(10),
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-1",
                    "delivery": "steer",
                    "state": "accepted",
                    "run_id": "run-1",
                    "position": 2
                }),
            ),
            (
                AgentEventKind::SteeringApplied {
                    run_id: "run-1".into(),
                    input_id: "input-1".into(),
                    boundary: "after_tools".into(),
                    latency_ms: 25,
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-1",
                    "delivery": "steer",
                    "state": "applied",
                    "run_id": "run-1",
                    "boundary": "after_tools",
                    "latency_ms": 25
                }),
            ),
            (
                AgentEventKind::SteeringDiscarded {
                    run_id: "run-1".into(),
                    input_id: "input-1".into(),
                    reason: "run_completed".into(),
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-1",
                    "delivery": "steer",
                    "state": "discarded",
                    "run_id": "run-1",
                    "reason": "run_completed"
                }),
            ),
            (
                AgentEventKind::InputQueued {
                    input_id: "input-2".into(),
                    position: 1,
                    blocks: vec![ContentBlock::Text(TextContent::new("queue"))],
                    accepted_at_ms: Some(20),
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-2",
                    "delivery": "queue",
                    "state": "queued",
                    "position": 1
                }),
            ),
            (
                AgentEventKind::QueuedInputStarted {
                    input_id: "input-2".into(),
                    run_id: "run-2".into(),
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-2",
                    "delivery": "queue",
                    "state": "started",
                    "run_id": "run-2"
                }),
            ),
            (
                AgentEventKind::QueuedInputDiscarded {
                    input_id: "input-3".into(),
                    reason: "removed_by_user".into(),
                },
                serde_json::json!({
                    "version": 1,
                    "session_id": "s-1",
                    "input_id": "input-3",
                    "delivery": "queue",
                    "state": "discarded",
                    "reason": "removed_by_user"
                }),
            ),
        ];

        for (index, (kind, expected)) in cases.into_iter().enumerate() {
            let event = EventEnvelope::Durable(DurableEvent {
                event_id: format!("input-event-{index}"),
                stream_seq: index as i64 + 1,
                session_id: "s-1".into(),
                timestamp: index as i64,
                origin: EventOrigin::Local,
                source_node: None,
                kind,
            });
            let notification = AcpLiveEventTranslator::new()
                .translate_notification(&event)
                .expect("input lifecycle notification");
            assert_eq!(notification["method"], QMT_NOTIFICATION_INPUT_STATE);
            assert_eq!(notification["params"], expected);
            assert!(translate_replay_event_to_notification(&event).is_none());
        }
    }

    #[test]
    fn elicitation_request_rejects_unknown_property_types() {
        let result = create_elicitation_request(
            "e-1".to_string(),
            "s-1".to_string(),
            "Invalid".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {"nested": {"type": "object"}}
            }),
            "test".to_string(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn elicitation_request_accepts_supported_property_types() {
        let result = create_elicitation_request(
            "e-1".to_string(),
            "s-1".to_string(),
            "Supported".to_string(),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "score": {"type": "number"},
                    "count": {"type": "integer"},
                    "enabled": {"type": "boolean"},
                    "choices": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["a", "b"]}
                    }
                }
            }),
            "test".to_string(),
        );

        assert!(result.is_ok());
    }

    fn tool_start_event(tool_name: &str, arguments: serde_json::Value) -> EventEnvelope {
        EventEnvelope::Durable(DurableEvent {
            event_id: "evt-1".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: tool_name.to_string(),
                arguments: arguments.to_string(),
            },
        })
    }

    fn tool_end_event(tool_name: &str) -> EventEnvelope {
        EventEnvelope::Durable(DurableEvent {
            event_id: "evt-2".into(),
            stream_seq: 2,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: tool_name.to_string(),
                result: "{}".to_string(),
                is_error: false,
            },
        })
    }

    #[test]
    fn prompt_received_preserves_message_id() {
        let event = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-prompt".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "hello".to_string(),
                message_id: Some("u-1".to_string()),
            },
        });

        let Some(SessionUpdate::UserMessageChunk(chunk)) = translate_replay_event_to_update(&event)
        else {
            panic!("expected user message chunk");
        };

        assert_eq!(
            chunk.message_id.as_ref().map(|id| id.0.as_ref()),
            Some("u-1")
        );
        assert!(chunk.meta.is_none());
    }

    #[test]
    fn live_prompt_blocks_preserve_order_and_share_message_id() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("before")),
            ContentBlock::Image(crate::acp::protocol::ImageContent::new("AQID", "image/png")),
            ContentBlock::Text(TextContent::new("after")),
        ];
        let mut translator = AcpLiveEventTranslator::new();
        let updates = blocks
            .into_iter()
            .enumerate()
            .map(|(index, block)| {
                translator
                    .translate_update(&EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
                        session_id: "s-1".to_string(),
                        timestamp: index as i64,
                        origin: EventOrigin::Local,
                        source_node: None,
                        kind: AgentEventKind::UserPromptBlock {
                            message_id: "u-1".to_string(),
                            client_prompt_id: Some("client-1".to_string()),
                            block,
                        },
                    }))
                    .expect("prompt block should translate")
            })
            .collect::<Vec<_>>();

        assert_eq!(updates.len(), 3);
        for update in &updates {
            let SessionUpdate::UserMessageChunk(chunk) = update else {
                panic!("expected user message chunk");
            };
            assert_eq!(
                chunk.message_id.as_ref().map(|id| id.0.as_ref()),
                Some("u-1")
            );
            assert_eq!(
                chunk
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("querymt"))
                    .and_then(|querymt| querymt.get("client_prompt_id"))
                    .and_then(serde_json::Value::as_str),
                Some("client-1")
            );
            let wire = serde_json::to_value(update).expect("serialize user message chunk");
            assert_eq!(wire["sessionUpdate"], "user_message_chunk");
            assert_eq!(wire["_meta"]["querymt"]["client_prompt_id"], "client-1");
        }
        assert!(matches!(
            &updates[1],
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(chunk.content, ContentBlock::Image(_))
        ));

        let summary = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-prompt".into(),
            stream_seq: 4,
            timestamp: 4,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "before\n\nafter".to_string(),
                message_id: Some("u-1".to_string()),
            },
        });
        assert!(translator.translate_update(&summary).is_none());
    }

    #[test]
    fn rpc_error_response_omits_result_field() {
        let response = RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(serde_json::json!({
                "code": -32602,
                "message": "invalid params"
            })),
            id: serde_json::json!(10),
        };

        let value = serde_json::to_value(response).expect("serialize rpc response");
        assert!(value.get("result").is_none());
        assert_eq!(value["error"]["code"], serde_json::json!(-32602));
    }

    #[test]
    fn rpc_message_deserializes_notification_without_id() {
        let message: RpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": "s-1"}
        }))
        .expect("notification envelope should deserialize without id");

        assert_eq!(message.method, AGENT_METHOD_NAMES.session_cancel);
        assert!(message.id.is_none());
    }

    #[test]
    fn mesh_notification_builders_use_expected_methods() {
        let nodes_changed = mesh_nodes_changed_notification("peer-1", "discovered");
        assert_eq!(
            nodes_changed["method"],
            serde_json::json!(QMT_NOTIFICATION_MESH_NODES_CHANGED)
        );
        assert_eq!(
            nodes_changed["params"]["peer_id"],
            serde_json::json!("peer-1")
        );
        assert_eq!(
            nodes_changed["params"]["change"],
            serde_json::json!("discovered")
        );

        let peer_expired = mesh_peer_expired_notification("peer-2");
        assert_eq!(
            peer_expired["method"],
            serde_json::json!(QMT_NOTIFICATION_MESH_PEER_EXPIRED)
        );
        assert_eq!(
            peer_expired["params"]["peer_id"],
            serde_json::json!("peer-2")
        );

        let models_changed = models_changed_notification("manual_refresh");
        assert_eq!(
            models_changed["method"],
            serde_json::json!(QMT_NOTIFICATION_MODELS_CHANGED)
        );
        assert_eq!(
            models_changed["params"]["reason"],
            serde_json::json!("manual_refresh")
        );

        let schedules_changed = schedules_changed_notification(
            crate::control::notifications::SchedulesChangedNotification {
                node_id: Some("node-a".to_string()),
                session_id: Some("session-a".to_string()),
                schedule_public_id: "sched-1".to_string(),
                change: "created".to_string(),
                schedule: None,
            },
        );
        assert_eq!(
            schedules_changed["method"],
            serde_json::json!(QMT_NOTIFICATION_SCHEDULES_CHANGED)
        );
        assert_eq!(
            schedules_changed["params"]["schedule_public_id"],
            serde_json::json!("sched-1")
        );
        assert_eq!(
            schedules_changed["params"]["change"],
            serde_json::json!("created")
        );
    }

    #[test]
    fn assistant_updates_preserve_message_id() {
        let stored = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-stored".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantMessageStored {
                content: "answer".to_string(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("a-1".to_string()),
            },
        });
        let delta = EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "ans".to_string(),
                message_id: "a-1".to_string(),
            },
        });

        let Some(SessionUpdate::AgentMessageChunk(stored_chunk)) =
            translate_replay_event_to_update(&stored)
        else {
            panic!("expected stored assistant chunk");
        };
        let Some(SessionUpdate::AgentMessageChunk(delta_chunk)) =
            AcpLiveEventTranslator::new().translate_update(&delta)
        else {
            panic!("expected delta assistant chunk");
        };

        assert_eq!(
            stored_chunk.message_id.as_ref().map(|id| id.0.as_ref()),
            Some("a-1")
        );
        assert_eq!(
            delta_chunk.message_id.as_ref().map(|id| id.0.as_ref()),
            Some("a-1")
        );
    }

    #[test]
    fn live_translator_suppresses_stored_message_after_streaming_delta() {
        let delta = EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "ans".to_string(),
                message_id: "a-1".to_string(),
            },
        });
        let stored = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-stored".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantMessageStored {
                content: "answer".to_string(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("a-1".to_string()),
            },
        });

        let mut translator = AcpLiveEventTranslator::new();
        assert!(matches!(
            translator.translate_update(&delta),
            Some(SessionUpdate::AgentMessageChunk(_))
        ));
        assert!(translator.translate_update(&stored).is_none());
    }

    #[test]
    fn live_translator_forwards_non_streaming_stored_message() {
        let stored = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-stored".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantMessageStored {
                content: "answer".to_string(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("a-1".to_string()),
            },
        });

        let mut translator = AcpLiveEventTranslator::new();
        let Some(SessionUpdate::AgentMessageChunk(chunk)) = translator.translate_update(&stored)
        else {
            panic!("expected stored assistant chunk for non-streaming provider");
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "answer");
        assert_eq!(
            chunk.message_id.as_ref().map(|id| id.0.as_ref()),
            Some("a-1")
        );
    }

    #[test]
    fn replay_translation_skips_streaming_deltas() {
        let delta = EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "ans".to_string(),
                message_id: "a-1".to_string(),
            },
        });

        assert!(translate_replay_event_to_update(&delta).is_none());
    }

    fn thought_text(chunk: &ContentChunk) -> &str {
        let ContentBlock::Text(text) = &chunk.content else {
            panic!("expected text content");
        };
        text.text.as_str()
    }

    fn reasoning_part_id(chunk: &ContentChunk) -> Option<&str> {
        chunk
            .meta
            .as_ref()
            .and_then(|meta| meta.get("querymt"))
            .and_then(|querymt| querymt.get("reasoning_part_id"))
            .and_then(serde_json::Value::as_str)
    }

    #[test]
    fn live_translator_forwards_thinking_delta_as_agent_thought_chunk() {
        let delta = EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantThinkingDelta {
                content: "thinking".to_string(),
                message_id: "a-1".to_string(),
                part_id: Some("rs_1:summary:0".to_string()),
            },
        });

        let Some(SessionUpdate::AgentThoughtChunk(chunk)) =
            AcpLiveEventTranslator::new().translate_update(&delta)
        else {
            panic!("expected thought chunk");
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "thinking");
        assert_eq!(
            chunk.message_id.as_ref().map(|id| id.0.as_ref()),
            Some("a-1")
        );
        assert_eq!(
            chunk
                .meta
                .as_ref()
                .and_then(|meta| meta.get("querymt"))
                .and_then(|querymt| querymt.get("reasoning_part_id"))
                .and_then(serde_json::Value::as_str),
            Some("rs_1:summary:0")
        );
    }

    #[test]
    fn replay_projects_each_summary_part_as_its_own_thought_chunk() {
        let stored = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-stored".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantMessageStored {
                content: "answer".to_string(),
                thinking: Some("one\n\ntwo".to_string()),
                reasoning_parts: vec![
                    ReasoningPartStored {
                        id: "rs_1:summary:0".to_string(),
                        text: "one".to_string(),
                    },
                    ReasoningPartStored {
                        id: "rs_1:summary:1".to_string(),
                        text: "two".to_string(),
                    },
                ],
                message_id: Some("a-1".to_string()),
            },
        });

        let updates = translate_replay_event_to_updates(&stored);
        assert_eq!(updates.len(), 3);
        let SessionUpdate::AgentThoughtChunk(first) = &updates[0] else {
            panic!("expected first summary");
        };
        let SessionUpdate::AgentThoughtChunk(second) = &updates[1] else {
            panic!("expected second summary");
        };
        let SessionUpdate::AgentMessageChunk(answer) = &updates[2] else {
            panic!("expected answer");
        };
        assert_eq!(thought_text(first), "one");
        assert_eq!(thought_text(second), "two");
        assert_eq!(reasoning_part_id(first), Some("rs_1:summary:0"));
        assert_eq!(reasoning_part_id(second), Some("rs_1:summary:1"));
        let ContentBlock::Text(text) = &answer.content else {
            panic!("expected answer text");
        };
        assert_eq!(text.text, "answer");
    }

    #[test]
    fn live_translator_skips_empty_thinking_delta() {
        let delta = EventEnvelope::Ephemeral(crate::events::EphemeralEvent {
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantThinkingDelta {
                content: String::new(),
                message_id: "a-1".to_string(),
                part_id: None,
            },
        });

        assert!(
            AcpLiveEventTranslator::new()
                .translate_update(&delta)
                .is_none()
        );
    }

    #[test]
    fn replay_agent_events_emit_session_notifications() {
        let notifications = replay_agent_events_to_session_notifications(
            "s-1",
            vec![
                AgentEvent {
                    seq: 1,
                    timestamp: 1,
                    session_id: "s-1".to_string(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind: AgentEventKind::PromptReceived {
                        content: "hello".to_string(),
                        message_id: Some("u-1".to_string()),
                    },
                },
                AgentEvent {
                    seq: 2,
                    timestamp: 2,
                    session_id: "s-1".to_string(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind: AgentEventKind::AssistantMessageStored {
                        content: "hi".to_string(),
                        thinking: None,
                        reasoning_parts: Vec::new(),
                        message_id: Some("a-1".to_string()),
                    },
                },
                AgentEvent {
                    seq: 3,
                    timestamp: 3,
                    session_id: "s-1".to_string(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind: AgentEventKind::AssistantContentDelta {
                        content: "ignored".to_string(),
                        message_id: "a-1".to_string(),
                    },
                },
            ],
        );

        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0].update,
            SessionUpdate::UserMessageChunk(_)
        ));
        assert!(matches!(
            notifications[1].update,
            SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn replay_uses_persisted_structured_prompt_blocks_when_available() {
        let event = AgentEvent {
            seq: 1,
            timestamp: 1,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::PromptReceived {
                content: "compact summary".to_string(),
                message_id: Some("u-1".to_string()),
            },
        };
        let prompt_blocks = HashMap::from([(
            "u-1".to_string(),
            vec![
                ContentBlock::Image(crate::acp::protocol::ImageContent::new("AQID", "image/png")),
                ContentBlock::Text(TextContent::new("describe this")),
            ],
        )]);
        let notifications =
            replay_agent_events_with_user_prompts("s-1", vec![event], &prompt_blocks);

        assert_eq!(notifications.len(), 2);
        assert!(matches!(
            notifications[0].update,
            SessionUpdate::UserMessageChunk(ref chunk)
                if matches!(chunk.content, ContentBlock::Image(_))
        ));
        for notification in notifications {
            let SessionUpdate::UserMessageChunk(chunk) = notification.update else {
                panic!("expected user message chunk");
            };
            assert_eq!(
                chunk.message_id.as_ref().map(|id| id.0.as_ref()),
                Some("u-1")
            );
        }
    }

    #[test]
    fn todo_tool_start_translates_to_plan_update() {
        let event = tool_start_event(
            "todowrite",
            serde_json::json!({
                "todos": [
                    {"id": "a", "content": "task a", "status": "pending", "priority": "high"},
                    {"id": "b", "content": "task b", "status": "in_progress", "priority": "medium"},
                    {"id": "c", "content": "task c", "status": "completed", "priority": "low"}
                ]
            }),
        );

        let update = translate_replay_event_to_update(&event);
        let Some(SessionUpdate::Plan(plan)) = update else {
            panic!("expected plan update");
        };

        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].content, "task a");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::Pending);
        assert_eq!(plan.entries[0].priority, PlanEntryPriority::High);
    }

    #[test]
    fn cancelled_todos_are_omitted_from_plan() {
        let event = tool_start_event(
            "mcp_todowrite",
            serde_json::json!({
                "todos": [
                    {"id": "a", "content": "task a", "status": "cancelled", "priority": "high"},
                    {"id": "b", "content": "task b", "status": "pending", "priority": "low"}
                ]
            }),
        );

        let update = translate_replay_event_to_update(&event);
        let Some(SessionUpdate::Plan(plan)) = update else {
            panic!("expected plan update");
        };

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].content, "task b");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::Pending);
    }

    #[test]
    fn malformed_todowrite_arguments_do_not_emit_update() {
        let event = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-3".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "todowrite".to_string(),
                arguments: "{ not-json".to_string(),
            },
        });

        assert!(translate_replay_event_to_update(&event).is_none());
    }

    #[test]
    fn todo_tools_do_not_emit_tool_call_end_updates() {
        assert!(translate_replay_event_to_update(&tool_end_event("todowrite")).is_none());
        assert!(translate_replay_event_to_update(&tool_end_event("mcp_todowrite")).is_none());
    }

    #[test]
    fn non_todo_tools_still_emit_tool_call_updates() {
        let start = tool_start_event("read_tool", serde_json::json!({"path": "src/main.rs"}));
        let end = tool_end_event("read_tool");

        assert!(matches!(
            translate_replay_event_to_update(&start),
            Some(SessionUpdate::ToolCall(_))
        ));
        assert!(matches!(
            translate_replay_event_to_update(&end),
            Some(SessionUpdate::ToolCallUpdate(_))
        ));
    }

    #[test]
    fn mode_config_option_contains_expected_shape() {
        use crate::acp::protocol::SessionConfigOptionCategory;
        let options = session_config_options(AgentMode::Plan, None);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id.0.as_ref(), "mode");
        assert_eq!(options[0].category, Some(SessionConfigOptionCategory::Mode));

        let select = match &options[0].kind {
            crate::acp::protocol::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "plan");
    }

    #[test]
    fn reasoning_effort_config_option_contains_expected_shape() {
        use crate::acp::protocol::SessionConfigOptionCategory;
        let options =
            session_config_options(AgentMode::Build, Some(querymt::chat::ReasoningEffort::High));
        assert_eq!(options.len(), 2);
        assert_eq!(options[1].id.0.as_ref(), "reasoning_effort");
        assert_eq!(
            options[1].category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );

        let select = match &options[1].kind {
            crate::acp::protocol::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "high");
    }

    #[test]
    fn reasoning_effort_config_option_auto_when_none() {
        let options = session_config_options(AgentMode::Build, None);
        let select = match &options[1].kind {
            crate::acp::protocol::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "auto");
    }

    struct CancelTestAgent {
        cancelled_session: Arc<Mutex<Option<String>>>,
        prompt_started: Option<Arc<Notify>>,
        release_prompt: Option<Arc<Notify>>,
        cancel_seen: Option<Arc<Notify>>,
        local_handle: Option<Arc<AgentHandle>>,
        reject_ext_method: bool,
        ext_method_control: Option<Arc<ExtMethodControl>>,
    }

    struct ExtMethodControl {
        calls: AtomicUsize,
        first_started: Notify,
        second_started: Notify,
        release_first: Notify,
    }

    struct RejectPostAttachmentHooks {
        session_load: bool,
        remote_session: bool,
    }

    #[async_trait::async_trait]
    impl AcpSessionHooks for RejectPostAttachmentHooks {
        async fn on_session_loaded(
            &self,
            _: &AgentHandle,
            _: &str,
            _: &mut serde_json::Value,
        ) -> Result<(), Error> {
            if self.session_load {
                Err(Error::invalid_params().data("session load hook rejected"))
            } else {
                Ok(())
            }
        }

        async fn on_remote_session_attached(
            &self,
            _: &AgentHandle,
            _: &str,
            _: &mut serde_json::Value,
        ) -> Result<(), Error> {
            if self.remote_session {
                Err(Error::invalid_params().data("remote session hook rejected"))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl SendAgent for CancelTestAgent {
        async fn initialize(
            &self,
            _: crate::acp::protocol::InitializeRequest,
        ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
            unreachable!()
        }
        async fn authenticate(
            &self,
            _: crate::acp::protocol::AuthenticateRequest,
        ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
            unreachable!()
        }
        async fn new_session(
            &self,
            _: crate::acp::protocol::NewSessionRequest,
        ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
            Ok(crate::acp::protocol::NewSessionResponse::new("s-lifecycle"))
        }
        async fn prompt(
            &self,
            _: crate::acp::protocol::PromptRequest,
        ) -> Result<crate::acp::protocol::PromptResponse, Error> {
            if let Some(prompt_started) = &self.prompt_started {
                prompt_started.notify_one();
            }
            if let Some(release_prompt) = &self.release_prompt {
                release_prompt.notified().await;
            }
            Ok(crate::acp::protocol::PromptResponse::new(
                crate::acp::protocol::StopReason::Cancelled,
            ))
        }
        async fn cancel(
            &self,
            notif: crate::acp::protocol::CancelNotification,
        ) -> Result<(), Error> {
            *self.cancelled_session.lock().await = Some(notif.session_id.to_string());
            if let Some(cancel_seen) = &self.cancel_seen {
                cancel_seen.notify_one();
            }
            Ok(())
        }
        async fn load_session(
            &self,
            _: crate::acp::protocol::LoadSessionRequest,
        ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
            Ok(crate::acp::protocol::LoadSessionResponse::new())
        }
        async fn list_sessions(
            &self,
            _: crate::acp::protocol::ListSessionsRequest,
        ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
            unreachable!()
        }
        async fn fork_session(
            &self,
            _: crate::acp::protocol::ForkSessionRequest,
        ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
            unreachable!()
        }
        async fn resume_session(
            &self,
            _: crate::acp::protocol::ResumeSessionRequest,
        ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
            unreachable!()
        }
        async fn close_session(
            &self,
            _: crate::acp::protocol::CloseSessionRequest,
        ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
            unreachable!()
        }
        async fn delete_session(
            &self,
            _: crate::acp::protocol::DeleteSessionRequest,
        ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
            unreachable!()
        }
        async fn set_session_model(
            &self,
            _: crate::acp::protocol::SetSessionModelRequest,
        ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
            unreachable!()
        }
        async fn ext_method(
            &self,
            req: crate::acp::protocol::ExtRequest,
        ) -> Result<crate::acp::protocol::ExtResponse, Error> {
            if let Some(control) = &self.ext_method_control {
                if control.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    control.first_started.notify_one();
                    control.release_first.notified().await;
                    return Err(Error::invalid_params().data("rejected extension request"));
                }
                control.second_started.notify_one();
                let raw = serde_json::value::RawValue::from_string("null".to_string()).unwrap();
                return Ok(crate::acp::protocol::ExtResponse::new(Arc::from(raw)));
            }
            if self.reject_ext_method {
                return Err(Error::invalid_params().data("rejected extension request"));
            }
            let response = if req.method.as_ref() == "querymt/remote/createSession" {
                serde_json::json!({
                    "session_id": "s-remote-created",
                    "node_id": "n-1",
                    "attached": false,
                    "config_options": []
                })
            } else {
                serde_json::json!({"attached": true})
            };
            let raw = serde_json::value::RawValue::from_string(response.to_string()).unwrap();
            Ok(crate::acp::protocol::ExtResponse::new(Arc::from(raw)))
        }
        async fn ext_notification(
            &self,
            _: crate::acp::protocol::ExtNotification,
        ) -> Result<(), Error> {
            unreachable!()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self.local_handle
                .as_deref()
                .map_or(self as &dyn std::any::Any, |handle| handle)
        }
    }

    fn cancel_test_agent() -> (CancelTestAgent, Arc<Mutex<Option<String>>>) {
        let cancelled_session = Arc::new(Mutex::new(None));
        (
            CancelTestAgent {
                cancelled_session: cancelled_session.clone(),
                prompt_started: None,
                release_prompt: None,
                cancel_seen: None,
                local_handle: None,
                reject_ext_method: false,
                ext_method_control: None,
            },
            cancelled_session,
        )
    }

    fn rpc_cancel_message(id: Option<serde_json::Value>, params: serde_json::Value) -> RpcMessage {
        RpcMessage {
            jsonrpc: "2.0".to_string(),
            method: AGENT_METHOD_NAMES.session_cancel.to_string(),
            params,
            id,
        }
    }

    #[tokio::test]
    async fn concurrent_dispatch_keeps_cancel_unblocked_by_prompt() {
        let prompt_started = Arc::new(Notify::new());
        let release_prompt = Arc::new(Notify::new());
        let cancel_seen = Arc::new(Notify::new());
        let cancelled_session = Arc::new(Mutex::new(None));
        let agent = Arc::new(CancelTestAgent {
            prompt_started: Some(prompt_started.clone()),
            release_prompt: Some(release_prompt.clone()),
            cancel_seen: Some(cancel_seen.clone()),
            cancelled_session: cancelled_session.clone(),
            local_handle: None,
            reject_ext_method: false,
            ext_method_control: None,
        });
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel(2);

        let prompt_task = tokio::spawn(dispatch_rpc_message(
            agent.clone(),
            session_owners.clone(),
            pending_permissions.clone(),
            pending_elicitations.clone(),
            "conn-1".to_string(),
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_prompt.to_string(),
                params: serde_json::json!({
                    "sessionId": "s-cancel",
                    "prompt": [{"type": "text", "text": "long task"}]
                }),
                id: Some(serde_json::json!(1)),
            },
            tx.clone(),
        ));

        timeout(Duration::from_secs(2), prompt_started.notified())
            .await
            .expect("prompt should start");

        let cancel_task = tokio::spawn(dispatch_rpc_message(
            agent,
            session_owners,
            pending_permissions,
            pending_elicitations,
            "conn-1".to_string(),
            rpc_cancel_message(None, serde_json::json!({"sessionId": "s-cancel"})),
            tx,
        ));

        timeout(Duration::from_secs(2), cancel_seen.notified())
            .await
            .expect("cancel should be dispatched before prompt completes");
        assert_eq!(cancelled_session.lock().await.as_deref(), Some("s-cancel"));
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "cancel notification must not emit a response"
        );

        release_prompt.notify_one();
        let response = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("prompt response should arrive")
            .expect("response channel should remain open");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).expect("response should be JSON")
                ["id"],
            serde_json::json!(1)
        );

        prompt_task.await.expect("prompt task should finish");
        cancel_task.await.expect("cancel task should finish");
    }

    #[tokio::test]
    async fn session_cancel_notification_dispatches_without_response() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (agent, cancelled_session) = cancel_test_agent();

        let output = handle_rpc_message(
            &agent,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            rpc_cancel_message(None, serde_json::json!({"sessionId": "s-cancel"})),
        )
        .await;

        assert!(output.response.is_none());
        assert_eq!(cancelled_session.lock().await.as_deref(), Some("s-cancel"));
    }

    #[tokio::test]
    async fn rejected_input_extension_rolls_back_only_new_session_ownership() {
        let session_owners = SessionOwnerMap::default();
        session_owners.lock().await.insert(
            "s-existing".to_string(),
            HashSet::from(["conn-ext".to_string(), "conn-other".to_string()]),
        );
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let agent = CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: None,
            release_prompt: None,
            cancel_seen: None,
            local_handle: None,
            reject_ext_method: true,
            ext_method_control: None,
        };

        for (method, session_id) in [
            ("querymt/session/steer", "s-steer"),
            ("querymt/session/queue", "s-queue"),
            ("querymt/session/discardQueuedInput", "s-discard"),
            ("querymt/session/queue", "s-existing"),
        ] {
            let output = handle_rpc_message(
                &agent,
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                "conn-ext",
                RpcMessage {
                    jsonrpc: "2.0".to_string(),
                    method: method.to_string(),
                    params: serde_json::json!({
                        "session_id": session_id,
                        "client_input_id": "input-1",
                        "input_id": "input-1",
                        "prompt": [{"type": "text", "text": "queued"}]
                    }),
                    id: Some(serde_json::json!(1)),
                },
            )
            .await;
            assert!(
                output
                    .response
                    .expect("request should produce response")
                    .error
                    .is_some()
            );
        }

        let owners = session_owners.lock().await;
        for session_id in ["s-steer", "s-queue", "s-discard"] {
            assert!(!owners.contains_key(session_id));
        }
        assert_eq!(
            owners.get("s-existing"),
            Some(&HashSet::from([
                "conn-ext".to_string(),
                "conn-other".to_string()
            ]))
        );
    }

    #[tokio::test]
    async fn concurrent_input_extensions_keep_successful_session_ownership() {
        let control = Arc::new(ExtMethodControl {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            second_started: Notify::new(),
            release_first: Notify::new(),
        });
        let agent = Arc::new(CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: None,
            release_prompt: None,
            cancel_seen: None,
            local_handle: None,
            reject_ext_method: false,
            ext_method_control: Some(control.clone()),
        });
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        let spawn_request = |id| {
            let agent = agent.clone();
            let session_owners = session_owners.clone();
            let pending_permissions = pending_permissions.clone();
            let pending_elicitations = pending_elicitations.clone();
            tokio::spawn(async move {
                handle_rpc_message(
                    agent.as_ref(),
                    &session_owners,
                    &pending_permissions,
                    &pending_elicitations,
                    "conn-ext",
                    RpcMessage {
                        jsonrpc: "2.0".to_string(),
                        method: "querymt/session/queue".to_string(),
                        params: serde_json::json!({
                            "session_id": "s-concurrent",
                            "client_input_id": format!("input-{id}"),
                            "prompt": [{"type": "text", "text": "queued"}]
                        }),
                        id: Some(serde_json::json!(id)),
                    },
                )
                .await
            })
        };

        let rejected = spawn_request(1);
        timeout(Duration::from_secs(2), control.first_started.notified())
            .await
            .expect("first extension should start");
        let accepted = spawn_request(2);
        assert!(
            timeout(Duration::from_millis(50), control.second_started.notified())
                .await
                .is_err(),
            "second extension must wait for the first ownership decision"
        );

        control.release_first.notify_one();
        let rejected = rejected.await.expect("rejected request should finish");
        assert!(
            rejected
                .response
                .expect("rejected request should produce response")
                .error
                .is_some()
        );
        timeout(Duration::from_secs(2), control.second_started.notified())
            .await
            .expect("second extension should start after rollback");
        let accepted = accepted.await.expect("accepted request should finish");
        assert!(
            accepted
                .response
                .expect("accepted request should produce response")
                .error
                .is_none()
        );
        assert_eq!(
            session_owners.lock().await.get("s-concurrent").cloned(),
            Some(HashSet::from(["conn-ext".to_string()]))
        );
    }

    #[tokio::test]
    async fn failed_session_load_hook_rolls_back_only_new_ownership() {
        let local_fixture = crate::test_utils::TestAgent::new().await;
        let agent = CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: None,
            release_prompt: None,
            cancel_seen: None,
            local_handle: Some(local_fixture.handle),
            reject_ext_method: false,
            ext_method_control: None,
        };
        let session_owners = SessionOwnerMap::default();
        session_owners.lock().await.insert(
            "s-load-existing".to_string(),
            HashSet::from(["conn-hook".to_string(), "conn-other".to_string()]),
        );
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let context = RpcDispatchContext {
            session_hooks: Some(Arc::new(RejectPostAttachmentHooks {
                session_load: true,
                remote_session: false,
            })),
            session_bridge: None,
            elicitation_recovery: None,
            translator: None,
        };

        for session_id in ["s-load-new", "s-load-existing"] {
            let output = handle_rpc_message_with_context(
                &agent,
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                "conn-hook",
                RpcMessage {
                    jsonrpc: "2.0".to_string(),
                    method: AGENT_METHOD_NAMES.session_load.to_string(),
                    params: serde_json::json!({
                        "sessionId": session_id,
                        "cwd": "/tmp",
                        "mcpServers": []
                    }),
                    id: Some(serde_json::json!(1)),
                },
                context.clone(),
            )
            .await;
            assert!(
                output
                    .response
                    .expect("request should produce response")
                    .error
                    .is_some()
            );
        }

        let owners = session_owners.lock().await;
        assert!(!owners.contains_key("s-load-new"));
        assert_eq!(
            owners.get("s-load-existing"),
            Some(&HashSet::from([
                "conn-hook".to_string(),
                "conn-other".to_string()
            ]))
        );
    }

    #[tokio::test]
    async fn failed_remote_attachment_hook_rolls_back_only_new_ownership() {
        let local_fixture = crate::test_utils::TestAgent::new().await;
        let agent = CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: None,
            release_prompt: None,
            cancel_seen: None,
            local_handle: Some(local_fixture.handle),
            reject_ext_method: false,
            ext_method_control: None,
        };
        let session_owners = SessionOwnerMap::default();
        session_owners.lock().await.insert(
            "s-remote-existing".to_string(),
            HashSet::from(["conn-hook".to_string(), "conn-other".to_string()]),
        );
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let context = RpcDispatchContext {
            session_hooks: Some(Arc::new(RejectPostAttachmentHooks {
                session_load: false,
                remote_session: true,
            })),
            session_bridge: None,
            elicitation_recovery: None,
            translator: None,
        };

        for (method, params) in [
            (
                "querymt/remote/attachSession",
                serde_json::json!({"session_id": "s-remote-new", "node_id": "n-1"}),
            ),
            (
                "querymt/remote/attachSession",
                serde_json::json!({"session_id": "s-remote-existing", "node_id": "n-1"}),
            ),
            (
                "querymt/remote/createSession",
                serde_json::json!({"node_id": "n-1"}),
            ),
        ] {
            let output = handle_rpc_message_with_context(
                &agent,
                &session_owners,
                &pending_permissions,
                &pending_elicitations,
                "conn-hook",
                RpcMessage {
                    jsonrpc: "2.0".to_string(),
                    method: method.to_string(),
                    params,
                    id: Some(serde_json::json!(1)),
                },
                context.clone(),
            )
            .await;
            assert!(
                output
                    .response
                    .expect("request should produce response")
                    .error
                    .is_some()
            );
        }

        let owners = session_owners.lock().await;
        for session_id in ["s-remote-new", "s-remote-created"] {
            assert!(!owners.contains_key(session_id));
        }
        assert_eq!(
            owners.get("s-remote-existing"),
            Some(&HashSet::from([
                "conn-hook".to_string(),
                "conn-other".to_string()
            ]))
        );
    }

    #[tokio::test]
    async fn lifecycle_response_survives_bridge_attachment_failure() {
        let local_fixture = crate::test_utils::TestAgent::new().await;
        let (bridge_tx, _bridge_rx) = mpsc::channel(1);
        let agent = CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: None,
            release_prompt: None,
            cancel_seen: None,
            local_handle: Some(local_fixture.handle),
            reject_ext_method: false,
            ext_method_control: None,
        };
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let output = handle_rpc_message_with_context(
            &agent,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-lifecycle",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_new.to_string(),
                params: serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext {
                session_hooks: None,
                session_bridge: Some(
                    crate::acp::client_bridge::ClientBridgeSender::for_connection(
                        bridge_tx,
                        "conn-lifecycle",
                    ),
                ),
                elicitation_recovery: None,
                translator: None,
            },
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.error.is_none());
        assert_eq!(
            response.result,
            Some(serde_json::json!({"sessionId": "s-lifecycle"}))
        );
        assert_eq!(
            session_owners.lock().await.get("s-lifecycle").cloned(),
            Some(HashSet::from(["conn-lifecycle".to_string()]))
        );
    }

    #[tokio::test]
    async fn prompt_rejects_bridge_attachment_failure_before_execution() {
        let local_fixture = crate::test_utils::TestAgent::new().await;
        let prompt_started = Arc::new(Notify::new());
        let (bridge_tx, _bridge_rx) = mpsc::channel(1);
        let agent = CancelTestAgent {
            cancelled_session: Arc::new(Mutex::new(None)),
            prompt_started: Some(prompt_started.clone()),
            release_prompt: None,
            cancel_seen: None,
            local_handle: Some(local_fixture.handle),
            reject_ext_method: false,
            ext_method_control: None,
        };
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let output = handle_rpc_message_with_context(
            &agent,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-prompt",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_prompt.to_string(),
                params: serde_json::json!({
                    "sessionId": "s-missing",
                    "prompt": [{"type": "text", "text": "must not run"}]
                }),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext {
                session_hooks: None,
                session_bridge: Some(
                    crate::acp::client_bridge::ClientBridgeSender::for_connection(
                        bridge_tx,
                        "conn-prompt",
                    ),
                ),
                elicitation_recovery: None,
                translator: None,
            },
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert!(!session_owners.lock().await.contains_key("s-missing"));
        assert!(
            timeout(Duration::from_millis(50), prompt_started.notified())
                .await
                .is_err(),
            "prompt must not execute after bridge attachment fails"
        );
    }

    #[tokio::test]
    async fn session_new_rpc_dispatches_to_agent_new_session() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                Ok(crate::acp::protocol::NewSessionResponse::new("s-plain"))
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                unreachable!()
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                unreachable!()
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message_with_context(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_new.to_string(),
                params: serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext {
                session_hooks: None,
                session_bridge: None,
                elicitation_recovery: None,
                translator: None,
            },
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.error.is_none());
        assert_eq!(
            response.result,
            Some(serde_json::json!({"sessionId": "s-plain"}))
        );
        assert!(output.notifications.is_empty());
    }

    fn docs_slash_command() -> crate::slash_commands::SlashCommand {
        crate::slash_commands::SlashCommand {
            name: "docs".to_string(),
            source: crate::slash_commands::SlashCommandSource::Global(std::path::PathBuf::from(
                "/tmp",
            )),
            path: std::path::PathBuf::from("/tmp/docs.md"),
            description: "Read the docs".to_string(),
            argument_hint: None,
            tags: Vec::new(),
            template: "Read the docs".to_string(),
            script: None,
            requires_script: false,
        }
    }

    fn assert_docs_catalog(notification: &serde_json::Value, session_id: &str) {
        assert_eq!(notification["jsonrpc"], "2.0");
        assert_eq!(notification["method"], "session/update");
        assert_eq!(notification["params"]["sessionId"], session_id);
        assert_eq!(
            notification["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        assert_eq!(
            notification["params"]["update"]["availableCommands"][0]["name"],
            "/docs"
        );
    }

    async fn dispatch_session_rpc(
        agent: &crate::agent::LocalAgentHandle,
        method: &str,
        params: serde_json::Value,
    ) -> RpcDispatchOutput {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        handle_rpc_message_with_context(
            agent,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params,
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext {
                session_hooks: None,
                session_bridge: None,
                elicitation_recovery: None,
                translator: None,
            },
        )
        .await
    }

    async fn dispatch_session_load_with_capabilities(
        agent: &crate::agent::LocalAgentHandle,
        session_id: &str,
        capabilities: AcpSessionUpdateCapabilities,
    ) -> RpcDispatchOutput {
        let translator = Arc::new(std::sync::Mutex::new(AcpLiveEventTranslator::new()));
        translator.lock().unwrap().set_capabilities(capabilities);
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        handle_rpc_message_with_context(
            agent,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_load.to_string(),
                params: serde_json::json!({
                    "sessionId": session_id,
                    "cwd": "/tmp",
                    "mcpServers": [],
                }),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext {
                session_hooks: None,
                session_bridge: None,
                elicitation_recovery: None,
                translator: Some(translator),
            },
        )
        .await
    }

    #[tokio::test]
    async fn session_load_replays_materialized_updates_for_connection_capabilities() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let session_id = fixture.create_session().await;
        let journal = fixture.storage.event_journal();
        for kind in [
            AgentEventKind::ToolCallStart {
                tool_call_id: "todo-1".to_string(),
                tool_name: "todowrite".to_string(),
                arguments: serde_json::json!({
                    "todos": [{
                        "content": "ship replay",
                        "status": "in_progress",
                        "priority": "high"
                    }]
                })
                .to_string(),
            },
            AgentEventKind::CompactionStart {
                token_estimate: 10,
                compaction_id: Some("compaction-1".to_string()),
            },
            AgentEventKind::CompactionSummaryChunk {
                compaction_id: "compaction-1".to_string(),
                content: "streamed summary".to_string(),
            },
            AgentEventKind::CompactionEnd {
                summary: "final summary".to_string(),
                summary_len: 13,
                compaction_id: Some("compaction-1".to_string()),
                context_tokens: Some(5),
            },
            AgentEventKind::HookNotice {
                event_name: "post_tool".to_string(),
                message: "live only".to_string(),
                is_error: false,
            },
        ] {
            journal
                .append_durable(&NewDurableEvent {
                    session_id: session_id.clone(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    kind,
                })
                .await
                .expect("persist replay event");
        }

        let capable = dispatch_session_load_with_capabilities(
            fixture.handle.as_ref(),
            &session_id,
            AcpSessionUpdateCapabilities {
                plan_operations: true,
                notices: true,
                compaction: true,
            },
        )
        .await;
        assert!(capable.response.unwrap().error.is_none());
        let capable_updates: Vec<&serde_json::Value> = capable
            .notifications
            .iter()
            .filter_map(|notification| notification["params"].get("update"))
            .collect();
        assert!(capable_updates.iter().any(|update| {
            update["sessionUpdate"] == "plan_update"
                && update["plan"]["planId"] == QUERYMT_TODO_PLAN_ID
        }));
        assert!(capable_updates.iter().any(|update| {
            update["sessionUpdate"] == "compaction_update"
                && update["compactionId"] == "compaction-1"
                && update["status"] == "completed"
        }));
        assert!(capable_updates.iter().all(|update| {
            !matches!(
                update["sessionUpdate"].as_str(),
                Some("notice" | "compaction_summary_chunk")
            )
        }));

        let legacy = dispatch_session_load_with_capabilities(
            fixture.handle.as_ref(),
            &session_id,
            AcpSessionUpdateCapabilities::default(),
        )
        .await;
        assert!(legacy.response.unwrap().error.is_none());
        let legacy_updates: Vec<&serde_json::Value> = legacy
            .notifications
            .iter()
            .filter_map(|notification| notification["params"].get("update"))
            .collect();
        assert!(
            legacy_updates
                .iter()
                .any(|update| update["sessionUpdate"] == "plan")
        );
        assert!(legacy_updates.iter().all(|update| {
            !matches!(
                update["sessionUpdate"].as_str(),
                Some(
                    "plan_update"
                        | "plan_removed"
                        | "notice"
                        | "compaction_update"
                        | "compaction_summary_chunk"
                )
            )
        }));
    }

    #[tokio::test]
    async fn session_new_load_and_resume_advertise_slash_commands() {
        let mut registry = crate::slash_commands::SlashCommandRegistry::new();
        registry.register(docs_slash_command());
        let fixture = crate::test_utils::TestAgent::with_slash_command_registry(registry).await;

        let created = dispatch_session_rpc(
            fixture.handle.as_ref(),
            AGENT_METHOD_NAMES.session_new,
            serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
        )
        .await;
        let response = created.response.expect("session/new should respond");
        assert!(response.error.is_none());
        let session_id = response.result.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();
        assert_eq!(created.notifications.len(), 1);
        assert_docs_catalog(&created.notifications[0], &session_id);

        let loaded = dispatch_session_rpc(
            fixture.handle.as_ref(),
            AGENT_METHOD_NAMES.session_load,
            serde_json::json!({"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}),
        )
        .await;
        assert!(
            loaded
                .response
                .expect("session/load should respond")
                .error
                .is_none()
        );
        assert_eq!(loaded.notifications.len(), 1);
        assert_docs_catalog(&loaded.notifications[0], &session_id);

        let resumed = dispatch_session_rpc(
            fixture.handle.as_ref(),
            AGENT_METHOD_NAMES.session_resume,
            serde_json::json!({"sessionId": session_id, "cwd": "/tmp"}),
        )
        .await;
        assert!(
            resumed
                .response
                .expect("session/resume should respond")
                .error
                .is_none()
        );
        assert_eq!(resumed.notifications.len(), 1);
        assert_docs_catalog(&resumed.notifications[0], &session_id);
    }

    #[tokio::test]
    async fn dispatch_rpc_message_sends_slash_catalog_after_response() {
        let mut registry = crate::slash_commands::SlashCommandRegistry::new();
        registry.register(docs_slash_command());
        let fixture = crate::test_utils::TestAgent::with_slash_command_registry(registry).await;
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::channel(8);

        dispatch_rpc_message(
            fixture.handle.clone(),
            session_owners,
            pending_permissions,
            pending_elicitations,
            "conn-1".to_string(),
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_new.to_string(),
                params: serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
                id: Some(serde_json::json!(1)),
            },
            tx,
        )
        .await;

        let response: serde_json::Value =
            serde_json::from_str(&rx.recv().await.expect("session/new response"))
                .expect("response json");
        assert_eq!(response["id"], 1);
        let session_id = response["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        let notification: serde_json::Value =
            serde_json::from_str(&rx.recv().await.expect("catalog notification"))
                .expect("notification json");
        assert_docs_catalog(&notification, &session_id);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remote_attach_extension_records_session_owner_with_snake_case_payload() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                unreachable!()
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                req: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                assert_eq!(req.method.as_ref(), "querymt/remote/attachSession");
                let raw = serde_json::value::RawValue::from_string(
                    serde_json::json!({"attached": true}).to_string(),
                )
                .unwrap();
                Ok(crate::acp::protocol::ExtResponse::new(Arc::from(raw)))
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message_with_context(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-9",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: "_querymt/remote/attachSession".to_string(),
                params: serde_json::json!({"session_id": "s-remote", "node_id": "n-1"}),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext::default(),
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.error.is_none());
        assert_eq!(
            session_owners.lock().await.get("s-remote").cloned(),
            Some(HashSet::from(["conn-9".to_string()]))
        );
    }

    #[tokio::test]
    async fn remote_create_session_records_session_owner_from_snake_case_response() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                unreachable!()
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                let raw = serde_json::value::RawValue::from_string(
                    serde_json::json!({"session_id": "s-cr","node_id": "n-1","attached":false,"config_options":[]}).to_string(),
                )
                .unwrap();
                Ok(crate::acp::protocol::ExtResponse::new(Arc::from(raw)))
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message_with_context(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-9",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: "querymt/remote/createSession".to_string(),
                params: serde_json::json!({"node_id": "n-1"}),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext::default(),
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.error.is_none());
        assert_eq!(
            session_owners.lock().await.get("s-cr").cloned(),
            Some(HashSet::from(["conn-9".to_string()]))
        );
    }

    #[tokio::test]
    async fn session_load_records_session_owner_for_live_updates() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                Ok(crate::acp::protocol::LoadSessionResponse::new())
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                unreachable!()
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                unreachable!()
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message_with_context(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-load",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_load.to_string(),
                params: serde_json::json!({
                    "sessionId": "s-load",
                    "cwd": "/tmp",
                    "mcpServers": []
                }),
                id: Some(serde_json::json!(1)),
            },
            RpcDispatchContext::default(),
        )
        .await;

        let response = output.response.expect("request should produce response");
        assert!(response.error.is_none());
        assert_eq!(
            session_owners.lock().await.get("s-load").cloned(),
            Some(HashSet::from(["conn-load".to_string()]))
        );
    }

    #[tokio::test]
    async fn session_close_rpc_forwards_to_send_agent() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                Ok(crate::acp::protocol::CloseSessionResponse::new())
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                Ok(crate::acp::protocol::DeleteSessionResponse::new())
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                unreachable!()
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: AGENT_METHOD_NAMES.session_close.to_string(),
                params: serde_json::json!({
                    "sessionId": "s-1"
                }),
                id: Some(serde_json::json!(1)),
            },
        )
        .await;
        let response = output.response.expect("request should produce response");

        assert!(
            response.error.is_none(),
            "expected successful close response"
        );
        assert_eq!(response.result, Some(serde_json::json!({})));
    }

    /// Verify that the RPC dispatcher correctly forwards set_session_config_option
    /// to the SendAgent trait method (default impl returns method_not_found).
    #[tokio::test]
    async fn set_config_option_rpc_forwards_to_send_agent() {
        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        // Minimal SendAgent that returns method_not_found for set_session_config_option
        // (the default impl). This proves the dispatcher forwards correctly.
        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: crate::acp::protocol::InitializeRequest,
            ) -> Result<crate::acp::protocol::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: crate::acp::protocol::AuthenticateRequest,
            ) -> Result<crate::acp::protocol::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: crate::acp::protocol::NewSessionRequest,
            ) -> Result<crate::acp::protocol::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: crate::acp::protocol::PromptRequest,
            ) -> Result<crate::acp::protocol::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: crate::acp::protocol::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: crate::acp::protocol::LoadSessionRequest,
            ) -> Result<crate::acp::protocol::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: crate::acp::protocol::ListSessionsRequest,
            ) -> Result<crate::acp::protocol::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: crate::acp::protocol::ForkSessionRequest,
            ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: crate::acp::protocol::ResumeSessionRequest,
            ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn close_session(
                &self,
                _: crate::acp::protocol::CloseSessionRequest,
            ) -> Result<crate::acp::protocol::CloseSessionResponse, Error> {
                unreachable!()
            }
            async fn delete_session(
                &self,
                _: crate::acp::protocol::DeleteSessionRequest,
            ) -> Result<crate::acp::protocol::DeleteSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: crate::acp::protocol::SetSessionModelRequest,
            ) -> Result<crate::acp::protocol::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: crate::acp::protocol::ExtRequest,
            ) -> Result<crate::acp::protocol::ExtResponse, Error> {
                unreachable!()
            }
            async fn ext_notification(
                &self,
                _: crate::acp::protocol::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let output = handle_rpc_message(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: "session/set_config_option".to_string(),
                params: serde_json::json!({
                    "sessionId": "s-1",
                    "configId": "mode",
                    "value": "plan"
                }),
                id: Some(serde_json::json!(1)),
            },
        )
        .await;
        let response = output.response.expect("request should produce response");

        // Default SendAgent impl returns method_not_found
        assert!(response.error.is_some(), "expected error from default impl");
        let err: agent_client_protocol::Error =
            serde_json::from_value(response.error.unwrap()).unwrap();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }

    #[tokio::test]
    async fn elicitation_result_routes_to_delegate_pending_map() {
        let fixture = DelegateTestFixture::new().await.unwrap();

        let elicitation_id = "delegate-elicitation-rpc".to_string();
        let (tx, rx) = oneshot::channel();
        fixture.delegate.pending_elicitations().lock().await.insert(
            elicitation_id.clone(),
            crate::elicitation::PendingElicitation::for_test("delegate-session", tx),
        );

        let session_owners = SessionOwnerMap::default();
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations = fixture.planner.pending_elicitations();

        let output = handle_rpc_message(
            fixture.planner.as_ref(),
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcMessage {
                jsonrpc: "2.0".to_string(),
                method: "elicitation_result".to_string(),
                params: serde_json::json!({
                    "elicitation_id": elicitation_id,
                    "action": "accept",
                    "content": {"selection": "allow_once"}
                }),
                id: Some(serde_json::json!(1)),
            },
        )
        .await;
        let response = output.response.expect("request should produce response");

        assert!(
            response.error.is_none(),
            "rpc should succeed: {:?}",
            response.error
        );
        let delivered = rx
            .await
            .expect("delegate elicitation should receive response");
        assert_eq!(delivered.action, ElicitationAction::Accept);
        assert_eq!(
            delivered.content,
            Some(serde_json::json!({"selection": "allow_once"}))
        );
    }

    // ─── OTel in-memory test harness ──────────────────────────────────────────

    mod otel_trace_tests {
        use super::*;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
        use std::sync::{Arc, Mutex};
        use tracing::Subscriber;
        use tracing_subscriber::prelude::*;

        #[derive(Clone, Default, Debug)]
        struct TestExporter(Arc<Mutex<Vec<SpanData>>>);

        impl SpanExporter for TestExporter {
            async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
                self.0.lock().unwrap().append(&mut batch);
                Ok(())
            }
        }

        fn test_tracer() -> (SdkTracerProvider, TestExporter, impl Subscriber) {
            let exporter = TestExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let tracer = provider.tracer("acp-test");
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer));
            (provider, exporter, subscriber)
        }

        /// Helper: run a closure with the test subscriber, flush, return exported spans.
        fn with_test_spans<F, T>(f: F) -> Vec<SpanData>
        where
            F: FnOnce() -> T,
        {
            let (_provider, exporter, subscriber) = test_tracer();
            tracing::subscriber::with_default(subscriber, f);
            drop(_provider); // flush
            exporter.0.lock().unwrap().clone()
        }

        // ─── Tests ──────────────────────────────────────────────────────────

        #[test]
        fn run_with_acp_span_load_session() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({});
                    run_with_acp_span(AGENT_METHOD_NAMES.session_load, &params, async {}).await
                });
            });

            assert_eq!(
                spans.len(),
                1,
                "expected exactly one span, got {}",
                spans.len()
            );
            assert_eq!(spans[0].name, "acp.load_session");
        }

        #[test]
        fn run_with_acp_span_new_session() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({});
                    run_with_acp_span(AGENT_METHOD_NAMES.session_new, &params, async {}).await
                });
            });

            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].name, "acp.new_session");
        }

        #[test]
        fn run_with_acp_span_extension() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({});
                    run_with_acp_span("querymt/models", &params, async {}).await
                });
            });

            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].name, "acp.ext_method");
        }

        #[test]
        fn run_with_acp_span_traceparent_sets_parent() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({
                        "_meta": {
                            "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                        }
                    });
                    run_with_acp_span(AGENT_METHOD_NAMES.session_load, &params, async {}).await
                });
            });

            assert_eq!(spans.len(), 1, "expected exactly one span");
            let span = &spans[0];

            // Trace ID must match the remote parent.
            assert_eq!(
                span.span_context.trace_id().to_string(),
                "0af7651916cd43dd8448eb211c80319c",
                "trace ID should match remote parent"
            );

            // Parent span ID must match the remote parent's span ID.
            assert_eq!(
                span.parent_span_id.to_string(),
                "b7ad6b7169203331",
                "parent span ID should match remote parent"
            );
        }

        #[test]
        fn run_with_acp_span_no_traceparent_creates_root_span() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({});
                    run_with_acp_span(AGENT_METHOD_NAMES.session_new, &params, async {}).await
                });
            });

            assert_eq!(spans.len(), 1);
            let span = &spans[0];
            assert_eq!(span.name, "acp.new_session");

            // Parent span ID should be all zeros (root).
            let parent_id = span.parent_span_id.to_string();
            let all_zeros = parent_id.chars().all(|c| c == '0');
            assert!(
                all_zeros,
                "expected zero parent span ID for root span, got {parent_id}"
            );
        }

        #[test]
        fn run_with_acp_span_has_rpc_attributes() {
            let spans = with_test_spans(|| {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let params = serde_json::json!({});
                    run_with_acp_span(AGENT_METHOD_NAMES.session_prompt, &params, async {}).await
                });
            });

            assert_eq!(spans.len(), 1);
            let span = &spans[0];

            let attrs: std::collections::HashMap<&str, &opentelemetry::Value> = span
                .attributes
                .iter()
                .map(|kv| (kv.key.as_str(), &kv.value))
                .collect();

            assert!(attrs.contains_key("rpc.system"), "missing rpc.system");
            assert!(attrs.contains_key("rpc.method"), "missing rpc.method");

            assert_eq!(attrs["rpc.system"].as_str(), "jsonrpc");
            assert_eq!(
                attrs["rpc.method"].as_str(),
                AGENT_METHOD_NAMES.session_prompt
            );
        }
    }

    mod prompt_response_json {
        use crate::acp::protocol::{PromptResponse, StopReason};

        #[test]
        fn end_turn_serializes_to_acp_json() {
            let response = PromptResponse::new(StopReason::EndTurn);
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(
                json.get("stopReason").and_then(|v| v.as_str()),
                Some("end_turn"),
                "expected camelCase stopReason in ACP PromptResponse"
            );
        }

        #[test]
        fn cancelled_serializes_to_acp_json() {
            let response = PromptResponse::new(StopReason::Cancelled);
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(
                json.get("stopReason").and_then(|v| v.as_str()),
                Some("cancelled"),
                "expected camelCase stopReason for cancelled"
            );
        }

        #[test]
        fn max_tokens_serializes_to_acp_json() {
            let response = PromptResponse::new(StopReason::MaxTokens);
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(
                json.get("stopReason").and_then(|v| v.as_str()),
                Some("max_tokens")
            );
        }

        #[test]
        fn max_turn_requests_serializes_to_acp_json() {
            let response = PromptResponse::new(StopReason::MaxTurnRequests);
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(
                json.get("stopReason").and_then(|v| v.as_str()),
                Some("max_turn_requests")
            );
        }

        #[test]
        fn refusal_serializes_to_acp_json() {
            let response = PromptResponse::new(StopReason::Refusal);
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(
                json.get("stopReason").and_then(|v| v.as_str()),
                Some("refusal")
            );
        }
    }
}

#[cfg(test)]
mod session_update_tests {
    use super::*;
    use crate::agent::core::AgentMode;
    use crate::events::{AgentEventKind, DurableEvent, EventEnvelope, EventOrigin};
    use std::collections::HashMap;

    // ── Helpers ────────────────────────────────────────────────────────────

    fn envelope(seq: i64, kind: AgentEventKind) -> EventEnvelope {
        EventEnvelope::Durable(DurableEvent {
            event_id: format!("evt-{seq}"),
            stream_seq: seq,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        })
    }

    fn agent_event(seq: i64, kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        }
    }

    fn plan_ops() -> AcpSessionUpdateCapabilities {
        AcpSessionUpdateCapabilities {
            plan_operations: true,
            notices: true,
            compaction: true,
        }
    }

    fn todo_args(entries: &[(&str, &str)]) -> String {
        let todos: Vec<serde_json::Value> = entries
            .iter()
            .map(|(content, status)| {
                serde_json::json!({"content": content, "status": status, "priority": "medium"})
            })
            .collect();
        serde_json::json!({"todos": todos}).to_string()
    }

    fn todo_start(args: &str) -> EventEnvelope {
        envelope(
            1,
            AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "todowrite".to_string(),
                arguments: args.to_string(),
            },
        )
    }

    fn usage_end(seq: i64, context_tokens: u64, cumulative_cost: Option<f64>) -> EventEnvelope {
        envelope(
            seq,
            AgentEventKind::LlmRequestEnd {
                usage: None,
                tool_calls: 0,
                finish_reason: None,
                cost_usd: None,
                cumulative_cost_usd: cumulative_cost,
                context_tokens,
                metrics: Default::default(),
            },
        )
    }

    fn provider_changed(seq: i64, limit: Option<u64>) -> EventEnvelope {
        envelope(
            seq,
            AgentEventKind::ProviderChanged {
                provider: "local".to_string(),
                model: "m".to_string(),
                config_id: 1,
                context_limit: limit,
                provider_node_id: None,
            },
        )
    }

    // ── 1.2 Capability snapshot ────────────────────────────────────────────

    #[test]
    fn capability_snapshot_covers_every_combination() {
        let none = AcpSessionUpdateCapabilities::from_client_capabilities(
            &crate::acp::protocol::ClientCapabilities::new(),
        );
        assert_eq!(none, AcpSessionUpdateCapabilities::default());

        // Top-level plan capability.
        let plan_only = AcpSessionUpdateCapabilities::from_client_capabilities(
            &crate::acp::protocol::ClientCapabilities::new()
                .plan(crate::acp::protocol::PlanCapabilities::default()),
        );
        assert!(plan_only.plan_operations && !plan_only.notices && !plan_only.compaction);

        // Session-scoped notices and compaction.
        let session_scoped = AcpSessionUpdateCapabilities::from_client_capabilities(
            &crate::acp::protocol::ClientCapabilities::new().session(
                crate::acp::protocol::ClientSessionCapabilities::new()
                    .notices(crate::acp::protocol::NoticeCapabilities::new())
                    .compaction(crate::acp::protocol::CompactionCapabilities::new()),
            ),
        );
        assert!(!session_scoped.plan_operations);
        assert!(session_scoped.notices && session_scoped.compaction);
    }

    #[test]
    fn capability_snapshot_treats_omitted_and_null_as_unsupported() {
        let omitted = AcpSessionUpdateCapabilities::from_client_capabilities(
            &crate::acp::protocol::ClientCapabilities::new(),
        );
        assert_eq!(omitted, AcpSessionUpdateCapabilities::default());

        let null_session = AcpSessionUpdateCapabilities::from_client_capabilities(
            &crate::acp::protocol::ClientCapabilities::new().session(None),
        );
        assert!(!null_session.notices && !null_session.compaction);
    }

    #[test]
    fn capability_snapshot_matches_documented_client_capabilities_json() {
        // Mirrors the JSON advertised in the ACP documentation.
        let capabilities: crate::acp::protocol::ClientCapabilities =
            serde_json::from_value(serde_json::json!({
                "plan": {},
                "session": {"notices": {}, "compaction": {}}
            }))
            .expect("documented capabilities deserialize");

        let snapshot = AcpSessionUpdateCapabilities::from_client_capabilities(&capabilities);
        assert_eq!(snapshot, plan_ops());
    }

    #[test]
    fn capability_snapshot_is_isolated_between_connections() {
        let mut capable = AcpLiveEventTranslator::new();
        capable.set_capabilities(plan_ops());
        let mut legacy = AcpLiveEventTranslator::new();
        legacy.set_capabilities(AcpSessionUpdateCapabilities::default());

        let args = todo_args(&[("a", "pending")]);
        assert!(matches!(
            capable.translate_updates(&todo_start(&args)).as_slice(),
            [SessionUpdate::PlanUpdate(_)]
        ));
        assert!(matches!(
            legacy.translate_updates(&todo_start(&args)).as_slice(),
            [SessionUpdate::Plan(_)]
        ));
    }

    #[test]
    fn mixed_capability_clients_receive_only_supported_preview_variants() {
        let mut full = AcpLiveEventTranslator::new();
        full.set_capabilities(plan_ops());
        let mut compaction_only = AcpLiveEventTranslator::new();
        compaction_only.set_capabilities(AcpSessionUpdateCapabilities {
            plan_operations: false,
            notices: false,
            compaction: true,
        });
        let mut legacy = AcpLiveEventTranslator::new();

        let notice = envelope(
            1,
            AgentEventKind::HookNotice {
                event_name: "hook".to_string(),
                message: "m".to_string(),
                is_error: false,
            },
        );
        assert!(matches!(
            full.translate_updates(&notice).as_slice(),
            [SessionUpdate::Notice(_)]
        ));
        assert!(compaction_only.translate_updates(&notice).is_empty());
        assert!(legacy.translate_updates(&notice).is_empty());

        let plan = todo_start(&todo_args(&[("a", "pending")]));
        assert!(matches!(
            full.translate_updates(&plan).as_slice(),
            [SessionUpdate::PlanUpdate(_)]
        ));
        // Compaction-only clients fall back to the legacy plan replacement.
        assert!(matches!(
            compaction_only.translate_updates(&plan).as_slice(),
            [SessionUpdate::Plan(_)]
        ));

        let compaction = envelope(
            3,
            AgentEventKind::CompactionStart {
                token_estimate: 1,
                compaction_id: Some("c".to_string()),
            },
        );
        assert!(matches!(
            full.translate_updates(&compaction).as_slice(),
            [SessionUpdate::CompactionUpdate(_)]
        ));
        assert!(matches!(
            compaction_only.translate_updates(&compaction).as_slice(),
            [SessionUpdate::CompactionUpdate(_)]
        ));
        assert!(legacy.translate_updates(&compaction).is_empty());

        // Stable configuration updates reach every client regardless of
        // Preview support.
        let config = envelope(
            4,
            AgentEventKind::SessionConfigChanged {
                mode: AgentMode::Build,
                reasoning_effort: None,
            },
        );
        for translator in [&mut full, &mut compaction_only, &mut legacy] {
            assert!(matches!(
                translator.translate_updates(&config).as_slice(),
                [SessionUpdate::ConfigOptionUpdate(_)]
            ));
        }
    }

    // ── 1.3 Zero-to-many ordering ──────────────────────────────────────────

    #[test]
    fn multiple_updates_retain_order() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());
        translator.translate_updates(&provider_changed(1, Some(200_000)));

        // Terminal compaction plus a reduced usage snapshot in one event.
        let updates = translator.translate_updates(&envelope(
            2,
            AgentEventKind::CompactionEnd {
                summary: "short".to_string(),
                summary_len: 5,
                compaction_id: Some("c-1".to_string()),
                context_tokens: Some(1_000),
            },
        ));
        assert!(matches!(updates[0], SessionUpdate::CompactionUpdate(_)));
        assert!(matches!(updates[1], SessionUpdate::UsageUpdate(_)));
    }

    #[test]
    fn mode_change_orders_current_mode_before_config_replacements() {
        let mut translator = AcpLiveEventTranslator::new();
        let updates = translator.translate_updates(&envelope(
            1,
            AgentEventKind::SessionModeChanged {
                mode: AgentMode::Plan,
            },
        ));
        assert!(matches!(
            updates.as_slice(),
            [SessionUpdate::CurrentModeUpdate(_)]
        ));

        let updates = translator.translate_updates(&envelope(
            2,
            AgentEventKind::SessionConfigChanged {
                mode: AgentMode::Plan,
                reasoning_effort: Some("high".to_string()),
            },
        ));
        let [SessionUpdate::ConfigOptionUpdate(config)] = updates.as_slice() else {
            panic!("expected config option update");
        };
        let mode = config
            .config_options
            .iter()
            .find(|option| option.id.0.as_ref() == "mode")
            .expect("mode option");
        assert_eq!(
            serde_json::to_value(mode).unwrap()["currentValue"],
            serde_json::json!("plan")
        );
    }

    // ── 1.4 Serialization discriminators ───────────────────────────────────

    #[test]
    fn session_update_discriminators_match_acp_v1_wire_shape() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());

        let mode_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    1,
                    AgentEventKind::SessionModeChanged {
                        mode: AgentMode::Build,
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(mode_json["sessionUpdate"], "current_mode_update");
        assert_eq!(mode_json["currentModeId"], "build");

        let config_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    2,
                    AgentEventKind::SessionConfigChanged {
                        mode: AgentMode::Build,
                        reasoning_effort: None,
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(config_json["sessionUpdate"], "config_option_update");
        assert!(config_json["configOptions"].is_array());

        let info_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    3,
                    AgentEventKind::SessionMetadataUpdated {
                        title: Some(Some("Renamed".to_string())),
                        updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(info_json["sessionUpdate"], "session_info_update");
        assert_eq!(info_json["title"], "Renamed");
        assert_eq!(info_json["updatedAt"], "2026-01-01T00:00:00Z");

        translator.translate_updates(&provider_changed(4, Some(100)));
        let usage_json = serde_json::to_value(
            translator
                .translate_updates(&usage_end(5, 42, Some(0.5)))
                .remove(0),
        )
        .unwrap();
        assert_eq!(usage_json["sessionUpdate"], "usage_update");
        assert_eq!(usage_json["used"], 42);
        assert_eq!(usage_json["size"], 100);
        assert_eq!(usage_json["cost"]["amount"], 0.5);
        assert_eq!(usage_json["cost"]["currency"], "USD");

        let plan_json = serde_json::to_value(
            translator
                .translate_updates(&todo_start(&todo_args(&[("a", "pending")])))
                .remove(0),
        )
        .unwrap();
        assert_eq!(plan_json["sessionUpdate"], "plan_update");
        assert_eq!(plan_json["plan"]["type"], "items");
        assert_eq!(plan_json["plan"]["planId"], QUERYMT_TODO_PLAN_ID);

        // Remove the announced plan so a removal variant is produced.
        let removed_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    7,
                    AgentEventKind::ToolCallStart {
                        tool_call_id: "tc-2".to_string(),
                        tool_name: "todowrite".to_string(),
                        arguments: todo_args(&[]).to_string(),
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(removed_json["sessionUpdate"], "plan_removed");
        assert_eq!(removed_json["planId"], QUERYMT_TODO_PLAN_ID);

        let notice_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    8,
                    AgentEventKind::HookNotice {
                        event_name: "pre_compaction".to_string(),
                        message: "careful".to_string(),
                        is_error: false,
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(notice_json["sessionUpdate"], "notice");
        assert_eq!(notice_json["severity"], "info");
        assert_eq!(notice_json["title"], "pre_compaction");
        assert_eq!(notice_json["description"], "careful");

        let compaction_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    9,
                    AgentEventKind::CompactionStart {
                        token_estimate: 10,
                        compaction_id: Some("c-1".to_string()),
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(compaction_json["sessionUpdate"], "compaction_update");
        assert_eq!(compaction_json["compactionId"], "c-1");
        assert_eq!(compaction_json["status"], "in_progress");

        let chunk_json = serde_json::to_value(
            translator
                .translate_updates(&envelope(
                    10,
                    AgentEventKind::CompactionSummaryChunk {
                        compaction_id: "c-1".to_string(),
                        content: "summary text".to_string(),
                    },
                ))
                .remove(0),
        )
        .unwrap();
        assert_eq!(chunk_json["sessionUpdate"], "compaction_summary_chunk");
        assert_eq!(chunk_json["compactionId"], "c-1");
        assert_eq!(chunk_json["content"]["type"], "text");
    }

    // ── 2.4 Metadata patch semantics ───────────────────────────────────────

    #[test]
    fn metadata_patches_cover_title_timestamp_omission_and_clear() {
        let mut translator = AcpLiveEventTranslator::new();

        let title_only = translator.translate_updates(&envelope(
            1,
            AgentEventKind::SessionMetadataUpdated {
                title: Some(Some("T".to_string())),
                updated_at: None,
            },
        ));
        let json = serde_json::to_value(&title_only[0]).unwrap();
        assert_eq!(json["title"], "T");
        assert!(json.get("updatedAt").is_none());

        let timestamp_only = translator.translate_updates(&envelope(
            2,
            AgentEventKind::SessionMetadataUpdated {
                title: None,
                updated_at: Some("2026-02-03T04:05:06Z".to_string()),
            },
        ));
        let json = serde_json::to_value(&timestamp_only[0]).unwrap();
        assert!(json.get("title").is_none());
        assert_eq!(json["updatedAt"], "2026-02-03T04:05:06Z");

        let cleared = translator.translate_updates(&envelope(
            3,
            AgentEventKind::SessionMetadataUpdated {
                title: Some(None),
                updated_at: None,
            },
        ));
        let json = serde_json::to_value(&cleared[0]).unwrap();
        assert!(json["title"].is_null());
    }

    // ── 3.1/3.2 Usage meter ────────────────────────────────────────────────

    #[test]
    fn usage_uses_latest_limit_and_includes_cost_and_cached_tokens() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.translate_updates(&provider_changed(1, Some(100_000)));
        // Cached tokens are already included in context_tokens.
        let updates = translator.translate_updates(&usage_end(2, 12_345, Some(1.25)));
        let SessionUpdate::UsageUpdate(usage) = &updates[0] else {
            panic!("expected usage update");
        };
        assert_eq!(usage.used, 12_345);
        assert_eq!(usage.size, 100_000);
        assert_eq!(usage.cost.as_ref().unwrap().amount, 1.25);

        // A different known limit applies to the next snapshot. A request
        // without a fresh cost value keeps the last cumulative session cost.
        translator.translate_updates(&provider_changed(3, Some(50_000)));
        let updates = translator.translate_updates(&usage_end(4, 100, None));
        let SessionUpdate::UsageUpdate(usage) = &updates[0] else {
            panic!("expected usage update");
        };
        assert_eq!(usage.size, 50_000);
        assert_eq!(usage.cost.as_ref().unwrap().amount, 1.25);

        // With no cost observed anywhere, the cost field stays omitted.
        let mut fresh = AcpLiveEventTranslator::new();
        fresh.translate_updates(&provider_changed(1, Some(50_000)));
        let updates = fresh.translate_updates(&usage_end(2, 100, None));
        let SessionUpdate::UsageUpdate(usage) = &updates[0] else {
            panic!("expected usage update");
        };
        assert!(usage.cost.is_none());
    }

    #[test]
    fn usage_is_suppressed_when_limit_is_unknown() {
        let mut translator = AcpLiveEventTranslator::new();
        assert!(
            translator
                .translate_updates(&usage_end(1, 10, None))
                .is_empty()
        );

        translator.translate_updates(&provider_changed(2, None));
        assert!(
            translator
                .translate_updates(&usage_end(3, 10, None))
                .is_empty()
        );

        // Zero is not a meaningful effective limit.
        translator.translate_updates(&provider_changed(4, Some(0)));
        assert!(
            translator
                .translate_updates(&usage_end(5, 10, None))
                .is_empty()
        );
    }

    #[test]
    fn usage_state_is_cleared_on_session_close() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.translate_updates(&provider_changed(1, Some(1_000)));
        assert!(
            !translator
                .translate_updates(&usage_end(2, 10, None))
                .is_empty()
        );

        translator.forget_session("s-1");
        assert!(
            translator
                .translate_updates(&usage_end(3, 10, None))
                .is_empty()
        );
    }

    // ── 3.3 Compaction reduces context ─────────────────────────────────────

    #[test]
    fn compaction_completion_projects_reduced_usage() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());
        translator.translate_updates(&provider_changed(1, Some(100_000)));
        translator.translate_updates(&usage_end(2, 90_000, Some(2.0)));

        let updates = translator.translate_updates(&envelope(
            3,
            AgentEventKind::CompactionEnd {
                summary: "s".to_string(),
                summary_len: 1,
                compaction_id: Some("c-1".to_string()),
                context_tokens: Some(1_000),
            },
        ));
        let SessionUpdate::UsageUpdate(usage) = &updates[1] else {
            panic!("expected usage update after compaction");
        };
        assert_eq!(usage.used, 1_000);
        assert_eq!(usage.size, 100_000);
        assert_eq!(usage.cost.as_ref().unwrap().amount, 2.0);
    }

    // ── 4.1/4.2 Plan lifecycle ─────────────────────────────────────────────

    #[test]
    fn capable_client_plan_lifecycle_create_update_remove_recreate() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());

        let created = translator.translate_updates(&todo_start(&todo_args(&[("a", "pending")])));
        let [SessionUpdate::PlanUpdate(update)] = created.as_slice() else {
            panic!("expected plan update");
        };
        let PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected item-based plan");
        };
        assert_eq!(items.plan_id.0.as_ref(), QUERYMT_TODO_PLAN_ID);
        assert_eq!(items.entries.len(), 1);

        let updated = translator.translate_updates(&todo_start(&todo_args(&[
            ("a", "completed"),
            ("b", "in_progress"),
        ])));
        let [SessionUpdate::PlanUpdate(update)] = updated.as_slice() else {
            panic!("expected plan update");
        };
        let PlanUpdateContent::Items(items) = &update.plan else {
            panic!("expected item-based plan");
        };
        assert_eq!(items.entries.len(), 2);

        let removed = translator.translate_updates(&todo_start(&todo_args(&[])));
        assert!(matches!(
            removed.as_slice(),
            [SessionUpdate::PlanRemoved(_)]
        ));

        // Repeated empty snapshots do not emit duplicate removals.
        assert!(
            translator
                .translate_updates(&todo_start(&todo_args(&[])))
                .is_empty()
        );

        // Recreating the plan works after removal.
        let recreated = translator.translate_updates(&todo_start(&todo_args(&[("c", "pending")])));
        assert!(matches!(
            recreated.as_slice(),
            [SessionUpdate::PlanUpdate(_)]
        ));
    }

    #[test]
    fn legacy_client_keeps_full_replacement_plan_and_empty_plan() {
        let mut translator = AcpLiveEventTranslator::new();

        let created = translator.translate_updates(&todo_start(&todo_args(&[("a", "pending")])));
        let [SessionUpdate::Plan(plan)] = created.as_slice() else {
            panic!("expected legacy plan");
        };
        assert_eq!(plan.entries.len(), 1);

        let emptied = translator.translate_updates(&todo_start(&todo_args(&[])));
        let [SessionUpdate::Plan(plan)] = emptied.as_slice() else {
            panic!("expected legacy empty replacement");
        };
        assert!(plan.entries.is_empty());
    }

    #[test]
    fn malformed_todowrite_emits_nothing() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());
        assert!(
            translator
                .translate_updates(&todo_start("not json"))
                .is_empty()
        );
    }

    // ── 4.3 Notices ────────────────────────────────────────────────────────

    #[test]
    fn notices_are_gated_and_map_severity() {
        let mut legacy = AcpLiveEventTranslator::new();
        assert!(
            legacy
                .translate_updates(&envelope(
                    1,
                    AgentEventKind::HookNotice {
                        event_name: "hook".to_string(),
                        message: "m".to_string(),
                        is_error: true,
                    },
                ))
                .is_empty()
        );

        let mut capable = AcpLiveEventTranslator::new();
        capable.set_capabilities(plan_ops());
        let error = capable.translate_updates(&envelope(
            1,
            AgentEventKind::HookNotice {
                event_name: "hook".to_string(),
                message: "m".to_string(),
                is_error: true,
            },
        ));
        let SessionUpdate::Notice(notice) = &error[0] else {
            panic!("expected notice");
        };
        assert_eq!(notice.severity, NoticeSeverity::Error);
        assert_eq!(notice.description.as_deref(), Some("m"));

        let info = capable.translate_updates(&envelope(
            2,
            AgentEventKind::HookNotice {
                event_name: String::new(),
                message: "m".to_string(),
                is_error: false,
            },
        ));
        let SessionUpdate::Notice(notice) = &info[0] else {
            panic!("expected notice");
        };
        assert_eq!(notice.severity, NoticeSeverity::Info);
        assert!(!notice.title.is_empty());
    }

    // ── 5.3 Compaction lifecycle projection ────────────────────────────────

    #[test]
    fn compaction_updates_require_capability_and_valid_ordering() {
        let mut legacy = AcpLiveEventTranslator::new();
        assert!(
            legacy
                .translate_updates(&envelope(
                    1,
                    AgentEventKind::CompactionStart {
                        token_estimate: 1,
                        compaction_id: Some("c".to_string()),
                    },
                ))
                .is_empty()
        );
        assert!(
            legacy
                .translate_updates(&envelope(
                    2,
                    AgentEventKind::CompactionSummaryChunk {
                        compaction_id: "c".to_string(),
                        content: "x".to_string(),
                    },
                ))
                .is_empty()
        );

        let mut capable = AcpLiveEventTranslator::new();
        capable.set_capabilities(plan_ops());
        let started = capable.translate_updates(&envelope(
            1,
            AgentEventKind::CompactionStart {
                token_estimate: 1,
                compaction_id: Some("c".to_string()),
            },
        ));
        let SessionUpdate::CompactionUpdate(update) = &started[0] else {
            panic!("expected compaction update");
        };
        assert_eq!(update.status, CompactionStatus::InProgress);

        let completed = capable.translate_updates(&envelope(
            2,
            AgentEventKind::CompactionEnd {
                summary: "final".to_string(),
                summary_len: 5,
                compaction_id: Some("c".to_string()),
                context_tokens: None,
            },
        ));
        let SessionUpdate::CompactionUpdate(update) = &completed[0] else {
            panic!("expected compaction update");
        };
        assert_eq!(update.status, CompactionStatus::Completed);
        assert!(!matches!(update.summary, MaybeUndefined::Value(_)));
    }

    #[test]
    fn failed_and_cancelled_compactions_carry_only_valid_fields() {
        let mut translator = AcpLiveEventTranslator::new();
        translator.set_capabilities(plan_ops());

        let failed = translator.translate_updates(&envelope(
            1,
            AgentEventKind::CompactionFailed {
                compaction_id: "c".to_string(),
                reason: "boom".to_string(),
                cancelled: false,
            },
        ));
        let SessionUpdate::CompactionUpdate(update) = &failed[0] else {
            panic!("expected compaction update");
        };
        assert_eq!(update.status, CompactionStatus::Failed);
        assert!(matches!(&update.error, MaybeUndefined::Value(v) if v == "boom"));

        let cancelled = translator.translate_updates(&envelope(
            2,
            AgentEventKind::CompactionFailed {
                compaction_id: "c".to_string(),
                reason: "stop".to_string(),
                cancelled: true,
            },
        ));
        let SessionUpdate::CompactionUpdate(update) = &cancelled[0] else {
            panic!("expected compaction update");
        };
        assert_eq!(update.status, CompactionStatus::Cancelled);
        assert!(!matches!(update.error, MaybeUndefined::Value(_)));
    }

    // ── 5.1 Event round trips (new and legacy) ─────────────────────────────

    #[test]
    fn compaction_events_round_trip_new_and_legacy_records() {
        let new_end = serde_json::json!({
            "type": "compaction_end",
            "data": {
                "summary": "s",
                "summary_len": 1,
                "compaction_id": "c-1",
                "context_tokens": 123
            }
        });
        let parsed: AgentEventKind = serde_json::from_value(new_end).unwrap();
        assert!(matches!(
            parsed,
            AgentEventKind::CompactionEnd {
                context_tokens: Some(123),
                ..
            }
        ));

        // Legacy record: no ID or token count.
        let legacy_end = serde_json::json!({
            "type": "compaction_end",
            "data": {"summary": "s", "summary_len": 1}
        });
        let parsed: AgentEventKind = serde_json::from_value(legacy_end).unwrap();
        assert!(matches!(
            parsed,
            AgentEventKind::CompactionEnd {
                compaction_id: None,
                context_tokens: None,
                ..
            }
        ));

        let legacy_start = serde_json::json!({
            "type": "compaction_start",
            "data": {"token_estimate": 10}
        });
        let parsed: AgentEventKind = serde_json::from_value(legacy_start).unwrap();
        assert!(matches!(
            parsed,
            AgentEventKind::CompactionStart {
                compaction_id: None,
                ..
            }
        ));
    }

    // ── 6.1/6.2 Materialized replay ────────────────────────────────────────

    fn replay_updates(
        events: Vec<AgentEvent>,
        capabilities: AcpSessionUpdateCapabilities,
    ) -> Vec<SessionUpdate> {
        replay_agent_events_materialized("s-1", events, &HashMap::new(), &capabilities)
            .into_iter()
            .map(|notification| notification.update)
            .collect()
    }

    #[test]
    fn replay_materializes_only_current_state_with_stable_ids() {
        let events = vec![
            agent_event(
                1,
                AgentEventKind::SessionModeChanged {
                    mode: AgentMode::Build,
                },
            ),
            agent_event(
                2,
                AgentEventKind::SessionConfigChanged {
                    mode: AgentMode::Plan,
                    reasoning_effort: Some("high".to_string()),
                },
            ),
            agent_event(
                3,
                AgentEventKind::SessionMetadataUpdated {
                    title: Some(Some("First".to_string())),
                    updated_at: None,
                },
            ),
            agent_event(
                4,
                AgentEventKind::SessionMetadataUpdated {
                    title: None,
                    updated_at: Some("2026-05-05T00:00:00Z".to_string()),
                },
            ),
            agent_event(
                5,
                AgentEventKind::ProviderChanged {
                    provider: "local".to_string(),
                    model: "m".to_string(),
                    config_id: 1,
                    context_limit: Some(100),
                    provider_node_id: None,
                },
            ),
            agent_event(
                6,
                AgentEventKind::LlmRequestEnd {
                    usage: None,
                    tool_calls: 0,
                    finish_reason: None,
                    cost_usd: None,
                    cumulative_cost_usd: Some(3.0),
                    context_tokens: 50,
                    metrics: Default::default(),
                },
            ),
            agent_event(
                7,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "tc".to_string(),
                    tool_name: "todowrite".to_string(),
                    arguments: todo_args(&[("a", "pending")]),
                },
            ),
            agent_event(
                8,
                AgentEventKind::CompactionStart {
                    token_estimate: 1,
                    compaction_id: Some("c-1".to_string()),
                },
            ),
            agent_event(
                9,
                AgentEventKind::CompactionSummaryChunk {
                    compaction_id: "c-1".to_string(),
                    content: "chunk".to_string(),
                },
            ),
            agent_event(
                10,
                AgentEventKind::CompactionEnd {
                    summary: "final summary".to_string(),
                    summary_len: 13,
                    compaction_id: Some("c-1".to_string()),
                    context_tokens: Some(5),
                },
            ),
            agent_event(
                11,
                AgentEventKind::HookNotice {
                    event_name: "hook".to_string(),
                    message: "noisy".to_string(),
                    is_error: false,
                },
            ),
        ];

        let updates = replay_updates(events.clone(), plan_ops());

        let mode_updates: Vec<&SessionUpdate> = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::CurrentModeUpdate(_)))
            .collect();
        assert_eq!(mode_updates.len(), 1, "one materialized mode update");
        let SessionUpdate::CurrentModeUpdate(update) = mode_updates[0] else {
            unreachable!()
        };
        assert_eq!(update.current_mode_id.0.as_ref(), "plan");

        let config_count = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::ConfigOptionUpdate(_)))
            .count();
        assert_eq!(config_count, 1);

        let info: Vec<&SessionUpdate> = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::SessionInfoUpdate(_)))
            .collect();
        assert_eq!(info.len(), 1);
        let SessionUpdate::SessionInfoUpdate(update) = info[0] else {
            unreachable!()
        };
        assert!(matches!(&update.title, MaybeUndefined::Value(v) if v == "First"));
        assert!(
            matches!(&update.updated_at, MaybeUndefined::Value(v) if v == "2026-05-05T00:00:00Z")
        );

        let usage: Vec<&SessionUpdate> = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::UsageUpdate(_)))
            .collect();
        assert_eq!(usage.len(), 1, "only the latest valid usage snapshot");
        let SessionUpdate::UsageUpdate(usage) = usage[0] else {
            unreachable!()
        };
        assert_eq!(usage.used, 5);
        assert_eq!(usage.cost.as_ref().unwrap().amount, 3.0);

        let plans: Vec<&SessionUpdate> = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::PlanUpdate(_)))
            .collect();
        assert_eq!(plans.len(), 1);
        let SessionUpdate::PlanUpdate(plan) = plans[0] else {
            unreachable!()
        };
        let PlanUpdateContent::Items(items) = &plan.plan else {
            unreachable!()
        };
        assert_eq!(items.plan_id.0.as_ref(), QUERYMT_TODO_PLAN_ID);

        let compactions: Vec<&SessionUpdate> = updates
            .iter()
            .filter(|u| matches!(u, SessionUpdate::CompactionUpdate(_)))
            .collect();
        assert_eq!(compactions.len(), 1, "one terminal compaction update");
        let SessionUpdate::CompactionUpdate(compaction) = compactions[0] else {
            unreachable!()
        };
        assert_eq!(compaction.compaction_id.0.as_ref(), "c-1");
        assert_eq!(compaction.status, CompactionStatus::Completed);
        assert!(matches!(
            &compaction.summary,
            MaybeUndefined::Value(blocks) if !blocks.is_empty()
        ));

        // No live-only variants in replay.
        assert!(updates.iter().all(|u| !matches!(
            u,
            SessionUpdate::Notice(_) | SessionUpdate::CompactionSummaryChunk(_)
        )));
    }

    #[test]
    fn replay_excludes_unsupported_preview_variants() {
        let events = vec![
            agent_event(
                1,
                AgentEventKind::SessionConfigChanged {
                    mode: AgentMode::Build,
                    reasoning_effort: None,
                },
            ),
            agent_event(
                2,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "tc".to_string(),
                    tool_name: "todowrite".to_string(),
                    arguments: todo_args(&[("a", "pending")]),
                },
            ),
            agent_event(
                3,
                AgentEventKind::CompactionEnd {
                    summary: "s".to_string(),
                    summary_len: 1,
                    compaction_id: Some("c-1".to_string()),
                    context_tokens: None,
                },
            ),
            agent_event(
                4,
                AgentEventKind::HookNotice {
                    event_name: "h".to_string(),
                    message: "m".to_string(),
                    is_error: false,
                },
            ),
        ];

        let updates = replay_updates(events, AcpSessionUpdateCapabilities::default());
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::ConfigOptionUpdate(_)))
        );
        assert!(updates.iter().any(|u| matches!(u, SessionUpdate::Plan(_))));
        assert!(updates.iter().all(|u| !matches!(
            u,
            SessionUpdate::PlanUpdate(_)
                | SessionUpdate::PlanRemoved(_)
                | SessionUpdate::CompactionUpdate(_)
                | SessionUpdate::CompactionSummaryChunk(_)
                | SessionUpdate::Notice(_)
        )));
    }

    #[test]
    fn replay_pairs_legacy_compaction_terminal_with_stable_id() {
        let events = vec![
            agent_event(
                1,
                AgentEventKind::CompactionStart {
                    token_estimate: 10,
                    compaction_id: None,
                },
            ),
            agent_event(
                2,
                AgentEventKind::CompactionEnd {
                    summary: "legacy".to_string(),
                    summary_len: 6,
                    compaction_id: None,
                    context_tokens: None,
                },
            ),
        ];

        let updates = replay_updates(events, plan_ops());
        let SessionUpdate::CompactionUpdate(compaction) = &updates[0] else {
            panic!("expected compaction update");
        };
        assert!(
            compaction
                .compaction_id
                .0
                .starts_with("querymt-compaction-legacy-")
        );
        assert_eq!(compaction.status, CompactionStatus::Completed);
    }

    #[test]
    fn replay_drops_obsolete_in_progress_compaction() {
        let events = vec![agent_event(
            1,
            AgentEventKind::CompactionStart {
                token_estimate: 1,
                compaction_id: Some("in-flight".to_string()),
            },
        )];
        let updates = replay_updates(events, plan_ops());
        assert!(
            updates
                .iter()
                .all(|u| !matches!(u, SessionUpdate::CompactionUpdate(_)))
        );
    }
}
