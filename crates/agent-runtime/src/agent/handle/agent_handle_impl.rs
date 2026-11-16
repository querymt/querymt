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
