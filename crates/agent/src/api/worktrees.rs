//! Simple-API surface for managed worktree workspaces.
//!
//! [`SessionOptions`] lets an embedder ask for an isolated session explicitly
//! (`Agent::chat_session_with`); [`AgentWorktrees`] is the management facade
//! (`Agent::worktrees`). Both are thin adapters over the typed operations core
//! in [`crate::worktree::ops`], so ownership, lease and removal rules cannot
//! drift from the ACP extensions or the Python bindings.
//!
//! Isolation is always an explicit per-request decision: a default
//! [`SessionOptions`] is a plain local session, whether or not the workspace
//! service is enabled.

use crate::agent::LocalAgentHandle;
use crate::worktree::{
    BaselineMode, ExecutionMode, WorkspaceTarget, WorktreeCreateRequest, WorktreeCreateResponse,
    WorktreeListResponse, WorktreeOpError, WorktreeOps, WorktreePublishResponse,
    WorktreeRecoverResponse, WorktreeRemoveResponse, WorktreeResultResponse,
    WorktreeStatusResponse, WorktreeSummary,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Options for creating a session. The default is a plain local session.
///
/// ```
/// use querymt_agent::api::SessionOptions;
///
/// // Isolated checkout seeded from HEAD (or the configured default baseline).
/// let isolated = SessionOptions::worktree().name("refactor");
/// // Isolated checkout that also captures uncommitted edits.
/// let with_edits = SessionOptions::worktree().include_local_changes(true);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOptions {
    execution: ExecutionMode,
    name: Option<String>,
    base_ref: Option<String>,
    include_local_changes: Option<bool>,
}

impl SessionOptions {
    /// A local session in the agent's own working directory (the default).
    #[must_use]
    pub fn local() -> Self {
        Self::default()
    }

    /// An isolated session in its own managed worktree checkout.
    #[must_use]
    pub fn worktree() -> Self {
        Self {
            execution: ExecutionMode::Worktree,
            ..Self::default()
        }
    }

    /// Human-readable session name.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Branch the checkout from an explicit ref (commit baseline).
    #[must_use]
    pub fn base_ref(mut self, base_ref: impl Into<String>) -> Self {
        self.base_ref = Some(base_ref.into());
        self
    }

    /// `true` captures uncommitted edits into the baseline, `false` pins a
    /// commit baseline. Unset leaves the choice to the configured default.
    #[must_use]
    pub fn include_local_changes(mut self, include: bool) -> Self {
        self.include_local_changes = Some(include);
        self
    }

    /// Whether these options request an isolated worktree.
    pub fn requests_isolation(&self) -> bool {
        self.execution == ExecutionMode::Worktree
    }

    /// Optional session name.
    pub(crate) fn session_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Reject contradictory combinations before any session is created.
    pub fn validate(&self) -> Result<(), WorktreeOpError> {
        crate::worktree::SessionCreationOptions {
            execution: self.execution,
            baseline: if self.include_local_changes == Some(true) {
                BaselineMode::CurrentWorkingTree
            } else {
                BaselineMode::Commit
            },
            base_ref: self.base_ref.clone(),
        }
        .validate()
        .map_err(WorktreeOpError::InvalidOptions)
    }

    /// The `_meta.querymt.sessionWorkspace` value for session creation, or
    /// `None` for a local session (which sends no workspace metadata at all).
    ///
    /// The materializer remains the single parser of this value.
    pub(crate) fn session_workspace_meta(&self) -> Option<serde_json::Value> {
        if !self.requests_isolation() {
            return None;
        }
        let mut value = serde_json::json!({ "execution": "worktree" });
        match self.include_local_changes {
            Some(true) => value["baseline"] = "current_working_tree".into(),
            Some(false) => value["baseline"] = "commit".into(),
            None => {}
        }
        if let Some(base_ref) = &self.base_ref {
            value["baseRef"] = base_ref.clone().into();
        }
        Some(value)
    }

    /// Full `_meta` object for a `session/new` request.
    pub(crate) fn new_session_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let workspace = self.session_workspace_meta()?;
        let mut querymt = serde_json::Map::new();
        querymt.insert(
            crate::worktree::SESSION_WORKSPACE_META_KEY.to_string(),
            workspace,
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
            querymt.into(),
        );
        Some(meta)
    }
}

