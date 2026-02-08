//! Undo/redo logic for filesystem changes
//!
//! This module provides message-level undo and redo capabilities by leveraging
//! the git-based snapshot backend. When undoing, it reverts all filesystem changes
//! made after a specific message. Redo restores the pre-undo state.

use crate::agent::core::QueryMTAgent;
use crate::model::MessagePart;
use crate::session::domain::RevertState;
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
}

/// Result of a redo operation
#[derive(Debug, Clone)]
pub struct RedoResult {
    /// Whether the redo was successful
    pub restored: bool,
}

impl QueryMTAgent {
    /// Undo: revert filesystem to state at the given message_id
    ///
    /// This creates a snapshot of the current state (for redo), then restores
    /// files that were changed by tool calls after the target message.
    pub async fn undo(&self, session_id: &str, message_id: &str) -> Result<UndoResult> {
        let backend = self
            .snapshot_backend
            .as_ref()
            .ok_or_else(|| anyhow!("No snapshot backend configured"))?;

        // Get worktree from session runtime
        let worktree = {
            let runtime_map = self.session_runtime.lock().await;
            let runtime = runtime_map
                .get(session_id)
                .ok_or_else(|| anyhow!("Session runtime not found for session: {}", session_id))?;
            runtime
                .cwd
                .as_ref()
                .ok_or_else(|| {
                    anyhow!(
                        "No working directory configured for session: {}",
                        session_id
                    )
                })?
                .to_path_buf()
        };

        // 1. Snapshot current state for redo
        let pre_revert_snapshot = backend.track(&worktree).await?;
        info!(
            "Undo: created pre-revert snapshot {} for session {}",
            pre_revert_snapshot, session_id
        );

        // 2. Get message history from parent session
        let history = self
            .provider
            .history_store()
            .get_history(session_id)
            .await
            .map_err(|e| anyhow!("Failed to get history: {}", e))?;

        // 3. Find the target message index
        let target_idx = history
            .iter()
            .position(|m| m.id == message_id)
            .ok_or_else(|| anyhow!("Message not found: {}", message_id))?;

        // 4. Get child sessions (for delegation scenarios)
        let child_sessions = self
            .provider
            .history_store()
            .list_child_sessions(session_id)
            .await
            .unwrap_or_default();

        debug!(
            "Undo: found {} child sessions for session {}",
            child_sessions.len(),
            session_id
        );

        // 5. Collect all TurnSnapshotPatch/TurnSnapshotStart pairs from parent AND child sessions
        let mut all_reverted_files = Vec::new();

        // Collect sessions to scan: parent + all children
        let mut sessions_to_scan = vec![session_id.to_string()];
        sessions_to_scan.extend(child_sessions);

        // Build a global map of turn_id -> pre-snapshot_id from TurnSnapshotStart parts
        let mut pre_snapshots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut all_patches: Vec<(String, String, Vec<String>)> = Vec::new();

        // Scan all sessions (parent + children) for snapshot parts
        for scan_session_id in &sessions_to_scan {
            let session_history = if scan_session_id == session_id {
                // Already have parent history
                &history[target_idx + 1..]
            } else {
                // Get child session history (all messages, not just after target)
                match self
                    .provider
                    .history_store()
                    .get_history(scan_session_id)
                    .await
                {
                    Ok(child_history) => {
                        debug!(
                            "Undo: scanning {} messages from child session {}",
                            child_history.len(),
                            scan_session_id
                        );
                        // Collect TurnSnapshotStart from child
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
                        // Collect TurnSnapshotPatch from child
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
                    Err(e) => {
                        warn!(
                            "Undo: failed to get history for child session {}: {}",
                            scan_session_id, e
                        );
                        continue;
                    }
                }
            };

            // For parent session, scan messages after target
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

        let patches_to_undo = all_patches;

        // Undo patches in reverse order
        for (turn_id, pre_snapshot, changed_paths) in patches_to_undo.iter().rev() {
            let paths: Vec<PathBuf> = changed_paths.iter().map(PathBuf::from).collect();
            match backend.restore_paths(&worktree, pre_snapshot, &paths).await {
                Ok(()) => {
                    debug!(
                        "Undo: restored {} files from turn {} snapshot {}",
                        paths.len(),
                        turn_id,
                        pre_snapshot
                    );
                    all_reverted_files.extend(changed_paths.iter().cloned());
                }
                Err(e) => {
                    warn!("Undo: failed to restore files from turn {}: {}", turn_id, e);
                }
            }
        }

        // If no patches found, try a restore to the snapshot at the target message
        if patches_to_undo.is_empty() {
            // Look for a TurnSnapshotPatch in the target message itself
            // to get the snapshot state at that point
            for part in &history[target_idx].parts {
                if let MessagePart::TurnSnapshotPatch { snapshot_id, .. } = part {
                    debug!("Undo: fallback restore to snapshot {}", snapshot_id);

                    // Compute diff to determine what needs to change
                    let current_snapshot = backend.track(&worktree).await?;
                    let changed = backend
                        .diff(&worktree, &current_snapshot, snapshot_id)
                        .await?;

                    if !changed.is_empty() {
                        backend
                            .restore_paths(&worktree, snapshot_id, &changed)
                            .await?;
                        all_reverted_files
                            .extend(changed.iter().map(|p| p.to_string_lossy().to_string()));
                    }
                    break;
                }
            }
        }

        // 5. Store revert state for redo
        let revert_state = RevertState {
            public_id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            snapshot_id: pre_revert_snapshot,
            backend_id: "git".to_string(),
            created_at: OffsetDateTime::now_utc(),
        };

        self.provider
            .history_store()
            .set_revert_state(session_id, Some(revert_state))
            .await
            .map_err(|e| anyhow!("Failed to store revert state: {}", e))?;

        info!(
            "Undo: reverted {} files for session {}",
            all_reverted_files.len(),
            session_id
        );

        Ok(UndoResult {
            reverted_files: all_reverted_files,
        })
    }

    /// Redo: restore to pre-undo state
    pub async fn redo(&self, session_id: &str) -> Result<RedoResult> {
        let backend = self
            .snapshot_backend
            .as_ref()
            .ok_or_else(|| anyhow!("No snapshot backend configured"))?;

        // Get worktree from session runtime
        let worktree = {
            let runtime_map = self.session_runtime.lock().await;
            let runtime = runtime_map
                .get(session_id)
                .ok_or_else(|| anyhow!("Session runtime not found for session: {}", session_id))?;
            runtime
                .cwd
                .as_ref()
                .ok_or_else(|| {
                    anyhow!(
                        "No working directory configured for session: {}",
                        session_id
                    )
                })?
                .to_path_buf()
        };

        let revert_state = self
            .provider
            .history_store()
            .get_revert_state(session_id)
            .await
            .map_err(|e| anyhow!("Failed to get revert state: {}", e))?
            .ok_or_else(|| anyhow!("Nothing to redo"))?;

        info!(
            "Redo: restoring to pre-undo snapshot {} for session {}",
            revert_state.snapshot_id, session_id
        );

        // Take current snapshot and compute diff to target state
        let current_snapshot = backend.track(&worktree).await?;
        let changed = backend
            .diff(&worktree, &current_snapshot, &revert_state.snapshot_id)
            .await?;

        // Use restore_paths to correctly handle both file restoration and deletion
        if !changed.is_empty() {
            debug!("Redo: restoring {} changed files", changed.len());
            backend
                .restore_paths(&worktree, &revert_state.snapshot_id, &changed)
                .await?;
        } else {
            debug!("Redo: no file changes needed");
        }

        // Clear revert state
        self.provider
            .history_store()
            .set_revert_state(session_id, None)
            .await
            .map_err(|e| anyhow!("Failed to clear revert state: {}", e))?;

        Ok(RedoResult { restored: true })
    }

    /// Called when a new prompt is sent while in reverted state.
    /// This deletes messages after the revert point and clears the revert state,
    /// effectively "committing" the undo.
    pub(crate) async fn cleanup_revert_on_prompt(&self, session_id: &str) -> Result<()> {
        let revert_state = self
            .provider
            .history_store()
            .get_revert_state(session_id)
            .await
            .map_err(|e| anyhow!("Failed to get revert state: {}", e))?;

        if let Some(revert_state) = revert_state {
            info!(
                "Cleaning up revert state for session {}: deleting messages after {}",
                session_id, revert_state.message_id
            );

            // Delete messages after the revert point
            let deleted = self
                .provider
                .history_store()
                .delete_messages_after(session_id, &revert_state.message_id)
                .await
                .map_err(|e| anyhow!("Failed to delete messages: {}", e))?;

            debug!("Deleted {} messages after revert point", deleted);

            // Clear revert state
            self.provider
                .history_store()
                .set_revert_state(session_id, None)
                .await
                .map_err(|e| anyhow!("Failed to clear revert state: {}", e))?;
        }

        Ok(())
    }
}
