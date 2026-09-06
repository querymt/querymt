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
    /// merged bookmark persistence. First-time attaches reuse an
    /// already-installed healthy attachment instead of re-attaching; re-attaches
    /// of an already-bookmarked remote session are repair reconnects and always
    /// reinstall through lookup/resume/attach/health validation.
    #[cfg(feature = "remote")]
    pub(crate) async fn attach_remote_session_for_ext(
        &self,
        node_id: &str,
        session_id: &str,
        handoff: Option<crate::agent::remote::node_manager::SessionHandoff>,
    ) -> Result<serde_json::Value, Error> {
        // A persisted remote identity means this attach is a repair reconnect
        // (plan §12), not a first-time attach: reinstall the attachment instead
        // of early-returning a possibly-broken one.
        let reconnect = match self
            .config
            .provider
            .history_store()
            .get_remote_session_bookmark(session_id)
            .await
        {
            Ok(bookmark) => bookmark.is_some(),
            Err(error) => {
                log::warn!(
                    "attach_remote_session: bookmark lookup failed for {}: {}; \
                     falling back to first-time attach",
                    session_id,
                    error
                );
                false
            }
        };
        let (reason, replace) = if reconnect {
            (
                RemoteConnectReason::ExplicitReconnect,
                RemoteReplacePolicy::ReplaceCurrent,
            )
        } else {
            (
                RemoteConnectReason::ExtensionAttach,
                RemoteReplacePolicy::ReuseIfPresent,
            )
        };
        let _connected = self
            .connect_remote_session(
                session_id,
                RemoteConnectOptions {
                    node_hint: Some(node_id),
                    reason,
                    replace,
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
