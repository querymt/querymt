use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_capabilities(&self) -> Result<ExtResponse, Error> {
        let mut capabilities = crate::control::capabilities::get_capabilities(self);
        if self.has_mesh_access_store() {
            capabilities.mesh_access_version = Some(1);
            capabilities
                .methods
                .extend(super::ext_mesh::ACCESS_METHODS.map(str::to_owned));
            capabilities
                .notifications
                .push(crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED.to_owned());
        }
        super::utils::ext_json_response(&capabilities)
    }

    /// Dispatch an extension method to its handler.
    ///
    /// The handler future is built and boxed in its own small function (see
    /// [`ext_method_handler`]), never in this one: an unoptimized build gives
    /// a single `match` over awaited handlers one stack slot per arm, which
    /// made this frame the sum of every handler's state (673 KiB) and
    /// overflowed 2 MiB threads on the remote attach path.
    pub(super) fn handle_ext_method(&self, req: ExtRequest) -> ExtHandlerFuture<'_> {
        match ext_method_handler(req.method.as_ref()) {
            Some(handler) => handler(self, req),
            None => Box::pin(std::future::ready(Err(Error::method_not_found()))),
        }
    }

    /// The `acp.ext_notification` span is opened by the `SendAgent` entry point.
    pub(super) async fn handle_ext_notification(
        &self,
        _notif: ExtNotification,
    ) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }
}

/// A boxed extension handler future.
pub(super) type ExtHandlerFuture<'a> =
    futures_util::future::BoxFuture<'a, Result<ExtResponse, Error>>;

/// Builds and boxes one handler's future. Each entry is its own function, so
/// only that handler's state is ever on the stack while it is created.
type ExtHandler = for<'a> fn(&'a LocalAgentHandle, ExtRequest) -> ExtHandlerFuture<'a>;

