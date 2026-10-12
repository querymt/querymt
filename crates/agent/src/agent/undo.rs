//! Undo/redo logic for filesystem changes
//!
//! This module provides message-level undo and redo capabilities by leveraging
//! the git-based snapshot backend. When undoing, it reverts all filesystem changes
//! made after a specific message. Redo restores the pre-undo state.

use crate::events::AgentEventKind;
use crate::model::MessagePart;
use log::{debug, warn};
use std::path::PathBuf;
use time::OffsetDateTime;
use tracing::instrument;
use uuid::Uuid;

/// Errors that can occur during undo/redo operations.
///
/// This type is `Serialize + Deserialize` so it can be sent as a kameo remote
/// message reply across the mesh when undo/redo is invoked on a remote session.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum UndoError {
    /// No snapshot backend has been configured for this agent.
    #[error("No snapshot backend configured")]
    NoSnapshotBackend,

    /// The session has no working directory set.
    #[error("No working directory configured")]
    NoWorkingDirectory,

    /// An error occurred in the underlying snapshot store (git backend).
    #[error("Snapshot error: {0}")]
    Snapshot(String),

    /// An error occurred reading from or writing to the session store.
    #[error("Session store error: {0}")]
    Store(String),

    /// The requested message ID was not found in session history.
    #[error("Message not found: {0}")]
    MessageNotFound(String),

    /// There is no undo state to redo (redo stack is empty).
    #[error("Nothing to redo")]
    NothingToRedo,

    /// A revert frame was captured against a different workspace than the
    /// session's current execution checkout. Redo rejects it before
    /// restoring anything, so a cross-workspace redo never partially
    /// restores files.
    #[error(
        "revert frame was captured against workspace {}, but the session runs \
         in {} — refusing to restore across workspaces",
        recorded.display(),
        current.display()
    )]
    WorkspaceMismatch {
        recorded: std::path::PathBuf,
        current: std::path::PathBuf,
    },

    /// The kameo actor message could not be delivered or the reply failed.
    #[error("Actor send error: {0}")]
    ActorSend(String),

    /// A classified remote transport failure (failure kind + delivery
    /// certainty), preserved for safe retry decisions by the operation layer.
    #[error("{0}")]
    Transport(querymt_remote::RemoteTransportFailure),

    /// Catch-all for unexpected errors.
    #[error("{0}")]
    Other(String),
}

impl UndoError {
    /// Wrap a classified remote transport failure.
    pub fn from_transport_failure(f: querymt_remote::RemoteTransportFailure) -> Self {
        UndoError::Transport(f)
    }
}

impl From<crate::snapshot::SnapshotError> for UndoError {
    fn from(e: crate::snapshot::SnapshotError) -> Self {
        UndoError::Snapshot(e.to_string())
    }
}

/// Result of an undo operation
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UndoResult {
    /// Files that were reverted
    pub reverted_files: Vec<String>,
    /// The message ID used as the undo frontier
    pub message_id: String,
}

/// Result of a redo operation
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RedoResult {
    /// Whether the redo was successful
    pub restored: bool,
}

// ══════════════════════════════════════════════════════════════════════════
//  Free functions for SessionActor usage
// ══════════════════════════════════════════════════════════════════════════

