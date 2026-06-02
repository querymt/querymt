use super::*;

// ── Model registry convenience ────────────────────────────────────────────

impl LocalAgentHandle {
    /// Invalidate the model cache, forcing a fresh enumeration on next call.
    ///
    /// Delegates to `ModelInventory::invalidate_all`.
    pub async fn invalidate_model_cache(&self) {
        self.model_inventory.invalidate_all().await;
    }

    /// Attempt to re-attach a remote session from a persisted bookmark.
    ///
    /// Performs a DHT lookup for the session, and if found, attaches it to the
    /// local registry (spawning an EventRelayActor and sending SubscribeEvents).
    ///
    /// Returns `Ok(session_ref)` on success, or an error if the mesh is not
    /// active, the session is not found in the DHT, or the attach fails.
    #[cfg(feature = "remote")]
    pub async fn reattach_from_bookmark(
        &self,
        bookmark: &crate::session::store::RemoteSessionBookmark,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        let mesh = self
            .mesh()
            .ok_or(crate::error::AgentError::MeshNotBootstrapped)?;

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut remote_ref = None;
        let mut matched_scope = None;
        for scope in runtime.active_scopes() {
            let dht_name =
                crate::agent::remote::scope::scoped_session(&scope, &bookmark.session_id);
            let lookup = runtime
                .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name.clone())
                .await
                .map_err(|e| crate::error::AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })?;
            if let Some(found) = lookup {
                remote_ref = Some(found);
                matched_scope = Some(scope);
                break;
            }
        }
        let remote_ref =
            remote_ref.ok_or_else(|| crate::error::AgentError::RemoteSessionNotFound {
                details: format!(
                    "bookmarked session {} not found in DHT",
                    bookmark.session_id
                ),
            })?;

        let mut registry = self.registry.lock().await;
        let session_ref = registry
            .attach_remote_session(
                bookmark.session_id.clone(),
                remote_ref,
                bookmark.peer_label.clone(),
                Some(mesh),
                matched_scope,
                Some(bookmark.node_id.clone()),
            )
            .await;

        Ok(session_ref)
    }

    /// Like [`reattach_from_bookmark`] but uses a single DHT lookup with **no
    /// retries**.
    ///
    /// Intended for bulk bookmark reattach during session listing where we
    /// prefer a fast failure over spending ~1.75 s per stale bookmark.
    #[cfg(feature = "remote")]
    pub async fn reattach_from_bookmark_quick(
        &self,
        bookmark: &crate::session::store::RemoteSessionBookmark,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        let mesh = self
            .mesh()
            .ok_or(crate::error::AgentError::MeshNotBootstrapped)?;

        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let mut remote_ref = None;
        let mut matched_scope = None;
        for scope in runtime.active_scopes() {
            let dht_name =
                crate::agent::remote::scope::scoped_session(&scope, &bookmark.session_id);
            let lookup = runtime
                .lookup_actor_no_retry::<crate::agent::session_actor::SessionActor>(
                    dht_name.clone(),
                )
                .await
                .map_err(|e| crate::error::AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })?;
            if let Some(found) = lookup {
                remote_ref = Some(found);
                matched_scope = Some(scope);
                break;
            }
        }
        let remote_ref =
            remote_ref.ok_or_else(|| crate::error::AgentError::RemoteSessionNotFound {
                details: format!(
                    "bookmarked session {} not found in DHT",
                    bookmark.session_id
                ),
            })?;

        let mut registry = self.registry.lock().await;
        let session_ref = registry
            .attach_remote_session(
                bookmark.session_id.clone(),
                remote_ref,
                bookmark.peer_label.clone(),
                Some(mesh),
                matched_scope,
                Some(bookmark.node_id.clone()),
            )
            .await;

        Ok(session_ref)
    }

    /// Resolve a `SessionHandoff` into a concrete remote actor reference.
    ///
    /// - `DirectRemote` → return the embedded ref directly.
    /// - `LookupOnly` → DHT lookup for the session.
    /// - `NoAttachPath` → error.
    #[cfg(feature = "remote")]
    pub async fn resolve_handoff(
        &self,
        session_id: &str,
        handoff: crate::agent::remote::node_manager::SessionHandoff,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        agent_client_protocol::Error,
    > {
        use crate::error::AgentError;

        match handoff {
            crate::agent::remote::node_manager::SessionHandoff::DirectRemote { session_ref } => {
                Ok(session_ref)
            }
            crate::agent::remote::node_manager::SessionHandoff::LookupOnly => {
                let mesh = self.mesh().ok_or_else(|| {
                    agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped)
                })?;
                let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                for scope in runtime.active_scopes() {
                    let dht_name = crate::agent::remote::scope::scoped_session(&scope, session_id);
                    if let Some(found) = runtime
                        .lookup_actor::<crate::agent::session_actor::SessionActor>(dht_name)
                        .await
                        .map_err(|e| {
                            agent_client_protocol::Error::from(AgentError::SwarmLookupFailed {
                                key: session_id.to_string(),
                                reason: e.to_string(),
                            })
                        })?
                    {
                        return Ok(found);
                    }
                }
                Err(agent_client_protocol::Error::from(
                    AgentError::RemoteSessionNotFound {
                        details: format!(
                            "session {} registered but not found in DHT after lookup",
                            session_id
                        ),
                    },
                ))
            }
            crate::agent::remote::node_manager::SessionHandoff::NoAttachPath => Err(
                agent_client_protocol::Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} was created but the remote node cannot provide an attach path",
                        session_id
                    ),
                }),
            ),
        }
    }

    /// Build a lightweight `SessionLoadSnapshot` from the locally-attached
    /// remote session's event stream. Used by the ACP extension path to
    /// return history to mobile clients.
    #[cfg(feature = "remote")]
    pub async fn build_remote_attach_snapshot(
        &self,
        session_id: &str,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        use crate::error::AgentError;

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        };
        let Some(session_ref) = session_ref else {
            return Err(agent_client_protocol::Error::from(
                AgentError::SessionNotFound {
                    session_id: session_id.to_string(),
                },
            ));
        };

        let events = session_ref.get_event_stream().await.unwrap_or_default();
        log::info!(
            "remote attach snapshot built from attached session ref: session_id={}, events={}",
            session_id,
            events.len()
        );
        let cursor = crate::session::cursor_from_events(&events);
        let audit = crate::session::projection::AuditView {
            session_id: session_id.to_string(),
            events,
            tasks: Vec::new(),
            intent_snapshots: Vec::new(),
            decisions: Vec::new(),
            progress_entries: Vec::new(),
            artifacts: Vec::new(),
            delegations: Vec::new(),
            generated_at: time::OffsetDateTime::now_utc(),
        };

        Ok(serde_json::json!({
            "audit": audit,
            "cursor": cursor,
        }))
    }
}
