//! `querymt/workspace/files`: fuzzy, bounded query of a workspace index.

use super::utils::ext_json_response;
use super::*;
use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse};
use crate::workspace_files::{
    self, Target, WorkspaceFilesRequest, WorkspaceFilesResponse, cache, clamp_limit,
};
use std::path::PathBuf;
use std::time::Duration;

/// How long a `cwd`-only query waits for an index to be built before it
/// reports `not_ready`. The build continues in the workspace manager.
const COLD_INDEX_WAIT: Duration = Duration::from_millis(250);

fn invalid_params(message: impl std::fmt::Display) -> Error {
    Error::invalid_params().data(serde_json::json!({ "message": message.to_string() }))
}

/// Run the CPU-bound ranking off the async worker threads.
async fn rank_blocking(
    workspace_root: String,
    cwd: Option<String>,
    files: Vec<crate::index::FileIndexEntry>,
    query: String,
    limit: usize,
) -> Result<WorkspaceFilesResponse, Error> {
    tokio::task::spawn_blocking(move || {
        workspace_files::query_index(&workspace_root, cwd.as_deref(), &files, &query, limit)
    })
    .await
    .map_err(|error| Error::internal_error().data(error.to_string()))
}

/// Fetch a session's index through its actor, treating "not built yet" as
/// `None`. Remote indexes are cached; local ones are lock-free and always fresh.
async fn fetch_index(
    session_id: String,
    session_ref: &SessionActorRef,
) -> Result<Option<Arc<GetFileIndexResponse>>, crate::error::AgentError> {
    match session_ref.get_file_index().await {
        Ok(index) => {
            let index = Arc::new(index);
            if session_ref.is_remote() {
                cache::insert(&session_id, Arc::clone(&index));
            }
            Ok(Some(index))
        }
        Err(FileProxyError::IndexNotReady) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

impl LocalAgentHandle {
    pub(super) async fn handle_ext_workspace_files(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let request: WorkspaceFilesRequest =
            serde_json::from_str(req.params.get()).map_err(invalid_params)?;
        let limit = clamp_limit(request.limit);
        let query = request.query.clone().unwrap_or_default();
        let response = match request.target().map_err(invalid_params)? {
            Target::Session(session_id) => {
                self.workspace_files_for_session(&session_id, query, limit)
                    .await?
            }
            Target::Cwd(cwd) => self.workspace_files_for_cwd(cwd, query, limit).await?,
        };
        ext_json_response(&response)
    }

    async fn workspace_files_for_session(
        &self,
        session_id: &str,
        query: String,
        limit: usize,
    ) -> Result<WorkspaceFilesResponse, Error> {
        let index = match cache::get(session_id) {
            Some(index) => Some(index),
            None => self.session_file_index(session_id).await?,
        };
        let Some(index) = index else {
            return Ok(WorkspaceFilesResponse::not_ready());
        };
        rank_blocking(
            index.workspace_root.clone(),
            index.cwd.clone(),
            index.files.clone(),
            query,
            limit,
        )
        .await
    }

    #[cfg(feature = "remote")]
    async fn session_file_index(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<GetFileIndexResponse>>, Error> {
        use super::session_operation::{SessionOperation, SessionOperationError};
        let id = session_id.to_owned();
        self.execute_session_operation(
            session_id,
            SessionOperation::ReadWorkspaceFiles,
            |session_ref| Box::pin(fetch_index(id.clone(), session_ref)),
        )
        .await
        .map_err(|error| match error {
            SessionOperationError::NotFound { .. } => {
                Error::resource_not_found(Some(session_id.to_owned()))
            }
            other => other.into_acp_error(),
        })
    }

    #[cfg(not(feature = "remote"))]
    async fn session_file_index(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<GetFileIndexResponse>>, Error> {
        let session_ref = self
            .session_ref_for_agent_session(session_id)
            .await
            .map_err(|error| {
                if error.code == crate::acp::protocol::ErrorCode::InvalidParams {
                    Error::resource_not_found(Some(session_id.to_owned()))
                } else {
                    error
                }
            })?;
        fetch_index(session_id.to_owned(), &session_ref)
            .await
            .map_err(Error::from)
    }

    async fn workspace_files_for_cwd(
        &self,
        cwd: PathBuf,
        query: String,
        limit: usize,
    ) -> Result<WorkspaceFilesResponse, Error> {
        let is_dir = tokio::fs::metadata(&cwd)
            .await
            .is_ok_and(|metadata| metadata.is_dir());
        if !is_dir {
            return Err(invalid_params("cwd must be an existing directory"));
        }
        let root = crate::index::resolve_workspace_root(&cwd);
        let handle = match self
            .workspace_manager_actor()
            .ask(crate::index::GetOrCreate { root })
            .mailbox_timeout(
                crate::index::WorkspaceIndexManagerActor::GET_OR_CREATE_MAILBOX_TIMEOUT,
            )
            .reply_timeout(COLD_INDEX_WAIT)
            .send()
            .await
        {
            Ok(handle) => handle,
            Err(kameo::error::SendError::Timeout(_)) => {
                return Ok(WorkspaceFilesResponse::not_ready());
            }
            Err(error) => return Err(Error::internal_error().data(error.to_string())),
        };
        let Some(index) = handle.file_index() else {
            return Ok(WorkspaceFilesResponse::not_ready());
        };
        rank_blocking(
            index.root.display().to_string(),
            Some(cwd.display().to_string()),
            index.files,
            query,
            limit,
        )
        .await
    }
}