/// Extract the disclosed workspace binding from a `session/new` response.
pub(crate) fn disclosed_workspace(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<serde_json::Value> {
    meta?
        .get(crate::acp::shared::QUERYMT_META_NAMESPACE)?
        .get(crate::worktree::SESSION_WORKSPACE_META_KEY)
        .cloned()
}

/// Management operations for an agent's managed worktrees.
///
/// Obtained from [`Agent::worktrees`](super::Agent::worktrees); `None` when the
/// agent has no workspace service. Every error is a [`WorktreeOpError`] whose
/// [`code`](WorktreeOpError::code) is stable across all surfaces.
#[derive(Clone)]
pub struct AgentWorktrees {
    ops: WorktreeOps,
    handle: Arc<LocalAgentHandle>,
    default_cwd: Option<PathBuf>,
}

impl AgentWorktrees {
    pub(super) fn new(
        ops: WorktreeOps,
        handle: Arc<LocalAgentHandle>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            ops,
            handle,
            default_cwd,
        }
    }

    /// The typed operations core backing this facade.
    pub fn ops(&self) -> &WorktreeOps {
        &self.ops
    }

    /// Create a new isolated session with its own checkout.
    ///
    /// An empty `request.cwd` means the agent's configured working directory.
    /// Never moves an existing session.
    pub async fn create(
        &self,
        mut request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, WorktreeOpError> {
        if request.cwd.is_empty() {
            request.cwd = self
                .default_cwd
                .as_ref()
                .ok_or_else(|| {
                    WorktreeOpError::InvalidOptions(
                        "no cwd supplied and the agent has no configured working directory".into(),
                    )
                })?
                .display()
                .to_string();
        }
        self.ops.create(self.handle.as_ref(), request).await
    }

    /// All managed workspaces, newest first.
    pub async fn list(&self) -> Result<WorktreeListResponse, WorktreeOpError> {
        self.ops.list().await
    }

    /// Durable record plus live Git state.
    pub async fn status(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeStatusResponse, WorktreeOpError> {
        self.ops.status(workspace_id).await
    }

    /// Retained baseline-relative review of published results.
    pub async fn result(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeResultResponse, WorktreeOpError> {
        self.ops.result(workspace_id).await
    }

    /// Publish the workspace's current working state as an immutable result.
    pub async fn publish(
        &self,
        target: WorkspaceTarget,
    ) -> Result<WorktreePublishResponse, WorktreeOpError> {
        self.ops.publish(&target).await
    }

    /// Retire inactive execution, preserving history and ownership.
    pub async fn archive(&self, workspace_id: &str) -> Result<WorktreeSummary, WorktreeOpError> {
        self.ops.archive(workspace_id).await
    }

    /// Conservative removal; there is no force option.
    pub async fn remove(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeRemoveResponse, WorktreeOpError> {
        self.ops.remove(workspace_id).await
    }

    /// Complete an interrupted provisioning or removal.
    pub async fn recover(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeRecoverResponse, WorktreeOpError> {
        self.ops.recover(workspace_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_local_and_send_no_metadata() {
        let options = SessionOptions::default();
        assert!(!options.requests_isolation());
        assert!(options.session_workspace_meta().is_none());
        assert!(options.new_session_meta().is_none());
        options.validate().unwrap();
    }

    #[test]
    fn worktree_options_emit_the_session_workspace_meta() {
        let meta = SessionOptions::worktree()
            .include_local_changes(true)
            .name("n")
            .new_session_meta()
            .unwrap();
        assert_eq!(
            meta["querymt"]["sessionWorkspace"],
            serde_json::json!({ "execution": "worktree", "baseline": "current_working_tree" })
        );
        let by_ref = SessionOptions::worktree()
            .base_ref("v1")
            .session_workspace_meta();
        assert_eq!(
            by_ref.unwrap(),
            serde_json::json!({ "execution": "worktree", "baseRef": "v1" })
        );
        // An explicit `false` is a pinned commit baseline, not "unset".
        let pinned = SessionOptions::worktree()
            .include_local_changes(false)
            .session_workspace_meta()
            .unwrap();
        assert_eq!(pinned["baseline"], "commit");
    }

    #[test]
    fn contradictory_options_fail_validation_with_a_stable_code() {
        for options in [
            SessionOptions::local().base_ref("main"),
            SessionOptions::local().include_local_changes(true),
            SessionOptions::worktree()
                .include_local_changes(true)
                .base_ref("main"),
        ] {
            assert_eq!(options.validate().unwrap_err().code(), "invalid_options");
        }
        SessionOptions::worktree()
            .include_local_changes(false)
            .base_ref("main")
            .validate()
            .unwrap();
    }

    #[test]
    fn disclosed_workspace_reads_the_response_meta() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt".into(),
            serde_json::json!({ "sessionWorkspace": { "workspaceId": "ws-1" } }),
        );
        assert_eq!(
            disclosed_workspace(Some(&meta)).unwrap()["workspaceId"],
            "ws-1"
        );
        assert!(disclosed_workspace(None).is_none());
        assert!(disclosed_workspace(Some(&serde_json::Map::new())).is_none());
    }
}
