//! Typed worktree management operations.
//!
//! Every management operation (create, list, status, result, publish,
//! archive, remove, recover) is implemented exactly once, here, as typed
//! requests/responses plus [`WorktreeOpError`]. The ACP `querymt/worktree/*`
//! extensions, the native `/worktree` command, the simple-API facade and the
//! Python bindings are thin adapters: they parse their own input, call
//! [`WorktreeOps`], and render the result. Ownership, lease and owner-binding
//! rules therefore cannot drift between surfaces.

use crate::worktree::{
    BaselineMode, BaselineSource, ResultProvenance, ResultVerificationStatus, WorkspaceId,
    WorkspaceLifecycle, WorkspaceRecord, WorkspaceService, WorktreeError,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

// ── Errors ───────────────────────────────────────────────────────────────────

/// Failure of a management operation.
///
/// [`WorktreeOpError::code`] is stable across every surface; the `Display`
/// text is the human-readable diagnostic and may be reworded.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeOpError {
    /// No workspace service is configured on this agent.
    #[error(
        "no workspace service is configured; worktree operations are unavailable \
         (enable isolation on the agent builder)"
    )]
    ServiceUnavailable,
    /// The request itself is malformed or self-contradictory.
    #[error("{0}")]
    InvalidOptions(String),
    /// The workspace id is unknown.
    #[error("unknown workspace: {0}")]
    UnknownWorkspace(String),
    /// Removal was refused or only partially performed; nothing was force-deleted.
    #[error("workspace {workspace_id} was not removed: {source}")]
    RemovalRefused {
        workspace_id: String,
        #[source]
        source: WorktreeError,
    },
    /// The session that should hold the new workspace could not be started.
    #[error("{0}")]
    SessionStart(String),
    /// Session creation was rejected with a stable code by the session layer
    /// (for example `workspace_service_unavailable` or `isolation_unsupported_layout`).
    #[error("{message}")]
    Rejected { code: String, message: String },
    /// A workspace-level failure.
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    /// An invariant of the operation was violated.
    #[error("{0}")]
    Internal(String),
}

