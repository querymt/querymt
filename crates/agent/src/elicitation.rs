//! MCP elicitation handler and types
//!
//! This module provides the unified elicitation system that handles both:
//! - MCP server elicitation requests (via ClientHandler trait)
//! - Built-in QuestionTool requests (converted to MCP format)
//!
//! All elicitation requests flow through the same event system and pending map.

use rmcp::RoleClient;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientInfo, CreateElicitationRequestParams, CreateElicitationResult,
    Implementation,
};
use rmcp::service::{NotificationContext, RequestContext};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

/// Action taken in response to an elicitation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl From<ElicitationAction> for rmcp::model::ElicitationAction {
    fn from(action: ElicitationAction) -> Self {
        match action {
            ElicitationAction::Accept => rmcp::model::ElicitationAction::Accept,
            ElicitationAction::Decline => rmcp::model::ElicitationAction::Decline,
            ElicitationAction::Cancel => rmcp::model::ElicitationAction::Cancel,
        }
    }
}

impl From<rmcp::model::ElicitationAction> for ElicitationAction {
    fn from(action: rmcp::model::ElicitationAction) -> Self {
        match action {
            rmcp::model::ElicitationAction::Accept => ElicitationAction::Accept,
            rmcp::model::ElicitationAction::Decline => ElicitationAction::Decline,
            rmcp::model::ElicitationAction::Cancel => ElicitationAction::Cancel,
        }
    }
}

/// Response to an elicitation request from the UI/ACP client
#[derive(Debug, Clone)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingElicitationForm {
    pub message: String,
    pub requested_schema: serde_json::Value,
    pub source: String,
}

impl Default for PendingElicitationForm {
    fn default() -> Self {
        Self {
            message: String::new(),
            requested_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            source: String::new(),
        }
    }
}

pub struct PendingElicitationRegistration {
    pub session_id: String,
    pub form: PendingElicitationForm,
    /// Process-lifetime bearer authority assigned by the owning ACP connection.
    pub owner_authority: Option<String>,
    /// Active run that owns this waiter. MCP-originated waiters may not have one.
    pub run_id: Option<String>,
    /// Tool invocation within the run, used for exact cleanup after parallel calls.
    pub tool_call_id: Option<String>,
}

pub struct PendingElicitation {
    pub session_id: String,
    pub form: PendingElicitationForm,
    pub owner_authority: Option<String>,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    /// Incremented whenever the question is delivered to a replacement connection.
    pub delivery_generation: u64,
    /// Connection allowed to resolve the current delivery generation.
    pub delivery_connection: Option<String>,
    pub sender: oneshot::Sender<ElicitationResponse>,
}

#[cfg(test)]
impl PendingElicitation {
    pub(crate) fn for_test(
        session_id: impl Into<String>,
        sender: oneshot::Sender<ElicitationResponse>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            form: PendingElicitationForm::default(),
            owner_authority: None,
            run_id: None,
            tool_call_id: None,
            delivery_generation: 0,
            delivery_connection: None,
            sender,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct PendingElicitationSnapshot {
    pub elicitation_id: String,
    pub session_id: String,
    pub form: PendingElicitationForm,
    pub owner_authority: Option<String>,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub delivery_generation: u64,
    pub delivery_connection: Option<String>,
}

/// Type alias for the pending elicitation map (elicitation_id -> session-aware response sender)
pub type PendingElicitationMap = Arc<Mutex<HashMap<String, PendingElicitation>>>;

struct PendingElicitationCleanup {
    pending_map: PendingElicitationMap,
    elicitation_id: String,
}

impl PendingElicitationCleanup {
    fn new(pending_map: PendingElicitationMap, elicitation_id: String) -> Self {
        Self {
            pending_map,
            elicitation_id,
        }
    }
}

impl Drop for PendingElicitationCleanup {
    fn drop(&mut self) {
        let pending_map = self.pending_map.clone();
        let elicitation_id = self.elicitation_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                pending_map.lock().await.remove(&elicitation_id);
            });
        }
    }
}

