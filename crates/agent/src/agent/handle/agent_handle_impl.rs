use super::*;

// ══════════════════════════════════════════════════════════════════════════
//  AgentHandle trait implementation for LocalAgentHandle
// ══════════════════════════════════════════════════════════════════════════

#[async_trait]
impl AgentHandle for LocalAgentHandle {
    async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error> {
        SendAgent::new_session(self, req).await
    }

    async fn prompt(&self, req: PromptRequest) -> std::result::Result<PromptResponse, Error> {
        SendAgent::prompt(self, req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> std::result::Result<(), Error> {
        SendAgent::cancel(self, notif).await
    }

    async fn load_session(
        &self,
        req: LoadSessionRequest,
    ) -> std::result::Result<LoadSessionResponse, Error> {
        SendAgent::load_session(self, req).await
    }

    async fn create_delegation_session(
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

    fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.config.event_sink.fanout().subscribe()
    }

    fn event_fanout(&self) -> &Arc<EventFanout> {
        self.config.event_sink.fanout()
    }

    fn emit_event(&self, session_id: &str, kind: AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    #[cfg(feature = "remote")]
    fn set_mesh_handle(&self, mesh: crate::agent::remote::MeshHandle) {
        self.set_mesh(mesh);
    }
}
