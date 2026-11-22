use super::*;

// ══════════════════════════════════════════════════════════════════════════
//  Delegation session creation for LocalAgentHandle
// ══════════════════════════════════════════════════════════════════════════

impl LocalAgentHandle {
    pub async fn create_delegation_session(
        &self,
        cwd: Option<String>,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        let cwd_path = cwd.map(std::path::PathBuf::from).unwrap_or_default();
        let mut meta = serde_json::Map::new();
        meta.insert(
            "parent_session_id".to_string(),
            serde_json::Value::String(parent_session_id),
        );
        let req = NewSessionRequest::new(cwd_path).meta(meta);

        // Use the 3-phase materialization pattern (no registry lock held during DB/actor work)
        let resp = self.new_session(req).await?;
        let session_id = resp.session_id.to_string();
        let session_ref = self.registry.lock().await;
        let session_ref = session_ref.get(&session_id).cloned().ok_or_else(|| {
            Error::internal_error().data("Session created but not found in registry")
        })?;

        Ok((session_id, session_ref))
    }

    #[cfg(feature = "remote")]
    pub(crate) fn supports_confined_delegation(&self) -> bool {
        self.profiles().is_none()
            && self.config.auth_methods.is_empty()
            && self.session_materializer.supports_restricted_cold_resume()
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn create_confined_delegation_session(
        &self,
        parent_session_id: String,
        authority: &crate::agent::session_actor::TurnAuthority,
    ) -> Result<(String, SessionActorRef), Error> {
        if !self.supports_confined_delegation() {
            return Err(Error::from(crate::error::AgentError::MeshAccessDenied));
        }
        authority.check().await.map_err(Error::from)?;
        let req = NewSessionRequest::new(std::path::PathBuf::new()).meta(
            serde_json::Map::from_iter([("parent_session_id".into(), parent_session_id.into())]),
        );
        let prepared = self.session_materializer.prepare_new_session(req).await?;
        let session_ref = self
            .registry_lock()
            .await
            .register_prepared_session(&prepared)
            .await;
        self.session_materializer
            .finalize_session_with_index(&prepared, false)
            .await?;
        Ok((prepared.session_id, session_ref))
    }

    pub async fn set_session_lifecycle(
        &self,
        session_id: &str,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> crate::session::error::SessionResult<crate::session::lifecycle::SessionLifecycle> {
        self.patch_session_lifecycle_unloaded(session_id, &patch)
            .await
    }

    pub fn supports_local_workspaces(&self) -> bool {
        // Ready only when this target can actually bind and run a managed
        // workspace. A target without its own workspace service would
        // reject the preallocated checkout after the parent had already
        // provisioned it, leaking an unowned Ready allocation.
        self.config.workspace_service().is_some()
    }

    pub async fn create_isolated_delegation_session(
        &self,
        workspace: crate::worktree::WorkspaceBinding,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        let mut meta = serde_json::Map::new();
        meta.insert("parent_session_id".to_string(), parent_session_id.into());
        let req = NewSessionRequest::new(workspace.effective_cwd.clone()).meta(meta);
        let prepared = Box::pin(
            self.session_materializer
                .prepare_new_session_with_workspace(req, Some(workspace)),
        )
        .await?;
        let session_ref = self
            .registry_lock()
            .await
            .register_prepared_session(&prepared)
            .await;
        self.session_materializer
            .finalize_session(&prepared)
            .await?;
        Ok((prepared.session_id, session_ref))
    }

    /// The runtime's event stream.
    pub fn event_fanout(&self) -> &Arc<EventFanout> {
        self.config.event_sink.fanout()
    }
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use super::*;
    use crate::agent::session_actor::{PeerTurn, TurnAuthority};
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
    use querymt_remote::{
        NodeId,
        peer_access::{GrantStore, PeerGrant, PeerGrantKey},
    };

    #[tokio::test]
    async fn confined_child_creates_no_workspace_and_refuses_stale_origin() {
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let store = Arc::new(
            querymt_agent_sqlite::mesh_access::SqliteMeshAccess::open_exclusive(
                fixture._tempdir.path().join("peer-access.db"),
            )
            .await
            .unwrap(),
        );
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(libp2p::PeerId::random()),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let initial = store
            .replace_grant(
                &key,
                uuid::Uuid::nil(),
                PeerGrant::Custom {
                    sessions: true,
                    filesystem: false,
                    providers: vec![],
                },
            )
            .await
            .unwrap();
        let authority = TurnAuthority::Peer(PeerTurn::test_peer(
            key.clone(),
            initial.grant,
            store.clone(),
        ));
        let (env, _) = fixture.turn().await;
        let parent = env.session_id;
        let (child, _) = fixture
            .handle
            .create_confined_delegation_session(parent.clone(), &authority)
            .await
            .unwrap();
        let row = fixture
            .config
            .provider
            .history_store()
            .get_session(&child)
            .await
            .unwrap()
            .unwrap();
        assert!(row.cwd.is_none());
        store
            .replace_grant(&key, initial.revision, PeerGrant::None)
            .await
            .unwrap();
        assert!(
            fixture
                .handle
                .create_confined_delegation_session(parent, &authority)
                .await
                .is_err()
        );
    }
}
