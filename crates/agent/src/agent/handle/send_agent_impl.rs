use super::*;

/// SendAgent implementation for LocalAgentHandle
///
/// All methods delegate to either the kameo session registry or the shared config.
/// This replaces the `impl SendAgent for QueryMTAgent` from protocol.rs.
#[async_trait]
impl SendAgent for LocalAgentHandle {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        self.handle_initialize(req).await
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        self.handle_authenticate(req).await
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        self.new_session(req).await
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = self.session_ref_for_agent_session(&session_id).await?;
        session_ref.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();
        let Ok(session_ref) = self.session_ref_for_agent_session(&session_id).await else {
            return Ok(());
        };
        session_ref.cancel().await.map_err(Error::from)
    }

    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        self.load_session(req).await
    }

    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        self.handle_list_sessions(req).await
    }

    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        self.handle_fork_session(req).await
    }

    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        self.handle_resume_session(req).await
    }

    async fn close_session(&self, req: CloseSessionRequest) -> Result<CloseSessionResponse, Error> {
        self.handle_close_session(req).await
    }

    async fn delete_session(
        &self,
        req: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, Error> {
        self.handle_delete_session(req).await
    }

    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        self.handle_set_session_model(req).await
    }

    async fn set_session_mode(
        &self,
        req: crate::acp::protocol::SetSessionModeRequest,
    ) -> Result<crate::acp::protocol::SetSessionModeResponse, Error> {
        self.handle_set_session_mode(req).await
    }

    async fn set_session_config_option(
        &self,
        req: crate::acp::protocol::SetSessionConfigOptionRequest,
    ) -> Result<crate::acp::protocol::SetSessionConfigOptionResponse, Error> {
        self.handle_set_session_config_option(req).await
    }

    async fn ext_method(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        self.handle_ext_method(req).await
    }

    #[tracing::instrument(name = "acp.ext_notification", skip_all)]
    async fn ext_notification(&self, notif: ExtNotification) -> Result<(), Error> {
        self.handle_ext_notification(notif).await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
