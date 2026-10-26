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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// What a pending interaction asks the user.
///
/// Both kinds share the session-owned pending map, claim/generation protocol,
/// single-winner resolution and cancel/tool-exit cleanup. Answers travel as an
/// [`ElicitationResponse`]: a permission answer is `Accept` with
/// `{"option_id": "<selected option>"}`; `Decline`/`Cancel` reject the call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionKind {
    #[default]
    Elicitation,
    Permission {
        request: Box<crate::acp::protocol::RequestPermissionRequest>,
    },
}

/// Content key carrying the selected option of a permission answer.
pub const PERMISSION_OPTION_KEY: &str = "option_id";

impl ElicitationResponse {
    /// Permission answer selecting `option_id`.
    pub fn permission_selected(option_id: impl Into<String>) -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({ PERMISSION_OPTION_KEY: option_id.into() })),
        }
    }

    /// Option selected by a permission answer, if any.
    pub fn selected_permission_option(&self) -> Option<&str> {
        if self.action != ElicitationAction::Accept {
            return None;
        }
        self.content
            .as_ref()
            .and_then(|content| content.get(PERMISSION_OPTION_KEY))
            .and_then(|value| value.as_str())
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
    pub kind: InteractionKind,
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
            kind: InteractionKind::Elicitation,
            owner_authority: None,
            run_id: None,
            tool_call_id: None,
            delivery_generation: 0,
            delivery_connection: None,
            sender,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingElicitationSnapshot {
    pub elicitation_id: String,
    pub session_id: String,
    pub form: PendingElicitationForm,
    #[serde(default)]
    pub kind: InteractionKind,
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
            kind: InteractionKind::Elicitation,
            owner_authority: registration.owner_authority,
            run_id: registration.run_id,
            tool_call_id: registration.tool_call_id,
            delivery_generation: 0,
            delivery_connection: None,
            sender,
        },
    );
}

