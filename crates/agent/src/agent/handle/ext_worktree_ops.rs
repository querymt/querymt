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
use crate::acp::protocol::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use crate::worktree::{
    IsolatedSessionSpawner, WorkspaceTarget, WorktreeCreateRequest, WorktreeError, WorktreeOpError,
    WorktreeOps, WorktreeRecoverResponse,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const NATIVE_USAGE: &str = "usage: /worktree create [ref] [name] | create --include-local-changes [name], \
     list, status [id], result [id], publish [id], remove <id>, recover <id>, archive <id>; \
     no force removal";
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
    base.data(serde_json::json!({ "code": err.code(), "message": err.to_string() }))
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

/// Explicit publication target: a workspace id, or (defaulting to the
/// caller's own binding) a session id.
#[derive(Debug, Deserialize)]
struct WorktreePublishRequest {
    #[serde(default, alias = "workspaceId")]
    workspace_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
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
    List,
    Status(Option<String>),
    Result(Option<String>),
    Publish(Option<String>),
    Remove(String),
    Recover(String),
    Archive(String),
}

impl NativeCommand {
    fn parse(words: &[String]) -> Result<Self, Error> {
        const INCLUDE: &str = "--include-local-changes";
        let usage = || Error::invalid_params().data(NATIVE_USAGE);
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
                    return Err(Error::invalid_params().data(CREATE_USAGE));
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
            "list" if args.is_empty() => Ok(Self::List),
            "status" => optional_id().map(Self::Status),
            "result" => optional_id().map(Self::Result),
            "publish" => optional_id().map(Self::Publish),
            "remove" => single_id().map(Self::Remove),
            "recover" => single_id().map(Self::Recover),
            "archive" => single_id().map(Self::Archive),
            _ => Err(usage()),
        }
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

/// Session creation for [`WorktreeOps::create`] goes through the normal ACP
/// materialization path, so isolation behaves exactly like `session/new`.
#[async_trait::async_trait]
impl IsolatedSessionSpawner for LocalAgentHandle {
    async fn spawn(
        &self,
        cwd: PathBuf,
        session_workspace: serde_json::Value,
        name: Option<String>,
    ) -> Result<String, WorktreeOpError> {
        let mut querymt_meta = serde_json::Map::new();
        querymt_meta.insert(
            crate::worktree::SESSION_WORKSPACE_META_KEY.to_string(),
            session_workspace,
        );
        let mut meta = serde_json::Map::new();
        meta.insert("querymt".to_string(), querymt_meta.into());

        let response = self
            .new_session(crate::acp::protocol::NewSessionRequest::new(cwd).meta(meta))
            .await
            // Preserve the stable isolation `code` attached by the session layer.
            .map_err(|err| WorktreeOpError::from_acp(&err))?;
        let session_id = response.session_id.to_string();
        if let Some(name) = name {
            self.config
                .storage
                .session_store()
                .set_session_name(&session_id, Some(name))
                .await
                .map_err(WorktreeOpError::from)?;
        }
        Ok(session_id)
    }
}

impl LocalAgentHandle {
    fn worktree_ops(&self) -> Result<WorktreeOps, WorktreeOpError> {
        self.config
            .workspace_service()
            .map(WorktreeOps::new)
            .ok_or(WorktreeOpError::ServiceUnavailable)
    }

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
        let command = NativeCommand::parse(&words)?;
        let value = self
            .run_native_command(&session_id, command)
            .await
            .map_err(op_error_to_acp)?;

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
                        TextContent::new(value.to_string()),
                    ))),
                ))
                .await?;
        }
        let mut meta = serde_json::Map::new();
        meta.insert("querymt.worktree".into(), value);
        Ok(Some(PromptResponse::new(StopReason::EndTurn).meta(meta)))
    }

    /// Execute a parsed native command against the typed operations core.
    async fn run_native_command(
        &self,
        session_id: &str,
        command: NativeCommand,
    ) -> Result<serde_json::Value, WorktreeOpError> {
        fn to_value<T: Serialize>(value: &T) -> Result<serde_json::Value, WorktreeOpError> {
            serde_json::to_value(value).map_err(|e| WorktreeOpError::Internal(e.to_string()))
        }
        let ops = self.worktree_ops()?;
        // A command without an explicit id targets the session's own workspace.
        let own_or = |id: Option<String>| {
            let ops = ops.clone();
            async move {
                match id {
                    Some(id) => Ok(id),
                    None => ops
                        .workspace_of_session(session_id)
                        .await
                        .map(|r| r.id.to_string()),
                }
            }
        };
        match command {
            NativeCommand::Create {
                include_local_changes,
                base_ref,
                name,
            } => {
                // Isolate from the session's effective workspace, never from a
                // stale row cwd copied from a fork source.
                let persisted = self
                    .config
                    .storage
                    .session_store()
                    .get_session(session_id)
                    .await?
                    .ok_or_else(|| WorktreeOpError::InvalidOptions("session not found".into()))?
                    .cwd;
                let source_cwd = ops
                    .service()
                    .resolve_session_cwd(session_id, persisted)
                    .await?
                    .ok_or_else(|| WorktreeOpError::InvalidOptions("session has no cwd".into()))?;
                let response = ops
                    .create(
                        self,
                        WorktreeCreateRequest {
                            cwd: source_cwd.display().to_string(),
                            include_local_changes: include_local_changes.then_some(true),
                            base_ref,
                            name,
                        },
                    )
                    .await?;
                to_value(&response)
            }
            NativeCommand::List => to_value(&ops.list().await?),
            NativeCommand::Status(id) => to_value(&ops.status(&own_or(id).await?).await?),
            NativeCommand::Result(id) => to_value(&ops.result(&own_or(id).await?).await?),
            NativeCommand::Publish(Some(id)) => {
                to_value(&ops.publish(&WorkspaceTarget::Workspace(id)).await?)
            }
            NativeCommand::Publish(None) => to_value(
                &ops.publish(&WorkspaceTarget::Session(session_id.to_string()))
                    .await?,
            ),
            NativeCommand::Remove(id) => to_value(&ops.remove(&id).await?),
            NativeCommand::Recover(id) => to_value(&ops.recover(&id).await?),
            NativeCommand::Archive(id) => to_value(&ops.archive(&id).await?),
        }
    }

    /// `querymt/worktree/create`: create a new isolated session with its own
    /// managed checkout. Returns the new session identity without moving the
    /// current session's workspace.
    pub(super) async fn handle_ext_worktree_create(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeCreateRequest = parse_workspace_op_request(req)?;
        render(async { self.worktree_ops()?.create(self, parsed).await }.await)
    }

    /// `querymt/worktree/list`: all managed workspaces, newest first.
    pub(super) async fn handle_ext_worktree_list(&self) -> Result<ExtResponse, Error> {
        render(async { self.worktree_ops()?.list().await }.await)
    }

    /// `querymt/worktree/status`: durable record plus live Git state.
    pub(super) async fn handle_ext_worktree_status(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(async { self.worktree_ops()?.status(&parsed.workspace_id).await }.await)
    }

    /// `querymt/worktree/result`: retained baseline-relative review of
    /// published results; absence never implies integration. Read-only.
    pub(super) async fn handle_ext_worktree_result(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(async { self.worktree_ops()?.result(&parsed.workspace_id).await }.await)
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
                let ops = self.worktree_ops()?;
                // Default to the caller's own workspace when only a session is given.
                let target = WorkspaceTarget::from_parts(parsed.workspace_id, parsed.session_id)?;
                ops.publish(&target).await
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
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(async { self.worktree_ops()?.remove(&parsed.workspace_id).await }.await)
    }

    /// Archive inactive execution while preserving the durable session binding.
    pub(super) async fn handle_ext_worktree_archive(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: WorktreeIdRequest = parse_workspace_op_request(req)?;
        render(async { self.worktree_ops()?.archive(&parsed.workspace_id).await }.await)
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
                let response = self
                    .session_materializer
                    .recover_pending_fork(&session_id)
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
            (workspace_id, session_id) => render(
                async {
                    // Rejects "both" and "neither" with the shared diagnostics.
                    match WorkspaceTarget::from_parts(workspace_id, session_id)? {
                        WorkspaceTarget::Workspace(id) => {
                            let recovered: WorktreeRecoverResponse =
                                self.worktree_ops()?.recover(&id).await?;
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

    fn parse(line: &str) -> Result<NativeCommand, Error> {
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
        assert_eq!(parse("list").unwrap(), NativeCommand::List);
        assert_eq!(parse("status").unwrap(), NativeCommand::Status(None));
        assert_eq!(
            parse("publish ws-1").unwrap(),
            NativeCommand::Publish(Some("ws-1".into()))
        );
        assert_eq!(
            parse("remove ws-1").unwrap(),
            NativeCommand::Remove("ws-1".into())
        );
    }

    #[test]
    fn rejects_malformed_and_forced_commands() {
        for invalid in [
            "",
            "list extra",
            "create --force",
            "create HEAD name extra",
            "create --include-local-changes HEAD name",
            "create --include-local-changes --include-local-changes",
            "remove",
            "remove --force",
            "status a b",
            "bogus",
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?} must be rejected");
        }
    }
}