impl WorktreeOpError {
    /// A refusal carrying a stable `code` chosen by the operation itself.
    pub fn rejected(code: &str, message: impl Into<String>) -> Self {
        Self::Rejected {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    /// Stable machine-readable error code.
    pub fn code(&self) -> &str {
        match self {
            Self::Rejected { code, .. } => code,
            Self::ServiceUnavailable => "workspace_service_unavailable",
            Self::InvalidOptions(_) => "invalid_options",
            Self::UnknownWorkspace(_) => "workspace_not_found",
            Self::RemovalRefused { .. } => "removal_refused",
            Self::SessionStart(_) => "session_start_failed",
            Self::Worktree(err) => err.code(),
            Self::Internal(_) => "internal_error",
        }
    }
}

impl WorktreeOpError {
    /// Interpret an ACP session-creation error: the session layer attaches a
    /// stable `code` to isolation failures, which is preserved here.
    pub fn from_acp(err: &agent_client_protocol::Error) -> Self {
        let data = err.data.as_ref();
        let field = |key: &str| {
            data.and_then(|d| d.get(key))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        match field("code") {
            Some(code) => Self::Rejected {
                code,
                message: field("message").unwrap_or_else(|| err.to_string()),
            },
            None => Self::SessionStart(field("message").unwrap_or_else(|| err.to_string())),
        }
    }
}

impl From<crate::session::error::SessionError> for WorktreeOpError {
    fn from(err: crate::session::error::SessionError) -> Self {
        Self::Worktree(err.into())
    }
}

// ── Requests ─────────────────────────────────────────────────────────────────

/// Create a new isolated session. Never moves the caller's own workspace.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorktreeCreateRequest {
    /// Source repository cwd to isolate from.
    pub cwd: String,
    /// `Some(true)` captures the source's uncommitted working state,
    /// `Some(false)` pins a commit baseline, `None` leaves the choice to the
    /// service's configured default baseline.
    #[serde(default)]
    pub include_local_changes: Option<bool>,
    /// Explicit base ref (commit-mode isolation).
    #[serde(default, alias = "baseRef")]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Session the new worktree session descends from (lineage only: no
    /// history is copied). When set, an empty `cwd` defaults to the source's
    /// effective cwd and a `cwd` outside the source's repository is rejected.
    #[serde(default, alias = "sourceSessionId")]
    pub source_session_id: Option<String>,
}

/// Which workspace an operation targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceTarget {
    Workspace(String),
    Session(String),
}

impl WorkspaceTarget {
    /// Exactly one of the two identifiers must be supplied.
    pub fn from_parts(
        workspace_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<Self, WorktreeOpError> {
        match (workspace_id, session_id) {
            (Some(_), Some(_)) => Err(WorktreeOpError::InvalidOptions(
                "provide either workspace_id or session_id, not both".into(),
            )),
            (Some(id), None) => Ok(Self::Workspace(id)),
            (None, Some(id)) => Ok(Self::Session(id)),
            (None, None) => Err(WorktreeOpError::InvalidOptions(
                "provide workspace_id or session_id".into(),
            )),
        }
    }
}

// ── Responses (JSON shapes are part of the ACP extension contract) ───────────

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeCreateResponse {
    pub session_id: String,
    pub workspace_id: String,
    pub checkout_path: String,
    /// The cwd the new session executes in.
    pub session_cwd: String,
    pub branch: String,
    /// The resolved baseline mode (request value, else the configured default).
    pub baseline_mode: String,
    pub baseline_commit: Option<String>,
    /// Ignored untracked files that were not copied (current-working-tree only).
    pub ignored_files: Vec<String>,
    pub local_changes_excluded: bool,
    pub ignored_untracked_files_excluded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeSummary {
    pub workspace_id: String,
    pub project: String,
    pub checkout_path: String,
    pub branch: String,
    pub lifecycle: String,
    pub baseline_mode: Option<String>,
    pub baseline_commit: Option<String>,
    pub owner_session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub parent_workspace_id: Option<String>,
    pub delegation_id: Option<String>,
    pub delegation_batch_id: Option<String>,
}

impl From<&WorkspaceRecord> for WorktreeSummary {
    fn from(record: &WorkspaceRecord) -> Self {
        Self {
            workspace_id: record.id.to_string(),
            project: record.repository.project_name.clone(),
            checkout_path: record.checkout_path.display().to_string(),
            branch: record.branch.as_str().to_string(),
            lifecycle: record.lifecycle.to_string(),
            baseline_mode: record.baseline.as_ref().map(|b| b.mode.to_string()),
            baseline_commit: record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
            owner_session_id: record.ownership.owner_session_id.clone(),
            parent_session_id: record.ownership.parent_session_id.clone(),
            parent_workspace_id: record
                .ownership
                .parent_workspace_id
                .as_ref()
                .map(ToString::to_string),
            delegation_id: record.ownership.delegation_id.clone(),
            delegation_batch_id: record.ownership.delegation_batch_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeListResponse {
    pub workspaces: Vec<WorktreeSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeStatusResponse {
    #[serde(flatten)]
    pub summary: WorktreeSummary,
    pub checkout_exists: bool,
    pub registered: bool,
    pub branch_head: Option<String>,
    pub uncommitted: Vec<String>,
    pub untracked: Vec<String>,
    pub ignored: Vec<String>,
    pub unique_commit_count: u64,
    pub dirty: bool,
    pub clean_for_removal: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeChangedPath {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeResultEntry {
    pub baseline_commit: String,
    pub result_commit: Option<String>,
    pub result_ref: Option<String>,
    /// HEAD of the checkout at capture time, distinct from live `branch_head`.
    pub captured_head: Option<String>,
    pub verification: Option<String>,
    pub captured_at: String,
    /// Baseline-relative changes from the immutable result.
    pub changed_paths: Vec<WorktreeChangedPath>,
    /// Present when the immutable diff could not be computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeResultResponse {
    pub workspace_id: String,
    pub checkout_path: String,
    pub branch: String,
    pub results: Vec<WorktreeResultEntry>,
    /// Whether the baseline captured uncommitted parent edits; merging the
    /// private branch directly would import them.
    pub baseline_includes_uncommitted_edits: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_warning: Option<String>,
    // Live state, reported separately from immutable review data.
    pub live_head: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreePublishResponse {
    pub workspace_id: String,
    pub checkout_path: String,
    pub branch: String,
    pub baseline_includes_uncommitted_edits: bool,
    pub integration_warning: Option<String>,
    pub result: WorktreeResultEntry,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeRemoveResponse {
    pub workspace_id: String,
    pub checkout_removed: bool,
    pub removed_branch: Option<String>,
    /// Set when an external writer moved the branch between the safety checks
    /// and deletion: the branch was retained, never deleted unconditionally.
    pub branch_moved_during_removal: Option<String>,
    pub retained_refs: Vec<String>,
}

/// Outcome of recovering an interrupted workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WorktreeRecoverResponse {
    /// Interrupted provisioning completed.
    Provisioned {
        summary: WorktreeSummary,
        #[serde(rename = "baselineSource")]
        baseline_source: &'static str,
    },
    /// Interrupted removal completed.
    Removed(WorktreeRemoveResponse),
}

/// Result of an ownership handoff.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeHandoffResponse {
    /// The new owner session, started in the same checkout.
    pub session_id: String,
    pub previous_session_id: String,
    pub workspace_id: String,
    pub checkout_path: String,
    pub session_cwd: String,
    pub branch: String,
    /// Whether the successor was seeded with a summary of the previous conversation.
    pub summary_seeded: bool,
}

/// Optional inputs of [`WorktreeOps::handoff`]. The default is a clean,
/// unnamed handoff with no history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HandoffOptions {
    /// Display name for the successor session.
    #[serde(default)]
    pub name: Option<String>,
    /// Seed the successor with a summary of the previous conversation.
    #[serde(default)]
    pub summary: bool,
}

/// The session-layer steps of an ownership handoff. Implemented by the agent
/// handle; the ordering and the ownership transfer itself live in
/// [`WorktreeOps::handoff`] so every surface shares them.
#[async_trait::async_trait]
pub trait HandoffHost: Send + Sync {
    /// Whether `session_id` is hosted on this node's filesystem.
    async fn is_local(&self, session_id: &str) -> Result<bool, WorktreeOpError>;

    /// Unload `session_id`'s actor if it is loaded and idle, releasing its
    /// shared activity lease. A running turn, queued or steered input is
    /// refused with `owner_active` and the actor is left untouched.
    async fn quiesce_owner(&self, session_id: &str) -> Result<(), WorktreeOpError>;

    /// Summarize `previous`'s conversation with the compaction summarizer.
    /// `None` when there is no history to summarize. Any failure is
    /// `summary_failed` and aborts the handoff before anything is created.
    async fn summarize(&self, previous: &str) -> Result<Option<String>, WorktreeOpError>;

    /// Seed `successor` (still behind its startup barrier) with `summary` as
    /// its initial context.
    async fn seed_summary(&self, successor: &str, summary: &str) -> Result<(), WorktreeOpError>;

    /// Persist the successor session (cwd = the workspace's session cwd,
    /// parent = `previous`, origin `worktree`) behind a durable startup
    /// barrier that records the handoff, so it cannot run before it owns the
    /// workspace. Returns its id.
    async fn create_pending_successor(
        &self,
        previous: &str,
        record: &WorkspaceRecord,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError>;

    /// Release the successor's startup barrier once it owns the workspace.
    async fn publish_successor(&self, successor: &str) -> Result<(), WorktreeOpError>;

    /// Announce the committed handoff on the previous owner's event stream.
    ///
    /// Best effort: a failure is logged by the host and never fails the
    /// handoff, which is already committed. Returns the announcement's
    /// `session_seq` so a prompt response can wait for its delivery.
    async fn announce_handoff(
        &self,
        previous: &str,
        workspace_id: &str,
        successor: &str,
        summary_seeded: bool,
    ) -> Option<i64>;

    /// Delete a successor that never became the owner.
    async fn discard_successor(&self, successor: &str);
}

// ── Session spawning (the one operation that needs the session layer) ────────

/// Starts the isolated session for [`WorktreeOps::create`]. Implemented by the
/// agent handle so creation goes through the same materialization path as
/// ACP `session/new`.
#[async_trait::async_trait]
pub trait IsolatedSessionSpawner: Send + Sync {
    /// Create a session at `cwd` carrying `session_workspace`
    /// (`_meta.querymt.sessionWorkspace`), optionally named. Returns its id.
    async fn spawn(
        &self,
        cwd: PathBuf,
        session_workspace: serde_json::Value,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError>;

    /// Effective execution cwd of `session_id` (its bound checkout cwd when
    /// managed), or `None` when the session is unknown or not local.
    async fn source_session_cwd(
        &self,
        session_id: &str,
    ) -> Result<Option<PathBuf>, WorktreeOpError>;
}

// ── Operations ───────────────────────────────────────────────────────────────

/// The management operations over one [`WorkspaceService`].
#[derive(Clone)]
pub struct WorktreeOps {
    service: Arc<WorkspaceService>,
}

fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn includes_uncommitted_edits(record: &WorkspaceRecord) -> bool {
    record
        .baseline
        .as_ref()
        .is_some_and(|b| b.mode == BaselineMode::CurrentWorkingTree)
}

/// Dirty-baseline guidance: a private branch based on uncommitted parent
/// edits must not be merged raw.
fn dirty_baseline_warning(record: &WorkspaceRecord) -> Option<String> {
    includes_uncommitted_edits(record).then(|| {
        "the baseline includes inherited uncommitted parent edits; merging the \
         private branch directly can import them — apply only the reviewed \
         baseline-relative delta"
            .to_string()
    })
}

impl WorktreeOps {
    pub fn new(service: Arc<WorkspaceService>) -> Self {
        Self { service }
    }

    /// The backing service.
    pub fn service(&self) -> &Arc<WorkspaceService> {
        &self.service
    }

    /// Look up a workspace by id.
    pub async fn lookup(&self, workspace_id: &str) -> Result<WorkspaceRecord, WorktreeOpError> {
        self.service
            .registry()
            .get_workspace(&WorkspaceId::from_raw(workspace_id))
            .await?
            .ok_or_else(|| WorktreeOpError::UnknownWorkspace(workspace_id.to_string()))
    }

    /// The workspace owned by `session_id`.
    pub async fn workspace_of_session(
        &self,
        session_id: &str,
    ) -> Result<WorkspaceRecord, WorktreeOpError> {
        self.service
            .registry()
            .get_workspace_by_session(session_id)
            .await?
            .ok_or_else(|| {
                WorktreeOpError::InvalidOptions(
                    "current session has no managed workspace; supply an id".into(),
                )
            })
    }

    async fn resolve(&self, target: &WorkspaceTarget) -> Result<WorkspaceRecord, WorktreeOpError> {
        match target {
            WorkspaceTarget::Workspace(id) => self.lookup(id).await,
            WorkspaceTarget::Session(id) => {
                let registry = self.service.registry();
                if let Some(record) = registry.get_workspace_by_session(id).await? {
                    return Ok(record);
                }
                // A previous owner is read-only: say so instead of "no workspace".
                Err(match registry.current_owner_after_handoff(id).await? {
                    Some(owner) => WorktreeOpError::rejected(
                        "owner_transferred",
                        format!("session {id} handed its workspace to {owner} and is read-only"),
                    ),
                    None => {
                        WorktreeOpError::InvalidOptions("session has no managed workspace".into())
                    }
                })
            }
        }
    }

    /// The cwd to isolate from. Without a source session it is the request's
    /// cwd verbatim. With one, an empty cwd defaults to the source's
    /// effective cwd, and any cwd must lie in the source's repository.
    async fn resolve_create_cwd(
        &self,
        spawner: &dyn IsolatedSessionSpawner,
        request: &WorktreeCreateRequest,
    ) -> Result<PathBuf, WorktreeOpError> {
        let Some(source) = request.source_session_id.as_deref() else {
            return Ok(PathBuf::from(&request.cwd));
        };
        let source_cwd = spawner.source_session_cwd(source).await?.ok_or_else(|| {
            WorktreeOpError::InvalidOptions(format!(
                "source session {source} does not exist or is not hosted locally"
            ))
        })?;
        if request.cwd.is_empty() {
            return Ok(source_cwd);
        }
        let cwd = PathBuf::from(&request.cwd);
        let same_repository = match (
            crate::worktree::resolve_repository(&source_cwd),
            crate::worktree::resolve_repository(&cwd),
        ) {
            (Ok(a), Ok(b)) => a.identity.common_dir == b.identity.common_dir,
            _ => false,
        };
        if same_repository {
            Ok(cwd)
        } else {
            Err(WorktreeOpError::InvalidOptions(format!(
                "cwd {} is outside the repository of source session {source}",
                cwd.display()
            )))
        }
    }

    /// Create a new isolated session with its own managed checkout.
    ///
    /// The baseline is the request's choice when it made one; otherwise the
    /// service's configured default. The response discloses the resolved mode.
    pub async fn create(
        &self,
        spawner: &dyn IsolatedSessionSpawner,
        request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, WorktreeOpError> {
        let cwd = self.resolve_create_cwd(spawner, &request).await?;
        let unpinned = request.include_local_changes.is_none() && request.base_ref.is_none();
        let will_capture_working_tree = request.include_local_changes == Some(true)
            || (unpinned && self.service.default_baseline() == BaselineMode::CurrentWorkingTree);

        let ignored_files = if will_capture_working_tree {
            let source = crate::worktree::resolve_repository(&cwd)?;
            self.service
                .ops()
                .checkout_status(&source.checkout.root)
                .await?
                .ignored
        } else {
            Vec::new()
        };

        let mut session_workspace = serde_json::json!({ "execution": "worktree" });
        match request.include_local_changes {
            Some(true) => session_workspace["baseline"] = "current_working_tree".into(),
            Some(false) => session_workspace["baseline"] = "commit".into(),
            None => {}
        }
        if let Some(base_ref) = &request.base_ref {
            session_workspace["baseRef"] = base_ref.clone().into();
        }
        if let Some(source) = &request.source_session_id {
            session_workspace["sourceSessionId"] = source.clone().into();
        }

        let session_id = spawner.spawn(cwd, session_workspace, request.name).await?;
        let record = self
            .service
            .registry()
            .get_workspace_by_session(&session_id)
            .await?
            .ok_or_else(|| {
                WorktreeOpError::Internal(
                    "session was created without a workspace binding; isolation failed \
                     closed before execution and the session must not be used"
                        .into(),
                )
            })?;

        let baseline_mode = record
            .baseline
            .as_ref()
            .map_or(BaselineMode::Commit, |b| b.mode);
        Ok(WorktreeCreateResponse {
            session_id,
            workspace_id: record.id.to_string(),
            checkout_path: record.checkout_path.display().to_string(),
            session_cwd: record.session_cwd().display().to_string(),
            branch: record.branch.as_str().to_string(),
            baseline_mode: baseline_mode.to_string(),
            baseline_commit: record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
            ignored_files,
            local_changes_excluded: baseline_mode == BaselineMode::Commit,
            ignored_untracked_files_excluded: true,
        })
    }

    /// Take the checkout's exclusive activity lease, tolerating the short
    /// window in which an just-unloaded actor is still dropping its shared
    /// lease. Anything still holding it after the grace period (a running
    /// turn, shared delegates, another process) is `owner_active`.
    async fn acquire_exclusive_activity(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<crate::worktree::gate::CheckoutActivityLease, WorktreeOpError> {
        const ATTEMPTS: u32 = 40;
        const PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
        let mut attempt = 1;
        loop {
            match crate::worktree::gate::CheckoutActivityLease::acquire(
                self.service.storage_root(),
                &record.checkout_path,
                true,
            ) {
                Ok(lease) => return Ok(lease),
                Err(err) if attempt == ATTEMPTS => {
                    return Err(WorktreeOpError::rejected("owner_active", err.to_string()));
                }
                Err(_) => {
                    attempt += 1;
                    tokio::time::sleep(PAUSE).await;
                }
            }
        }
    }

    /// Hand ownership of a ready workspace to a new session in the same
    /// checkout. Files, branch, index and baseline are untouched.
    ///
    /// Order: resolve the owner, require a local ready workspace, unload the
    /// idle owner actor, take the exclusive activity lease (else
    /// `owner_active`), persist the successor behind its startup barrier,
    /// transfer ownership with one compare-and-swap, then release the
    /// barrier. A crash before the transfer leaves the previous owner in
    /// place; a crash after it leaves a successor that owns the workspace but
    /// stays non-runnable until `recover` releases the barrier.
    ///
    /// # Errors
    /// `not_owner`, `workspace_not_ready`, `owner_not_local`, `owner_transferred`
    /// and `owner_active` refusals create nothing.
    pub async fn handoff(
        &self,
        host: &dyn HandoffHost,
        target: &WorkspaceTarget,
        options: HandoffOptions,
    ) -> Result<WorktreeHandoffResponse, WorktreeOpError> {
        self.handoff_announced(host, target, options)
            .await
            .map(|(response, _tip)| response)
    }

    /// [`Self::handoff`], additionally returning the `session_seq` of the
    /// durable announcement (`None` when it could not be recorded). The
    /// native command uses it as the response's event tip; it is deliberately
    /// not part of [`WorktreeHandoffResponse`].
    ///
    /// # Errors
    /// Same as [`Self::handoff`].
    pub async fn handoff_announced(
        &self,
        host: &dyn HandoffHost,
        target: &WorkspaceTarget,
        options: HandoffOptions,
    ) -> Result<(WorktreeHandoffResponse, Option<i64>), WorktreeOpError> {
        let HandoffOptions { name, summary } = options;
        let registry = self.service.registry();
        let record = self.resolve(target).await?;
        let previous = record.ownership.owner_session_id.clone().ok_or_else(|| {
            WorktreeOpError::rejected("not_owner", "workspace has no owner session to hand off")
        })?;
        if record.lifecycle != WorkspaceLifecycle::Ready {
            return Err(WorktreeOpError::rejected(
                "workspace_not_ready",
                format!("workspace {} is {}", record.id, record.lifecycle),
            ));
        }
        if !host.is_local(&previous).await? {
            return Err(WorktreeOpError::rejected(
                "owner_not_local",
                format!("owner session {previous} is not hosted on this node"),
            ));
        }

        host.quiesce_owner(&previous).await?;
        let lease = self.acquire_exclusive_activity(&record).await?;

        // The record may have moved while the owner was being unloaded.
        let record = self.lookup(record.id.as_str()).await?;
        if record.ownership.owner_session_id.as_deref() != Some(previous.as_str())
            || record.lifecycle != WorkspaceLifecycle::Ready
        {
            return Err(WorktreeOpError::rejected(
                "not_owner",
                "workspace ownership changed while the handoff was starting",
            ));
        }

        // Summarize before creating anything: a failure here leaves no trace.
        let summary = if summary {
            host.summarize(&previous).await?
        } else {
            None
        };

        let successor = host
            .create_pending_successor(&previous, &record, name)
            .await?;
        if let Some(summary) = &summary
            && let Err(err) = host.seed_summary(&successor, summary).await
        {
            host.discard_successor(&successor).await;
            return Err(err);
        }
        let moved = match registry
            .transfer_owner(&record.id, &previous, &successor)
            .await
        {
            Ok(moved) => moved,
            Err(err) => {
                host.discard_successor(&successor).await;
                return Err(err.into());
            }
        };
        drop(lease);
        host.publish_successor(&successor).await.map_err(|err| {
            WorktreeOpError::rejected(
                "handoff_incomplete",
                format!(
                    "session {successor} owns workspace {} but is not yet runnable ({err}); \
                     run recover for session {successor}",
                    moved.id
                ),
            )
        })?;

        let summary_seeded = summary.is_some();
        let tip = host
            .announce_handoff(&previous, moved.id.as_str(), &successor, summary_seeded)
            .await;

        let response = WorktreeHandoffResponse {
            session_id: successor,
            previous_session_id: previous,
            workspace_id: moved.id.to_string(),
            checkout_path: moved.checkout_path.display().to_string(),
            session_cwd: moved.session_cwd().display().to_string(),
            branch: moved.branch.as_str().to_string(),
            summary_seeded,
        };
        Ok((response, tip))
    }

    /// All managed workspaces, newest first.
    pub async fn list(&self) -> Result<WorktreeListResponse, WorktreeOpError> {
        let records = self.service.registry().list_workspaces().await?;
        Ok(WorktreeListResponse {
            workspaces: records.iter().map(WorktreeSummary::from).collect(),
        })
    }

    /// Durable record plus live Git state.
    pub async fn status(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeStatusResponse, WorktreeOpError> {
        let record = self.lookup(workspace_id).await?;
        let status = self.service.manager().status(&record).await?;
        let dirty = status.is_dirty();
        let clean_for_removal = status.is_clean_for_removal();
        Ok(WorktreeStatusResponse {
            summary: WorktreeSummary::from(&record),
            checkout_exists: status.checkout_exists,
            registered: status.registered,
            branch_head: status.head_commit,
            dirty,
            clean_for_removal,
            uncommitted: status.uncommitted,
            untracked: status.untracked,
            ignored: status.ignored,
            unique_commit_count: status.unique_commit_count,
        })
    }

    /// One retained result: immutable provenance plus its baseline-relative
    /// delta. A failed diff is disclosed via `review_error`, never omitted or
    /// replaced by live state.
    async fn result_entry(
        &self,
        record: &WorkspaceRecord,
        provenance: &ResultProvenance,
    ) -> WorktreeResultEntry {
        let verification = |v: Option<ResultVerificationStatus>| v.map(|status| status.to_string());
        match self.service.review_result(record, provenance).await {
            Ok(review) => WorktreeResultEntry {
                baseline_commit: review.baseline_commit,
                result_commit: Some(review.result_commit),
                result_ref: review.result_ref,
                captured_head: Some(review.captured_head),
                verification: verification(review.verification),
                captured_at: rfc3339(review.captured_at),
                changed_paths: review
                    .changed_paths
                    .into_iter()
                    .map(|changed| WorktreeChangedPath {
                        path: changed.path,
                        kind: changed.kind.to_string(),
                    })
                    .collect(),
                review_error: None,
            },
            Err(err) => WorktreeResultEntry {
                baseline_commit: provenance.baseline_commit.clone(),
                result_commit: provenance.result_commit.clone(),
                result_ref: provenance.result_ref.clone(),
                captured_head: provenance.captured_head.clone(),
                verification: verification(provenance.verification),
                captured_at: rfc3339(provenance.captured_at),
                changed_paths: Vec::new(),
                review_error: Some(err.to_string()),
            },
        }
    }

    /// Retained baseline-relative review of published results. Read-only:
    /// inspection never republishes live state, and absence never implies
    /// integration.
    pub async fn result(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeResultResponse, WorktreeOpError> {
        let record = self.lookup(workspace_id).await?;
        let results = self.service.registry().list_results(&record.id).await?;
        let status = self.service.manager().status(&record).await.ok();
        let mut entries = Vec::with_capacity(results.len());
        for result in &results {
            entries.push(self.result_entry(&record, result).await);
        }
        Ok(WorktreeResultResponse {
            workspace_id: record.id.to_string(),
            checkout_path: record.checkout_path.display().to_string(),
            branch: record.branch.as_str().to_string(),
            baseline_includes_uncommitted_edits: includes_uncommitted_edits(&record),
            integration_warning: dirty_baseline_warning(&record),
            results: entries,
            live_head: status.as_ref().and_then(|s| s.head_commit.clone()),
            dirty: status.map(|s| s.is_dirty()).unwrap_or(true),
        })
    }

    /// Explicitly publish the current working state as an immutable result.
    ///
    /// Never stages, commits, resets or merges anything in the checkout.
    /// Verification is recorded as `skipped` because publication performs none.
    pub async fn publish(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<WorktreePublishResponse, WorktreeOpError> {
        let record = self.resolve(target).await?;
        let gate = crate::worktree::CheckoutMutationGate::new(
            self.service.storage_root(),
            &record.checkout_path,
        )?;
        let provenance = self
            .service
            .publish_result(
                &record,
                &record.checkout_path,
                Some(&gate),
                Some(ResultVerificationStatus::Skipped),
            )
            .await
            .map_err(|err| {
                WorktreeOpError::Worktree(WorktreeError::InvalidOperation(format!(
                    "workspace {} result was not published: {err}. The checkout and its \
                     files are retained unchanged; nothing was integrated.",
                    record.id
                )))
            })?;
        Ok(WorktreePublishResponse {
            workspace_id: record.id.to_string(),
            checkout_path: record.checkout_path.display().to_string(),
            branch: record.branch.as_str().to_string(),
            baseline_includes_uncommitted_edits: includes_uncommitted_edits(&record),
            integration_warning: dirty_baseline_warning(&record),
            result: self.result_entry(&record, &provenance).await,
        })
    }

    /// Retire inactive execution while preserving history and ownership.
    pub async fn archive(&self, workspace_id: &str) -> Result<WorktreeSummary, WorktreeOpError> {
        let record = self.lookup(workspace_id).await?;
        let archived = self.service.manager().archive(&record).await?;
        Ok(WorktreeSummary::from(&archived))
    }

    /// Conservative removal. Refuses active ownership or any content that
    /// would be lost, and reports exactly which refs remain.
    pub async fn remove(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeRemoveResponse, WorktreeOpError> {
        let record = self.lookup(workspace_id).await?;
        self.remove_record(&record)
            .await
            .map_err(|source| WorktreeOpError::RemovalRefused {
                workspace_id: workspace_id.to_string(),
                source,
            })
    }

    async fn remove_record(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<WorktreeRemoveResponse, WorktreeError> {
        let report = self.service.manager().remove(record).await?;
        Ok(WorktreeRemoveResponse {
            workspace_id: report.workspace_id.to_string(),
            checkout_removed: report.checkout_removed,
            removed_branch: report.removed_branch.map(|b| b.as_str().to_string()),
            branch_moved_during_removal: report
                .branch_moved_during_removal
                .map(|b| b.as_str().to_string()),
            retained_refs: report.retained_refs,
        })
    }

    /// Complete an interrupted provisioning or removal. Never launches an
    /// actor or binds a new owner.
    pub async fn recover(
        &self,
        workspace_id: &str,
    ) -> Result<WorktreeRecoverResponse, WorktreeOpError> {
        let record = self.lookup(workspace_id).await?;
        match record.lifecycle {
            WorkspaceLifecycle::Provisioning => {
                // Self-contained: administration and recovery-time capture use
                // the record's canonical common directory and recorded source.
                let recovered = self.service.allocator().recover(&record).await?;
                let baseline_source = match recovered.baseline_source {
                    BaselineSource::BoundBeforeInterruption => "bound-before-interruption",
                    BaselineSource::CapturedDuringRecovery => "captured-during-recovery",
                };
                Ok(WorktreeRecoverResponse::Provisioned {
                    summary: WorktreeSummary::from(&recovered.record),
                    baseline_source,
                })
            }
            WorkspaceLifecycle::Removing => Ok(WorktreeRecoverResponse::Removed(
                self.remove_record(&record).await?,
            )),
            _ => Err(WorktreeOpError::InvalidOptions(
                "workspace has no interrupted provisioning or removal to recover".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(
            WorktreeOpError::ServiceUnavailable.code(),
            "workspace_service_unavailable"
        );
        assert_eq!(
            WorktreeOpError::InvalidOptions(String::new()).code(),
            "invalid_options"
        );
        assert_eq!(
            WorktreeOpError::UnknownWorkspace("w".into()).code(),
            "workspace_not_found"
        );
        let refused = WorktreeOpError::RemovalRefused {
            workspace_id: "w".into(),
            source: WorktreeError::InvalidOperation("dirty".into()),
        };
        assert_eq!(refused.code(), "removal_refused");
        assert!(refused.to_string().contains("dirty"));
        assert_eq!(
            WorktreeOpError::from(WorktreeError::NotGitRepository { path: "/x".into() }).code(),
            "isolation_unsupported_layout"
        );
        assert_eq!(
            WorktreeOpError::from(WorktreeError::SessionCwdConflict {
                session_id: "s".into(),
                workspace_id: "w".into(),
                bound: "/a".into(),
                requested: "/b".into(),
            })
            .code(),
            "conflicting_cwd"
        );
        assert_eq!(
            WorktreeOpError::from(crate::session::error::SessionError::WorkspaceAlreadyOwned {
                workspace: "w".into(),
                owner: "s".into(),
            })
            .code(),
            "owner_conflict"
        );
    }

    #[test]
    fn target_requires_exactly_one_identifier() {
        assert_eq!(
            WorkspaceTarget::from_parts(Some("w".into()), None).unwrap(),
            WorkspaceTarget::Workspace("w".into())
        );
        assert_eq!(
            WorkspaceTarget::from_parts(None, Some("s".into())).unwrap(),
            WorkspaceTarget::Session("s".into())
        );
        for (w, s) in [(Some("w".to_string()), Some("s".to_string())), (None, None)] {
            let err = WorkspaceTarget::from_parts(w, s).unwrap_err();
            assert_eq!(err.code(), "invalid_options");
        }
    }

    #[test]
    fn create_request_distinguishes_unset_from_explicit_false() {
        let unset: WorktreeCreateRequest = serde_json::from_str(r#"{"cwd":"/r"}"#).unwrap();
        assert_eq!(unset.include_local_changes, None);
        let off: WorktreeCreateRequest =
            serde_json::from_str(r#"{"cwd":"/r","include_local_changes":false}"#).unwrap();
        assert_eq!(off.include_local_changes, Some(false));
        let aliased: WorktreeCreateRequest =
            serde_json::from_str(r#"{"cwd":"/r","baseRef":"v1"}"#).unwrap();
        assert_eq!(aliased.base_ref.as_deref(), Some("v1"));
    }
}
