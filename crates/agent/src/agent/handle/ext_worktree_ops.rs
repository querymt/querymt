//! Additive ACP extension surface for managed worktree workspaces.
//!
//! Five typed operations — create, list, status, result, remove — exposed as
//! `querymt/worktree/*` extension methods (see [`super::ext`]). All payloads
//! are plain JSON responses; upstream protocol types are not modified.
//!
//! `create` goes through the same materialization path as ACP session
//! creation (`_meta.querymt.sessionWorkspace`), so the new session receives
//! its own workspace and the current session is never moved. `list`,
//! `status`, `result`, and `remove` wrap the durable registry and the
//! lifecycle manager, including their conservative removal semantics.

use super::utils::ext_json_response;
use super::*;
use crate::acp::protocol::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use serde::{Deserialize, Serialize};

fn parse_workspace_op_request<T: serde::de::DeserializeOwned>(req: ExtRequest) -> Result<T, Error> {
    serde_json::from_str(req.params.get())
        .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e.to_string() })))
}

fn baseline_mode_label(mode: &crate::worktree::BaselineMode) -> &'static str {
    match mode {
        crate::worktree::BaselineMode::Commit => "commit",
        crate::worktree::BaselineMode::CurrentWorkingTree => "current_working_tree",
    }
}

// ── Request/response payloads ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorktreeCreateRequest {
    /// Source repository cwd to isolate from.
    cwd: String,
    /// Include the source's current uncommitted working state in the new
    /// checkout (explicit `current_working_tree` baseline).
    #[serde(default)]
    include_local_changes: bool,
    /// Explicit base ref for commit-mode isolation.
    #[serde(default, alias = "baseRef")]
    base_ref: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorktreeCreateResponse {
    session_id: String,
    workspace_id: String,
    checkout_path: String,
    /// The cwd the new session executes in.
    session_cwd: String,
    branch: String,
    baseline_mode: &'static str,
    baseline_commit: Option<String>,
    /// Disclosed for `current_working_tree` baselines: ignored untracked
    /// files are never copied.
    ignored_files: Vec<String>,
    local_changes_excluded: bool,
    ignored_untracked_files_excluded: bool,
}