/// Removes and returns a pending elicitation sender by ID.
///
/// Searches the primary agent first, then all registered delegate agents.
/// This allows UI/ACP responders to resolve delegate-originated elicitations
/// while holding only a reference to the primary agent.
pub async fn take_pending_elicitation_sender(
    agent: &crate::agent::LocalAgentHandle,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    if let Some(sender) = take_from_pending_map(&agent.pending_elicitations(), elicitation_id).await
    {
        return Some(sender);
    }

    let registry = agent.agent_registry();
    let mut seen_agents = HashSet::new();

    for info in registry.list_agents() {
        let Some(handle) = registry.get_handle(&info.id) else {
            continue;
        };

        let Some(delegate) = handle
            .as_any()
            .downcast_ref::<crate::agent::LocalAgentHandle>()
        else {
            continue;
        };

        let ptr = delegate as *const _ as usize;
        if !seen_agents.insert(ptr) {
            continue;
        }

        if let Some(sender) =
            take_from_pending_map(&delegate.pending_elicitations(), elicitation_id).await
        {
            return Some(sender);
        }
    }

    None
}

/// Removes and returns a pending elicitation sender for a specific session.
///
/// Profile-bound sessions execute in their profile runtime, so that runtime and
/// its delegates must be searched before falling back to the outer agent.
pub async fn take_pending_elicitation_sender_for_session(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    take_pending_elicitation_sender_for_session_with_profiles(
        agent,
        agent.profiles().as_ref(),
        session_id,
        elicitation_id,
    )
    .await
}

/// Session-aware variant for transports that own a profile manager separately
/// from the root agent handle.
pub async fn take_pending_elicitation_sender_for_session_with_profiles(
    agent: &crate::agent::LocalAgentHandle,
    profiles: Option<&crate::api::ProfileRuntimeHandle>,
    session_id: &str,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    let agent_profiles = agent.profiles();
    let profiles = profiles.or(agent_profiles.as_ref());

    if let Some(profiles) = profiles
        && let Some(binding) = profiles.session_binding(session_id).await
    {
        match profiles.runtime_for_profile(&binding.profile_id).await {
            Ok(runtime) => {
                let profile_agent = runtime.agent().handle();
                if let Some(sender) =
                    take_pending_elicitation_sender(profile_agent.as_ref(), elicitation_id).await
                {
                    return Some(sender);
                }
            }
            Err(err) => {
                log::warn!(
                    "Failed to load profile runtime for elicitation response: session_id={} profile_id={} elicitation_id={} error={}",
                    session_id,
                    binding.profile_id,
                    elicitation_id,
                    err
                );
            }
        }
    }

    take_pending_elicitation_sender(agent, elicitation_id).await
}

pub async fn insert_pending_elicitation(
    pending_map: &PendingElicitationMap,
    elicitation_id: String,
    session_id: String,
    sender: oneshot::Sender<ElicitationResponse>,
) {
    register_pending_elicitation(
        pending_map,
        elicitation_id,
        PendingElicitationRegistration {
            session_id,
            form: PendingElicitationForm::default(),
            owner_authority: None,
            run_id: None,
            tool_call_id: None,
        },
        sender,
    )
    .await;
}

pub async fn register_pending_elicitation(
    pending_map: &PendingElicitationMap,
    elicitation_id: String,
    registration: PendingElicitationRegistration,
    sender: oneshot::Sender<ElicitationResponse>,
) {
    let mut pending = pending_map.lock().await;
    pending.insert(
        elicitation_id,
        PendingElicitation {
            session_id: registration.session_id,
            form: registration.form,
            owner_authority: registration.owner_authority,
            run_id: registration.run_id,
            tool_call_id: registration.tool_call_id,
            delivery_generation: 0,
            delivery_connection: None,
            sender,
        },
    );
}