/// Free function: undo filesystem changes for a session.
///
/// Used by `SessionActor` which owns the runtime directly.
#[instrument(
    name = "undo",
    skip(backend, provider, worktree),
    fields(
        patches = tracing::field::Empty,
        reverted_files = tracing::field::Empty,
    )
)]
pub(crate) async fn undo_impl(
    backend: &dyn crate::snapshot::SnapshotBackend,
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
    message_id: &str,
    worktree: &std::path::Path,
) -> Result<UndoResult, UndoError> {
    // 1. Snapshot current state for redo
    let pre_revert_snapshot = backend.track(worktree).await?;

    // 2. Get message history
    let history = provider
        .history_store()
        .get_history(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to get history: {}", e)))?;

    // 3. Find the target message index
    let target_idx = history
        .iter()
        .position(|m| m.id == message_id)
        .ok_or_else(|| UndoError::MessageNotFound(message_id.to_string()))?;

    // 4. Get child sessions
    let child_sessions = provider
        .history_store()
        .list_child_sessions(session_id)
        .await
        .unwrap_or_default();

    // 5. Collect all snapshot patches. Messages copied into this session
    // by a fork from a different workspace are context-only: their snapshot
    // ids reference the parent's snapshot repository and must never be
    // restored or aggregated here.
    let inherited_count =
        inherited_foreign_workspace_message_count(provider, session_id, worktree, &history).await;
    // Scan the session's own history from after the undo target, but never
    // from inside the inherited fork prefix.
    let scan_start = (target_idx + 1).max(inherited_count).min(history.len());

    let mut all_reverted_files = Vec::new();
    let mut sessions_to_scan = vec![session_id.to_string()];
    sessions_to_scan.extend(child_sessions);

    let mut pre_snapshots: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut all_patches: Vec<(String, String, Vec<String>)> = Vec::new();

    for scan_session_id in &sessions_to_scan {
        let session_history = if scan_session_id == session_id {
            &history[scan_start..]
        } else {
            // Only aggregate child history captured in this session's own
            // workspace: isolated delegates record their snapshot patches
            // in their own checkouts, and restoring those here would
            // corrupt the parent's workspace. Children without a cwd
            // (legacy shared-checkout delegation) stay compatible.
            let compatible = match provider.history_store().get_session(scan_session_id).await {
                Ok(Some(child)) => child
                    .cwd
                    .as_deref()
                    .map(|child_cwd| same_workspace(child_cwd, worktree))
                    .unwrap_or(true),
                _ => false,
            };
            if !compatible {
                debug!(
                    "Undo: skipping child session {} captured in a different workspace",
                    scan_session_id
                );
                continue;
            }
            match provider.history_store().get_history(scan_session_id).await {
                Ok(child_history) => {
                    for msg in &child_history {
                        for part in &msg.parts {
                            validate_snapshot_workspace(part, worktree)?;
                            if let MessagePart::TurnSnapshotStart {
                                turn_id,
                                snapshot_id,
                                ..
                            } = part
                            {
                                pre_snapshots.insert(turn_id.clone(), snapshot_id.clone());
                            }
                        }
                    }
                    for msg in &child_history {
                        for part in &msg.parts {
                            validate_snapshot_workspace(part, worktree)?;
                            if let MessagePart::TurnSnapshotPatch {
                                turn_id,
                                snapshot_id: _,
                                changed_paths,
                                ..
                            } = part
                                && let Some(pre_snapshot) = pre_snapshots.get(turn_id)
                            {
                                all_patches.push((
                                    turn_id.clone(),
                                    pre_snapshot.clone(),
                                    changed_paths.clone(),
                                ));
                            }
                        }
                    }
                    continue;
                }
                Err(_) => continue,
            }
        };

        for msg in session_history {
            for part in &msg.parts {
                validate_snapshot_workspace(part, worktree)?;
                if let MessagePart::TurnSnapshotStart {
                    turn_id,
                    snapshot_id,
                    ..
                } = part
                {
                    pre_snapshots.insert(turn_id.clone(), snapshot_id.clone());
                }
            }
        }
        for msg in session_history {
            for part in &msg.parts {
                validate_snapshot_workspace(part, worktree)?;
                if let MessagePart::TurnSnapshotPatch {
                    turn_id,
                    snapshot_id: _,
                    changed_paths,
                    ..
                } = part
                    && let Some(pre_snapshot) = pre_snapshots.get(turn_id)
                {
                    all_patches.push((
                        turn_id.clone(),
                        pre_snapshot.clone(),
                        changed_paths.clone(),
                    ));
                }
            }
        }
    }

    tracing::Span::current().record("patches", all_patches.len());

    // Resolve every selected snapshot before restoring any path. A missing
    // or foreign snapshot must not leave a partially reverted workspace.
    for (_, pre_snapshot, _) in &all_patches {
        backend
            .diff(worktree, &pre_revert_snapshot, pre_snapshot)
            .await?;
    }
    // Undo patches in reverse order
    for (_turn_id, pre_snapshot, changed_paths) in all_patches.iter().rev() {
        let paths: Vec<PathBuf> = changed_paths.iter().map(PathBuf::from).collect();
        backend
            .restore_paths(worktree, pre_snapshot, &paths)
            .await?;
        all_reverted_files.extend(changed_paths.iter().cloned());
    }

    // If no patches found, try a restore to the snapshot at the target message.
    if all_patches.is_empty() {
        // Inherited foreign-workspace fork snapshots are context-only: an
        // undo landing on a message copied from the parent must not rewind
        // this session's workspace to the parent's filesystem state. New
        // turns made in this session remain fully undoable via the patch
        // scan above; legacy same-workspace forks and non-forked sessions
        // are unaffected.
        if target_idx < inherited_count {
            debug!("Undo: target message is inherited fork context; skipping snapshot restore");
        } else {
            for part in &history[target_idx].parts {
                validate_snapshot_workspace(part, worktree)?;
            }
            for part in &history[target_idx].parts {
                if let MessagePart::TurnSnapshotPatch { snapshot_id, .. } = part {
                    debug!("Undo: fallback restore to snapshot {}", snapshot_id);
                    let current_snapshot = backend.track(worktree).await?;
                    let changed = backend
                        .diff(worktree, &current_snapshot, snapshot_id)
                        .await?;
                    if !changed.is_empty() {
                        backend
                            .restore_paths(worktree, snapshot_id, &changed)
                            .await?;
                        all_reverted_files
                            .extend(changed.iter().map(|p| p.to_string_lossy().to_string()));
                    }
                    break;
                }
            }
        }
    }

    // Store a new stacked revert frame for redo.
    let revert_state = crate::session::domain::RevertState {
        public_id: Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        snapshot_id: pre_revert_snapshot,
        backend_id: "git".to_string(),
        // Workspace provenance: redo validates every selected frame
        // against the current execution workspace before restoring.
        workspace_root: Some(worktree.to_path_buf()),
        created_at: OffsetDateTime::now_utc(),
    };

    provider
        .history_store()
        .push_revert_state(session_id, revert_state)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to store revert state: {}", e)))?;

    tracing::Span::current().record("reverted_files", all_reverted_files.len());

    Ok(UndoResult {
        reverted_files: all_reverted_files,
        message_id: message_id.to_string(),
    })
}

/// Free function: redo for a session.
#[instrument(
    name = "redo",
    skip(backend, provider, worktree),
    fields(restored_files = tracing::field::Empty)
)]
pub(crate) async fn redo_impl(
    backend: &dyn crate::snapshot::SnapshotBackend,
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
    worktree: &std::path::Path,
) -> Result<RedoResult, UndoError> {
    let revert_state = provider
        .history_store()
        .peek_revert_state(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to get revert state: {}", e)))?
        .ok_or(UndoError::NothingToRedo)?;

    // Workspace provenance: validate the selected frame against the
    // current execution workspace BEFORE restoring anything, so a
    // cross-workspace redo never partially restores files. Frames recorded
    // without provenance (pre-dating the field) keep legacy behavior.
    validate_revert_workspace(&revert_state, worktree)?;

    let current_snapshot = backend.track(worktree).await?;
    let changed = backend
        .diff(worktree, &current_snapshot, &revert_state.snapshot_id)
        .await?;

    if !changed.is_empty() {
        backend
            .restore_paths(worktree, &revert_state.snapshot_id, &changed)
            .await?;
    }

    provider
        .history_store()
        .pop_revert_state(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to update revert state: {}", e)))?;

    tracing::Span::current().record("restored_files", changed.len());
    Ok(RedoResult { restored: true })
}

/// Validate a revert frame's workspace provenance against the current
/// execution workspace.
///
/// Called before any restore: a frame captured against a different
/// workspace is rejected with [`UndoError::WorkspaceMismatch`] so a
/// cross-workspace redo never partially restores files. Frames recorded
/// without provenance (pre-dating the field) keep legacy behavior and are
/// treated as same-workspace.
pub(crate) fn validate_revert_workspace(
    revert_state: &crate::session::domain::RevertState,
    worktree: &std::path::Path,
) -> Result<(), UndoError> {
    match &revert_state.workspace_root {
        Some(recorded) if !same_workspace(recorded, worktree) => {
            Err(UndoError::WorkspaceMismatch {
                recorded: recorded.clone(),
                current: worktree.to_path_buf(),
            })
        }
        _ => Ok(()),
    }
}

fn validate_snapshot_workspace(
    part: &MessagePart,
    worktree: &std::path::Path,
) -> Result<(), UndoError> {
    let recorded = match part {
        MessagePart::TurnSnapshotStart { workspace_root, .. }
        | MessagePart::TurnSnapshotPatch { workspace_root, .. } => workspace_root,
        _ => return Ok(()),
    };
    if let Some(recorded) = recorded
        && !same_workspace(recorded, worktree)
    {
        return Err(UndoError::WorkspaceMismatch {
            recorded: recorded.clone(),
            current: worktree.to_path_buf(),
        });
    }
    Ok(())
}

/// Canonicalized workspace comparison so symlink prefixes (e.g. macOS
/// `/var` vs `/private/var`) never reject a same-workspace frame.
fn same_workspace(a: &std::path::Path, b: &std::path::Path) -> bool {
    let canonical =
        |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

/// Count the history entries at the start of `history` that a fork copied
/// from a parent session whose workspace differs from `worktree`.
///
/// Inherited foreign-workspace snapshots are context-only: their snapshot
/// ids live in the parent's snapshot repository (keyed by the parent's
/// checkout), so undo must neither restore nor aggregate them. Returns 0
/// for non-forked sessions, delegation forks (which copy no history),
/// unresolvable fork parents, and legacy forks whose parent shares this
/// session's workspace — those keep the existing same-checkout behavior.
async fn inherited_foreign_workspace_message_count(
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
    worktree: &std::path::Path,
    history: &[crate::model::AgentMessage],
) -> usize {
    if let Some(last_foreign) = history.iter().rposition(|msg| {
        msg.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::TurnSnapshotStart {
                    inherited: true,
                    ..
                } | MessagePart::TurnSnapshotPatch {
                    inherited: true,
                    ..
                }
            ) && matches!(
                validate_snapshot_workspace(part, worktree),
                Err(UndoError::WorkspaceMismatch { .. })
            )
        })
    }) {
        return last_foreign + 1;
    }
    let Ok(Some(fork)) = provider
        .history_store()
        .get_session_fork_info(session_id)
        .await
    else {
        return 0;
    };

    // Delegation forks never copy conversation history.
    if matches!(
        fork.fork_origin,
        Some(crate::session::domain::ForkOrigin::Delegation)
    ) {
        return 0;
    }
    if fork.parent_session_id.is_none() || fork.fork_point_type.is_none() {
        return 0;
    }
    let Some(parent_internal_id) = fork.parent_session_id else {
        return 0;
    };

    // Resolve the fork parent's public id and workspace. A parent without a
    // resolvable workspace is treated as legacy: its snapshots still restore
    // exactly as before this change.
    let Ok(Some((parent_public_id, parent_cwd))) = provider
        .history_store()
        .get_session_ref_by_internal_id(parent_internal_id)
        .await
    else {
        return 0;
    };
    if parent_cwd
        .as_deref()
        .is_none_or(|parent_cwd| same_workspace(parent_cwd, worktree))
    {
        return 0;
    }

    // The copied prefix consists of verbatim parent messages (new public
    // ids, identical parts), so comparing against the parent's history
    // finds where the copies end without depending on fork-point refs.
    let Ok(parent_history) = provider
        .history_store()
        .get_history(&parent_public_id)
        .await
    else {
        return 0;
    };

    history
        .iter()
        .zip(parent_history.iter())
        .take_while(|(copied, parent)| is_copied_fork_message(copied, parent))
        .count()
}

