//! Workspace file query backing `@` file mentions.
//!
//! The `querymt/workspace/files` extension method answers a fuzzy, bounded
//! query against a workspace index. This module holds the transport-agnostic
//! pieces: wire types, cross-OS path scoping ([`scope`]), ranking ([`rank`])
//! and resolution of `file://` prompt links ([`mentions`]).

pub mod cache;
pub mod confined;
pub mod mentions;
pub mod rank;
pub mod scope;

use crate::index::FileIndexEntry;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Extension method name advertised in `querymt/capabilities`.
pub const WORKSPACE_FILES_METHOD: &str = "querymt/workspace/files";

/// Default number of entries returned when the client sends no `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Hard cap on the number of entries returned per query.
pub const MAX_LIMIT: usize = 200;

/// Request of `querymt/workspace/files`. Exactly one of `session_id` and `cwd`
/// must be set.
#[typeshare]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFilesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What a [`WorkspaceFilesRequest`] asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An existing session, possibly hosted on a mesh peer.
    Session(String),
    /// A directory on this node, before any session exists.
    Cwd(std::path::PathBuf),
}

/// A request whose target is missing, ambiguous or unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("exactly one of session_id and cwd is required, got both")]
    BothTargets,
    #[error("exactly one of session_id and cwd is required, got neither")]
    NoTarget,
    #[error("cwd must be an absolute path")]
    RelativeCwd,
    #[error("cwd must not be a filesystem root")]
    FilesystemRoot,
}

impl WorkspaceFilesRequest {
    /// Validate the purely syntactic rules of the target.
    ///
    /// # Errors
    ///
    /// Returns a [`TargetError`] when both or neither of `session_id` and
    /// `cwd` are set, or when `cwd` is relative or a filesystem root.
    pub fn target(&self) -> Result<Target, TargetError> {
        match (&self.session_id, &self.cwd) {
            (Some(_), Some(_)) => Err(TargetError::BothTargets),
            (None, None) => Err(TargetError::NoTarget),
            (Some(id), None) => Ok(Target::Session(id.clone())),
            (None, Some(cwd)) => {
                let path = std::path::PathBuf::from(cwd);
                if !path.is_absolute() {
                    Err(TargetError::RelativeCwd)
                } else if path.parent().is_none() {
                    Err(TargetError::FilesystemRoot)
                } else {
                    Ok(Target::Cwd(path))
                }
            }
        }
    }
}

/// Whether the workspace index could answer the query.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFilesStatus {
    Ready,
    NotReady,
}

/// One file or directory returned by a workspace query.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileEntry {
    /// Path relative to the query's working directory, always `/`-separated.
    pub path: String,
    /// Absolute path on the agent's machine, in that machine's native format.
    pub abs_path: String,
    /// `file://` URI of `abs_path`, built by the agent.
    pub uri: String,
    /// Base name of the entry.
    pub name: String,
    pub is_dir: bool,
}

/// Response of `querymt/workspace/files`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFilesResponse {
    pub status: WorkspaceFilesStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub entries: Vec<WorkspaceFileEntry>,
    pub truncated: bool,
}

impl WorkspaceFilesResponse {
    /// Answer for an index that is still being built.
    pub fn not_ready() -> Self {
        Self {
            status: WorkspaceFilesStatus::NotReady,
            workspace_root: None,
            cwd: None,
            entries: Vec::new(),
            truncated: false,
        }
    }
}

/// Resolve the client's `limit` against [`DEFAULT_LIMIT`] and [`MAX_LIMIT`].
pub fn clamp_limit(requested: Option<u32>) -> usize {
    requested.map_or(DEFAULT_LIMIT, |limit| {
        usize::try_from(limit)
            .unwrap_or(MAX_LIMIT)
            .clamp(1, MAX_LIMIT)
    })
}

/// Scope `files` to `cwd`, rank them against `query`, and build the response.
///
/// `workspace_root` and `cwd` are strings in the owning node's native path
/// format, so a Windows peer's index can be answered from a Unix node.
pub fn query_index(
    workspace_root: &str,
    cwd: Option<&str>,
    files: &[FileIndexEntry],
    query: &str,
    limit: usize,
) -> WorkspaceFilesResponse {
    let scoped = scope::ScopedIndex::new(workspace_root, cwd, files);
    let ranking = rank::rank(scoped.candidates(), query, limit);
    let entries = ranking
        .order
        .iter()
        .filter_map(|&index| scoped.entry(index))
        .collect();
    WorkspaceFilesResponse {
        status: WorkspaceFilesStatus::Ready,
        workspace_root: Some(workspace_root.to_owned()),
        cwd: Some(scoped.cwd().to_owned()),
        entries,
        truncated: ranking.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults_and_caps() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(100_000)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(7)), 7);
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&WorkspaceFilesStatus::NotReady).unwrap(),
            "\"not_ready\""
        );
        assert_eq!(
            serde_json::to_string(&WorkspaceFilesStatus::Ready).unwrap(),
            "\"ready\""
        );
    }

    #[test]
    fn response_roundtrips_ready_not_ready_and_truncated() {
        let ready = WorkspaceFilesResponse {
            status: WorkspaceFilesStatus::Ready,
            workspace_root: Some("/repo".into()),
            cwd: Some("/repo".into()),
            entries: vec![WorkspaceFileEntry {
                path: "src/a.ts".into(),
                abs_path: "/repo/src/a.ts".into(),
                uri: "file:///repo/src/a.ts".into(),
                name: "a.ts".into(),
                is_dir: false,
            }],
            truncated: true,
        };
        for response in [ready, WorkspaceFilesResponse::not_ready()] {
            let json = serde_json::to_string(&response).unwrap();
            let back: WorkspaceFilesResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(back, response);
        }
    }

    fn target_of(session_id: Option<&str>, cwd: Option<&str>) -> Result<Target, TargetError> {
        WorkspaceFilesRequest {
            session_id: session_id.map(Into::into),
            cwd: cwd.map(Into::into),
            ..Default::default()
        }
        .target()
    }

    #[test]
    fn target_requires_exactly_one_valid_target() {
        assert_eq!(
            target_of(Some("s"), Some("/r")),
            Err(TargetError::BothTargets)
        );
        assert_eq!(target_of(None, None), Err(TargetError::NoTarget));
        assert_eq!(
            target_of(None, Some("rel/dir")),
            Err(TargetError::RelativeCwd)
        );
        assert_eq!(target_of(None, Some("/")), Err(TargetError::FilesystemRoot));
        assert_eq!(target_of(Some("s"), None), Ok(Target::Session("s".into())));
        assert_eq!(
            target_of(None, Some("/repo")),
            Ok(Target::Cwd("/repo".into()))
        );
    }

    #[test]
    fn request_rejects_unknown_fields() {
        let result: Result<WorkspaceFilesRequest, _> =
            serde_json::from_str(r#"{"session_id":"s","bogus":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn query_index_returns_scoped_entries_with_uri() {
        let files = vec![
            FileIndexEntry {
                path: "src".into(),
                is_dir: true,
            },
            FileIndexEntry {
                path: "src/a.ts".into(),
                is_dir: false,
            },
        ];
        let response = query_index("/repo", Some("/repo"), &files, "a.ts", 10);
        assert_eq!(response.status, WorkspaceFilesStatus::Ready);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].uri, "file:///repo/src/a.ts");
        assert_eq!(response.entries[0].name, "a.ts");
        assert!(!response.truncated);
    }
}