/// Register a pending tool-permission interaction.
///
/// Shares the pending map, claim/generation protocol and cleanup with form
/// elicitations; only answer validation differs (see
/// [`validate_elicitation_response`]).
pub async fn register_pending_permission(
    pending_map: &PendingElicitationMap,
    interaction_id: String,
    session_id: String,
    request: crate::acp::protocol::RequestPermissionRequest,
    run_id: Option<String>,
    tool_call_id: Option<String>,
    sender: oneshot::Sender<ElicitationResponse>,
) {
    pending_map.lock().await.insert(
        interaction_id,
        PendingElicitation {
            session_id,
            form: PendingElicitationForm::default(),
            kind: InteractionKind::Permission {
                request: Box::new(request),
            },
            owner_authority: None,
            run_id,
            tool_call_id,
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
            kind: entry.kind.clone(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimedElicitationDelivery {
    pub elicitation_id: String,
    pub session_id: String,
    pub form: PendingElicitationForm,
    #[serde(default)]
    pub kind: InteractionKind,
    pub delivery_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

type SharedRegistry = Arc<Mutex<crate::agent::SessionRegistry>>;

fn push_agent_registries(
    agent: &crate::agent::LocalAgentHandle,
    registries: &mut Vec<SharedRegistry>,
    seen: &mut HashSet<usize>,
) {
    if seen.insert(Arc::as_ptr(&agent.registry) as usize) {
        registries.push(agent.registry.clone());
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
        if seen.insert(Arc::as_ptr(&delegate.registry) as usize) {
            registries.push(delegate.registry.clone());
        }
    }
}

/// Session registries of the agent, its materialized profile runtimes and its
/// delegates: every place a session (local, or remote attached locally) lives.
async fn all_registries(agent: &crate::agent::LocalAgentHandle) -> Vec<SharedRegistry> {
    let mut registries = Vec::new();
    let mut seen = HashSet::new();
    if let Some(profiles) = agent.profiles().as_ref() {
        for runtime in profiles.materialized_runtimes().await {
            push_agent_registries(
                runtime.agent().handle().as_ref(),
                &mut registries,
                &mut seen,
            );
        }
    }
    push_agent_registries(agent, &mut registries, &mut seen);
    registries
}

/// The `SessionActorRef` that owns `session_id`, wherever the session runs.
async fn find_session_ref(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
) -> Option<crate::agent::SessionActorRef> {
    for registry in all_registries(agent).await {
        if let Some(session_ref) = registry.lock().await.get(session_id) {
            return Some(session_ref.clone());
        }
    }
    None
}

/// Every remote-hosted session ref known to this agent. Local sessions share
/// the host maps, which are read directly where a whole-agent scan is needed.
async fn remote_session_refs(
    agent: &crate::agent::LocalAgentHandle,
) -> Vec<(String, crate::agent::SessionActorRef)> {
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for registry in all_registries(agent).await {
        let registry = registry.lock().await;
        for session_id in registry.session_ids() {
            if let Some(session_ref) = registry.get(&session_id)
                && session_ref.is_remote()
                && seen.insert(session_id.clone())
            {
                refs.push((session_id, session_ref.clone()));
            }
        }
    }
    refs
}

fn warn_interaction_call(what: &str, session_id: &str, error: &crate::error::AgentError) {
    log::warn!("Session interaction call failed: op={what} session_id={session_id} error={error}");
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

/// Adopt `authority_verifier` for unowned entries of `session_id` in one map.
/// Returns whether at least one entry is owned by that authority after assignment.
pub async fn assign_authority_in_map(
    map: &PendingElicitationMap,
    session_id: &str,
    authority_verifier: &str,
) -> bool {
    let mut assigned = false;
    let mut entries = map.lock().await;
    for entry in entries
        .values_mut()
        .filter(|entry| entry.session_id == session_id)
    {
        if entry.owner_authority.is_none() {
            entry.owner_authority = Some(authority_verifier.to_string());
        }
        assigned |= entry.owner_authority.as_deref() == Some(authority_verifier);
    }
    assigned
}

/// Claim delivery of matching entries of `session_id` in one map.
///
/// With `authority_verifier` the entry's owner authority must match (recovery
/// claim). Without it, `elicitation_id` selects a single live entry.
pub async fn claim_deliveries_in_map(
    map: &PendingElicitationMap,
    session_id: &str,
    elicitation_id: Option<&str>,
    authority_verifier: Option<&str>,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    let mut claims = Vec::new();
    let mut entries = map.lock().await;
    for (entry_id, entry) in entries.iter_mut().filter(|(entry_id, entry)| {
        entry.session_id == session_id
            && elicitation_id.is_none_or(|expected| entry_id.as_str() == expected)
            && authority_verifier
                .is_none_or(|verifier| entry.owner_authority.as_deref() == Some(verifier))
    }) {
        entry.delivery_generation = entry.delivery_generation.saturating_add(1);
        entry.delivery_connection = Some(connection_id.to_string());
        claims.push(ClaimedElicitationDelivery {
            elicitation_id: entry_id.clone(),
            session_id: entry.session_id.clone(),
            form: entry.form.clone(),
            kind: entry.kind.clone(),
            delivery_generation: entry.delivery_generation,
        });
    }
    claims.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    claims
}

/// Entries of `session_id` currently claimed by `connection_id`.
pub async fn claimed_deliveries_in_map(
    map: &PendingElicitationMap,
    session_id: &str,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    let entries = map.lock().await;
    let mut claims = entries
        .iter()
        .filter(|(_, entry)| {
            entry.session_id == session_id
                && entry.delivery_connection.as_deref() == Some(connection_id)
        })
        .map(|(elicitation_id, entry)| ClaimedElicitationDelivery {
            elicitation_id: elicitation_id.clone(),
            session_id: entry.session_id.clone(),
            form: entry.form.clone(),
            kind: entry.kind.clone(),
            delivery_generation: entry.delivery_generation,
        })
        .collect::<Vec<_>>();
    claims.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    claims
}

/// Validate and resolve one interaction in one map.
///
/// `delivery_generation: Some(g)` requires the exact claimed delivery.
/// `None` is the unclaimed path: allowed only for entries that were never
/// claimed and have no owner authority, or for the claiming connection.
/// Check, validate and remove happen under a single hold of the map lock.
/// `session_id: None` matches an entry of any session (legacy callers that
/// do not name the session). Returns `NotFound` when the entry is absent or
/// belongs to another session.
pub async fn resolve_in_map(
    map: &PendingElicitationMap,
    session_id: Option<&str>,
    elicitation_id: &str,
    connection_id: &str,
    delivery_generation: Option<u64>,
    response: ElicitationResponse,
) -> Result<ElicitationResolution, agent_client_protocol::Error> {
    let mut entries = map.lock().await;
    let Some(entry) = entries.get(elicitation_id) else {
        return Ok(ElicitationResolution::NotFound);
    };
    if session_id.is_some_and(|session_id| entry.session_id != session_id) {
        return Ok(ElicitationResolution::NotFound);
    }
    let allowed = match delivery_generation {
        Some(generation) => {
            entry.delivery_generation == generation
                && entry.delivery_connection.as_deref() == Some(connection_id)
        }
        None if entry.delivery_generation == 0 => entry.owner_authority.is_none(),
        None => entry.delivery_connection.as_deref() == Some(connection_id),
    };
    if !allowed {
        return Ok(ElicitationResolution::StaleDelivery);
    }
    validate_elicitation_response(entry, &response)?;
    let entry = entries
        .remove(elicitation_id)
        .expect("pending entry exists while registry lock is held");
    drop(entries);
    let _ = entry.sender.send(response);
    Ok(ElicitationResolution::Resolved)
}

pub async fn assign_pending_elicitation_authority(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    authority_verifier: &str,
) -> bool {
    if let Some(session_ref) = find_session_ref(agent, session_id).await {
        return match session_ref
            .list_pending_interactions(Some(authority_verifier.to_string()))
            .await
        {
            Ok(pending) => pending
                .iter()
                .any(|entry| entry.owner_authority.as_deref() == Some(authority_verifier)),
            Err(error) => {
                warn_interaction_call("assign_authority", session_id, &error);
                false
            }
        };
    }
    let mut assigned = false;
    for map in all_pending_maps(agent).await {
        assigned |= assign_authority_in_map(&map, session_id, authority_verifier).await;
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
    let remote_results =
        futures_util::future::join_all(remote_session_refs(agent).await.into_iter().map(
            |(session_id, session_ref)| async move {
                let result = session_ref.list_pending_interactions(None).await;
                (session_id, result)
            },
        ))
        .await;
    for (session_id, result) in remote_results {
        match result {
            Ok(pending) => {
                if pending
                    .iter()
                    .any(|entry| entry.owner_authority.as_deref() == Some(authority_verifier))
                {
                    session_ids.push(session_id);
                }
            }
            Err(error) => warn_interaction_call("list_pending", &session_id, &error),
        }
    }
    session_ids.sort();
    session_ids.dedup();
    session_ids
}

pub async fn has_pending_elicitation_for_agent_session(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
) -> bool {
    if let Some(session_ref) = find_session_ref(agent, session_id).await {
        return match session_ref.list_pending_interactions(None).await {
            Ok(pending) => !pending.is_empty(),
            Err(error) => {
                warn_interaction_call("has_pending", session_id, &error);
                // Unknown is treated as still pending: authority must not be
                // retired because the owner was momentarily unreachable.
                true
            }
        };
    }
    for map in all_pending_maps(agent).await {
        if has_pending_elicitation_for_session(&map, session_id).await {
            return true;
        }
    }
    false
}

/// IDs of the pending permission interactions of `session_id` that
/// `connection_id` does not already hold, wherever the session runs. Used to
/// re-deliver permission requests to a client that attaches after the request
/// was raised.
/// True when `session_id` has pending interactions and every one of them is a
/// permission request. Such a session needs no form-question support from the
/// client, so recovery does not have to require it.
pub async fn session_pending_is_permission_only(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
) -> bool {
    let pending = if let Some(session_ref) = find_session_ref(agent, session_id).await {
        session_ref
            .list_pending_interactions(None)
            .await
            .unwrap_or_else(|error| {
                warn_interaction_call("permission_only", session_id, &error);
                Vec::new()
            })
    } else {
        let mut snapshots = Vec::new();
        for map in all_pending_maps(agent).await {
            snapshots.extend(snapshot_pending_elicitations(&map).await);
        }
        snapshots
            .into_iter()
            .filter(|entry| entry.session_id == session_id)
            .collect()
    };
    !pending.is_empty()
        && pending
            .iter()
            .all(|entry| matches!(entry.kind, InteractionKind::Permission { .. }))
}

pub async fn undelivered_permission_ids_for_session(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    connection_id: &str,
) -> Vec<String> {
    let pending = if let Some(session_ref) = find_session_ref(agent, session_id).await {
        session_ref
            .list_pending_interactions(None)
            .await
            .unwrap_or_else(|error| {
                warn_interaction_call("pending_permissions", session_id, &error);
                Vec::new()
            })
    } else {
        let mut snapshots = Vec::new();
        for map in all_pending_maps(agent).await {
            snapshots.extend(snapshot_pending_elicitations(&map).await);
        }
        snapshots
            .into_iter()
            .filter(|entry| entry.session_id == session_id)
            .collect()
    };
    let mut ids = pending
        .into_iter()
        .filter(|entry| {
            matches!(entry.kind, InteractionKind::Permission { .. })
                && entry.delivery_connection.as_deref() != Some(connection_id)
        })
        .map(|entry| entry.elicitation_id)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

pub async fn claim_pending_elicitation_deliveries(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: Option<&str>,
    authority_verifier: &str,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    if let Some(session_ref) = find_session_ref(agent, session_id).await {
        return session_ref
            .claim_interaction_delivery(
                elicitation_id.map(str::to_string),
                connection_id.to_string(),
                Some(authority_verifier.to_string()),
            )
            .await
            .unwrap_or_else(|error| {
                warn_interaction_call("claim", session_id, &error);
                Vec::new()
            });
    }
    let mut claims = Vec::new();
    for map in all_pending_maps(agent).await {
        claims.extend(
            claim_deliveries_in_map(
                &map,
                session_id,
                elicitation_id,
                Some(authority_verifier),
                connection_id,
            )
            .await,
        );
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
    if let Some(session_ref) = find_session_ref(agent, session_id).await {
        return session_ref
            .claim_interaction_delivery(
                Some(elicitation_id.to_string()),
                connection_id.to_string(),
                None,
            )
            .await
            .unwrap_or_else(|error| {
                warn_interaction_call("claim_live", session_id, &error);
                Vec::new()
            })
            .into_iter()
            .next();
    }
    for map in all_pending_maps(agent).await {
        if let Some(claim) =
            claim_deliveries_in_map(&map, session_id, Some(elicitation_id), None, connection_id)
                .await
                .into_iter()
                .next()
        {
            return Some(claim);
        }
    }
    None
}

pub async fn claimed_elicitation_deliveries(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    connection_id: &str,
) -> Vec<ClaimedElicitationDelivery> {
    if let Some(session_ref) = find_session_ref(agent, session_id).await {
        let pending = session_ref
            .list_pending_interactions(None)
            .await
            .unwrap_or_else(|error| {
                warn_interaction_call("claimed", session_id, &error);
                Vec::new()
            });
        return pending
            .into_iter()
            .filter(|entry| entry.delivery_connection.as_deref() == Some(connection_id))
            .map(|entry| ClaimedElicitationDelivery {
                elicitation_id: entry.elicitation_id,
                session_id: entry.session_id,
                form: entry.form,
                kind: entry.kind,
                delivery_generation: entry.delivery_generation,
            })
            .collect();
    }
    let mut claims = Vec::new();
    for map in all_pending_maps(agent).await {
        claims.extend(claimed_deliveries_in_map(&map, session_id, connection_id).await);
    }
    claims.sort_by(|left, right| left.elicitation_id.cmp(&right.elicitation_id));
    claims
}

/// Resolve `elicitation_id` for a connection.
///
/// Routed through the owning session actor (local or remote). When no session
/// is named, every local pending map and then every remote-hosted session is
/// searched; a session without a registered actor falls back to the local maps.
pub async fn resolve_elicitation_from_connection(
    agent: &crate::agent::LocalAgentHandle,
    session_id: Option<&str>,
    elicitation_id: &str,
    connection_id: &str,
    response: ElicitationResponse,
) -> Result<ElicitationResolution, agent_client_protocol::Error> {
    resolve_interaction_routed(
        agent,
        session_id,
        elicitation_id,
        connection_id,
        None,
        response,
    )
    .await
}

async fn resolve_interaction_routed(
    agent: &crate::agent::LocalAgentHandle,
    session_id: Option<&str>,
    elicitation_id: &str,
    connection_id: &str,
    delivery_generation: Option<u64>,
    response: ElicitationResponse,
) -> Result<ElicitationResolution, agent_client_protocol::Error> {
    if let Some(session_id) = session_id
        && let Some(session_ref) = find_session_ref(agent, session_id).await
    {
        return session_ref
            .resolve_interaction(
                elicitation_id.to_string(),
                connection_id.to_string(),
                delivery_generation,
                response,
            )
            .await
            .map_err(agent_client_protocol::Error::from);
    }
    for map in all_pending_maps(agent).await {
        match resolve_in_map(
            &map,
            session_id,
            elicitation_id,
            connection_id,
            delivery_generation,
            response.clone(),
        )
        .await?
        {
            ElicitationResolution::NotFound => continue,
            resolved => return Ok(resolved),
        }
    }
    if session_id.is_none() {
        let mut last_error = None;
        for (remote_session_id, session_ref) in remote_session_refs(agent).await {
            match session_ref
                .resolve_interaction(
                    elicitation_id.to_string(),
                    connection_id.to_string(),
                    delivery_generation,
                    response.clone(),
                )
                .await
            {
                Ok(ElicitationResolution::NotFound) => continue,
                Ok(resolved) => return Ok(resolved),
                Err(error)
                    if error.transport_failure().is_some()
                        || matches!(
                            error,
                            crate::error::AgentError::RemoteNodeUpgradeRequired { .. }
                        ) =>
                {
                    // One unreachable or outdated node must not hide the
                    // interaction when another node owns it.
                    warn_interaction_call("resolve", &remote_session_id, &error);
                    last_error = Some(error);
                }
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(error) = last_error {
            return Err(error.into());
        }
    }
    Ok(ElicitationResolution::NotFound)
}

/// Serialize a form's requested schema, carrying request-level `_meta` (if any)
/// under the `"_meta"` key so `form_allows_custom` can see it. Forms without
/// metadata serialize exactly as before.
fn form_schema_with_meta<S: Serialize, M: Serialize>(
    requested_schema: &S,
    meta: Option<&M>,
) -> serde_json::Value {
    let mut schema = serde_json::to_value(requested_schema).unwrap_or(serde_json::Value::Null);
    if let Some(meta) = meta
        && let Ok(meta_value) = serde_json::to_value(meta)
        && !meta_value.is_null()
        && meta_value.as_object().is_none_or(|obj| !obj.is_empty())
        && let Some(obj) = schema.as_object_mut()
    {
        obj.insert("_meta".to_string(), meta_value);
    }
    schema
}

/// Whether the form advertises free-form ("custom") answers in addition to its
/// listed options. Mirrors the client-side rule used to render the custom field.
fn form_allows_custom(form: &PendingElicitationForm) -> bool {
    if form.source == "builtin:question" {
        return true;
    }
    let Some(meta) = form
        .requested_schema
        .get("_meta")
        .and_then(|m| m.get("querymt"))
    else {
        return false;
    };
    meta.get("allow_custom").and_then(|v| v.as_bool()) == Some(true)
        || meta.get("allowCustom").and_then(|v| v.as_bool()) == Some(true)
}

/// Copy of `schema` with per-property option constraints (`oneOf`, `anyOf`,
/// `enum`, `const`, and the same on array `items`) removed. Types, `required`
/// and object structure are preserved so malformed payloads are still rejected.
fn relax_schema_for_custom(schema: &serde_json::Value) -> serde_json::Value {
    fn strip_options(property: &mut serde_json::Value) {
        let Some(obj) = property.as_object_mut() else {
            return;
        };
        for key in ["oneOf", "anyOf", "enum", "const"] {
            obj.remove(key);
        }
        if let Some(items) = obj.get_mut("items") {
            strip_options(items);
        }
    }

    let mut relaxed = schema.clone();
    if let Some(properties) = relaxed
        .get_mut("properties")
        .and_then(|p| p.as_object_mut())
    {
        for property in properties.values_mut() {
            strip_options(property);
        }
    }
    relaxed
}

fn validate_elicitation_response(
    entry: &PendingElicitation,
    response: &ElicitationResponse,
) -> Result<(), agent_client_protocol::Error> {
    if let InteractionKind::Permission { request } = &entry.kind {
        if response.action != ElicitationAction::Accept {
            return Ok(());
        }
        let option_id = response.selected_permission_option().ok_or_else(|| {
            agent_client_protocol::Error::invalid_params()
                .data("Accepted permission response requires an option_id")
        })?;
        return if request
            .options
            .iter()
            .any(|option| option.option_id.0.as_ref() == option_id)
        {
            Ok(())
        } else {
            Err(agent_client_protocol::Error::invalid_params()
                .data(format!("Unknown permission option: {option_id}")))
        };
    }
    if response.action != ElicitationAction::Accept {
        return Ok(());
    }
    let content = response.content.as_ref().ok_or_else(|| {
        agent_client_protocol::Error::invalid_params()
            .data("Accepted elicitation response requires content")
    })?;
    let relaxed;
    let schema = if form_allows_custom(&entry.form) {
        relaxed = relax_schema_for_custom(&entry.form.requested_schema);
        &relaxed
    } else {
        &entry.form.requested_schema
    };
    let validator = jsonschema::validator_for(schema).map_err(|err| {
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
    resolve_interaction_routed(
        agent,
        Some(session_id),
        elicitation_id,
        connection_id,
        Some(delivery_generation),
        response,
    )
    .await
}

/// Cancel a claimed delivery that can no longer make progress (for example when
/// validation retries are exhausted).
///
/// The generation/connection check and the removal happen under a single hold
/// of the map lock, so a delivery re-claimed by another connection in the
/// meantime is never cancelled. The waiter is notified after the lock is
/// released.
pub async fn cancel_claimed_elicitation(
    agent: &crate::agent::LocalAgentHandle,
    session_id: &str,
    elicitation_id: &str,
    connection_id: &str,
    delivery_generation: u64,
) -> ElicitationResolution {
    // A cancel answer always validates, and the generation/connection check
    // plus removal happen atomically on the owning session, so a delivery
    // re-claimed by another connection is never cancelled.
    resolve_claimed_elicitation(
        agent,
        session_id,
        elicitation_id,
        connection_id,
        delivery_generation,
        ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
        },
    )
    .await
    .unwrap_or_else(|error| {
        log::warn!(
            "Failed to cancel claimed elicitation: session_id={session_id} elicitation_id={elicitation_id} error={}",
            error.message
        );
        ElicitationResolution::NotFound
    })
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
                    meta,
                    message,
                    requested_schema,
                } => (
                    message.clone(),
                    form_schema_with_meta(requested_schema, meta.as_ref()),
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
    async fn assigning_a_conflicting_authority_does_not_report_success() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let map = fixture.handle.pending_elicitations();
        let materialized = fixture
            .handle
            .new_session(crate::acp::protocol::NewSessionRequest::new(
                fixture._tempdir.path(),
            ))
            .await
            .unwrap()
            .session_id
            .to_string();
        for (id, session_id) in [
            ("fallback-question", "unmaterialized"),
            ("actor-question", materialized.as_str()),
        ] {
            let (sender, _receiver) = oneshot::channel();
            insert_pending_elicitation(&map, id.into(), session_id.into(), sender).await;
            assert!(
                assign_pending_elicitation_authority(&fixture.handle, session_id, "first").await
            );
            assert!(
                !assign_pending_elicitation_authority(&fixture.handle, session_id, "second").await
            );
            assert_eq!(
                map.lock().await[id].owner_authority.as_deref(),
                Some("first")
            );
        }
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
    async fn recovered_elicitation_routes_to_delegate() {
        let fixture = DelegateTestFixture::new().await.unwrap();
        let elicitation_id = "delegate-recovery-42";
        let (tx, rx) = oneshot::channel();
        register_pending_elicitation(
            &fixture.delegate.pending_elicitations(),
            elicitation_id.to_string(),
            PendingElicitationRegistration {
                session_id: "delegate-session".to_string(),
                form: PendingElicitationForm {
                    message: "Choose".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"selection": {"type": "string"}},
                        "required": ["selection"]
                    }),
                    source: "delegate:question".to_string(),
                },
                owner_authority: Some("delegate-authority".to_string()),
                run_id: Some("delegate-run".to_string()),
                tool_call_id: Some("delegate-tool".to_string()),
            },
            tx,
        )
        .await;

        let claim = claim_pending_elicitation_deliveries(
            fixture.planner.as_ref(),
            "delegate-session",
            None,
            "delegate-authority",
            "replacement",
        )
        .await
        .pop()
        .expect("delegate waiter should be discoverable from primary agent");
        assert_eq!(claim.elicitation_id, elicitation_id);
        assert_eq!(
            resolve_claimed_elicitation(
                fixture.planner.as_ref(),
                "delegate-session",
                elicitation_id,
                "replacement",
                claim.delivery_generation,
                ElicitationResponse {
                    action: ElicitationAction::Accept,
                    content: Some(serde_json::json!({"selection": "delegate"})),
                },
            )
            .await
            .unwrap(),
            ElicitationResolution::Resolved
        );
        assert_eq!(
            rx.await.unwrap().content,
            Some(serde_json::json!({"selection": "delegate"}))
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

    async fn register_test_permission(
        map: &PendingElicitationMap,
        id: &str,
        session_id: &str,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> oneshot::Receiver<ElicitationResponse> {
        use crate::acp::protocol::{
            PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionRequest,
            ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        let (tx, rx) = oneshot::channel();
        let request = RequestPermissionRequest::new(
            session_id.to_string(),
            ToolCallUpdate::new(
                ToolCallId::from(id.to_string()),
                ToolCallUpdateFields::new(),
            ),
            vec![PermissionOption::new(
                PermissionOptionId::from("allow_once"),
                "Allow once",
                PermissionOptionKind::AllowOnce,
            )],
        );
        register_pending_permission(
            map,
            id.to_string(),
            session_id.to_string(),
            request,
            run_id.map(str::to_string),
            tool_call_id.map(str::to_string),
            tx,
        )
        .await;
        rx
    }

    #[tokio::test]
    async fn session_cancel_and_tool_exit_retire_both_interaction_kinds() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        // Session cancel: a permission and an elicitation of the session are
        // both cancelled; another session's permission is untouched.
        let permission_rx = register_test_permission(&map, "perm", "session-a", None, None).await;
        let (form_tx, form_rx) = oneshot::channel();
        insert_pending_elicitation(&map, "form".into(), "session-a".into(), form_tx).await;
        let _other_rx = register_test_permission(&map, "other", "session-b", None, None).await;
        assert_eq!(
            cancel_pending_elicitations_for_session(&map, "session-a").await,
            2
        );
        for response in [permission_rx.await.unwrap(), form_rx.await.unwrap()] {
            assert_eq!(response.action, ElicitationAction::Cancel);
            // A cancelled permission selects no option, so it can never grant.
            assert_eq!(response.selected_permission_option(), None);
        }
        let remaining = snapshot_pending_elicitations(&map).await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].elicitation_id, "other");

        // Tool exit: only the exiting invocation's permission is removed.
        let exiting_rx =
            register_test_permission(&map, "exiting", "session-c", Some("run-1"), Some("tool-1"))
                .await;
        let _kept_rx =
            register_test_permission(&map, "kept", "session-c", Some("run-1"), Some("tool-2"))
                .await;
        assert_eq!(
            remove_pending_elicitations_for_tool(&map, "session-c", "run-1", "tool-1").await,
            1
        );
        assert!(exiting_rx.await.is_err());
        let ids: Vec<_> = snapshot_pending_elicitations(&map)
            .await
            .into_iter()
            .map(|entry| entry.elicitation_id)
            .collect();
        assert_eq!(ids, vec!["kept", "other"]);
    }

    #[tokio::test]
    async fn permission_answers_must_select_a_listed_option() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let mut rx = register_test_permission(&map, "perm", "session-a", None, None).await;

        for bad in [
            ElicitationResponse::permission_selected("not_an_option"),
            ElicitationResponse {
                action: ElicitationAction::Accept,
                content: None,
            },
        ] {
            let result = resolve_in_map(&map, Some("session-a"), "perm", "conn", None, bad).await;
            assert!(result.is_err(), "malformed answer must be rejected");
            assert!(rx.try_recv().is_err(), "request stays pending");
        }

        let resolved = resolve_in_map(
            &map,
            Some("session-a"),
            "perm",
            "conn",
            None,
            ElicitationResponse::permission_selected("allow_once"),
        )
        .await
        .unwrap();
        assert_eq!(resolved, ElicitationResolution::Resolved);
        assert_eq!(
            rx.await.unwrap().selected_permission_option(),
            Some("allow_once")
        );
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

    fn question_entry(schema: serde_json::Value, source: &str) -> PendingElicitation {
        let (tx, _rx) = oneshot::channel();
        let mut entry = PendingElicitation::for_test("s", tx);
        entry.form = PendingElicitationForm {
            message: "q".to_string(),
            requested_schema: schema,
            source: source.to_string(),
        };
        entry
    }

    fn accept(content: serde_json::Value) -> ElicitationResponse {
        ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(content),
        }
    }

    fn single_choice_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"selection": {
                "type": "string",
                "oneOf": [{"const": "A", "title": "A"}, {"const": "B", "title": "B"}]
            }},
            "required": ["selection"]
        })
    }

    fn multi_choice_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"selection": {
                "type": "array",
                "items": {"anyOf": [{"const": "A", "title": "A"}, {"const": "B", "title": "B"}]}
            }},
            "required": ["selection"]
        })
    }

    #[test]
    fn builtin_question_accepts_custom_single_answer() {
        let entry = question_entry(single_choice_schema(), "builtin:question");
        assert!(
            validate_elicitation_response(&entry, &accept(serde_json::json!({"selection": "A"})))
                .is_ok()
        );
        assert!(
            validate_elicitation_response(
                &entry,
                &accept(serde_json::json!({"selection": "something else entirely"}))
            )
            .is_ok()
        );
    }

    #[test]
    fn builtin_question_accepts_custom_multi_answer() {
        let entry = question_entry(multi_choice_schema(), "builtin:question");
        assert!(
            validate_elicitation_response(
                &entry,
                &accept(serde_json::json!({"selection": ["custom"]}))
            )
            .is_ok()
        );
    }

    #[test]
    fn custom_forms_still_reject_missing_required_and_wrong_types() {
        let entry = question_entry(single_choice_schema(), "builtin:question");
        assert!(validate_elicitation_response(&entry, &accept(serde_json::json!({}))).is_err());
        assert!(
            validate_elicitation_response(&entry, &accept(serde_json::json!({"selection": 42})))
                .is_err()
        );
        let multi = question_entry(multi_choice_schema(), "builtin:question");
        assert!(
            validate_elicitation_response(&multi, &accept(serde_json::json!({"selection": "A"})))
                .is_err()
        );
    }

    #[test]
    fn non_custom_forms_keep_strict_option_validation() {
        let entry = question_entry(single_choice_schema(), "mcp:server");
        assert!(
            validate_elicitation_response(
                &entry,
                &accept(serde_json::json!({"selection": "free"}))
            )
            .is_err()
        );
        assert!(
            validate_elicitation_response(&entry, &accept(serde_json::json!({"selection": "A"})))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn custom_question_answer_resolves_the_waiting_tool() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let map = fixture.handle.pending_elicitations();
        let (tx, rx) = oneshot::channel();
        register_pending_elicitation(
            &map,
            "question-custom".to_string(),
            PendingElicitationRegistration {
                session_id: "s1".to_string(),
                form: PendingElicitationForm {
                    message: "Pick".to_string(),
                    requested_schema: single_choice_schema(),
                    source: "builtin:question".to_string(),
                },
                owner_authority: None,
                run_id: None,
                tool_call_id: None,
            },
            tx,
        )
        .await;
        let claim = claim_live_elicitation_delivery(
            fixture.handle.as_ref(),
            "s1",
            "question-custom",
            "conn",
        )
        .await
        .expect("claim");

        let resolution = resolve_claimed_elicitation(
            fixture.handle.as_ref(),
            "s1",
            "question-custom",
            "conn",
            claim.delivery_generation,
            accept(serde_json::json!({"selection": "my own words"})),
        )
        .await
        .expect("custom answer must not be rejected");
        assert_eq!(resolution, ElicitationResolution::Resolved);
        assert_eq!(
            rx.await.expect("tool is released").content,
            Some(serde_json::json!({"selection": "my own words"}))
        );
        assert!(map.lock().await.is_empty());
    }

    #[test]
    fn form_meta_is_merged_into_schema_and_enables_custom() {
        let meta = serde_json::json!({"querymt": {"allow_custom": true}});
        let schema = form_schema_with_meta(&single_choice_schema(), Some(&meta));
        assert_eq!(schema["_meta"], meta);
        assert!(form_allows_custom(&PendingElicitationForm {
            message: String::new(),
            requested_schema: schema,
            source: "mcp:server".to_string(),
        }));
    }

    #[test]
    fn form_without_meta_is_unchanged() {
        let base = single_choice_schema();
        assert_eq!(
            form_schema_with_meta(&base, None::<&serde_json::Value>),
            base
        );
        let empty = serde_json::json!({});
        assert_eq!(form_schema_with_meta(&base, Some(&empty)), base);
        assert!(!form_allows_custom(&PendingElicitationForm {
            message: String::new(),
            requested_schema: base,
            source: "mcp:server".to_string(),
        }));
    }

    async fn register_claimed(
        fixture: &crate::test_utils::TestAgent,
        id: &str,
        connection: &str,
    ) -> (
        ClaimedElicitationDelivery,
        oneshot::Receiver<ElicitationResponse>,
    ) {
        let (tx, rx) = oneshot::channel();
        register_pending_elicitation(
            &fixture.handle.pending_elicitations(),
            id.to_string(),
            PendingElicitationRegistration {
                session_id: "s1".to_string(),
                form: PendingElicitationForm {
                    message: "Pick".to_string(),
                    requested_schema: single_choice_schema(),
                    source: "mcp:server".to_string(),
                },
                owner_authority: None,
                run_id: None,
                tool_call_id: None,
            },
            tx,
        )
        .await;
        let claim = claim_live_elicitation_delivery(fixture.handle.as_ref(), "s1", id, connection)
            .await
            .expect("claim");
        (claim, rx)
    }

    #[tokio::test]
    async fn cancel_claimed_elicitation_releases_waiter_and_removes_entry() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (claim, rx) = register_claimed(&fixture, "q-cancel", "conn").await;

        let resolution = cancel_claimed_elicitation(
            fixture.handle.as_ref(),
            "s1",
            "q-cancel",
            "conn",
            claim.delivery_generation,
        )
        .await;
        assert_eq!(resolution, ElicitationResolution::Resolved);
        assert_eq!(rx.await.unwrap().action, ElicitationAction::Cancel);
        assert!(
            fixture
                .handle
                .pending_elicitations()
                .lock()
                .await
                .is_empty()
        );

        let again = cancel_claimed_elicitation(
            fixture.handle.as_ref(),
            "s1",
            "q-cancel",
            "conn",
            claim.delivery_generation,
        )
        .await;
        assert_eq!(again, ElicitationResolution::NotFound);
    }

    #[tokio::test]
    async fn cancel_claimed_elicitation_ignores_stale_or_reclaimed_delivery() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (claim, mut rx) = register_claimed(&fixture, "q-stale", "conn-a").await;
        // Another connection re-claims the question.
        let newer =
            claim_live_elicitation_delivery(fixture.handle.as_ref(), "s1", "q-stale", "conn-b")
                .await
                .expect("reclaim");
        assert!(newer.delivery_generation > claim.delivery_generation);

        let resolution = cancel_claimed_elicitation(
            fixture.handle.as_ref(),
            "s1",
            "q-stale",
            "conn-a",
            claim.delivery_generation,
        )
        .await;
        assert_eq!(resolution, ElicitationResolution::StaleDelivery);
        assert!(rx.try_recv().is_err());
        assert!(
            has_pending_elicitation_for_session(&fixture.handle.pending_elicitations(), "s1").await
        );
    }

    #[tokio::test]
    async fn concurrent_cancel_and_resolve_settle_exactly_once() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let (claim, rx) = register_claimed(&fixture, "q-race", "conn").await;
        let handle = fixture.handle.clone();
        let generation = claim.delivery_generation;

        let cancel = {
            let handle = handle.clone();
            tokio::spawn(async move {
                cancel_claimed_elicitation(handle.as_ref(), "s1", "q-race", "conn", generation)
                    .await
            })
        };
        let resolve = {
            let handle = handle.clone();
            tokio::spawn(async move {
                resolve_claimed_elicitation(
                    handle.as_ref(),
                    "s1",
                    "q-race",
                    "conn",
                    generation,
                    accept(serde_json::json!({"selection": "A"})),
                )
                .await
                .expect("valid answer")
            })
        };
        let (cancel, resolve) = (cancel.await.unwrap(), resolve.await.unwrap());
        let resolved = [cancel, resolve]
            .iter()
            .filter(|r| **r == ElicitationResolution::Resolved)
            .count();
        assert_eq!(
            resolved, 1,
            "exactly one side may win: {cancel:?} {resolve:?}"
        );
        assert!(rx.await.is_ok());
        assert!(handle.pending_elicitations().lock().await.is_empty());
    }

    #[test]
    fn explicit_allow_custom_meta_relaxes_validation() {
        let mut schema = single_choice_schema();
        schema["_meta"] = serde_json::json!({"querymt": {"allow_custom": true}});
        let entry = question_entry(schema, "mcp:server");
        assert!(
            validate_elicitation_response(
                &entry,
                &accept(serde_json::json!({"selection": "free"}))
            )
            .is_ok()
        );
    }
}