/// A copied fork message keeps its parent's timestamp and message parts
/// verbatim; only the public id changes, so the history entries themselves
/// are identical.
fn is_copied_fork_message(
    copied: &crate::model::AgentMessage,
    parent: &crate::model::AgentMessage,
) -> bool {
    copied.created_at == parent.created_at
        && serde_json::to_string(&copied.parts).is_ok_and(|copied_parts| {
            serde_json::to_string(&parent.parts)
                .is_ok_and(|parent_parts| copied_parts == parent_parts)
        })
}

/// Free function: cleanup revert state on new prompt.
///
/// Deletes messages and event journal entries that belong to the undone turns
/// so they do not reappear after a page reload.  The frontier message itself
/// AND everything after it are removed — the frontier is the user prompt that
/// started the undone turn, so it must also be purged from history.
#[instrument(
    name = "undo.cleanup",
    skip(provider, event_journal),
    fields(
        frontier_message = tracing::field::Empty,
        deleted_messages = tracing::field::Empty,
        pruned_events = tracing::field::Empty,
    )
)]
pub(crate) async fn cleanup_revert_on_prompt(
    provider: &crate::session::provider::SessionProvider,
    event_journal: &dyn crate::session::projection::EventJournal,
    session_id: &str,
) -> Result<(), UndoError> {
    let revert_state = provider
        .history_store()
        .peek_revert_state(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to get revert state: {}", e)))?;

    let Some(revert_state) = revert_state else {
        return Ok(());
    };

    let span = tracing::Span::current();
    span.record("frontier_message", &revert_state.message_id);

    let deleted = provider
        .history_store()
        .delete_messages_after(session_id, &revert_state.message_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to delete messages: {}", e)))?;

    span.record("deleted_messages", deleted);

    // Prune the event journal: find the prompt_received event that
    // corresponds to the frontier message, then delete it and everything
    // after it so undone turns don't reappear on page reload.
    let frontier_message_id = revert_state.message_id.clone();
    if let Ok(events) = event_journal
        .load_session_stream(session_id, None, None)
        .await
    {
        let frontier_seq = events.iter().find_map(|e| {
            if let AgentEventKind::PromptReceived {
                message_id: Some(ref mid),
                ..
            } = e.kind
                && mid == &frontier_message_id
            {
                return Some(e.stream_seq);
            }
            None
        });

        if let Some(seq) = frontier_seq {
            match event_journal
                .delete_session_events_from(session_id, seq)
                .await
            {
                Ok(n) => {
                    span.record("pruned_events", n);
                }
                Err(e) => warn!("Failed to prune event journal: {}", e),
            }
        }
    }

    provider
        .history_store()
        .clear_revert_states(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to clear revert state: {}", e)))?;

    Ok(())
}
