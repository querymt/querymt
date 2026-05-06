//! `RemoteAgentHandle` — `AgentHandle` implementation for remote agents.
//!
//! This replaces both `RemoteAgentStub` (SendAgent impl) and
//! `AgentActorHandle::Remote` from the legacy code path. Remote agent
//! interaction now goes through the unified `AgentHandle` trait.

use crate::agent::handle::AgentHandle;
use crate::agent::remote::SessionActorRef;
use crate::agent::remote::provider_host::{
    CancelProviderStreamRequest, GetProviderStreamStatus, ProviderHostActor,
};
use crate::delegation::{AgentRegistry, DefaultAgentRegistry};
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EphemeralEvent, EventEnvelope, EventOrigin};

use agent_client_protocol::schema::{
    CancelNotification, Error, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
};
use async_trait::async_trait;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

use super::mesh::MeshHandle;
use super::node_manager::SessionHandoff;

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

    async fn best_effort_cancel_remote_provider_stream(&self, session_id: &str) {
        let Some(node_id) = self.mesh.resolve_peer_node_id(&self.peer_label).await else {
            return;
        };
        let provider_host_name = crate::agent::remote::dht_name::provider_host(&node_id);
        let Ok(Some(provider_host)) = self
            .mesh
            .lookup_actor::<ProviderHostActor>(&provider_host_name)
            .await
        else {
            return;
        };

        let status = provider_host
            .ask(&GetProviderStreamStatus {
                session_id: session_id.to_string(),
                request_id: None,
            })
            .await
            .ok()
            .flatten();

        let request_id = status.as_ref().map(|status| status.request_id.clone());
        let _ = provider_host
            .ask(&CancelProviderStreamRequest {
                session_id: session_id.to_string(),
                request_id,
                reason: Some("remote prompt request failed".to_string()),
            })
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn cache_session_for_test(
        &self,
        session_id: String,
        session_ref: SessionActorRef,
    ) {
        self.sessions.lock().await.insert(session_id, session_ref);
    }

    /// Create a remote session and return `(session_id, SessionActorRef)`.
    ///
    /// Looks up the remote `RemoteNodeManager` via DHT, sends
    /// `CreateRemoteSession`, and uses the returned direct session ref.
    #[tracing::instrument(
        name = "delegation.remote.create_session",
        skip(self, cwd),
        fields(
            peer_label = %self.peer_label,
            session_id = tracing::field::Empty,
            dht_lookup_node_ms = tracing::field::Empty,
            create_session_ms = tracing::field::Empty,
            dht_lookup_session_ms = tracing::field::Empty,
        )
    )]
    async fn create_remote_session_inner(
        &self,
        cwd: Option<String>,
    ) -> Result<(String, SessionActorRef), Error> {
        use crate::agent::remote::{CreateRemoteSession, RemoteNodeManager};
        use crate::error::AgentError;

        let span = tracing::Span::current();

        let t0 = std::time::Instant::now();
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
        span.record("dht_lookup_node_ms", t0.elapsed().as_millis() as u64);

        let t1 = std::time::Instant::now();
        let resp = node_manager
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| Error::from(AgentError::RemoteActor(e.to_string())))?;
        span.record("create_session_ms", t1.elapsed().as_millis() as u64);

        let session_id = resp.session_id.clone();
        span.record("session_id", session_id.as_str());

        let session_ref = match resp.handoff {
            SessionHandoff::DirectRemote { session_ref } => {
                SessionActorRef::remote(session_ref, self.peer_label.clone())
            }
            SessionHandoff::LookupOnly => {
                let dht_name = crate::agent::remote::dht_name::session(&session_id);
                let lookup_backoff_ms: [u64; 4] = [0, 120, 300, 700];
                let mut remote_ref = None;
                let mut last_error = None;

                for delay_ms in lookup_backoff_ms {
                    if delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }

                    match self
                        .mesh
                        .lookup_actor_no_retry::<crate::agent::session_actor::SessionActor>(
                            dht_name.clone(),
                        )
                        .await
                    {
                        Ok(Some(found)) => {
                            remote_ref = Some(found);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => last_error = Some(e.to_string()),
                    }
                }

                let remote_ref = remote_ref.ok_or_else(|| {
                    if let Some(reason) = last_error {
                        Error::from(AgentError::SwarmLookupFailed {
                            key: dht_name.clone(),
                            reason,
                        })
                    } else {
                        Error::from(AgentError::RemoteSessionNotFound {
                            details: format!(
                                "remote session {} created but not yet discoverable via lookup",
                                session_id
                            ),
                        })
                    }
                })?;
                SessionActorRef::remote(remote_ref, self.peer_label.clone())
            }
            SessionHandoff::NoAttachPath => {
                return Err(Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "remote session {} was created but the remote node cannot provide a direct or lookup attach path",
                        session_id
                    ),
                }));
            }
        };

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
    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
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

        let result = session_ref.prompt(req).await;
        if result.is_err() {
            self.best_effort_cancel_remote_provider_stream(&session_id)
                .await;
        }
        result
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
        _parent_session_id: String,
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
