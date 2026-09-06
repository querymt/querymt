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

    /// Extension/control-plane attach entry (`remote/attach_session`, UI attach).
    ///
    /// Delegates to the connection coordinator (plan §2) so every caller shares
    /// one recovery algorithm: single-flight per session, scoped DHT lookup,
    /// node-manager resume fallback, handoff resolution, bounded health check,
    /// merged bookmark persistence. An already-installed healthy attachment is
    /// reused instead of re-attaching (previously this re-spawned the relay and
    /// overwrote the registry entry on every call).
    #[cfg(feature = "remote")]
    pub(crate) async fn attach_remote_session_for_ext(
        &self,
        node_id: &str,
        session_id: &str,
        handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
    ) -> Result<serde_json::Value, Error> {
        let _connected = self
            .connect_remote_session(
                session_id,
                RemoteConnectOptions {
                    node_hint: Some(node_id),
                    reason: RemoteConnectReason::ExtensionAttach,
                    replace: RemoteReplacePolicy::ReuseIfPresent,
                    handoff,
                },
            )
            .await
            .map_err(|err| err.to_acp_error())?;

        self.build_remote_attach_snapshot(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.message))
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
