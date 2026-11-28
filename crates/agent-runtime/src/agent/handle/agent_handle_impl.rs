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
        if let Err(error) = self
            .session_materializer
            .finalize_session_with_index(&prepared, false)
            .await
        {
            prepared.actor_ref.kill();
            if let Err(rollback_error) = self
                .config
                .provider
                .history_store()
                .delete_session(&prepared.session_id)
                .await
            {
                tracing::warn!(session_id = %prepared.session_id, %rollback_error, "failed to roll back unregistered delegation session");
            }
            return Err(error);
        }
        let session_ref =
            crate::agent::session_registry::register_prepared_session(&self.registry, &prepared)
                .await;
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
        self.create_delegation_session_in_workspace(
            PreallocatedWorkspace::Fresh(workspace),
            parent_session_id,
        )
        .await
    }

    /// Opens a delegate session in an existing workspace whose owner was
    /// stopped by `WorktreeOps::begin_handoff`. Ownership moves to the new
    /// session with the pending handoff's transfer before its actor starts;
    /// the session is a delegate of `parent_session_id` (fork origin
    /// `delegation`), not a `worktree` child.
    pub async fn create_handoff_delegation_session(
        &self,
        workspace: crate::worktree::WorkspaceBinding,
        parent_session_id: String,
        pending: crate::worktree::PendingHandoff,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        self.create_delegation_session_in_workspace(
            PreallocatedWorkspace::Handoff {
                binding: workspace,
                pending: Box::new(pending),
            },
            parent_session_id,
        )
        .await
    }

    async fn create_delegation_session_in_workspace(
        &self,
        workspace: PreallocatedWorkspace,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        let cwd = match &workspace {
            PreallocatedWorkspace::Fresh(binding) => binding.effective_cwd.clone(),
            PreallocatedWorkspace::Handoff { binding, .. } => binding.effective_cwd.clone(),
        };
        // For a handoff, what is needed to return the workspace if the
        // session fails after ownership moved to it.
        let handoff = match &workspace {
            PreallocatedWorkspace::Fresh(_) => None,
            PreallocatedWorkspace::Handoff { binding, pending } => {
                Some((binding.clone(), pending.previous().to_owned()))
            }
        };
        let mut meta = serde_json::Map::new();
        meta.insert("parent_session_id".to_string(), parent_session_id.into());
        let req = NewSessionRequest::new(cwd).meta(meta);
        let prepared = Box::pin(
            self.session_materializer
                .prepare_new_session_with_workspace(req, Some(workspace)),
        )
        .await?;
        if let Err(error) = self.session_materializer.finalize_session(&prepared).await {
            prepared.actor_ref.kill();
            // Ownership already moved to the never-registered successor:
            // give the workspace back to the previous owner (or report
            // `handoff_incomplete`), as a failed actor start does.
            return Err(match handoff {
                Some((binding, previous)) => {
                    self.session_materializer
                        .roll_back_handoff_successor(
                            &binding,
                            &prepared.session_id,
                            &previous,
                            error,
                        )
                        .await
                }
                None => error,
            });
        }
        let session_ref =
            crate::agent::session_registry::register_prepared_session(&self.registry, &prepared)
                .await;
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
    async fn confined_child_finalization_failure_rolls_back_before_export() {
        let (plugins, _plugins_dir) = crate::test_utils::empty_plugin_registry().unwrap();
        let storage_dir = tempfile::tempdir().unwrap();
        let path = storage_dir.path().join("test.sqlite");
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(path.clone())
                .await
                .unwrap(),
        );
        let config = Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::new(
                Arc::new(plugins),
                storage,
                querymt::LLMParams::new().provider("mock").model("mock"),
            )
            .build(),
        );
        let parent = config
            .provider
            .history_store()
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let handle = LocalAgentHandle::from_config(config);
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TRIGGER fail_child_setup BEFORE INSERT ON event_journal BEGIN SELECT RAISE(ABORT, 'fixture journal failure'); END;").unwrap();
        let result = handle
            .create_confined_delegation_session(parent.public_id, &TurnAuthority::Local)
            .await;
        assert!(result.is_err());
        assert!(format!("{:?}", result.err().unwrap()).contains("fixture journal failure"));
        assert!(handle.registry.lock().await.is_empty());
        let rows: i64 = db
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn confined_child_creates_no_workspace_and_refuses_stale_origin() {
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let store = Arc::new(querymt_remote::MeshAccessStore::new(
            querymt_remote::MeshStateStore::shared(
                &fixture._tempdir.path().join("mesh_state.json"),
            )
            .unwrap(),
        ));
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
                    shell: false,
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
        let (child, session_ref) = fixture
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
        let SessionActorRef::Local(actor) = session_ref else {
            panic!("expected local child");
        };
        actor.kill();
        actor.wait_for_shutdown().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if fixture.handle.registry.lock().await.get(&child).is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("confined child evicted on shutdown");
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
