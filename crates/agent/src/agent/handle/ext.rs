use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_method(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        match req.method.as_ref() {
            "querymt/models" => self.handle_ext_models().await,
            "querymt/refreshModels" => self.handle_ext_refresh_models().await,
            "querymt/modelInfo" => self.handle_ext_model_info(req).await,
            "querymt/chat" | "querymt/tokenCount" => Err(Error::from(
                crate::error::AgentError::MethodNotImplemented {
                    method: req.method.to_string(),
                },
            )),
            "querymt/auth/status" => self.handle_ext_auth_status(req).await,
            "querymt/auth/start" => self.handle_ext_auth_start(req).await,
            "querymt/auth/complete" => self.handle_ext_auth_complete(req).await,
            "querymt/auth/logout" => self.handle_ext_auth_logout(req).await,
            "querymt/mesh/status" => self.handle_ext_mesh_status().await,
            "querymt/mesh/join" => self.handle_ext_mesh_join(req).await,
            "querymt/mesh/nodes" => self.handle_ext_mesh_nodes().await,
            "querymt/mesh/createInvite" => self.handle_ext_mesh_create_invite(req).await,
            "querymt/mesh/listInvites" => self.handle_ext_mesh_list_invites().await,
            "querymt/mesh/revokeInvite" => self.handle_ext_mesh_revoke_invite(req).await,
            "querymt/remote/sessions" => self.handle_ext_remote_sessions(req).await,
            "querymt/remote/createSession" => self.handle_ext_remote_create_session(req).await,
            "querymt/remote/attachSession" => self.handle_ext_remote_attach_session(req).await,
            "querymt/remote/dismissSession" => self.handle_ext_remote_dismiss_session(req).await,
            "querymt/updatePlugins" => self.handle_ext_update_plugins().await,
            _ => Err(Error::method_not_found()),
        }
    }

    #[tracing::instrument(name = "acp.ext_notification", skip_all)]
    pub(super) async fn handle_ext_notification(
        &self,
        _notif: ExtNotification,
    ) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }
}
