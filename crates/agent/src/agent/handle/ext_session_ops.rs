use super::utils::{ext_json_response, format_prefixed_error_chain};
use super::*;

#[derive(Debug, serde::Deserialize)]
struct UndoSessionRequest {
    session_id: String,
    message_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct SessionIdRequest {
    session_id: String,
}

#[derive(Debug, serde::Serialize)]
struct UndoStackFrameResponse {
    message_id: String,
}

#[derive(Debug, serde::Serialize)]
struct UndoSessionResponse {
    success: bool,
    message: Option<String>,
    reverted_files: Vec<String>,
    message_id: Option<String>,
    undo_stack: Vec<UndoStackFrameResponse>,
}

#[derive(Debug, serde::Serialize)]
struct RedoSessionResponse {
    success: bool,
    message: Option<String>,
    undo_stack: Vec<UndoStackFrameResponse>,
}

#[derive(Debug, serde::Serialize)]
struct UndoStackResponse {
    undo_stack: Vec<UndoStackFrameResponse>,
}

impl LocalAgentHandle {
    pub(super) async fn handle_ext_session_undo(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: UndoSessionRequest = parse_session_op_request(req)?;
        let profile_handle = self.profile_session_op_handle(&parsed.session_id).await?;
        let session_handle = profile_handle.as_deref().unwrap_or(self);
        let result = session_handle
            .undo(&parsed.session_id, &parsed.message_id)
            .await;
        let undo_stack = session_handle.load_ext_undo_stack(&parsed.session_id).await;

        match result {
            Ok(result) => ext_json_response(&UndoSessionResponse {
                success: true,
                message: None,
                reverted_files: result.reverted_files,
                message_id: Some(result.message_id),
                undo_stack,
            }),
            Err(err) => ext_json_response(&UndoSessionResponse {
                success: false,
                message: Some(err.to_string()),
                reverted_files: Vec::new(),
                message_id: None,
                undo_stack,
            }),
        }
    }

    pub(super) async fn handle_ext_session_redo(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: SessionIdRequest = parse_session_op_request(req)?;
        let profile_handle = self.profile_session_op_handle(&parsed.session_id).await?;
        let session_handle = profile_handle.as_deref().unwrap_or(self);
        let result = session_handle.redo(&parsed.session_id).await;
        let undo_stack = session_handle.load_ext_undo_stack(&parsed.session_id).await;

        match result {
            Ok(_) => ext_json_response(&RedoSessionResponse {
                success: true,
                message: None,
                undo_stack,
            }),
            Err(err) => ext_json_response(&RedoSessionResponse {
                success: false,
                message: Some(err.to_string()),
                undo_stack,
            }),
        }
    }

    pub(super) async fn handle_ext_session_undo_stack(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: SessionIdRequest = parse_session_op_request(req)?;
        let profile_handle = self.profile_session_op_handle(&parsed.session_id).await?;
        let session_handle = profile_handle.as_deref().unwrap_or(self);
        ext_json_response(&UndoStackResponse {
            undo_stack: session_handle.load_ext_undo_stack(&parsed.session_id).await,
        })
    }

    async fn profile_session_op_handle(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<LocalAgentHandle>>, Error> {
        let Some(profiles) = self.profiles() else {
            return Ok(None);
        };
        let Some(binding) = profiles.session_binding(session_id).await else {
            return Ok(None);
        };
        let runtime = profiles
            .runtime_for_profile(&binding.profile_id)
            .await
            .map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain(
                        &format!("Failed to load profile '{}'", binding.profile_id),
                        &err,
                    ),
                    "profileId": binding.profile_id,
                    "sessionId": session_id,
                }))
            })?;
        Ok(Some(runtime.agent().handle()))
    }

    async fn load_ext_undo_stack(&self, session_id: &str) -> Vec<UndoStackFrameResponse> {
        self.config
            .provider
            .history_store()
            .list_revert_states(session_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|frame| UndoStackFrameResponse {
                message_id: frame.message_id,
            })
            .collect()
    }
}

fn parse_session_op_request<T>(req: ExtRequest) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(req.params.get())
        .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e.to_string() })))
}