#[derive(Debug, Serialize)]
struct WorktreeSummaryResponse {
    workspace_id: String,
    project: String,
    checkout_path: String,
    branch: String,
    lifecycle: String,
    baseline_mode: Option<&'static str>,
    baseline_commit: Option<String>,
    owner_session_id: Option<String>,
    parent_session_id: Option<String>,
    parent_workspace_id: Option<String>,
    delegation_id: Option<String>,
    delegation_batch_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorktreeIdRequest {
    workspace_id: String,
}

/// Recovery targets: a workspace with an interrupted provisioning/removal
/// intent, or (forks only) a session whose pending isolation startup barrier
/// still blocks load/resume.
#[derive(Debug, Deserialize)]
struct WorktreeRecoverRequest {
    #[serde(default, alias = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorktreeStatusResponse {
    #[serde(flatten)]
    summary: WorktreeSummaryResponse,
    checkout_exists: bool,
    registered: bool,
    branch_head: Option<String>,
    uncommitted: Vec<String>,
    untracked: Vec<String>,
    ignored: Vec<String>,
    unique_commit_count: u64,
    dirty: bool,
    clean_for_removal: bool,
}

#[derive(Debug, Serialize)]
struct WorktreeResultEntry {
    baseline_commit: String,
    result_commit: Option<String>,
    result_ref: Option<String>,
    verification: Option<&'static str>,
    captured_at: String,
}

#[derive(Debug, Serialize)]
struct WorktreeResultResponse {
    workspace_id: String,
    checkout_path: String,
    branch: String,
    results: Vec<WorktreeResultEntry>,
    dirty: bool,
}

#[derive(Debug, Serialize)]
struct WorktreeRemoveResponse {
    workspace_id: String,
    checkout_removed: bool,
    removed_branch: Option<String>,
    /// Set when an external writer moved the branch between the safety
    /// checks and deletion: the branch (with its new history) was retained,
    /// never deleted unconditionally.
    branch_moved_during_removal: Option<String>,
    retained_refs: Vec<String>,
}

// ── Operations ───────────────────────────────────────────────────────────────

fn summarize_record(record: &crate::worktree::WorkspaceRecord) -> WorktreeSummaryResponse {
    WorktreeSummaryResponse {
        workspace_id: record.id.to_string(),
        project: record.repository.project_name.clone(),
        checkout_path: record.checkout_path.display().to_string(),
        branch: record.branch.as_str().to_string(),
        lifecycle: record.lifecycle.to_string(),
        baseline_mode: record
            .baseline
            .as_ref()
            .map(|b| baseline_mode_label(&b.mode)),
        baseline_commit: record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
        owner_session_id: record.ownership.owner_session_id.clone(),
        parent_session_id: record.ownership.parent_session_id.clone(),
        parent_workspace_id: record
            .ownership
            .parent_workspace_id
            .as_ref()
            .map(|id| id.to_string()),
        delegation_id: record.ownership.delegation_id.clone(),
        delegation_batch_id: record.ownership.delegation_batch_id.clone(),
    }
}

/// Native commands are parsed before prompt expansion and never sent to a model.
pub(super) fn native_worktree_command(
    prompt: &[ContentBlock],
) -> Result<Option<Vec<String>>, Error> {
    let Some(ContentBlock::Text(text)) = prompt.first() else {
        return Ok(None);
    };
    let words: Vec<String> = text.text.split_whitespace().map(str::to_owned).collect();
    if words.first().map(String::as_str) != Some("/worktree") {
        return Ok(None);
    }
    if prompt.len() != 1 {
        return Err(Error::invalid_params().data("worktree commands require a single text block"));
    }
    Ok(Some(words.into_iter().skip(1).collect()))
}

impl LocalAgentHandle {
    pub(super) async fn handle_native_worktree_prompt(
        &self,
        req: &PromptRequest,
        bridge: Option<&ClientBridgeSender>,
    ) -> Result<Option<PromptResponse>, Error> {
        let Some(words) = native_worktree_command(&req.prompt)? else {
            return Ok(None);
        };
        let session_id = req.session_id.to_string();
        // Require an accessible, locally hosted session before any administration.
        let actor = self.session_ref_for_agent_session(&session_id).await?;
        if actor.is_remote() {
            return Err(Error::invalid_request()
                .data("worktree administration requires local filesystem execution"));
        }
        let session = self
            .config
            .storage
            .session_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| Error::invalid_request().data("session not found"))?;
        let operation = words.first().map(String::as_str).unwrap_or("");
        let args = &words[words.len().min(1)..];
        let params = match operation {
            "create" => {
                let include = args.iter().any(|s| s == "--include-local-changes");
                let positional: Vec<_> = args.iter().filter(|s| s.as_str() != "--include-local-changes").collect();
                if positional.len() > if include { 1 } else { 2 } || positional.iter().any(|s| s.starts_with('-'))
                    || args.iter().filter(|s| s.as_str() == "--include-local-changes").count() > 1 {
                    return Err(Error::invalid_params().data("usage: /worktree create [ref] [name] OR /worktree create --include-local-changes [name]"));
                }
                // Resolve the isolation source through the single session-cwd
                // path: a bound session isolates from its own bound checkout,
                // never from a stale row cwd copied from a fork source.
                let source_cwd = match self.config.workspace_service() {
                    Some(service) => service
                        .resolve_session_cwd(&session_id, session.cwd.clone())
                        .await
                        .map_err(|err| {
                            Error::invalid_request().data(format!(
                                "cannot resolve the session's effective workspace: {err}"
                            ))
                        })?,
                    None => session.cwd,
                };
                serde_json::json!({"cwd": source_cwd.ok_or_else(|| Error::invalid_request().data("session has no cwd"))?,
                    "include_local_changes": include, "base_ref": if include { None } else { positional.first() }, "name": positional.get(if include { 0 } else { 1 })})
            }
            "list" if args.is_empty() => serde_json::json!({}),
            "status" | "result" if args.len() <= 1 => {
                let id = if let Some(id) = args.first() { id.clone() } else {
                    self.workspace_service()?.registry().get_workspace_by_session(&session_id).await
                        .map_err(|e| Error::internal_error().data(e.to_string()))?
                        .ok_or_else(|| Error::invalid_request().data("current session has no managed workspace; supply an id"))?.id.to_string()
                };
                serde_json::json!({"workspace_id": id})
            }
            "remove" | "recover" | "archive" if args.len() == 1 && !args[0].starts_with('-') => serde_json::json!({"workspace_id": args[0]}),
            _ => return Err(Error::invalid_params().data("usage: /worktree create [ref] [name] [--include-local-changes], list, status [id], result [id], remove <id>, recover <id>, archive <id>; no force removal")),
        };
        let raw = serde_json::value::RawValue::from_string(params.to_string())
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        let result = self
            .handle_ext_method(ExtRequest::new(
                format!("querymt/worktree/{operation}"),
                raw.into(),
            ))
            .await?;
        let session_bridge = self
            .config
            .session_bridges
            .lock()
            .ok()
            .and_then(|routes| routes.get(&session_id).map(|route| route.bridge.clone()));
        if let Some(bridge) = bridge.or(session_bridge.as_ref()) {
            bridge
                .notify(SessionNotification::new(
                    req.session_id.clone(),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(result.0.get()),
                    ))),
                ))
                .await?;
        }
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt.worktree".into(),
            serde_json::from_str(result.0.get())
                .map_err(|e| Error::internal_error().data(e.to_string()))?,
        );
        Ok(Some(PromptResponse::new(StopReason::EndTurn).meta(meta)))
    }

    fn workspace_service(
        &self,
    ) -> Result<std::sync::Arc<crate::worktree::WorkspaceService>, Error> {
        self.config.workspace_service().ok_or_else(|| {
            Error::internal_error().data(
                "no workspace service is configured; worktree operations are unavailable \
                 (enable isolation on the agent builder)",
            )
        })
    }

    /// `querymt/worktree/create`: create a new isolated session with its own
    /// managed checkout. Returns the new session identity without moving the
    /// current session's workspace.
    pub(super) async fn handle_ext_worktree_create(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let parsed: WorktreeCreateRequest = parse_workspace_op_request(req)?;
        let ignored_files = if parsed.include_local_changes {
            let source = crate::worktree::resolve_repository(std::path::Path::new(&parsed.cwd))
                .map_err(|e| Error::invalid_request().data(e.to_string()))?;
            service
                .ops()
                .checkout_status(&source.checkout.root)
                .await
                .map_err(|e| Error::invalid_request().data(e.to_string()))?
                .ignored
        } else {
            Vec::new()
        };

        let mut session_workspace = serde_json::json!({ "execution": "worktree" });
        if parsed.include_local_changes {
            session_workspace["baseline"] =
                serde_json::Value::String("current_working_tree".to_string());
        }
        if let Some(base_ref) = &parsed.base_ref {
            session_workspace["baseRef"] = serde_json::Value::String(base_ref.clone());
        }
        let mut querymt_meta = serde_json::Map::new();
        querymt_meta.insert(
            crate::worktree::SESSION_WORKSPACE_META_KEY.to_string(),
            session_workspace,
        );
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt".to_string(),
            serde_json::Value::Object(querymt_meta),
        );

        let new_session =
            crate::acp::protocol::NewSessionRequest::new(std::path::PathBuf::from(&parsed.cwd))
                .meta(meta);
        let response = self.new_session(new_session).await?;
        let session_id = response.session_id.to_string();
        if let Some(name) = parsed.name {
            self.config
                .storage
                .session_store()
                .set_session_name(&session_id, Some(name))
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
        }

        let record = service
            .registry()
            .get_workspace_by_session(&session_id)
            .await
            .map_err(|err| {
                Error::internal_error()
                    .data(format!("workspace lookup failed after creation: {err}"))
            })?
            .ok_or_else(|| {
                Error::internal_error().data(
                    "session was created without a workspace binding; isolation failed \
                     closed before execution and the session must not be used"
                        .to_string(),
                )
            })?;

        // Disclose the effective session cwd from the durable binding.
        ext_json_response(&WorktreeCreateResponse {
            session_id,
            workspace_id: record.id.to_string(),
            checkout_path: record.checkout_path.display().to_string(),
            session_cwd: record.session_cwd().display().to_string(),
            branch: record.branch.as_str().to_string(),
            baseline_mode: record
                .baseline
                .as_ref()
                .map(|b| baseline_mode_label(&b.mode))
                .unwrap_or("commit"),
            baseline_commit: record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
            ignored_files,
            local_changes_excluded: !parsed.include_local_changes,
            ignored_untracked_files_excluded: true,
        })
    }

    /// `querymt/worktree/list`: all managed workspaces, newest first.
    pub(super) async fn handle_ext_worktree_list(&self) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let records = service.registry().list_workspaces().await.map_err(|err| {
            Error::internal_error().data(format!("failed to list workspaces: {err}"))
        })?;
        ext_json_response(&serde_json::json!({
            "workspaces": records.iter().map(summarize_record).collect::<Vec<_>>(),
        }))
    }

    /// `querymt/worktree/status`: durable record plus live Git state.
    pub(super) async fn handle_ext_worktree_status(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        let record = self
            .lookup_workspace(&service, &parsed.workspace_id)
            .await?;
        let status = service.manager().status(&record).await.map_err(|err| {
            Error::internal_error().data(format!(
                "failed to inspect workspace {}: {err}",
                parsed.workspace_id
            ))
        })?;
        // Compute the derived flags before moving fields into the response.
        let dirty = status.is_dirty();
        let clean_for_removal = status.is_clean_for_removal();
        ext_json_response(&WorktreeStatusResponse {
            summary: summarize_record(&record),
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

    /// `querymt/worktree/result`: retained baseline-relative result
    /// provenance for review; absence never implies integration.
    pub(super) async fn handle_ext_worktree_result(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        let record = self
            .lookup_workspace(&service, &parsed.workspace_id)
            .await?;
        let results = service
            .registry()
            .list_results(&record.id)
            .await
            .map_err(|err| {
                Error::internal_error().data(format!(
                    "failed to load results for workspace {}: {err}",
                    parsed.workspace_id
                ))
            })?;
        let status = service.manager().status(&record).await.ok();
        ext_json_response(&WorktreeResultResponse {
            workspace_id: record.id.to_string(),
            checkout_path: record.checkout_path.display().to_string(),
            branch: record.branch.as_str().to_string(),
            results: results
                .into_iter()
                .map(|result| WorktreeResultEntry {
                    baseline_commit: result.baseline_commit,
                    result_commit: result.result_commit,
                    result_ref: result.result_ref,
                    verification: result.verification.map(|v| match v {
                        crate::worktree::ResultVerificationStatus::Passed => "passed",
                        crate::worktree::ResultVerificationStatus::Failed => "failed",
                        crate::worktree::ResultVerificationStatus::Pending => "pending",
                        crate::worktree::ResultVerificationStatus::Skipped => "skipped",
                    }),
                    captured_at: result
                        .captured_at
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                })
                .collect(),
            dirty: status.map(|s| s.is_dirty()).unwrap_or(true),
        })
    }

    /// `querymt/worktree/remove`: conservative removal. Refuses active
    /// ownership or any content that would be lost; reports exactly which
    /// refs remain for recovery.
    pub(super) async fn handle_ext_worktree_remove(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        let record = self
            .lookup_workspace(&service, &parsed.workspace_id)
            .await?;
        let report = service.manager().remove(&record).await.map_err(|err| {
            Error::invalid_request().data(format!(
                "workspace {} was not removed: {err}",
                parsed.workspace_id
            ))
        })?;
        ext_json_response(&WorktreeRemoveResponse {
            workspace_id: report.workspace_id.to_string(),
            checkout_removed: report.checkout_removed,
            removed_branch: report.removed_branch.map(|b| b.as_str().to_string()),
            branch_moved_during_removal: report
                .branch_moved_during_removal
                .map(|b| b.as_str().to_string()),
            retained_refs: report.retained_refs,
        })
    }

    /// Archive inactive execution while preserving the durable session binding.
    pub(super) async fn handle_ext_worktree_archive(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.workspace_service()?;
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        let record = self
            .lookup_workspace(&service, &parsed.workspace_id)
            .await?;
        let archived = service
            .manager()
            .archive(&record)
            .await
            .map_err(|err| Error::invalid_request().data(err.to_string()))?;
        ext_json_response(&summarize_record(&archived))
    }

    /// Explicitly recover persisted allocation/removal intent, or release a
    /// pending fork-isolation startup barrier by session id; never starts an
    /// actor.
    pub(super) async fn handle_ext_worktree_recover(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeRecoverRequest = parse_workspace_op_request(req)?;
        match (parsed.workspace_id.as_deref(), parsed.session_id.as_deref()) {
            (Some(_), Some(_)) => {
                Err(Error::invalid_params()
                    .data("provide either workspace_id or session_id, not both"))
            }
            (None, None) => Err(Error::invalid_params().data("provide workspace_id or session_id")),
            (None, Some(session_id)) => {
                // Session-scoped recovery: complete a pending isolated
                // fork's provisioning (reusing its bound workspace when a
                // previous attempt already attached it) and release the
                // durable startup barrier.
                let response = self
                    .session_materializer
                    .recover_pending_fork(session_id)
                    .await?;
                let workspace = response
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("querymt"))
                    .and_then(|querymt| querymt.get(crate::worktree::SESSION_WORKSPACE_META_KEY))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ext_json_response(&serde_json::json!({
                    "session_id": session_id,
                    "sessionWorkspace": workspace,
                }))
            }
            (Some(workspace_id), None) => {
                let service = self.workspace_service()?;
                let id = crate::worktree::WorkspaceId::from_raw(workspace_id);
                let record = service
                    .registry()
                    .get_workspace(&id)
                    .await
                    .map_err(|err| {
                        Error::internal_error()
                            .data(format!("failed to load workspace {workspace_id}: {err}"))
                    })?
                    .ok_or_else(|| {
                        Error::invalid_request().data(format!("unknown workspace: {workspace_id}"))
                    })?;
                match record.lifecycle {
                    crate::worktree::WorkspaceLifecycle::Provisioning => {
                        // Recovery is self-contained: administration and
                        // recovery-time capture use the record's canonical
                        // common directory and recorded source checkout.
                        let recovered = service
                            .allocator()
                            .recover(&record)
                            .await
                            .map_err(|err| Error::invalid_request().data(err.to_string()))?;
                        // Disclose whether the baseline was durably bound
                        // before the interruption or captured at recovery
                        // time from the recorded source.
                        let baseline_source = match recovered.baseline_source {
                            crate::worktree::BaselineSource::BoundBeforeInterruption => {
                                "bound-before-interruption"
                            }
                            crate::worktree::BaselineSource::CapturedDuringRecovery => {
                                "captured-during-recovery"
                            }
                        };
                        ext_json_response(&serde_json::json!({
                            "summary": summarize_record(&recovered.record),
                            "baselineSource": baseline_source,
                        }))
                    }
                    crate::worktree::WorkspaceLifecycle::Removing => {
                        let report = service
                            .manager()
                            .remove(&record)
                            .await
                            .map_err(|err| Error::invalid_request().data(err.to_string()))?;
                        ext_json_response(&WorktreeRemoveResponse {
                            workspace_id: report.workspace_id.to_string(),
                            checkout_removed: report.checkout_removed,
                            removed_branch: report.removed_branch.map(|b| b.as_str().to_string()),
                            branch_moved_during_removal: report
                                .branch_moved_during_removal
                                .map(|b| b.as_str().to_string()),
                            retained_refs: report.retained_refs,
                        })
                    }
                    _ => Err(Error::invalid_request()
                        .data("workspace has no interrupted provisioning or removal to recover")),
                }
            }
        }
    }

    async fn lookup_workspace(
        &self,
        service: &std::sync::Arc<crate::worktree::WorkspaceService>,
        workspace_id: &str,
    ) -> Result<crate::worktree::WorkspaceRecord, Error> {
        let id = crate::worktree::WorkspaceId::from_raw(workspace_id);
        service
            .registry()
            .get_workspace(&id)
            .await
            .map_err(|err| {
                Error::internal_error()
                    .data(format!("failed to load workspace {workspace_id}: {err}"))
            })?
            .ok_or_else(|| {
                Error::invalid_request().data(format!("unknown workspace: {workspace_id}"))
            })
    }
}
