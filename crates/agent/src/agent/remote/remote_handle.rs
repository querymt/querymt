//! `RemoteAgentHandle` — `AgentHandle` implementation for remote agents.
//!
//! This replaces both `RemoteAgentStub` (SendAgent impl) and
//! `AgentActorHandle::Remote` from the legacy code path. Remote agent
//! interaction now goes through the unified `AgentHandle` trait.

use crate::agent::handle::AgentHandle;
use crate::agent::remote::SessionActorRef;
use crate::delegation::{AgentRegistry, DefaultAgentRegistry};
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EphemeralEvent, EventEnvelope, EventOrigin};

use agent_client_protocol::{CancelNotification, Error, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse};
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

use super::mesh::MeshHandle;

/// A handle for interacting with a remote agent via the kameo mesh.
///
/// Sessions are created lazily via `CreateRemoteSession` on a
/// `RemoteNodeManager` and tracked in an internal map so that
/// subsequent `prompt` / `cancel` calls can route to the correct
/// remote `SessionActorRef`.
pub struct RemoteAgentHandle {
    peer_label: String,
    mesh: MeshHandle,
    event_fanout: Arc<EventFanout>,
    sessions: Mutex<HashMap<String, SessionActorRef>>,
}

impl RemoteAgentHandle {
    /// Create a new `RemoteAgentHandle` for a remote peer.
    pub fn new(peer_label: String, mesh: MeshHandle) -> Self {
        Self {
            peer_label,
            mesh,
            event_fanout: Arc::new(EventFanout::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Create a remote session and return `(session_id, SessionActorRef)`.
    ///
    /// Looks up the remote `RemoteNodeManager` via DHT, sends
    /// `CreateRemoteSession`, and resolves the `SessionActorRef` by
    /// DHT name lookup.
    async fn create_remote_session_inner(
        &self,
        cwd: Option<String>,
    ) -> Result<(String, SessionActorRef), Error> {
        use crate::agent::remote::{CreateRemoteSession, RemoteNodeManager};
        use crate::agent::session_actor::SessionActor;
        use crate::error::AgentError;

        let node_manager = self
            .mesh
            .lookup_actor::<RemoteNodeManager>(crate::agent::remote::dht_name::NODE_MANAGER)
            .await
            .map_err(|e| {
                Error::from(AgentError::SwarmLookupFailed {
                    key: "node_manager".to_string(),
                    reason: e.to_string(),
                })
            })?
            .ok_or_else(|| {
                Error::new(
                    -32001,
                    format!(
                        "Remote peer '{}' not found in DHT (is the mesh running on that machine?)",
                        self.peer_label
                    ),
                )
            })?;

        let resp = node_manager
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| {
                Error::from(AgentError::RemoteActor(e.to_string()))
            })?;

        let session_id = resp.session_id.clone();
        let dht_name = crate::agent::remote::dht_name::session(&session_id);

        let remote_session_ref = self
            .mesh
            .lookup_actor::<SessionActor>(dht_name.clone())
            .await
            .map_err(|e| {
                Error::from(AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })
            })?
            .ok_or_else(|| {
                Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} (actor_id={}) not found in DHT under '{}'",
                        session_id, resp.actor_id, dht_name
                    ),
                })
            })?;

        let session_ref = SessionActorRef::remote(remote_session_ref, self.peer_label.clone());

        // Store the session ref for future prompt/cancel calls.
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session_ref.clone());

        Ok((session_id, session_ref))
    }
}

#[async_trait]
impl AgentHandle for RemoteAgentHandle {
    async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> Result<NewSessionResponse, Error> {
        let cwd = req.cwd.to_str().map(|s| s.to_string());
        let (session_id, _) = self.create_remote_session_inner(cwd).await?;
        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();

        // Look up existing session, or create one lazily.
        let session_ref = {
            let guard = self.sessions.lock().await;
            guard.get(&session_id).cloned()
        };

        let session_ref = match session_ref {
            Some(r) => r,
            None => {
                // No session yet — create one.
                let (_, r) = self.create_remote_session_inner(None).await?;
                r
            }
        };

        session_ref.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();
        let session_ref = {
            let guard = self.sessions.lock().await;
            guard.get(&session_id).cloned()
        };

        if let Some(session_ref) = session_ref {
            let _ = session_ref.cancel().await;
        }
        Ok(())
    }

    async fn create_delegation_session(
        &self,
        cwd: Option<String>,
    ) -> Result<(String, SessionActorRef), Error> {
        self.create_remote_session_inner(cwd).await
    }

    fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.event_fanout.subscribe()
    }

    fn event_fanout(&self) -> &Arc<EventFanout> {
        &self.event_fanout
    }

    fn emit_event(&self, session_id: &str, kind: AgentEventKind) {
        let envelope = EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: session_id.to_string(),
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        });
        self.event_fanout.publish(envelope);
    }

    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        Arc::new(DefaultAgentRegistry::new())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
