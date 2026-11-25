//! Additive ACP extension surface for managed worktree workspaces.
//!
//! The `querymt/worktree/*` extension methods and the native `/worktree`
//! command are thin adapters over [`WorktreeOps`]: each parses its own input,
//! calls the typed operation, and renders the typed response. Ownership,
//! lease and removal rules live in the operations core, so this surface
//! cannot drift from the simple API or the Python bindings. Upstream protocol
//! types are not modified.
//!
//! `create` goes through the same materialization path as ACP session
//! creation (`_meta.querymt.sessionWorkspace`), so the new session receives
//! its own workspace and the current session is never moved.

use super::utils::ext_json_response;
use super::*;
use crate::acp::delivery::with_event_tip;
use crate::acp::protocol::{ContentBlock, StopReason};
use crate::agent::session_materializer::ReadOnlyReason;
use crate::worktree::{
    HandoffHost, HandoffOptions, IsolatedSessionSpawner, OwnerHost, WorkspaceTarget,
    WorktreeCreateRequest, WorktreeError, WorktreeOpError, WorktreeOps, WorktreeRecoverResponse,
};
use querymt_agent_core::events::AgentEventKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const NATIVE_USAGE: &str = "usage: /worktree create [ref] [name] | create --include-local-changes [name], \
     handoff [name] [--summary], list, status [id], result [id], publish [id], archive [id], \
     remove [id] [--archive], recover <id>; no force removal";
const REMOVE_USAGE: &str = "usage: /worktree remove [id] [--archive]";
const HANDOFF_USAGE: &str = "usage: /worktree handoff [name] [--summary]";
const CREATE_USAGE: &str =
    "usage: /worktree create [ref] [name] OR /worktree create --include-local-changes [name]";

fn parse_workspace_op_request<T: serde::de::DeserializeOwned>(req: ExtRequest) -> Result<T, Error> {
    serde_json::from_str(req.params.get())
        .map_err(|e| Error::invalid_params().data(serde_json::json!({ "error": e.to_string() })))
}

/// Map a typed operation error onto ACP error semantics. The stable `code`
/// travels in the error data on every failure; the message stays readable.
fn op_error_to_acp(err: WorktreeOpError) -> Error {
    let base = match &err {
        WorktreeOpError::RemovalRefused { source, .. } if source.is_typed_refusal() => {
            Error::invalid_request()
        }
        WorktreeOpError::ServiceUnavailable
        | WorktreeOpError::Internal(_)
        | WorktreeOpError::SessionStart(_) => Error::internal_error(),
        WorktreeOpError::InvalidOptions(_) => Error::invalid_params(),
        WorktreeOpError::Rejected { code, .. } if code == "invalid_options" => {
            Error::invalid_params()
        }
        WorktreeOpError::Rejected { .. } => Error::invalid_request(),
        WorktreeOpError::UnknownWorkspace(_) | WorktreeOpError::RemovalRefused { .. } => {
            Error::invalid_request()
        }
        WorktreeOpError::Worktree(inner) => match inner {
            WorktreeError::Storage(_) | WorktreeError::Io(_) => Error::internal_error(),
            WorktreeError::SessionCwdConflict { .. } => Error::invalid_params(),
            _ => Error::invalid_request(),
        },
    };
    base.data(err.error_data())
}

/// Render a typed outcome as an ACP extension response.
fn render<T: Serialize>(outcome: Result<T, WorktreeOpError>) -> Result<ExtResponse, Error> {
    ext_json_response(&outcome.map_err(op_error_to_acp)?)
}

// ── Request payloads that only exist at the ACP boundary ─────────────────────

#[derive(Debug, Deserialize)]
struct WorktreeIdRequest {
    workspace_id: String,
}

/// Archive/remove target: a workspace id, or the id of the session that owns
/// it. `archive` (remove only) archives an owned workspace first.
#[derive(Debug, Deserialize)]
struct WorktreeTargetRequest {
    #[serde(default, alias = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    archive: bool,
}

/// Explicit publication target: a workspace id, or (defaulting to the
/// caller's own binding) a session id.
#[derive(Debug, Deserialize)]
struct WorktreePublishRequest {
    #[serde(default, alias = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
}

