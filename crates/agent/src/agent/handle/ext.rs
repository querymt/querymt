use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_capabilities(&self) -> Result<ExtResponse, Error> {
        super::utils::ext_json_response(&crate::control::capabilities::get_capabilities(self))
    }

    pub(super) async fn handle_ext_method(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        match req.method.as_ref() {
            "querymt/models" => self.handle_ext_models().await,
            "querymt/profiles" => self.handle_ext_profiles().await,
            "querymt/profile/agents" => self.handle_ext_profile_agents(req).await,
            "querymt/profile/setActive" => self.handle_ext_set_active_profile(req).await,
            "querymt/session/setDelegateModel" => self.handle_ext_set_delegate_model(req).await,
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
            "querymt/session/undo" => self.handle_ext_session_undo(req).await,
            "querymt/session/redo" => self.handle_ext_session_redo(req).await,
            "querymt/session/undoStack" => self.handle_ext_session_undo_stack(req).await,
            "querymt/capabilities" => self.handle_ext_capabilities().await,
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
            "querymt/schedules/create" => self.handle_ext_schedule_create(req).await,
            "querymt/schedules/list" => self.handle_ext_schedule_list(req).await,
            "querymt/schedules/get" => self.handle_ext_schedule_get(req).await,
            "querymt/schedules/pause" => self.handle_ext_schedule_pause(req).await,
            "querymt/schedules/resume" => self.handle_ext_schedule_resume(req).await,
            "querymt/schedules/trigger" => self.handle_ext_schedule_trigger(req).await,
            "querymt/schedules/delete" => self.handle_ext_schedule_delete(req).await,
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