pub async fn snapshot_pending_elicitations(
    pending_map: &PendingElicitationMap,
) -> Vec<PendingElicitationSnapshot> {
    let pending = pending_map.lock().await;
    let mut snapshot = pending
        .iter()
        .map(|(elicitation_id, entry)| PendingElicitationSnapshot {
            elicitation_id: elicitation_id.clone(),
            session_id: entry.session_id.clone(),
            form: entry.form.clone(),
            owner_authority: entry.owner_authority.clone(),
            run_id: entry.run_id.clone(),
            tool_call_id: entry.tool_call_id.clone(),
            delivery_generation: entry.delivery_generation,
            delivery_connection: entry.delivery_connection.clone(),
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    snapshot
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedElicitationDelivery {
    pub elicitation_id: String,
    pub session_id: String,
    pub form: PendingElicitationForm,
    pub delivery_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationResolution {
    Resolved,
    StaleDelivery,
    NotFound,
}

fn push_agent_pending_maps(
    agent: &crate::agent::LocalAgentHandle,
    maps: &mut Vec<PendingElicitationMap>,
    seen: &mut HashSet<usize>,
) {
    let map = agent.pending_elicitations();
    if seen.insert(Arc::as_ptr(&map) as usize) {
        maps.push(map);
    }
    let registry = agent.agent_registry();
    for info in registry.list_agents() {
        let Some(handle) = registry.get_handle(&info.id) else {
            continue;
        };
        let Some(delegate) = handle
            .as_any()
            .downcast_ref::<crate::agent::LocalAgentHandle>()
        else {
            continue;
        };
        let map = delegate.pending_elicitations();
        if seen.insert(Arc::as_ptr(&map) as usize) {
            maps.push(map);
        }
    }
}

async fn all_pending_maps(agent: &crate::agent::LocalAgentHandle) -> Vec<PendingElicitationMap> {
    let mut maps = Vec::new();
    let mut seen = HashSet::new();
    if let Some(profiles) = agent.profiles().as_ref() {
        for runtime in profiles.materialized_runtimes().await {
            push_agent_pending_maps(runtime.agent().handle().as_ref(), &mut maps, &mut seen);
        }
    }
    push_agent_pending_maps(agent, &mut maps, &mut seen);
    maps
}

pub async fn assign_pending_elicitation_authority(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    authority_verifier: &str,
) -> bool {
    let mut assigned = false;
    for map in all_pending_maps(agent).await {
        let mut entries = map.lock().await;
        for entry in entries
            .values_mut()
            .filter(|entry| entry.session_id == session_id)
        {
            if entry.owner_authority.is_none() {
                entry.owner_authority = Some(authority_verifier.to_string());
            }
            assigned = true;
        }
    }
    assigned
}

pub async fn pending_sessions_for_authority(
    agent: &crate::agent::LocalAgentHandle,
    authority_verifier: &str,
) -> Vec<String> {
    let mut session_ids = Vec::new();
    for map in all_pending_maps(agent).await {
        let entries = map.lock().await;
        session_ids.extend(
            entries
                .values()
                .filter(|entry| entry.owner_authority.as_deref() == Some(authority_verifier))
                .map(|entry| entry.session_id.clone()),
        );
    }
    session_ids.sort();
    session_ids.dedup();
    session_ids
}

pub async fn claim_pending_elicitation_deliveries(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: Option<&str>,
    authority_verifier: &str,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    let mut claims = Vec::new();
    for map in all_pending_maps(agent).await {
        let mut entries = map.lock().await;
        for (entry_id, entry) in entries.iter_mut().filter(|(entry_id, entry)| {
            entry.session_id == session_id
                && elicitation_id.is_none_or(|expected| entry_id.as_str() == expected)
                && entry.owner_authority.as_deref() == Some(authority_verifier)
        }) {
            entry.delivery_generation = entry.delivery_generation.saturating_add(1);
            entry.delivery_connection = Some(connection_id.to_string());
            claims.push(ClaimedElicitationDelivery {
                elicitation_id: entry_id.clone(),
                session_id: entry.session_id.clone(),
                form: entry.form.clone(),
                delivery_generation: entry.delivery_generation,
            });
        }
    }
    claims.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    claims
}

pub async fn claim_live_elicitation_delivery(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: &str,
    connection_id: &str,
) -> Option<ClaimedElicitationDelivery> {
    for map in all_pending_maps(agent).await {
        let mut entries = map.lock().await;
        let Some(entry) = entries.get_mut(elicitation_id) else {
            continue;
        };
        if entry.session_id != session_id {
            continue;
        }
        entry.delivery_generation = entry.delivery_generation.saturating_add(1);
        entry.delivery_connection = Some(connection_id.to_string());
        return Some(ClaimedElicitationDelivery {
            elicitation_id: elicitation_id.to_string(),
            session_id: entry.session_id.clone(),
            form: entry.form.clone(),
            delivery_generation: entry.delivery_generation,
        });
    }
    None
}

pub async fn claimed_elicitation_deliveries(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    let mut claims = Vec::new();
    for map in all_pending_maps(agent).await {
        let entries = map.lock().await;
        claims.extend(
            entries
                .iter()
                .filter(|(_, entry)| {
                    entry.session_id == session_id
                        && entry.delivery_connection.as_deref() == Some(connection_id)
                })
                .map(|(elicitation_id, entry)| ClaimedElicitationDelivery {
                    elicitation_id: elicitation_id.clone(),
                    session_id: entry.session_id.clone(),
                    form: entry.form.clone(),
                    delivery_generation: entry.delivery_generation,
                }),
        );
    }
    claims.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    claims
}

pub async fn resolve_elicitation_from_connection(
    agent: &crate::agent::LocalAgentHandle,
    session_id: Option<&str>,
    elicitation_id: &str,
    connection_id: &str,
    response: ElicitationResponse,
) -> Result<ElicitationResolution, agent_client_protocol::Error> {
    for map in all_pending_maps(agent).await {
        let delivery = {
            let entries = map.lock().await;
            let Some(entry) = entries.get(elicitation_id) else {
                continue;
            };
            if session_id.is_some_and(|session_id| entry.session_id != session_id) {
                continue;
            }
            if entry.delivery_generation == 0 {
                if entry.owner_authority.is_some() {
                    return Ok(ElicitationResolution::StaleDelivery);
                }
                (entry.session_id.clone(), 0)
            } else if entry.delivery_connection.as_deref() == Some(connection_id) {
                (entry.session_id.clone(), entry.delivery_generation)
            } else {
                return Ok(ElicitationResolution::StaleDelivery);
            }
        };
        if delivery.1 == 0 {
            let mut entries = map.lock().await;
            let Some(entry) = entries.get(elicitation_id) else {
                return Ok(ElicitationResolution::NotFound);
            };
            validate_elicitation_response(entry, &response)?;
            let entry = entries.remove(elicitation_id).expect("entry checked above");
            drop(entries);
            let _ = entry.sender.send(response);
            return Ok(ElicitationResolution::Resolved);
        }
        return resolve_claimed_elicitation(
            agent,
            &delivery.0,
            elicitation_id,
            connection_id,
            delivery.1,
            response,
        )
        .await;
    }
    Ok(ElicitationResolution::NotFound)
}

fn validate_elicitation_response(
    entry: &PendingElicitation,
    response: &ElicitationResponse,
) -> Result<(), agent_client_protocol::Error> {
    if response.action != ElicitationAction::Accept {
        return Ok(());
    }
    let content = response.content.as_ref().ok_or_else(|| {
        agent_client_protocol::Error::invalid_params()
            .data("Accepted elicitation response requires content")
    })?;
    let validator = jsonschema::validator_for(&entry.form.requested_schema).map_err(|err| {
        agent_client_protocol::Error::invalid_params()
            .data(format!("Invalid elicitation schema: {err}"))
    })?;
    validator.validate(content).map_err(|err| {
        agent_client_protocol::Error::invalid_params()
            .data(format!("Invalid elicitation response content: {err}"))
    })
}

pub async fn resolve_claimed_elicitation(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: &str,
    connection_id: &str,
    delivery_generation: u64,
    response: ElicitationResponse,
) -> Result<ElicitationResolution, agent_client_protocol::Error> {
    for map in all_pending_maps(agent).await {
        let mut entries = map.lock().await;
        let Some(entry) = entries.get(elicitation_id) else {
            continue;
        };
        if entry.session_id != session_id {
            continue;
        }
        if entry.delivery_generation != delivery_generation
            || entry.delivery_connection.as_deref() != Some(connection_id)
        {
            return Ok(ElicitationResolution::StaleDelivery);
        }
        validate_elicitation_response(entry, &response)?;
        let entry = entries
            .remove(elicitation_id)
            .expect("pending entry exists while registry lock is held");
        drop(entries);
        let _ = entry.sender.send(response);
        return Ok(ElicitationResolution::Resolved);
    }
    Ok(ElicitationResolution::NotFound)
}

pub async fn has_pending_elicitation_for_session(
    pending_map: &PendingElicitationMap,
    session_id: &str,
) -> bool {
    let pending = pending_map.lock().await;
    pending.values().any(|entry| entry.session_id == session_id)
}

/// Cancel and remove every pending elicitation owned by a session.
///
/// Senders are removed while holding the map lock, then notified after the lock
/// is released so cancellation cannot deadlock with response handling.
pub async fn cancel_pending_elicitations_for_session(
    pending_map: &PendingElicitationMap,
    session_id: &str,
) -> usize {
    let senders =
        remove_pending_elicitations(pending_map, |entry| entry.session_id == session_id).await;

    let count = senders.len();
    for sender in senders {
        let _ = sender.send(ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
        });
    }
    count
}

/// Remove unresolved waiters after their owning tool invocation exits.
pub async fn remove_pending_elicitations_for_tool(
    pending_map: &PendingElicitationMap,
    session_id: &str,
    run_id: &str,
    tool_call_id: &str,
) -> usize {
    remove_pending_elicitations(pending_map, |entry| {
        entry.session_id == session_id
            && entry.run_id.as_deref() == Some(run_id)
            && entry.tool_call_id.as_deref() == Some(tool_call_id)
    })
    .await
    .len()
}

async fn remove_pending_elicitations(
    pending_map: &PendingElicitationMap,
    predicate: impl Fn(&PendingElicitation) -> bool,
) -> Vec<oneshot::Sender<ElicitationResponse>> {
    let mut pending = pending_map.lock().await;
    let ids = pending
        .iter()
        .filter(|(_, entry)| predicate(entry))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| pending.remove(&id).map(|entry| entry.sender))
        .collect()
}

async fn take_from_pending_map(
    pending_map: &PendingElicitationMap,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    let mut pending = pending_map.lock().await;
    pending.remove(elicitation_id).map(|entry| entry.sender)
}

/// MCP client handler — the single `ClientHandler` impl used for all MCP server
/// connections established by the agent.
///
/// Responsibilities:
/// - **Elicitation**: routes `create_elicitation` server requests through the
///   agent event system so the UI/ACP client can respond interactively.
/// - **Tool-list refresh**: on `tools/list_changed` notifications, re-fetches
///   the updated tool list from the server and atomically updates the session's
///   [`McpToolState`][crate::agent::core::McpToolState].
pub struct McpClientHandler {
    pending: PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    server_name: String,
    session_id: String,
    client_impl: Implementation,
    tool_state: Arc<crate::agent::core::McpToolState>,
}

impl McpClientHandler {
    pub fn new(
        pending: PendingElicitationMap,
        event_sink: Arc<crate::event_sink::EventSink>,
        server_name: String,
        session_id: String,
        client_impl: Implementation,
        tool_state: Arc<crate::agent::core::McpToolState>,
    ) -> Self {
        Self {
            pending,
            event_sink,
            server_name,
            session_id,
            client_impl,
            tool_state,
        }
    }
}

impl ClientHandler for McpClientHandler {
    #[allow(clippy::manual_async_fn)]
    fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let elicitation_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();

            // Extract message and schema from the enum variant.
            let (message, schema_json) = match &request {
                CreateElicitationRequestParams::FormElicitationParams {
                    message,
                    requested_schema,
                    ..
                } => (
                    message.clone(),
                    serde_json::to_value(requested_schema).unwrap_or(serde_json::Value::Null),
                ),
                CreateElicitationRequestParams::UrlElicitationParams { message, url, .. } => {
                    (message.clone(), serde_json::json!({ "url": url }))
                }
            };
            let source = format!("mcp:{}", self.server_name);

            register_pending_elicitation(
                &self.pending,
                elicitation_id.clone(),
                PendingElicitationRegistration {
                    session_id: self.session_id.clone(),
                    form: PendingElicitationForm {
                        message: message.clone(),
                        requested_schema: schema_json.clone(),
                        source: source.clone(),
                    },
                    owner_authority: None,
                    run_id: None,
                    tool_call_id: None,
                },
                tx,
            )
            .await;
            let _cleanup =
                PendingElicitationCleanup::new(self.pending.clone(), elicitation_id.clone());

            // Durable: elicitation must be visible in UI replay.
            if let Err(err) = self
                .event_sink
                .emit_durable(
                    &self.session_id,
                    crate::events::AgentEventKind::ElicitationRequested {
                        elicitation_id: elicitation_id.clone(),
                        session_id: self.session_id.clone(),
                        message,
                        requested_schema: schema_json,
                        source,
                    },
                )
                .await
            {
                log::warn!("failed to emit ElicitationRequested: {}", err);
            }

            // Wait for response from UI/ACP
            match rx.await {
                Ok(response) => Ok(CreateElicitationResult {
                    action: response.action.into(),
                    content: response.content,
                    meta: None,
                }),
                Err(_) => Ok(CreateElicitationResult {
                    action: rmcp::model::ElicitationAction::Cancel,
                    content: None,
                    meta: None,
                }),
            }
        }
    }

    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(ClientCapabilities::default(), self.client_impl.clone())
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) -> () {
        use querymt::mcp::adapter::McpToolAdapter;
        use querymt::tool_decorator::CallFunctionTool;

        let peer = context.peer;
        match peer.list_all_tools().await {
            Ok(new_tool_list) => {
                let mut new_tools = std::collections::HashMap::new();
                let mut new_defs = Vec::new();

                // Retain tools belonging to other servers from the current snapshot.
                {
                    let current = self.tool_state.load();
                    for (name, adapter) in current.tools.iter() {
                        if adapter.server_name() != self.server_name {
                            new_tools.insert(name.clone(), adapter.clone());
                        }
                    }
                    for def in current.tool_defs.iter() {
                        if let Some(adapter) = current.tools.get(&def.function.name)
                            && adapter.server_name() != self.server_name
                        {
                            new_defs.push(def.clone());
                        }
                    }
                }

                // Add the refreshed tools from this server.
                for tool in new_tool_list {
                    match McpToolAdapter::try_new(tool, peer.clone(), self.server_name.clone()) {
                        Ok(adapter) => {
                            let name = adapter.descriptor().function.name.clone();
                            if new_tools.contains_key(&name) {
                                log::warn!(
                                    "Duplicate MCP tool '{}' after refresh of '{}', keeping first",
                                    name,
                                    self.server_name,
                                );
                                continue;
                            }
                            new_defs.push(adapter.descriptor());
                            new_tools.insert(name, Arc::new(adapter));
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to adapt refreshed tool from '{}': {}",
                                self.server_name,
                                e
                            );
                        }
                    }
                }

                // Atomically swap the entire snapshot (tools + defs + cleared hash).
                self.tool_state.store(crate::agent::core::McpToolSnapshot {
                    tools: new_tools,
                    tool_defs: new_defs,
                    tools_hash: None,
                });

                log::info!(
                    "session={} server='{}': MCP tool list refreshed",
                    self.session_id,
                    self.server_name,
                );
            }
            Err(e) => {
                log::warn!(
                    "session={} server='{}': failed to refresh tool list: {}",
                    self.session_id,
                    self.server_name,
                    e,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::DelegateTestFixture;

    // ── ElicitationAction -> rmcp::model::ElicitationAction ───────────────

    #[test]
    fn elicitation_action_accept_converts_to_rmcp_accept() {
        let action = ElicitationAction::Accept;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Accept);
    }

    #[test]
    fn elicitation_action_decline_converts_to_rmcp_decline() {
        let action = ElicitationAction::Decline;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Decline);
    }

    #[test]
    fn elicitation_action_cancel_converts_to_rmcp_cancel() {
        let action = ElicitationAction::Cancel;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Cancel);
    }

    // ── rmcp::model::ElicitationAction -> ElicitationAction ───────────────

    #[test]
    fn rmcp_accept_converts_to_elicitation_action_accept() {
        let rmcp_action = rmcp::model::ElicitationAction::Accept;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Accept);
    }

    #[test]
    fn rmcp_decline_converts_to_elicitation_action_decline() {
        let rmcp_action = rmcp::model::ElicitationAction::Decline;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Decline);
    }

    #[test]
    fn rmcp_cancel_converts_to_elicitation_action_cancel() {
        let rmcp_action = rmcp::model::ElicitationAction::Cancel;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Cancel);
    }

    // ── ElicitationAction serde round-trip ─────────────────────────────────

    #[test]
    fn elicitation_action_accept_serializes_as_lowercase() {
        let action = ElicitationAction::Accept;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""accept""#);
    }

    #[test]
    fn elicitation_action_decline_serializes_as_lowercase() {
        let action = ElicitationAction::Decline;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""decline""#);
    }

    #[test]
    fn elicitation_action_cancel_serializes_as_lowercase() {
        let action = ElicitationAction::Cancel;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""cancel""#);
    }

    #[test]
    fn elicitation_action_deserializes_from_lowercase() {
        let json = r#""accept""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Accept);

        let json = r#""decline""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Decline);

        let json = r#""cancel""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Cancel);
    }

    #[test]
    fn all_elicitation_actions_round_trip() {
        let actions = vec![
            ElicitationAction::Accept,
            ElicitationAction::Decline,
            ElicitationAction::Cancel,
        ];

        for original in actions {
            let json = serde_json::to_string(&original).unwrap();
            let restored: ElicitationAction = serde_json::from_str(&json).unwrap();
            assert_eq!(original, restored);
        }
    }

    // ── ElicitationResponse construction ───────────────────────────────────

    #[test]
    fn elicitation_response_with_content() {
        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({"answer": "yes"})),
        };
        assert_eq!(response.action, ElicitationAction::Accept);
        assert!(response.content.is_some());
        assert_eq!(response.content.unwrap()["answer"], "yes");
    }

    #[test]
    fn elicitation_response_without_content() {
        let response = ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
        };
        assert_eq!(response.action, ElicitationAction::Cancel);
        assert!(response.content.is_none());
    }

    // ── PendingElicitationMap insert/remove lifecycle ──────────────────────

    #[tokio::test]
    async fn pending_elicitation_map_insert_and_retrieve() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();

        insert_pending_elicitation(&map, "elicit-1".to_string(), "s1".to_string(), tx).await;

        let has_entry = {
            let pending = map.lock().await;
            pending.contains_key("elicit-1")
        };

        assert!(has_entry);
    }

    #[tokio::test]
    async fn pending_elicitation_map_remove_on_response() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();

        insert_pending_elicitation(&map, "elicit-2".to_string(), "s1".to_string(), tx).await;

        // Simulate response
        let tx = {
            let mut pending = map.lock().await;
            pending.remove("elicit-2").map(|entry| entry.sender)
        };

        assert!(tx.is_some());

        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({"data": "test"})),
        };

        tx.unwrap().send(response).unwrap();

        let received = rx.await.unwrap();
        assert_eq!(received.action, ElicitationAction::Accept);
    }

    #[tokio::test]
    async fn pending_elicitation_registration_snapshots_recovery_metadata() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        register_pending_elicitation(
            &map,
            "elicit-snapshot".to_string(),
            PendingElicitationRegistration {
                session_id: "session-snapshot".to_string(),
                form: PendingElicitationForm {
                    message: "Choose".to_string(),
                    requested_schema: serde_json::json!({"type": "object"}),
                    source: "builtin:question".to_string(),
                },
                owner_authority: Some("process-secret".to_string()),
                run_id: Some("run-1".to_string()),
                tool_call_id: Some("tool-1".to_string()),
            },
            tx,
        )
        .await;

        let snapshot = snapshot_pending_elicitations(&map).await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].elicitation_id, "elicit-snapshot");
        assert_eq!(snapshot[0].session_id, "session-snapshot");
        assert_eq!(snapshot[0].form.message, "Choose");
        assert_eq!(snapshot[0].form.requested_schema["type"], "object");
        assert_eq!(snapshot[0].form.source, "builtin:question");
        assert_eq!(
            snapshot[0].owner_authority.as_deref(),
            Some("process-secret")
        );
        assert_eq!(snapshot[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(snapshot[0].tool_call_id.as_deref(), Some("tool-1"));
        assert_eq!(snapshot[0].delivery_generation, 0);
    }

    #[tokio::test]
    async fn recovery_generation_allows_one_valid_winner() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let map = fixture.handle.pending_elicitations();
        let (tx, rx) = oneshot::channel();
        register_pending_elicitation(
            &map,
            "stable-question".to_string(),
            PendingElicitationRegistration {
                session_id: "off-screen-session".to_string(),
                form: PendingElicitationForm {
                    message: "Choose".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"choice": {"type": "string"}},
                        "required": ["choice"],
                        "additionalProperties": false
                    }),
                    source: "builtin:question".to_string(),
                },
                owner_authority: Some("authority-verifier".to_string()),
                run_id: Some("run-1".to_string()),
                tool_call_id: Some("tool-1".to_string()),
            },
            tx,
        )
        .await;

        let original = claim_live_elicitation_delivery(
            fixture.handle.as_ref(),
            "off-screen-session",
            "stable-question",
            "old-connection",
        )
        .await
        .expect("original delivery claim");
        let recovered = claim_pending_elicitation_deliveries(
            fixture.handle.as_ref(),
            "off-screen-session",
            None,
            "authority-verifier",
            "new-connection",
        )
        .await
        .pop()
        .expect("recovery delivery claim");
        assert_eq!(original.elicitation_id, recovered.elicitation_id);
        assert!(recovered.delivery_generation > original.delivery_generation);

        assert_eq!(
            resolve_claimed_elicitation(
                fixture.handle.as_ref(),
                "off-screen-session",
                "stable-question",
                "old-connection",
                original.delivery_generation,
                ElicitationResponse {
                    action: ElicitationAction::Accept,
                    content: Some(serde_json::json!({"choice": "old"})),
                },
            )
            .await
            .expect("stale response is classified"),
            ElicitationResolution::StaleDelivery
        );
        assert!(map.lock().await.contains_key("stable-question"));

        assert!(
            resolve_claimed_elicitation(
                fixture.handle.as_ref(),
                "off-screen-session",
                "stable-question",
                "new-connection",
                recovered.delivery_generation,
                ElicitationResponse {
                    action: ElicitationAction::Accept,
                    content: Some(serde_json::json!({"choice": 42})),
                },
            )
            .await
            .is_err()
        );
        assert!(map.lock().await.contains_key("stable-question"));

        assert_eq!(
            resolve_claimed_elicitation(
                fixture.handle.as_ref(),
                "off-screen-session",
                "stable-question",
                "new-connection",
                recovered.delivery_generation,
                ElicitationResponse {
                    action: ElicitationAction::Accept,
                    content: Some(serde_json::json!({"choice": "new"})),
                },
            )
            .await
            .expect("valid response resolves"),
            ElicitationResolution::Resolved
        );
        assert_eq!(
            rx.await.expect("tool receives one response").content,
            Some(serde_json::json!({"choice": "new"}))
        );
        assert_eq!(
            resolve_claimed_elicitation(
                fixture.handle.as_ref(),
                "off-screen-session",
                "stable-question",
                "new-connection",
                recovered.delivery_generation,
                ElicitationResponse {
                    action: ElicitationAction::Decline,
                    content: None,
                },
            )
            .await
            .expect("duplicate response is classified"),
            ElicitationResolution::NotFound
        );
    }

    #[tokio::test]
    async fn tool_exit_removes_only_its_invocation_waiters() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, _second_rx) = oneshot::channel();
        for (id, tool_call_id, sender) in [
            ("first", "tool-1", first_tx),
            ("second", "tool-2", second_tx),
        ] {
            register_pending_elicitation(
                &map,
                id.to_string(),
                PendingElicitationRegistration {
                    session_id: "session-a".to_string(),
                    form: PendingElicitationForm::default(),
                    owner_authority: None,
                    run_id: Some("run-1".to_string()),
                    tool_call_id: Some(tool_call_id.to_string()),
                },
                sender,
            )
            .await;
        }

        assert_eq!(
            remove_pending_elicitations_for_tool(&map, "session-a", "run-1", "tool-1").await,
            1
        );
        assert!(first_rx.await.is_err());
        let snapshot = snapshot_pending_elicitations(&map).await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].elicitation_id, "second");
    }

    #[tokio::test]
    async fn dropped_mcp_waiter_cleans_up_its_pending_entry() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        insert_pending_elicitation(&map, "mcp-waiter".to_string(), "session-a".to_string(), tx)
            .await;
        let cleanup = PendingElicitationCleanup::new(map.clone(), "mcp-waiter".to_string());

        drop(cleanup);

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx)
                .await
                .expect("cleanup should drop the pending sender")
                .is_err()
        );
        assert!(snapshot_pending_elicitations(&map).await.is_empty());
    }

    #[tokio::test]
    async fn pending_elicitation_map_tracks_session_membership() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();

        insert_pending_elicitation(&map, "elicit-3".to_string(), "s-track".to_string(), tx).await;

        assert!(has_pending_elicitation_for_session(&map, "s-track").await);
        assert!(!has_pending_elicitation_for_session(&map, "other-session").await);
    }

    #[tokio::test]
    async fn cancelling_session_resolves_only_its_pending_elicitations() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, _second_rx) = oneshot::channel();
        insert_pending_elicitation(&map, "first".into(), "session-a".into(), first_tx).await;
        insert_pending_elicitation(&map, "second".into(), "session-b".into(), second_tx).await;

        assert_eq!(
            cancel_pending_elicitations_for_session(&map, "session-a").await,
            1
        );
        let response = first_rx.await.unwrap();
        assert_eq!(response.action, ElicitationAction::Cancel);
        assert!(response.content.is_none());
        assert!(!has_pending_elicitation_for_session(&map, "session-a").await);
        assert!(has_pending_elicitation_for_session(&map, "session-b").await);
    }

    #[tokio::test]
    async fn take_sender_resolves_delegate_pending_elicitation() {
        let fixture = DelegateTestFixture::new().await.unwrap();

        let elicitation_id = "delegate-elicitation-1".to_string();
        let (tx, rx) = oneshot::channel();
        fixture.delegate.pending_elicitations().lock().await.insert(
            elicitation_id.clone(),
            PendingElicitation::for_test("delegate-session", tx),
        );

        let sender = take_pending_elicitation_sender(fixture.planner.as_ref(), &elicitation_id)
            .await
            .expect("delegate pending elicitation should be resolved");

        sender
            .send(ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(serde_json::json!({"selection": "allow_once"})),
            })
            .unwrap();

        let response = rx.await.unwrap();
        assert_eq!(response.action, ElicitationAction::Accept);
        assert_eq!(
            response.content,
            Some(serde_json::json!({"selection": "allow_once"}))
        );
    }
}