/// Handoff target: a workspace id or its owner session id, plus the options.
#[derive(Debug, Deserialize)]
struct WorktreeHandoffRequest {
    #[serde(default, alias = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(flatten)]
    options: HandoffOptions,
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

// ── Native `/worktree` command ───────────────────────────────────────────────

/// A parsed native command. Parsing is pure so every malformed input is
/// rejected before any administration happens.
#[derive(Debug, PartialEq, Eq)]
enum NativeCommand {
    Create {
        include_local_changes: bool,
        base_ref: Option<String>,
        name: Option<String>,
    },
    Handoff(HandoffOptions),
    List,
    Status(Option<String>),
    Result(Option<String>),
    Publish(Option<String>),
    Remove {
        id: Option<String>,
        archive: bool,
    },
    Recover(String),
    Archive(Option<String>),
}

impl NativeCommand {
    /// Whether the command changes the session's own workspace or starts new
    /// work, so an archived (read-only) session must refuse it. Inspection and
    /// cleanup stay available.
    fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::Create { .. } | Self::Handoff(_) | Self::Publish(_) | Self::Archive(_)
        )
    }

    fn parse(words: &[String]) -> Result<Self, WorktreeOpError> {
        const INCLUDE: &str = "--include-local-changes";
        let usage = || WorktreeOpError::InvalidOptions(NATIVE_USAGE.into());
        let operation = words.first().map(String::as_str).unwrap_or("");
        let args = &words[words.len().min(1)..];
        let single_id = || match args {
            [id] if !id.starts_with('-') => Ok(id.clone()),
            _ => Err(usage()),
        };
        let optional_id = || match args {
            [] => Ok(None),
            [id] => Ok(Some(id.clone())),
            _ => Err(usage()),
        };
        match operation {
            "create" => {
                let flags = args.iter().filter(|a| a.as_str() == INCLUDE).count();
                let positional: Vec<&String> =
                    args.iter().filter(|a| a.as_str() != INCLUDE).collect();
                let include = flags == 1;
                let max_positional = if include { 1 } else { 2 };
                if flags > 1
                    || positional.len() > max_positional
                    || positional.iter().any(|a| a.starts_with('-'))
                {
                    return Err(WorktreeOpError::InvalidOptions(CREATE_USAGE.into()));
                }
                let (base_ref, name) = if include {
                    (None, positional.first())
                } else {
                    (positional.first(), positional.get(1))
                };
                Ok(Self::Create {
                    include_local_changes: include,
                    base_ref: base_ref.map(|s| (*s).clone()),
                    name: name.map(|s| (*s).clone()),
                })
            }
            "handoff" => {
                const SUMMARY: &str = "--summary";
                let summary = args.iter().filter(|a| a.as_str() == SUMMARY).count();
                let positional: Vec<&String> =
                    args.iter().filter(|a| a.as_str() != SUMMARY).collect();
                if summary > 1
                    || positional.len() > 1
                    || positional.iter().any(|a| a.starts_with('-'))
                {
                    return Err(WorktreeOpError::InvalidOptions(HANDOFF_USAGE.into()));
                }
                Ok(Self::Handoff(HandoffOptions {
                    name: positional.first().map(|n| (*n).clone()),
                    summary: summary == 1,
                }))
            }
            "list" if args.is_empty() => Ok(Self::List),
            "status" => optional_id().map(Self::Status),
            "result" => optional_id().map(Self::Result),
            "publish" => optional_id().map(Self::Publish),
            "remove" => {
                const ARCHIVE: &str = "--archive";
                let flags = args.iter().filter(|a| a.as_str() == ARCHIVE).count();
                let positional: Vec<&String> =
                    args.iter().filter(|a| a.as_str() != ARCHIVE).collect();
                if flags > 1
                    || positional.len() > 1
                    || positional.iter().any(|a| a.starts_with('-'))
                {
                    return Err(WorktreeOpError::InvalidOptions(REMOVE_USAGE.into()));
                }
                Ok(Self::Remove {
                    id: positional.first().map(|id| (*id).clone()),
                    archive: flags == 1,
                })
            }
            "recover" => single_id().map(Self::Recover),
            "archive" => match args {
                [] => Ok(Self::Archive(None)),
                [id] if !id.starts_with('-') => Ok(Self::Archive(Some(id.clone()))),
                _ => Err(usage()),
            },
            _ => Err(usage()),
        }
    }
}

/// Native commands are parsed before prompt expansion and never sent to a model.
pub(super) fn native_worktree_command(
    prompt: &[ContentBlock],
) -> Result<Option<Vec<String>>, Error> {
    use crate::slash_commands::builtin::{Builtin, BuiltinInvocation};
    Ok(match Builtin::parse(prompt)? {
        Some(BuiltinInvocation {
            builtin: Builtin::Worktree,
            args,
        }) => Some(args.split_whitespace().map(str::to_owned).collect()),
        _ => None,
    })
}

/// Creates worktree sessions for [`WorktreeOps::create`] through the same
/// path as `session/new`, in the runtime that hosts the source session (or,
/// without a source, the runtime `session/new` would pick). The new session
/// is bound to that runtime's profile before it can run.
struct WorktreeSpawner<'a> {
    connection: &'a LocalAgentHandle,
    host: SessionHost<'a>,
}

#[async_trait::async_trait]
impl IsolatedSessionSpawner for WorktreeSpawner<'_> {
    async fn spawn(
        &self,
        cwd: PathBuf,
        session_workspace: serde_json::Value,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError> {
        let meta = serde_json::Map::from_iter([(
            crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
            serde_json::json!({ crate::worktree::SESSION_WORKSPACE_META_KEY: session_workspace }),
        )]);
        let response = self
            .connection
            .materialize_new_session(&self.host, NewSessionRequest::new(cwd).meta(meta))
            .await
            // Preserve the stable isolation `code` attached by the session layer.
            .map_err(|err| WorktreeOpError::from_acp(&err))?;
        let session_id = response.session_id.to_string();
        if let Some(name) = name {
            self.host
                .handle()
                .config
                .storage
                .session_store()
                .set_session_name(&session_id, Some(name))
                .await?;
        }
        Ok(session_id)
    }

    async fn source_session_cwd(
        &self,
        session_id: &str,
    ) -> Result<Option<PathBuf>, WorktreeOpError> {
        let Some(session) = self
            .connection
            .config
            .storage
            .session_store()
            .get_session(session_id)
            .await?
        else {
            return Ok(None);
        };
        // Remote-hosted sessions have no local filesystem to isolate from.
        if let Ok(actor) = self
            .connection
            .session_ref_for_agent_session(session_id)
            .await
            && actor.is_remote()
        {
            return Ok(None);
        }
        let service = self
            .host
            .handle()
            .config
            .workspace_service()
            .ok_or(WorktreeOpError::ServiceUnavailable)?;
        Ok(service.resolve_session_cwd(session_id, session.cwd).await?)
    }
}

/// A native command without an id targets the issuing session's workspace.
fn native_target(id: Option<String>, session_id: &str) -> WorkspaceTarget {
    id.map_or_else(
        || WorkspaceTarget::Session(session_id.to_owned()),
        WorkspaceTarget::Workspace,
    )
}

