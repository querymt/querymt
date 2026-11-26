#[cfg(feature = "remote")]
use super::remote_connect::{RemoteConnectOptions, RemoteConnectReason, RemoteReplacePolicy};
use super::utils::ext_json_response;
use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_remote_sessions(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::RemoteSessionsRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::list_remote_sessions(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_remote_create_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::CreateRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::create_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_remote_attach_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::AttachRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::attach_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }

    /// Extension/control-plane attach (`remote/attachSession`,
    /// `remote/createSession`): connect through the coordinator, then load.
    ///
    /// A first-time attach reuses an already-installed healthy attachment;
    /// the attach of an already-bookmarked session is a repair reconnect and
    /// always reinstalls (decided by the coordinator under its gate, see
    /// [`RemoteConnectReason::ExtensionAttach`]).
    #[cfg(feature = "remote")]
    pub(crate) fn open_remote_session_for_ext<'a>(
        &'a self,
        node_id: &'a str,
        session_id: &'a str,
        handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
        window: Option<crate::session::history_window::LoadWindow>,
        node_manager: Option<super::model_registry::NodeManagerRef>,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<super::model_registry::OpenedRemoteSession, Error>,
    > {
        self.open_remote_session(
            session_id,
            node_id,
            RemoteConnectOptions {
                node_hint: Some(node_id),
                peer_label: None,
                preferred_scope: None,
                reason: RemoteConnectReason::ExtensionAttach,
                replace: RemoteReplacePolicy::ReuseIfPresent,
                handoff,
                windowed: window.is_some(),
                fork_origin: None,
            },
            window,
            node_manager,
        )
    }

    pub(super) async fn handle_ext_remote_dismiss_session(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::remote::DismissRemoteSessionRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::remote::dismiss_remote_session(self, parsed).await?;
        ext_json_response(&response)
    }
}