/// The handler of an extension method, or `None` when it is unknown.
fn ext_method_handler(method: &str) -> Option<ExtHandler> {
    use crate::agent::messages::InputDelivery;
    let handler: ExtHandler = match method {
        "querymt/models" => |h, _| Box::pin(h.handle_ext_models()),
        "querymt/profiles" => |h, req| Box::pin(h.handle_ext_profiles(req)),
        "querymt/agents/list" => |h, req| Box::pin(h.handle_ext_agents_list(req)),
        "querymt/agents/add" => |h, req| Box::pin(h.handle_ext_agents_add(req)),
        "querymt/agents/remove" => |h, req| Box::pin(h.handle_ext_agents_remove(req)),
        "querymt/profile/agents" => |h, req| Box::pin(h.handle_ext_profile_agents(req)),
        "querymt/profile/setActive" => |h, req| Box::pin(h.handle_ext_set_active_profile(req)),
        "querymt/session/setDelegateModel" => {
            |h, req| Box::pin(h.handle_ext_set_delegate_model(req))
        }
        "querymt/session/delegateModels" => |h, req| Box::pin(h.handle_ext_delegate_models(req)),
        "querymt/refreshModels" => |h, _| Box::pin(h.handle_ext_refresh_models()),
        "querymt/modelInfo" => |h, req| Box::pin(h.handle_ext_model_info(req)),
        "querymt/chat" | "querymt/tokenCount" => |_, req| {
            Box::pin(std::future::ready(Err(Error::from(
                crate::error::AgentError::MethodNotImplemented {
                    method: req.method.to_string(),
                },
            ))))
        },
        "querymt/auth/status" => |h, req| Box::pin(h.handle_ext_auth_status(req)),
        "querymt/auth/start" => |h, req| Box::pin(h.handle_ext_auth_start(req)),
        "querymt/auth/complete" => |h, req| Box::pin(h.handle_ext_auth_complete(req)),
        "querymt/auth/logout" => |h, req| Box::pin(h.handle_ext_auth_logout(req)),
        "querymt/auth/setApiToken" => |h, req| Box::pin(h.handle_ext_auth_set_api_token(req)),
        "querymt/auth/clearApiToken" => |h, req| Box::pin(h.handle_ext_auth_clear_api_token(req)),
        "querymt/auth/setMethod" => |h, req| Box::pin(h.handle_ext_auth_set_method(req)),
        "querymt/auth/account" => |h, req| Box::pin(h.handle_ext_auth_account(req)),
        "querymt/session/steer" => {
            |h, req| Box::pin(h.handle_ext_session_submit_input(req, InputDelivery::Steer))
        }
        "querymt/session/queue" => {
            |h, req| Box::pin(h.handle_ext_session_submit_input(req, InputDelivery::Queue))
        }
        "querymt/session/discardQueuedInput" => {
            |h, req| Box::pin(h.handle_ext_session_discard_queued_input(req))
        }
        "querymt/session/runtimeState" => {
            |h, req| Box::pin(h.handle_ext_session_runtime_state(req))
        }
        "querymt/session/history" => |h, req| Box::pin(h.handle_ext_session_history(req)),
        "querymt/session/toolContent" => |h, req| Box::pin(h.handle_ext_session_tool_content(req)),
        "querymt/session/undo" => |h, req| Box::pin(h.handle_ext_session_undo(req)),
        "querymt/session/redo" => |h, req| Box::pin(h.handle_ext_session_redo(req)),
        "querymt/session/undoStack" => |h, req| Box::pin(h.handle_ext_session_undo_stack(req)),
        "querymt/session/getLifecycle" => {
            |h, req| Box::pin(h.handle_ext_session_get_lifecycle(req))
        }
        "querymt/session/setLifecycle" => {
            |h, req| Box::pin(h.handle_ext_session_set_lifecycle(req))
        }
        "querymt/session/setPinned" => |h, req| Box::pin(h.handle_ext_session_set_pinned(req)),
        "querymt/worktree/create" => |h, req| Box::pin(h.handle_ext_worktree_create(req)),
        "querymt/worktree/handoff" => |h, req| Box::pin(h.handle_ext_worktree_handoff(req)),
        "querymt/worktree/list" => |h, _| Box::pin(h.handle_ext_worktree_list()),
        "querymt/worktree/status" => |h, req| Box::pin(h.handle_ext_worktree_status(req)),
        "querymt/worktree/result" => |h, req| Box::pin(h.handle_ext_worktree_result(req)),
        "querymt/worktree/publish" => |h, req| Box::pin(h.handle_ext_worktree_publish(req)),
        "querymt/worktree/merge" => |h, req| Box::pin(h.handle_ext_worktree_merge(req)),
        "querymt/worktree/remove" => |h, req| Box::pin(h.handle_ext_worktree_remove(req)),
        "querymt/worktree/recover" => |h, req| Box::pin(h.handle_ext_worktree_recover(req)),
        "querymt/worktree/archive" => |h, req| Box::pin(h.handle_ext_worktree_archive(req)),
        "querymt/capabilities" => |h, _| Box::pin(h.handle_ext_capabilities()),
        "querymt/mesh/status" => |h, _| Box::pin(h.handle_ext_mesh_status()),
        "querymt/mesh/join" => |h, req| Box::pin(h.handle_ext_mesh_join(req)),
        "querymt/mesh/nodes" => |h, _| Box::pin(h.handle_ext_mesh_nodes()),
        m if super::ext_mesh::ACCESS_METHODS.contains(&m) => {
            |h, req| Box::pin(h.handle_ext_mesh_access(req))
        }
        "querymt/mesh/createInvite" => |h, req| Box::pin(h.handle_ext_mesh_create_invite(req)),
        "querymt/mesh/listInvites" => |h, _| Box::pin(h.handle_ext_mesh_list_invites()),
        "querymt/mesh/revokeInvite" => |h, req| Box::pin(h.handle_ext_mesh_revoke_invite(req)),
        "querymt/remote/sessions" => |h, req| Box::pin(h.handle_ext_remote_sessions(req)),
        "querymt/remote/createSession" => {
            |h, req| Box::pin(h.handle_ext_remote_create_session(req))
        }
        "querymt/remote/attachSession" => {
            |h, req| Box::pin(h.handle_ext_remote_attach_session(req))
        }
        "querymt/remote/dismissSession" => {
            |h, req| Box::pin(h.handle_ext_remote_dismiss_session(req))
        }
        "querymt/schedules/create" => |h, req| Box::pin(h.handle_ext_schedule_create(req)),
        "querymt/schedules/list" => |h, req| Box::pin(h.handle_ext_schedule_list(req)),
        "querymt/schedules/get" => |h, req| Box::pin(h.handle_ext_schedule_get(req)),
        "querymt/schedules/pause" => |h, req| Box::pin(h.handle_ext_schedule_pause(req)),
        "querymt/schedules/resume" => |h, req| Box::pin(h.handle_ext_schedule_resume(req)),
        "querymt/schedules/trigger" => |h, req| Box::pin(h.handle_ext_schedule_trigger(req)),
        "querymt/schedules/delete" => |h, req| Box::pin(h.handle_ext_schedule_delete(req)),
        "querymt/updatePlugins" => |h, _| Box::pin(h.handle_ext_update_plugins()),
        _ => return None,
    };
    Some(handler)
}