/// State held while a handoff successor is recovered.
struct HandoffRecoveryGuard {
    previous: String,
    workspace_id: String,
    /// Exclusive activity lease, held until the transfer commits. `None` when
    /// the transfer had already committed.
    lease: Option<crate::worktree::gate::CheckoutActivityLease>,
}

impl HandoffRecoveryGuard {
    fn committed(previous: String, workspace_id: String) -> Self {
        Self {
            previous,
            workspace_id,
            lease: None,
        }
    }
}

/// Session-layer steps shared by handoff, archive and removal.
#[async_trait::async_trait]
impl OwnerHost for LocalAgentHandle {
    async fn is_local(&self, session_id: &str) -> Result<bool, WorktreeOpError> {
        if let Ok(actor) = self.session_ref_for_agent_session(session_id).await
            && actor.is_remote()
        {
            return Ok(false);
        }
        Ok(self
            .config
            .storage
            .session_store()
            .get_session(session_id)
            .await?
            .is_some())
    }

    async fn quiesce_owner(&self, session_id: &str) -> Result<(), WorktreeOpError> {
        let busy = self
            .unload_session_if_idle(session_id)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))?;
        if busy {
            return Err(WorktreeOpError::rejected(
                "owner_active",
                format!("session {session_id} has a running turn or queued input"),
            ));
        }
        Ok(())
    }

    async fn announce_archived(
        &self,
        owner: &str,
        workspace_id: &str,
        by_session_id: Option<&str>,
    ) -> Option<i64> {
        let config = self.hosting_config(owner).await?;
        let event = config
            .emit_event_persisted(
                owner,
                AgentEventKind::WorktreeArchived {
                    workspace_id: workspace_id.to_string(),
                    by_session_id: by_session_id.map(str::to_owned),
                },
            )
            .await;
        // The owner was unloaded by the quiesce step: no actor retires its stream.
        config.event_sink.retire_session_stream(owner).await;
        match event {
            Ok(event) => Some(event.session_seq),
            Err(err) => {
                tracing::warn!(owner, error = %err, "failed to record archive announcement");
                None
            }
        }
    }
}

/// Session-layer steps of an ownership handoff (see [`HandoffHost`]).
#[async_trait::async_trait]
impl HandoffHost for LocalAgentHandle {
    async fn summarize(&self, previous: &str) -> Result<Option<String>, WorktreeOpError> {
        self.op_host(previous)
            .await?
            .handle()
            .summarize_in_runtime(previous)
            .await
    }

    async fn seed_summary(&self, successor: &str, summary: &str) -> Result<(), WorktreeOpError> {
        self.op_host(successor)
            .await?
            .handle()
            .seed_summary_in_runtime(successor, summary)
            .await
    }

