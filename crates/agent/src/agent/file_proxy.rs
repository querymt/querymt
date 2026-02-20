//! Remote file proxy — types for proxying file index and file read requests
//! to remote sessions over the kameo mesh.
//!
//! Parallel to `undo.rs`, this module defines the error type and response
//! structs used by `GetFileIndex` and `ReadRemoteFile` session messages.

use crate::index::FileIndexEntry;
use serde::{Deserialize, Serialize};

/// Errors from remote file index/read operations.
///
/// `Serialize + Deserialize` so it can travel over the kameo mesh as a reply.
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum FileProxyError {
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
