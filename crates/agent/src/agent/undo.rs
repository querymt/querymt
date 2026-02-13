//! Undo/redo logic for filesystem changes
//!
//! This module provides message-level undo and redo capabilities by leveraging
//! the git-based snapshot backend. When undoing, it reverts all filesystem changes
//! made after a specific message. Redo restores the pre-undo state.

use crate::model::MessagePart;
use anyhow::{Result, anyhow};
use log::{debug, info, warn};
use std::path::PathBuf;
use time::OffsetDateTime;
use uuid::Uuid;

/// Result of an undo operation
#[derive(Debug, Clone)]
pub struct UndoResult {
    /// Files that were reverted
    pub reverted_files: Vec<String>,
    /// The message ID used as the undo frontier
    pub message_id: String,
}

/// Result of a redo operation
#[derive(Debug, Clone)]
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
pub(crate) async fn undo_impl(
    backend: &dyn crate::snapshot::SnapshotBackend,
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
    message_id: &str,
    worktree: &std::path::Path,
) -> Result<UndoResult> {
    // 1. Snapshot current state for redo
    let pre_revert_snapshot = backend.track(worktree).await?;

    // 2. Get message history
    let history = provider
        .history_store()
        .get_history(session_id)
        .await
        .map_err(|e| anyhow!("Failed to get history: {}", e))?;

    // 3. Find the target message index
    let target_idx = history
        .iter()
        .position(|m| m.id == message_id)
        .ok_or_else(|| anyhow!("Message not found: {}", message_id))?;

    // 4. Get child sessions
    let child_sessions = provider
        .history_store()
        .list_child_sessions(session_id)
        .await
        .unwrap_or_default();

    // 5. Collect all snapshot patches
    let mut all_reverted_files = Vec::new();
    let mut sessions_to_scan = vec![session_id.to_string()];
    sessions_to_scan.extend(child_sessions);

    let mut pre_snapshots: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut all_patches: Vec<(String, String, Vec<String>)> = Vec::new();

    for scan_session_id in &sessions_to_scan {
        let session_history = if scan_session_id == session_id {
            &history[target_idx + 1..]
        } else {
            match provider.history_store().get_history(scan_session_id).await {
                Ok(child_history) => {
                    for msg in &child_history {
                        for part in &msg.parts {
                            if let MessagePart::TurnSnapshotStart {
                                turn_id,
                                snapshot_id,
                            } = part
                            {
                                pre_snapshots.insert(turn_id.clone(), snapshot_id.clone());
                            }
                        }
                    }
                    for msg in &child_history {
                        for part in &msg.parts {
                            if let MessagePart::TurnSnapshotPatch {
                                turn_id,
                                snapshot_id: _,
                                changed_paths,
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
                if let MessagePart::TurnSnapshotStart {
                    turn_id,
                    snapshot_id,
                } = part
                {
                    pre_snapshots.insert(turn_id.clone(), snapshot_id.clone());
                }
            }
        }
        for msg in session_history {
            for part in &msg.parts {
                if let MessagePart::TurnSnapshotPatch {
                    turn_id,
                    snapshot_id: _,
                    changed_paths,
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

    // Undo patches in reverse order
    for (turn_id, pre_snapshot, changed_paths) in all_patches.iter().rev() {
        let paths: Vec<PathBuf> = changed_paths.iter().map(PathBuf::from).collect();
        match backend.restore_paths(worktree, pre_snapshot, &paths).await {
            Ok(()) => all_reverted_files.extend(changed_paths.iter().cloned()),
            Err(e) => warn!("Undo: failed to restore files from turn {}: {}", turn_id, e),
        }
    }

    // If no patches found, try a restore to the snapshot at the target message.
    if all_patches.is_empty() {
        for part in &history[target_idx].parts {
            if let MessagePart::TurnSnapshotPatch { snapshot_id, .. } = part {
                debug!("Undo: fallback restore to snapshot {}", snapshot_id);
                let current_snapshot = backend.track(worktree).await?;
                let changed = backend.diff(worktree, &current_snapshot, snapshot_id).await?;
                if !changed.is_empty() {
                    backend.restore_paths(worktree, snapshot_id, &changed).await?;
                    all_reverted_files
                        .extend(changed.iter().map(|p| p.to_string_lossy().to_string()));
                }
                break;
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
        created_at: OffsetDateTime::now_utc(),
    };

    provider
        .history_store()
        .push_revert_state(session_id, revert_state)
        .await
        .map_err(|e| anyhow!("Failed to store revert state: {}", e))?;

    info!(
        "Undo: reverted {} files for session {}",
        all_reverted_files.len(),
        session_id
    );

    Ok(UndoResult {
        reverted_files: all_reverted_files,
        message_id: message_id.to_string(),
    })
}

/// Free function: redo for a session.
pub(crate) async fn redo_impl(
    backend: &dyn crate::snapshot::SnapshotBackend,
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
    worktree: &std::path::Path,
) -> Result<RedoResult> {
    let revert_state = provider
        .history_store()
        .peek_revert_state(session_id)
        .await
        .map_err(|e| anyhow!("Failed to get revert state: {}", e))?
        .ok_or_else(|| anyhow!("Nothing to redo"))?;

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
        .map_err(|e| anyhow!("Failed to update revert state: {}", e))?;

    Ok(RedoResult { restored: true })
}

/// Free function: cleanup revert state on new prompt.
pub(crate) async fn cleanup_revert_on_prompt(
    provider: &crate::session::provider::SessionProvider,
    session_id: &str,
) -> Result<()> {
    let revert_state = provider
        .history_store()
        .peek_revert_state(session_id)
        .await
        .map_err(|e| anyhow!("Failed to get revert state: {}", e))?;

    if let Some(revert_state) = revert_state {
        info!(
            "Cleaning up revert state for session {}: deleting messages after {}",
            session_id, revert_state.message_id
        );

        let deleted = provider
            .history_store()
            .delete_messages_after(session_id, &revert_state.message_id)
            .await
            .map_err(|e| anyhow!("Failed to delete messages: {}", e))?;

        debug!("Deleted {} messages after revert point", deleted);

        provider
            .history_store()
            .clear_revert_states(session_id)
            .await
            .map_err(|e| anyhow!("Failed to clear revert state: {}", e))?;
    }

    Ok(())
}