    /// The successor is created by the runtime hosting the previous owner (so
    /// it carries that runtime's execution config) and bound to the same
    /// profile/runtime before it can be published; otherwise it could never
    /// load once ownership moved.
    async fn create_pending_successor(
        &self,
        previous: &str,
        record: &crate::worktree::WorkspaceRecord,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError> {
        let host = self.op_host(previous).await?;
        let runtime = host.handle();
        let successor = runtime
            .create_pending_successor_in_runtime(previous, record, name)
            .await?;
        if let Some(binding) = host.binding()
            && let Some(profiles) = self.profiles()
            && let Err(err) = profiles
                .set_session_binding(&successor, binding.clone())
                .await
        {
            runtime.discard_successor_in_runtime(&successor).await;
            return Err(WorktreeOpError::SessionStart(format!(
                "failed to bind handoff successor to the owner's profile: {err}"
            )));
        }
        Ok(successor)
    }

    async fn publish_successor(&self, successor: &str) -> Result<(), WorktreeOpError> {
        self.op_host(successor)
            .await?
            .handle()
            .publish_successor_in_runtime(successor)
            .await
    }

    async fn announce_handoff(
        &self,
        previous: &str,
        workspace_id: &str,
        successor: &str,
        summary_seeded: bool,
    ) -> Option<i64> {
        let config = self.hosting_config(previous).await?;
        let event = config
            .emit_event_persisted(
                previous,
                querymt_agent_core::events::AgentEventKind::WorktreeOwnershipTransferred {
                    workspace_id: workspace_id.to_string(),
                    successor_session_id: successor.to_string(),
                    summary_seeded,
                },
            )
            .await;
        // No actor remains to retire the previous owner's stream.
        config.event_sink.retire_session_stream(previous).await;
        match event {
            Ok(event) => Some(event.session_seq),
            Err(err) => {
                tracing::warn!(previous, error = %err, "failed to record handoff announcement");
                None
            }
        }
    }

    async fn discard_successor(&self, successor: &str) {
        match self.session_host(successor).await {
            Ok(host) => host.handle().discard_successor_in_runtime(successor).await,
            Err(_) => self.discard_successor_in_runtime(successor).await,
        }
    }
}

impl LocalAgentHandle {
    /// [`Self::session_host`] with the error as a typed operation error.
    async fn op_host(&self, session_id: &str) -> Result<SessionHost<'_>, WorktreeOpError> {
        self.session_host(session_id)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))
    }

    async fn summarize_in_runtime(
        &self,
        previous: &str,
    ) -> Result<Option<String>, WorktreeOpError> {
        let failed = |message: String| WorktreeOpError::rejected("summary_failed", message);
        // The owner's actor is already unloaded, so `session/cancel` finds no
        // actor to stop: it reaches this step through the registration.
        let cancel = self.admin_cancels.begin(previous);
        let handle = self
            .config
            .provider
            .with_session(previous)
            .await
            .map_err(|err| failed(err.to_string()))?;
        let messages = handle
            .get_effective_agent_history()
            .await
            .map_err(|err| failed(err.to_string()))?;
        if messages.is_empty() {
            return Ok(None);
        }
        // The summary is sent where `[agent.execution.compaction]` says (the
        // previous session's provider and model when it says nothing), like
        // any other compaction.
        // Building a provider can wait (a retry, a credential refresh, a mesh
        // lookup) while the workspace is held exclusively, so the cancel
        // reaches this step too. Nothing has been created yet, and the
        // accounting and stream below are not set up, so there is nothing to
        // release.
        let target = tokio::select! {
            biased;
            () = cancel.token().cancelled() => {
                return Err(WorktreeOpError::rejected(
                    "summary_cancelled",
                    "the handoff summary was cancelled; ownership is unchanged",
                ));
            }
            target = handle.resolve_compaction_model(&self.config.execution_policy.compaction) => {
                target.map_err(|err| failed(format!("no LLM provider for summary: {err}")))?
            }
        };
        // The handoff summary is a paid request outside any prompt: charge it
        // to a maintenance cycle of the previous session.
        let accounting = self
            .config
            .event_sink
            .session_accounting(previous)
            .await
            .map_err(|err| failed(format!("accounting unavailable: {err}")))?;
        let cycle =
            querymt_agent_services::accounting::CycleAccounting::begin_maintenance(accounting);
        let recorder = Arc::new(
            querymt_agent_services::accounting::AttemptRecorder::with_projection(
                self.config.event_sink.session_stream(previous),
                cycle.session_accounting().clone(),
            ),
        );
        let ledger = querymt_agent_services::accounting::DispatchLedger {
            operation: Arc::new(
                querymt_agent_services::accounting::AttemptOperation::new(
                    recorder,
                    &cycle,
                    querymt_agent_services::accounting::AttemptPurpose::Compaction,
                    target.provider_name.as_str(),
                    target.model.as_str(),
                )
                .with_budget_pricing(self.config.budget_pricing())
                .with_budget_cap(self.config.budget_cap_usd()),
            ),
            basis: target.basis.clone(),
        };
        let dispatch = crate::session::compaction::CompactionDispatch {
            provider: target.provider.clone(),
            config: &self.config,
            // The dispatcher watches it everywhere it waits (between retries,
            // for a reply, between stream chunks) and settles the attempt in
            // flight, with the usage seen so far, when it fires.
            cancel: cancel.token(),
            ledger: &ledger,
        };
        let result = self
            .config
            .compaction
            .process(&messages, None, None, &dispatch)
            .await
            .map_err(|err| match ledger.operation.budget_refusal() {
                // Nothing was sent: say so, instead of blaming the provider.
                Some((cap, spent)) => failed(format!(
                    "summary not requested: price limit reached: ${spent:.4} >= ${cap:.2}"
                )),
                None => failed(err.to_string()),
            })
            .and_then(|result| {
                crate::session::compaction::SessionCompaction::validate_summary(&result.summary)
                    .map(|()| result.summary)
                    .map_err(|err| failed(err.to_string()))
            });
        // The previous owner was unloaded by quiesce, so no actor retires the
        // stream and accounting set up above. Release them now so no later
        // failure leaks them; a successful announcement reopens the stream.
        // The local holders must go first or the stream is not released.
        drop(ledger);
        drop(cycle);
        self.config.event_sink.retire_session_stream(previous).await;
        // The step stays cancellable until here: deregister it, then honour
        // any cancel it accepted, including one that arrived as the summary
        // finished or while the stream was retired. The handoff has created
        // nothing, so nothing is left behind.
        let token = cancel.token().clone();
        drop(cancel);
        if token.is_cancelled() {
            return Err(WorktreeOpError::rejected(
                "summary_cancelled",
                "the handoff summary was cancelled; ownership is unchanged",
            ));
        }
        result.map(Some)
    }

    async fn seed_summary_in_runtime(
        &self,
        successor: &str,
        summary: &str,
    ) -> Result<(), WorktreeOpError> {
        let (request, answer) =
            crate::session::compaction::SessionCompaction::create_compaction_messages(
                successor,
                summary,
                summary.len() / 4,
            );
        let store = self.config.provider.history_store();
        for message in [request, answer] {
            store
                .add_message(successor, message)
                .await
                .map_err(|err| WorktreeOpError::rejected("summary_failed", err.to_string()))?;
        }
        Ok(())
    }

    async fn create_pending_successor_in_runtime(
        &self,
        previous: &str,
        record: &crate::worktree::WorkspaceRecord,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError> {
        self.session_materializer
            .create_pending_handoff_successor(previous, record, name)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))
    }

    async fn publish_successor_in_runtime(&self, successor: &str) -> Result<(), WorktreeOpError> {
        self.session_materializer
            .publish_handoff_successor(successor)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))
    }

    async fn discard_successor_in_runtime(&self, successor: &str) {
        self.session_materializer
            .discard_pending_handoff_successor(successor)
            .await;
    }

    /// Config of the runtime hosting `session_id`. Each profile runtime owns
    /// its own event sink, so session events must go through this one.
    /// `None` (logged) when the runtime cannot be resolved.
    pub(crate) async fn hosting_config(&self, session_id: &str) -> Option<Arc<AgentConfig>> {
        match self.session_host(session_id).await {
            Ok(host) => Some(host.handle().config.clone()),
            Err(err) => {
                tracing::warn!(session_id, error = %err, "worktree: runtime lookup failed");
                None
            }
        }
    }

    /// Preconditions for recovering a handoff successor whose previous owner
    /// still owns the workspace (crash before the transfer): the owner must
    /// be idle and unloaded, and the exclusive activity lease is held until
    /// the transfer commits. `None` for non-handoff recovery or when the
    /// transfer already committed.
    async fn guard_handoff_recovery(
        &self,
        successor: &str,
    ) -> Result<Option<HandoffRecoveryGuard>, WorktreeOpError> {
        let Some(intent) = self
            .config
            .provider
            .history_store()
            .get_pending_fork_isolation(successor)
            .await?
        else {
            return Ok(None);
        };
        let Some(workspace_id) = intent.handoff_workspace_id.clone() else {
            return Ok(None);
        };
        let previous = intent.source_session_id;
        let ops = self
            .worktree_ops(Some(&WorkspaceTarget::Workspace(workspace_id.clone())))
            .await?;
        let record = ops.lookup(&workspace_id).await?;
        let owner = record.ownership.owner_session_id.as_deref();
        if owner == Some(successor) {
            // Transfer already committed; only the barrier and (possibly) the
            // announcement are missing.
            return Ok(Some(HandoffRecoveryGuard::committed(
                previous,
                workspace_id,
            )));
        }
        if owner != Some(previous.as_str()) {
            // Let the service produce its stable refusal.
            return Ok(None);
        }
        self.quiesce_owner(&previous).await?;
        let lease = ops.acquire_exclusive_activity(&record).await?;
        // The owner may have changed while it was being unloaded.
        let record = ops.lookup(&workspace_id).await?;
        if record.ownership.owner_session_id.as_deref() != Some(previous.as_str()) {
            return Err(WorktreeOpError::rejected(
                "not_owner",
                "workspace ownership changed while recovery was starting",
            ));
        }
        Ok(Some(HandoffRecoveryGuard {
            previous,
            workspace_id,
            lease: Some(lease),
        }))
    }

    /// Record the handoff announcement on the previous owner's stream when
    /// recovery completed a transfer that never announced it. Best effort.
    async fn announce_recovered_handoff(
        &self,
        previous: &str,
        successor: &str,
        workspace_id: &str,
    ) {
        let Some(config) = self.hosting_config(previous).await else {
            return;
        };
        let already = match config
            .event_sink
            .journal()
            .load_session_stream(previous, None, None)
            .await
        {
            Ok(events) => events.into_iter().any(|event| {
                matches!(
                    querymt_agent_core::events::AgentEvent::from(event).kind,
                    AgentEventKind::WorktreeOwnershipTransferred {
                        successor_session_id: ref s, ..
                    } if s == successor
                )
            }),
            Err(err) => {
                tracing::warn!(previous, error = %err, "handoff recovery: journal unreadable");
                return;
            }
        };
        if already {
            return;
        }
        let seeded = self
            .config
            .provider
            .history_store()
            .get_history(successor)
            .await
            .map(|history| !history.is_empty())
            .unwrap_or(false);
        self.announce_handoff(previous, workspace_id, successor, seeded)
            .await;
    }

    /// Cancels the administration step running for `session_id` without a
    /// session actor (a handoff summary), on the runtime that hosts it.
    /// Returns whether there was one.
    pub(super) async fn cancel_admin_operation(&self, session_id: &str) -> bool {
        match self.session_host(session_id).await {
            Ok(host) => host.handle().admin_cancels.cancel(session_id),
            Err(_) => self.admin_cancels.cancel(session_id),
        }
    }

    /// Operations over the workspace service that administers `target`.
    ///
    /// Each runtime's service shares the host's workspace registry but owns
    /// its storage root, where checkout locks live, so the choice matters:
    ///
    /// - a session: the service of the runtime hosting it, which is the one
    ///   its `/worktree` catalog entry was advertised from;
    /// - a workspace: its owner's, else the service whose storage root holds
    ///   the checkout;
    /// - nothing (listing): the first service, in [`Self::workspace_services`]
    ///   order.
    ///
    /// # Errors
    ///
    /// [`WorktreeOpError::ServiceUnavailable`] when no service applies.
    async fn worktree_ops(
        &self,
        target: Option<&WorkspaceTarget>,
    ) -> Result<WorktreeOps, WorktreeOpError> {
        let service = match target {
            Some(WorkspaceTarget::Session(session_id)) => self
                .op_host(session_id)
                .await?
                .handle()
                .config
                .workspace_service(),
            Some(WorkspaceTarget::Workspace(workspace_id)) => {
                self.workspace_service_for(workspace_id).await?
            }
            None => self.workspace_services().await.into_iter().next(),
        };
        service
            .map(WorktreeOps::new)
            .ok_or(WorktreeOpError::ServiceUnavailable)
    }

    /// Workspace services in a stable order: this handle's, then those of
    /// the loaded profile runtimes by profile id.
    async fn workspace_services(&self) -> Vec<Arc<crate::worktree::WorkspaceService>> {
        let mut runtimes = match self.profiles() {
            Some(profiles) => profiles.materialized_runtimes().await,
            None => Vec::new(),
        };
        runtimes.sort_unstable_by(|a, b| a.profile_id().cmp(b.profile_id()));
        self.config
            .workspace_service()
            .into_iter()
            .chain(
                runtimes
                    .iter()
                    .filter_map(|runtime| runtime.handle.config.workspace_service()),
            )
            .collect()
    }

    /// The service administering workspace `workspace_id` (see
    /// [`Self::worktree_ops`]).
    async fn workspace_service_for(
        &self,
        workspace_id: &str,
    ) -> Result<Option<Arc<crate::worktree::WorkspaceService>>, WorktreeOpError> {
        let record = match self.config.storage.workspace_repository() {
            Some(registry) => {
                registry
                    .get_workspace(&crate::worktree::WorkspaceId::from_raw(workspace_id))
                    .await?
            }
            None => None,
        };
        if let Some(owner) = record
            .as_ref()
            .and_then(|record| record.ownership.owner_session_id.as_deref())
            && let Ok(host) = self.session_host(owner).await
            && let Some(service) = host.handle().config.workspace_service()
        {
            return Ok(Some(service));
        }
        let services = self.workspace_services().await;
        let holding_checkout = record.as_ref().and_then(|record| {
            services
                .iter()
                .find(|service| record.checkout_path.starts_with(service.storage_root()))
        });
        Ok(holding_checkout.or(services.first()).cloned())
    }

    /// The runtime a new worktree session is created in: the one hosting
    /// its source session, or the one `session/new` would pick.
    async fn worktree_creation_host(
        &self,
        source_session_id: Option<&str>,
    ) -> Result<SessionHost<'_>, WorktreeOpError> {
        let Some(source) = source_session_id else {
            return match self.profiles() {
                Some(profiles) => {
                    let runtime = profiles
                        .active_runtime()
                        .await
                        .map_err(|err| WorktreeOpError::SessionStart(format!("{err:#}")))?;
                    Ok(SessionHost::Profile {
                        handle: runtime.handle(),
                        binding: runtime.session_binding(),
                    })
                }
                None => Ok(SessionHost::Connection(self)),
            };
        };
        // Like load/resume: a session from before profiles gets a binding,
        // so its child lands where the source will run.
        self.adopt_unbound_session_binding(source)
            .await
            .map_err(|err| WorktreeOpError::from_acp(&err))?;
        self.op_host(source).await
    }

    /// Create a session with its own managed worktree, in the runtime that
    /// hosts its source session (its workspace service, execution config and
    /// profile binding), or where `session/new` would create it.
    ///
    /// # Errors
    ///
    /// Fails with a stable [`WorktreeOpError::code`] when the hosting runtime
    /// has no workspace service, the options are invalid, or isolation fails.
    pub async fn create_worktree_session(
        &self,
        request: WorktreeCreateRequest,
    ) -> Result<crate::worktree::WorktreeCreateResponse, WorktreeOpError> {
        let host = self
            .worktree_creation_host(request.source_session_id.as_deref())
            .await?;
        let service = host
            .handle()
            .config
            .workspace_service()
            .ok_or(WorktreeOpError::ServiceUnavailable)?;
        let spawner = WorktreeSpawner {
            connection: self,
            host,
        };
        WorktreeOps::new(service).create(&spawner, request).await
    }

    pub(super) async fn handle_native_worktree_prompt(
        &self,
        req: &PromptRequest,
    ) -> Result<Option<PromptResponse>, Error> {
        let Some(words) = native_worktree_command(&req.prompt)? else {
            return Ok(None);
        };
        let session_id = req.session_id.to_string();
        // Require an accessible, locally hosted session before any
        // administration. A read-only session has no actor, so locality comes
        // from the session store rather than the actor registry. A session
        // that is not hosted here has no local stream to answer on, so this
        // refusal is an error response, with a stable code like every other.
        if !OwnerHost::is_local(self, &session_id)
            .await
            .map_err(op_error_to_acp)?
        {
            return Err(op_error_to_acp(WorktreeOpError::rejected(
                "not_local",
                "worktree administration requires local filesystem execution",
            )));
        }
        // A previous owner has no workspace left to administer.
        let read_only = self
            .session_materializer
            .read_only_reason(&session_id)
            .await?;
        if let Some(reason @ ReadOnlyReason::OwnerTransferred { .. }) = &read_only {
            return Err(reason.error(&session_id));
        }

        // Every failure below, usage errors included, is a visible reply: a
        // successful turn carrying the error code, never a bare JSON-RPC error.
        let outcome = async {
            let command = NativeCommand::parse(&words)?;
            if let Some(ReadOnlyReason::WorkspaceArchived { workspace_id }) = read_only
                && command.is_mutating()
            {
                return Err(WorktreeOpError::Worktree(
                    WorktreeError::WorkspaceArchived { workspace_id },
                ));
            }
            self.run_native_command(&session_id, command).await
        }
        .await;

        let (payload, text, announced_tip) = match outcome {
            Ok((value, tip)) => {
                let text = value.to_string();
                (value, text, tip)
            }
            Err(err) => (
                serde_json::json!({ "error": err.error_data() }),
                err.to_string(),
                None,
            ),
        };

        // Output is a session update on the issuing session's ordered stream,
        // so the transport's event-tip hold delivers it before the response on
        // every connection attached to the session. An announcement already
        // emitted on this stream (handoff, self-archive) is the output.
        let tip = match announced_tip {
            Some(tip) => tip,
            None => match self.hosting_config(&session_id).await {
                Some(config) => {
                    let tip = config
                        .emit_ephemeral_event(
                            &session_id,
                            AgentEventKind::WorktreeCommandOutput { text },
                        )
                        .await;
                    // A session without an actor has nothing else to retire
                    // its stream.
                    if self
                        .session_ref_for_agent_session(&session_id)
                        .await
                        .is_err()
                    {
                        config.event_sink.retire_session_stream(&session_id).await;
                    }
                    tip
                }
                None => 0,
            },
        };
        let mut meta = serde_json::Map::new();
        meta.insert("querymt.worktree".into(), payload);
        let response = PromptResponse::new(StopReason::EndTurn).meta(meta);
        Ok(Some(with_event_tip(response, tip)))
    }

    /// Execute a parsed native command against the typed operations core.
    async fn run_native_command(
        &self,
        session_id: &str,
        command: NativeCommand,
    ) -> Result<(serde_json::Value, Option<i64>), WorktreeOpError> {
        fn to_value<T: Serialize>(value: &T) -> Result<serde_json::Value, WorktreeOpError> {
            serde_json::to_value(value).map_err(|e| WorktreeOpError::Internal(e.to_string()))
        }
        // Creation resolves the source's effective cwd (never a stale row cwd
        // copied from a fork source) and its runtime itself.
        let command = match command {
            NativeCommand::Create {
                include_local_changes,
                base_ref,
                name,
            } => {
                let response = self
                    .create_worktree_session(WorktreeCreateRequest {
                        cwd: String::new(),
                        include_local_changes: include_local_changes.then_some(true),
                        base_ref,
                        name,
                        source_session_id: Some(session_id.to_string()),
                    })
                    .await?;
                return Ok((to_value(&response)?, None));
            }
            other => other,
        };
        let own = WorkspaceTarget::Session(session_id.to_string());
        let ops = self.worktree_ops(Some(&own)).await?;
        // A mutation of another workspace runs under that workspace's service.
        let ops_for = async |target: &WorkspaceTarget| match target {
            WorkspaceTarget::Session(_) => Ok(ops.clone()),
            WorkspaceTarget::Workspace(_) => self.worktree_ops(Some(target)).await,
        };
        // A read without an explicit id targets the session's own workspace.
        let own_or = async |id: Option<String>| match id {
            Some(id) => Ok(id),
            None => ops
                .workspace_of_session(session_id)
                .await
                .map(|record| record.id.to_string()),
        };
        let value = match command {
            NativeCommand::Create { .. } => unreachable!("handled above"),
            // The owner actor is unloaded by the handoff, so the reply is the
            // durable announcement on the previous owner's stream.
            NativeCommand::Handoff(options) => {
                let (response, tip) = ops.handoff_announced(self, &own, options).await?;
                return Ok((to_value(&response)?, tip));
            }
            NativeCommand::List => to_value(&ops.list().await?),
            NativeCommand::Status(id) => to_value(&ops.status(&own_or(id).await?).await?),
            NativeCommand::Result(id) => to_value(&ops.result(&own_or(id).await?).await?),
            NativeCommand::Publish(id) => {
                let target = native_target(id, session_id);
                to_value(&ops_for(&target).await?.publish(&target).await?)
            }
            NativeCommand::Remove { id, archive } => {
                let target = native_target(id, session_id);
                let (response, _) = ops_for(&target)
                    .await?
                    .remove_announced(self, &target, archive, Some(session_id))
                    .await?;
                to_value(&response)
            }
            NativeCommand::Recover(id) => {
                let target = WorkspaceTarget::Workspace(id);
                to_value(&ops_for(&target).await?.recover(target.id()).await?)
            }
            // The owner is unloaded by the archive, so when the issuing session
            // is the owner the reply is the durable announcement on its stream.
            NativeCommand::Archive(id) => {
                let target = native_target(id, session_id);
                let outcome = ops_for(&target)
                    .await?
                    .archive_announced(self, &target, Some(session_id))
                    .await?;
                let tip = outcome
                    .announcement
                    .filter(|announcement| announcement.session_id == session_id)
                    .map(|announcement| announcement.seq);
                return Ok((to_value(&outcome.summary)?, tip));
            }
        }?;
        Ok((value, None))
    }

    /// `querymt/worktree/create`: create a new isolated session with its own
    /// managed checkout. Returns the new session identity without moving the
    /// current session's workspace.
    pub(super) async fn handle_ext_worktree_create(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeCreateRequest = parse_workspace_op_request(req)?;
        render(self.create_worktree_session(parsed).await)
    }

    /// `querymt/worktree/handoff`: move ownership of a ready workspace to a
    /// new session in the same checkout. The owner must be local and idle.
    pub(super) async fn handle_ext_worktree_handoff(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeHandoffRequest = parse_workspace_op_request(req)?;
        render(
            async {
                let target = WorkspaceTarget::from_parts(parsed.workspace_id, parsed.session_id)?;
                self.worktree_ops(Some(&target))
                    .await?
                    .handoff(self, &target, parsed.options)
                    .await
            }
            .await,
        )
    }

    /// `querymt/worktree/list`: all managed workspaces, newest first.
    pub(super) async fn handle_ext_worktree_list(&self) -> Result<ExtResponse, Error> {
        render(async { self.worktree_ops(None).await?.list().await }.await)
    }

    /// `querymt/worktree/status`: durable record plus live Git state.
    pub(super) async fn handle_ext_worktree_status(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(
            async {
                let target = WorkspaceTarget::Workspace(parsed.workspace_id);
                self.worktree_ops(Some(&target))
                    .await?
                    .status(target.id())
                    .await
            }
            .await,
        )
    }

    /// `querymt/worktree/result`: retained baseline-relative review of
    /// published results; absence never implies integration. Read-only.
    pub(super) async fn handle_ext_worktree_result(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(
            async {
                let target = WorkspaceTarget::Workspace(parsed.workspace_id);
                self.worktree_ops(Some(&target))
                    .await?
                    .result(target.id())
                    .await
            }
            .await,
        )
    }

    /// `querymt/worktree/publish`: explicitly publish the current working
    /// state of a workspace as an immutable, reviewable result.
    pub(super) async fn handle_ext_worktree_publish(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreePublishRequest = parse_workspace_op_request(req)?;
        render(
            async {
                // Default to the caller's own workspace when only a session is given.
                let target = WorkspaceTarget::from_parts(parsed.workspace_id, parsed.session_id)?;
                self.worktree_ops(Some(&target))
                    .await?
                    .publish(&target)
                    .await
            }
            .await,
        )
    }

    /// `querymt/worktree/remove`: conservative removal. Refuses active
    /// ownership or any content that would be lost; reports retained refs.
    pub(super) async fn handle_ext_worktree_remove(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeTargetRequest = parse_workspace_op_request(req)?;
        render(
            async {
                let target = WorkspaceTarget::from_parts(parsed.workspace_id, parsed.session_id)?;
                self.worktree_ops(Some(&target))
                    .await?
                    .remove(self, &target, parsed.archive)
                    .await
            }
            .await,
        )
    }

    /// Archive inactive execution while preserving the durable session binding.
    pub(super) async fn handle_ext_worktree_archive(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeTargetRequest = parse_workspace_op_request(req)?;
        render(
            async {
                let target = WorkspaceTarget::from_parts(parsed.workspace_id, parsed.session_id)?;
                self.worktree_ops(Some(&target))
                    .await?
                    .archive(self, &target)
                    .await
            }
            .await,
        )
    }

    /// Explicitly recover persisted allocation/removal intent, or release a
    /// pending fork-isolation startup barrier by session id; never starts an
    /// actor.
    pub(super) async fn handle_ext_worktree_recover(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeRecoverRequest = parse_workspace_op_request(req)?;
        match (parsed.workspace_id, parsed.session_id) {
            (None, Some(session_id)) => {
                // Session-scoped recovery: complete a pending isolated fork's
                // provisioning (reusing its bound workspace when a previous
                // attempt already attached it) and release the startup barrier.
                //
                // A handoff successor may still be waiting on the previous
                // owner: quiesce it and hold the exclusive lease around the
                // ownership transfer, exactly like a live handoff.
                let guard = self
                    .guard_handoff_recovery(&session_id)
                    .await
                    .map_err(op_error_to_acp)?;
                let response = self
                    .session_materializer
                    .recover_pending_fork(&session_id)
                    .await?;
                if let Some(guard) = guard {
                    drop(guard.lease);
                    self.announce_recovered_handoff(
                        &guard.previous,
                        &session_id,
                        &guard.workspace_id,
                    )
                    .await;
                }
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
            (workspace_id, session_id) => render(
                async {
                    // Rejects "both" and "neither" with the shared diagnostics.
                    match WorkspaceTarget::from_parts(workspace_id, session_id)? {
                        target @ WorkspaceTarget::Workspace(_) => {
                            let recovered: WorktreeRecoverResponse = self
                                .worktree_ops(Some(&target))
                                .await?
                                .recover(target.id())
                                .await?;
                            Ok(recovered)
                        }
                        WorkspaceTarget::Session(_) => unreachable!("handled above"),
                    }
                }
                .await,
            ),
        }
    }
}

