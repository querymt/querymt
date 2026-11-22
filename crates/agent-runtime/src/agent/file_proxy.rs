//! Remote file proxy — types for proxying file index and file read requests
//! to remote sessions over the kameo mesh.
//!
//! Parallel to `undo.rs`, this module defines the error type and response
//! structs used by `GetFileIndex` and `ReadRemoteFile` session messages.

use querymt_agent_index::FileIndexEntry;
use serde::{Deserialize, Serialize};

/// Errors from remote file index/read operations.
///
/// `Serialize + Deserialize` so it can travel over the kameo mesh as a reply.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum FileProxyError {
    #[error("mesh filesystem access denied")]
    AccessDenied,
    #[error("No working directory configured")]
    NoWorkingDirectory,
    #[error("Workspace index not ready")]
    IndexNotReady,
    #[error("Path outside workspace: {0}")]
    PathOutsideWorkspace(String),
    #[error("Cannot resolve path: {0}")]
    PathResolution(String),
    #[error("Read error: {0}")]
    ReadError(String),
    #[error("Actor send error: {0}")]
    ActorSend(String),
    /// A classified remote transport failure (failure kind + delivery
    /// certainty), preserved for safe retry decisions by the operation layer.
    #[error("{0}")]
    Transport(querymt_remote::RemoteTransportFailure),
}

impl From<FileProxyError> for crate::error::AgentError {
    fn from(error: FileProxyError) -> Self {
        match error {
            FileProxyError::Transport(failure) => Self::from_transport_failure(failure),
            FileProxyError::AccessDenied => Self::MeshAccessDenied,
            other => Self::Internal(other.to_string()),
        }
    }
}

impl FileProxyError {
    /// Wrap a classified remote transport failure.
    pub fn from_transport_failure(f: querymt_remote::RemoteTransportFailure) -> Self {
        FileProxyError::Transport(f)
    }
}

// Do not expose hidden workspace files, including repository/config metadata.
#[cfg(feature = "remote")]
pub(crate) fn is_shareable_path(path: &std::path::Path) -> bool {
    use std::path::Component;
    !path.as_os_str().is_empty()
        && path.components().all(|component| matches!(component, Component::Normal(name) if !name.as_encoded_bytes().starts_with(b".")))
}

impl ReadRemoteFileResponse {
    pub(crate) fn from_blocks(blocks: Vec<querymt::chat::ToolResultPart>) -> Self {
        use base64::Engine as _;
        match blocks.into_iter().next() {
            Some(querymt::chat::ToolResultPart::Attachment(media)) => Self::Image {
                mime_type: media
                    .media_type()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                base64_data: match media.source() {
                    querymt::chat::MediaSource::Inline { data } => {
                        base64::engine::general_purpose::STANDARD.encode(data)
                    }
                    _ => String::new(),
                },
            },
            Some(querymt::chat::ToolResultPart::Text { text }) => Self::Text(text),
            None => Self::Binary,
        }
    }
}

/// Successful response from `GetFileIndex`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFileIndexResponse {
    /// File/directory entries relative to the workspace root.
    pub files: Vec<FileIndexEntry>,
    /// Unix timestamp when the index was generated.
    pub generated_at: u64,
    /// Workspace root path on the remote node (as a string for cross-OS compat).
    pub workspace_root: String,
}

/// Successful response from `ReadRemoteFile`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReadRemoteFileResponse {
    /// UTF-8 text content (rendered with line numbers via `render_read_output`).
    Text(String),
    /// Image file, base64-encoded.
    Image {
        mime_type: String,
        base64_data: String,
    },
    /// Non-text, non-image binary file that cannot be inlined.
    Binary,
}
