//! Undo/redo logic for filesystem changes
//!
//! This module provides message-level undo and redo capabilities by leveraging
//! the git-based snapshot backend. When undoing, it reverts all filesystem changes
//! made after a specific message. Redo restores the pre-undo state.

use crate::events::AgentEventKind;
use crate::model::{MessagePart, TurnId};
use crate::snapshot::SnapshotId;
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

    let mut all_patches: Vec<TurnPatch> = Vec::new();

    for scan_session_id in &sessions_to_scan {
        if scan_session_id == session_id {
            all_patches.extend(resolve_turn_patches(
                &history,
                scan_start,
                inherited_count,
                Some(target_idx),
                worktree,
            )?);
            continue;
        }
        // Only aggregate child history captured in this session's own
        // workspace: isolated delegates record their snapshot patches
        // in their own checkouts, and restoring those here would
        // corrupt the parent's workspace. Children without a cwd
        // (legacy shared-checkout delegation) stay compatible.
        //
        // Workspace-resolution audit: `worktree` is the parent session's
        // effective cwd as resolved through the single session-cwd path
        // (durable workspace binding first), and a managed child's row
        // cwd equals its own bound checkout, so this comparison matches
        // workspaces — not merely copied source paths. Patches whose
        // provenance disagrees are additionally rejected by
        // `validate_snapshot_workspace` instead of being restored.
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
        if let Ok(child_history) = provider.history_store().get_history(scan_session_id).await {
            all_patches.extend(resolve_turn_patches(&child_history, 0, 0, None, worktree)?);
        }
    }

    tracing::Span::current().record("patches", all_patches.len());
    for patch in &all_patches {
        debug!(
            "Undo: turn {} restores {} path(s) from snapshot {}",
            patch.turn_id,
            patch.changed_paths.len(),
            patch.base_snapshot_id
        );
    }

    // Resolve every selected snapshot before restoring any path. A missing
    // or foreign snapshot must not leave a partially reverted workspace.
    for patch in &all_patches {
        backend
            .diff(worktree, &pre_revert_snapshot, &patch.base_snapshot_id)
            .await?;
    }
    // If no patches were found, fall back to restoring the snapshot recorded at
    // the target message. Everything that can reject the undo (provenance
    // validation, resolving the snapshot, diffing) happens here, before any
    // file is touched and before a redo frame exists: a rejected undo must
    // leave no frame behind.
    let mut fallback: Option<(SnapshotId, Vec<PathBuf>)> = None;
    if all_patches.is_empty() {
        for part in &history[target_idx].parts {
            validate_snapshot_workspace(part, worktree)?;
        }
        if target_idx < inherited_count {
            // Inherited foreign-workspace fork snapshots are context-only: an
            // undo landing on a message copied from the parent must not rewind
            // this session's workspace to the parent's filesystem state.
            debug!("Undo: target message is inherited fork context; skipping snapshot restore");
        } else if let Some(snapshot_id) =
            history[target_idx]
                .parts
                .iter()
                .find_map(|part| match part {
                    MessagePart::TurnSnapshotPatch { snapshot_id, .. } => Some(snapshot_id),
                    _ => None,
                })
        {
            debug!("Undo: fallback restore to snapshot {snapshot_id}");
            let current_snapshot = backend.track(worktree).await?;
            let changed = backend
                .diff(worktree, &current_snapshot, snapshot_id)
                .await?;
            if !changed.is_empty() {
                fallback = Some((snapshot_id.clone(), changed));
            }
        }
    }

    // Undo patches in reverse order, then the fallback snapshot.
    let mut steps: Vec<(SnapshotId, Vec<PathBuf>)> = all_patches
        .iter()
        .rev()
        .map(|patch| {
            (
                patch.base_snapshot_id.clone(),
                patch.changed_paths.iter().map(PathBuf::from).collect(),
            )
        })
        .collect();
    if let Some((snapshot_id, changed)) = &fallback {
        steps.push((snapshot_id.clone(), changed.clone()));
    }

    // Redo can only bring back what the pre-undo snapshot captured. A file
    // that exists now but is outside it (e.g. an ignore rule was added after
    // the turn) would be overwritten for good, so reject before any file
    // changes and before a redo frame exists.
    let mut doomed: Vec<PathBuf> = steps.iter().flat_map(|(_, paths)| paths.clone()).collect();
    doomed.sort();
    doomed.dedup();
    let uncaptured = backend
        .uncaptured_paths(worktree, &pre_revert_snapshot, &doomed)
        .await?;
    if !uncaptured.is_empty() {
        return Err(UndoError::Snapshot(format!(
            "refusing to undo: {} would be overwritten but is outside the undo snapshot \
             (ignored since the turn ran), so redo could not bring it back: {}",
            if uncaptured.len() == 1 {
                "a file"
            } else {
                "files"
            },
            uncaptured
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // From here on files change. Run the restores as one unit so the redo
    // frame below is recorded whether or not they succeed: a partially failed
    // undo can always be redone back to the state before the undo. The backend
    // keeps every needed snapshot alive for the whole batch.
    let restore_result: Result<(), UndoError> = backend
        .restore_batch(worktree, &steps)
        .await
        .map_err(UndoError::from);
    if restore_result.is_ok() {
        all_reverted_files.extend(
            steps
                .iter()
                .flat_map(|(_, paths)| paths.iter())
                .map(|p| p.to_string_lossy().into_owned()),
        );
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
        // A failed restore is a recovery point for an immediate redo; it is
        // not a committed undo, so it never deletes history, and a new prompt
        // discards it.
        restore_failed: restore_result.is_err(),
        created_at: OffsetDateTime::now_utc(),
    };

    if let Err(e) = provider
        .history_store()
        .push_revert_state(session_id, revert_state)
        .await
    {
        // Do not lose the restore failure behind the store failure.
        if let Err(restore_error) = &restore_result {
            tracing::warn!(
                %session_id,
                error = %restore_error,
                "undo restore failed and its redo frame could not be stored"
            );
        }
        return Err(UndoError::Store(format!(
            "Failed to store revert state: {e}"
        )));
    }

    restore_result.map_err(|e| {
        UndoError::Snapshot(format!(
            "{e}. Redo restores the state before this undo; sending a new prompt discards that option."
        ))
    })?;
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
        // Same guard as undo: never overwrite a file the current snapshot
        // does not hold (it is ignored now) with an older capture.
        let uncaptured = backend
            .uncaptured_paths(worktree, &current_snapshot, &changed)
            .await?;
        if !uncaptured.is_empty() {
            return Err(UndoError::Snapshot(format!(
                "refusing to redo: files outside the current snapshot (ignored now) \
                 would be overwritten: {}",
                uncaptured
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        backend
            .restore_batch(
                worktree,
                &[(revert_state.snapshot_id.clone(), changed.clone())],
            )
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

/// One turn's file changes and the snapshot to restore them from.
#[derive(Debug)]
struct TurnPatch {
    turn_id: TurnId,
    /// The workspace as it was before the turn ran.
    base_snapshot_id: SnapshotId,
    changed_paths: Vec<String>,
}

/// A turn-snapshot record found in a history slice.
enum SnapshotRecord<'a> {
    Start {
        snapshot_id: &'a SnapshotId,
    },
    Patch {
        base: Option<&'a SnapshotId>,
        changed_paths: &'a [String],
    },
}

/// Find each turn's pre-turn snapshot for the patches at index `>= from`,
/// newest record format first (design D16), and refuse what can't be undone
/// as whole turns (design D17).
///
/// Start records are looked up from `starts_from`. `target` is the undo
/// target's index in `history` (`None` for child sessions, which are undone
/// as a whole).
///
/// 1. The start with the patch's turn id.
/// 2. Otherwise, if the patch has no `base_snapshot_id`, the record right
///    before the patch, if it is a start that no patch in the history uses
///    (records from builds that minted a fresh turn id for the start).
///
/// The base is the patch's own `base_snapshot_id`, else the start's snapshot.
fn resolve_turn_patches(
    history: &[crate::model::AgentMessage],
    from: usize,
    starts_from: usize,
    target: Option<usize>,
    worktree: &std::path::Path,
) -> Result<Vec<TurnPatch>, UndoError> {
    let from = from.min(history.len());
    let starts_from = starts_from.min(history.len());
    for msg in &history[from..] {
        for part in &msg.parts {
            validate_snapshot_workspace(part, worktree)?;
        }
    }

    // (message index, part, record) in history order.
    let mut records: Vec<(usize, &MessagePart, TurnId, SnapshotRecord<'_>)> = Vec::new();
    for (idx, msg) in history.iter().enumerate().skip(starts_from) {
        for part in &msg.parts {
            match part {
                MessagePart::TurnSnapshotStart {
                    turn_id,
                    snapshot_id,
                    ..
                } => records.push((idx, part, *turn_id, SnapshotRecord::Start { snapshot_id })),
                MessagePart::TurnSnapshotPatch {
                    turn_id,
                    base_snapshot_id,
                    changed_paths,
                    ..
                } => records.push((
                    idx,
                    part,
                    *turn_id,
                    SnapshotRecord::Patch {
                        base: base_snapshot_id.as_ref(),
                        changed_paths,
                    },
                )),
                _ => {}
            }
        }
    }

    let claimed: std::collections::HashSet<TurnId> = records
        .iter()
        .filter(|(_, _, _, record)| matches!(record, SnapshotRecord::Patch { .. }))
        .map(|(_, _, turn_id, _)| *turn_id)
        .collect();
    let mut starts: std::collections::HashMap<TurnId, (usize, &MessagePart, &SnapshotId)> =
        std::collections::HashMap::new();
    for (idx, part, turn_id, record) in &records {
        if let SnapshotRecord::Start { snapshot_id } = record {
            starts.insert(*turn_id, (*idx, *part, *snapshot_id));
        }
    }

    let mut patches = Vec::new();
    for (pos, (idx, _, turn_id, record)) in records.iter().enumerate() {
        let SnapshotRecord::Patch {
            base,
            changed_paths,
        } = record
        else {
            continue;
        };
        if *idx < from {
            continue;
        }
        let start = starts.get(turn_id).copied().or_else(|| {
            // A patch with its own base needs no positional guess.
            if base.is_some() {
                return None;
            }
            let (start_idx, part, start_turn, record) = records.get(pos.checked_sub(1)?)?;
            match record {
                SnapshotRecord::Start { snapshot_id } if !claimed.contains(start_turn) => {
                    Some((*start_idx, *part, *snapshot_id))
                }
                _ => None,
            }
        });
        if let Some((start_idx, start_part, _)) = start {
            validate_snapshot_workspace(start_part, worktree)?;
            if let Some(target_idx) = target
                && start_idx < target_idx
            {
                return Err(UndoError::Snapshot(format!(
                    "undo reverts whole turns: message {} is inside turn {turn_id}; \
                     undo to that turn's prompt instead",
                    history[target_idx].id
                )));
            }
        }
        let Some(base_snapshot_id) = base.or_else(|| start.map(|(_, _, id)| id)) else {
            if changed_paths.is_empty() {
                continue;
            }
            return Err(UndoError::Snapshot(format!(
                "cannot undo: turn {turn_id} changed {} file(s) but its pre-turn snapshot is not recorded",
                changed_paths.len()
            )));
        };
        patches.push(TurnPatch {
            turn_id: *turn_id,
            base_snapshot_id: base_snapshot_id.clone(),
            changed_paths: changed_paths.to_vec(),
        });
    }
    Ok(patches)
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

    if revert_state.is_none() {
        return Ok(());
    }

    // Frames whose restore failed never committed an undo, so they must not
    // delete history. They are recovery points for an immediate redo only:
    // once a new prompt runs, redo could overwrite its work, so they are
    // discarded with the rest of the stack below. The frontier is the newest
    // frame whose undo completed.
    let frames = provider
        .history_store()
        .list_revert_states(session_id)
        .await
        .map_err(|e| UndoError::Store(format!("Failed to list revert states: {}", e)))?;
    for frame in frames.iter().filter(|f| f.restore_failed) {
        tracing::warn!(
            %session_id,
            snapshot_id = %frame.snapshot_id,
            "discarding the recovery frame of a failed undo because a new prompt started"
        );
    }
    let Some(revert_state) = frames.into_iter().rev().find(|f| !f.restore_failed) else {
        provider
            .history_store()
            .clear_revert_states(session_id)
            .await
            .map_err(|e| UndoError::Store(format!("Failed to clear revert state: {}", e)))?;
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