#[cfg(test)]
mod native_command_tests {
    use super::*;

    fn parse(line: &str) -> Result<NativeCommand, WorktreeOpError> {
        let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        NativeCommand::parse(&words)
    }

    #[test]
    fn parses_every_supported_command() {
        assert_eq!(
            parse("create").unwrap(),
            NativeCommand::Create {
                include_local_changes: false,
                base_ref: None,
                name: None
            }
        );
        assert_eq!(
            parse("create v1 child").unwrap(),
            NativeCommand::Create {
                include_local_changes: false,
                base_ref: Some("v1".into()),
                name: Some("child".into())
            }
        );
        assert_eq!(
            parse("create --include-local-changes child").unwrap(),
            NativeCommand::Create {
                include_local_changes: true,
                base_ref: None,
                name: Some("child".into())
            }
        );
        assert_eq!(
            parse("handoff").unwrap(),
            NativeCommand::Handoff(HandoffOptions::default())
        );
        let HandoffOptions { name, summary } = match parse("handoff next --summary").unwrap() {
            NativeCommand::Handoff(options) => options,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!((name.as_deref(), summary), (Some("next"), true));
        assert_eq!(parse("list").unwrap(), NativeCommand::List);
        assert_eq!(parse("status").unwrap(), NativeCommand::Status(None));
        assert_eq!(
            parse("publish ws-1").unwrap(),
            NativeCommand::Publish(Some("ws-1".into()))
        );
        assert_eq!(
            parse("remove ws-1").unwrap(),
            NativeCommand::Remove {
                id: Some("ws-1".into()),
                archive: false
            }
        );
        assert_eq!(
            parse("remove --archive").unwrap(),
            NativeCommand::Remove {
                id: None,
                archive: true
            }
        );
        assert_eq!(
            parse("remove ws-1 --archive").unwrap(),
            NativeCommand::Remove {
                id: Some("ws-1".into()),
                archive: true
            }
        );
        assert_eq!(parse("archive").unwrap(), NativeCommand::Archive(None));
        assert_eq!(
            parse("archive ws-1").unwrap(),
            NativeCommand::Archive(Some("ws-1".into()))
        );
        for mutating in ["create", "handoff", "publish", "archive"] {
            assert!(parse(mutating).unwrap().is_mutating(), "{mutating}");
        }
        for allowed in ["list", "status", "result", "remove", "recover ws-1"] {
            assert!(!parse(allowed).unwrap().is_mutating(), "{allowed}");
        }
    }

    #[test]
    fn rejects_malformed_and_forced_commands() {
        for invalid in [
            "",
            "list extra",
            "handoff a b",
            "handoff --summary --summary",
            "handoff --force",
            "create --force",
            "create HEAD name extra",
            "create --include-local-changes HEAD name",
            "create --include-local-changes --include-local-changes",
            "remove a b",
            "remove --archive --archive",
            "remove --force",
            "archive a b",
            "archive --force",
            "status a b",
            "bogus",
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?} must be rejected");
        }
    }
}
