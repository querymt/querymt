//! Integration tests for undo/redo with full agent setup
//!
//! These tests validate the complete undo flow including:
//! - Message history lookup
//! - Cross-session delegation awareness
//! - File restoration
//! - Proper reverted files list

use crate::agent::undo::UndoError;
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::projection::{EventJournal, NewDurableEvent};
use crate::test_utils::UndoTestFixture;
use anyhow::Result;
use querymt_agent_core::events::{AgentEventKind, EventOrigin};
use querymt_agent_core::model::MessagePart;
use std::sync::Arc;
use tempfile::TempDir;

// ==================== Test Suite: Basic Undo with Message History ====================

#[tokio::test]
async fn test_undo_rejects_foreign_snapshot_before_any_restore() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("test.txt", "original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    let (turn, pre) = fixture.take_pre_snapshot(&session).await?;
    fixture.write_file("test.txt", "modified")?;
    fixture.take_post_snapshot(&session, &turn, &pre).await?;
    let foreign = TempDir::new()?;
    // Inject a direct foreign entry after a valid patch. Preflight must
    // reject the whole selection, not restore the valid patch first.
    let mut message = querymt_agent_core::model::AgentMessage::new(
        session.clone(),
        querymt::chat::ChatRole::Assistant,
    );
    message.parts.push(MessagePart::TurnSnapshotStart {
        turn_id: querymt_agent_core::model::TurnId::new(),
        snapshot_id: pre,
        workspace_root: Some(foreign.path().to_path_buf()),
        inherited: false,
    });
    fixture
        .storage
        .session_store()
        .add_message(&session, message)
        .await?;
    let error = fixture.undo(&session, &target).await.unwrap_err();
    assert!(
        matches!(error, UndoError::WorkspaceMismatch { .. }),
        "unexpected error: {error:?}"
    );
    assert_eq!(fixture.read_file("test.txt")?, "modified");
    Ok(())
}

#[tokio::test]
async fn test_undo_single_agent_with_file_changes() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Setup
    fixture.write_file("test.txt", "original")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Change the file")
        .await?;
    let (step_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Make file changes
    fixture.write_file("test.txt", "modified")?;
    fixture
        .take_post_snapshot(&session_id, &step_id, &pre_snapshot)
        .await?;

    // Verify modified
    assert_eq!(fixture.read_file("test.txt")?, "modified");

    // Undo
    let result = fixture.undo(&session_id, &user_msg_id).await?;

    // Verify reverted
    assert_eq!(fixture.read_file("test.txt")?, "original");
    assert_eq!(result.reverted_files.len(), 1);
    assert!(result.reverted_files.contains(&"test.txt".to_string()));
    assert_eq!(result.message_id, user_msg_id);

    Ok(())
}

#[tokio::test]
async fn test_undo_multiple_files() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Setup multiple files
    fixture.write_file("a.txt", "a-original")?;
    fixture.write_file("b.txt", "b-original")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Change files")
        .await?;
    let (step_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Modify both files
    fixture.write_file("a.txt", "a-modified")?;
    fixture.write_file("b.txt", "b-modified")?;
    fixture
        .take_post_snapshot(&session_id, &step_id, &pre_snapshot)
        .await?;

    // Undo
    let result = fixture.undo(&session_id, &user_msg_id).await?;

    // Verify both files reverted
    assert_eq!(fixture.read_file("a.txt")?, "a-original");
    assert_eq!(fixture.read_file("b.txt")?, "b-original");
    assert_eq!(result.reverted_files.len(), 2);

    Ok(())
}

#[tokio::test]
async fn test_undo_file_deletion() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Setup
    fixture.write_file("original.txt", "content")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture.add_user_message(&session_id, "Add file").await?;
    let (step_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Add new file
    fixture.write_file("new.txt", "new content")?;
    fixture
        .take_post_snapshot(&session_id, &step_id, &pre_snapshot)
        .await?;

    // Verify new file exists
    assert!(fixture.worktree.path().join("new.txt").exists());

    // Undo
    let _result = fixture.undo(&session_id, &user_msg_id).await?;

    // Verify new file was removed
    assert!(!fixture.worktree.path().join("new.txt").exists());

    Ok(())
}

// ==================== Test Suite: Cross-Session Undo (Delegation) ====================

#[tokio::test]
async fn test_undo_cross_session_delegation() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    fixture.write_file("test.txt", "original")?;

    // Create parent and child sessions
    let parent_id = fixture.create_session().await?;
    let child_id = fixture.create_child_session(&parent_id).await?;

    // User message in parent
    let user_msg_id = fixture.add_user_message(&parent_id, "Make changes").await?;

    // Snapshots in CHILD session (delegation scenario)
    let (step_id, pre_snapshot) = fixture.take_pre_snapshot(&child_id).await?;
    fixture.write_file("test.txt", "modified by delegate")?;
    fixture
        .take_post_snapshot(&child_id, &step_id, &pre_snapshot)
        .await?;

    // Verify file is modified
    assert_eq!(fixture.read_file("test.txt")?, "modified by delegate");

    // Undo on PARENT session (this is the critical test!)
    let result = fixture.undo(&parent_id, &user_msg_id).await?;

    // Should revert child session's changes
    assert_eq!(fixture.read_file("test.txt")?, "original");
    assert_eq!(result.reverted_files.len(), 1);
    assert!(result.reverted_files.contains(&"test.txt".to_string()));

    Ok(())
}

#[tokio::test]
async fn test_undo_multiple_child_sessions() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    fixture.write_file("a.txt", "a-original")?;
    fixture.write_file("b.txt", "b-original")?;

    // Create parent and two child sessions
    let parent_id = fixture.create_session().await?;
    let child1_id = fixture.create_child_session(&parent_id).await?;
    let child2_id = fixture.create_child_session(&parent_id).await?;

    // User message in parent
    let user_msg_id = fixture.add_user_message(&parent_id, "Make changes").await?;

    // Child 1: Modify a.txt
    let (step1_id, pre1) = fixture.take_pre_snapshot(&child1_id).await?;
    fixture.write_file("a.txt", "a-modified")?;
    fixture
        .take_post_snapshot(&child1_id, &step1_id, &pre1)
        .await?;

    // Child 2: Modify b.txt
    let (step2_id, pre2) = fixture.take_pre_snapshot(&child2_id).await?;
    fixture.write_file("b.txt", "b-modified")?;
    fixture
        .take_post_snapshot(&child2_id, &step2_id, &pre2)
        .await?;

    // Verify both files are modified
    assert_eq!(fixture.read_file("a.txt")?, "a-modified");
    assert_eq!(fixture.read_file("b.txt")?, "b-modified");

    // Undo on parent should revert both children's changes
    let result = fixture.undo(&parent_id, &user_msg_id).await?;

    assert_eq!(fixture.read_file("a.txt")?, "a-original");
    assert_eq!(fixture.read_file("b.txt")?, "b-original");
    assert_eq!(result.reverted_files.len(), 2);

    Ok(())
}

// Parent undo aggregates only shared-workspace children: an isolated
// delegate (a child captured in its own workspace) is never restored into
// the parent's checkout, and undo still succeeds.
#[tokio::test]
async fn test_parent_undo_skips_isolated_child() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let isolated_cwd = TempDir::new()?;

    fixture.write_file("a.txt", "a-original")?;

    // Parent (shared worktree) plus a shared delegate child and an
    // isolated delegate child (captured in its own workspace).
    let parent_id = fixture.create_session().await?;
    let shared_child = fixture.create_child_session(&parent_id).await?;
    let isolated_child = fixture
        .storage
        .session_store()
        .create_session(
            None,
            Some(isolated_cwd.path().to_path_buf()),
            Some(parent_id.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await?
        .public_id;

    let user_msg_id = fixture.add_user_message(&parent_id, "Make changes").await?;

    // Shared child modifies a.txt.
    let (shared_step, shared_pre) = fixture.take_pre_snapshot(&shared_child).await?;
    fixture.write_file("a.txt", "a-modified")?;
    fixture
        .take_post_snapshot(&shared_child, &shared_step, &shared_pre)
        .await?;

    // The isolated child's history also claims changes to a.txt — those
    // patches belong to ITS workspace and must not touch the parent's.
    let (iso_step, iso_pre) = fixture.take_pre_snapshot(&isolated_child).await?;
    fixture.write_file("a.txt", "a-isolated")?;
    fixture
        .take_post_snapshot(&isolated_child, &iso_step, &iso_pre)
        .await?;

    assert_eq!(fixture.read_file("a.txt")?, "a-isolated");

    // Undo succeeds and reverts only the shared child's delta.
    let result = fixture.undo(&parent_id, &user_msg_id).await?;
    assert_eq!(fixture.read_file("a.txt")?, "a-original");
    assert_eq!(result.reverted_files, vec!["a.txt".to_string()]);

    Ok(())
}

// ==================== Test Suite: Redo (Undo the Undo) ====================

#[tokio::test]
async fn test_redo_restores_file_modification() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Setup
    fixture.write_file("test.txt", "original")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Change the file")
        .await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Make file changes
    fixture.write_file("test.txt", "modified")?;
    fixture
        .take_post_snapshot(&session_id, &turn_id, &pre_snapshot)
        .await?;

    // Verify modified
    assert_eq!(fixture.read_file("test.txt")?, "modified");

    // Undo - should restore to "original"
    fixture.undo(&session_id, &user_msg_id).await?;
    assert_eq!(fixture.read_file("test.txt")?, "original");

    // Redo - should restore to "modified"
    let redo_result = fixture.redo(&session_id).await?;
    assert!(redo_result.restored);
    assert_eq!(fixture.read_file("test.txt")?, "modified");

    Ok(())
}

#[tokio::test]
async fn test_redo_restores_file_deletion() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Setup - file exists initially
    fixture.write_file("to_delete.txt", "content")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Delete the file")
        .await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Delete the file
    std::fs::remove_file(fixture.worktree.path().join("to_delete.txt"))?;
    fixture
        .take_post_snapshot(&session_id, &turn_id, &pre_snapshot)
        .await?;

    // Verify file is deleted
    assert!(!fixture.worktree.path().join("to_delete.txt").exists());

    // Undo - should restore the file
    fixture.undo(&session_id, &user_msg_id).await?;
    assert!(fixture.worktree.path().join("to_delete.txt").exists());
    assert_eq!(fixture.read_file("to_delete.txt")?, "content");

    // Redo - should delete the file again (THIS IS THE BUG FIX TEST)
    let redo_result = fixture.redo(&session_id).await?;
    assert!(redo_result.restored);
    assert!(!fixture.worktree.path().join("to_delete.txt").exists());

    Ok(())
}

#[tokio::test]
async fn test_redo_restores_file_creation() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Create a new file")
        .await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Create a new file
    fixture.write_file("new.txt", "new content")?;
    fixture
        .take_post_snapshot(&session_id, &turn_id, &pre_snapshot)
        .await?;

    // Verify file exists
    assert!(fixture.worktree.path().join("new.txt").exists());

    // Undo - should remove the newly created file
    fixture.undo(&session_id, &user_msg_id).await?;
    assert!(!fixture.worktree.path().join("new.txt").exists());

    // Redo - should recreate the file
    let redo_result = fixture.redo(&session_id).await?;
    assert!(redo_result.restored);
    assert!(fixture.worktree.path().join("new.txt").exists());
    assert_eq!(fixture.read_file("new.txt")?, "new content");

    Ok(())
}

#[tokio::test]
async fn test_redo_with_no_revert_state_fails() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;

    // Try to redo without having done an undo first
    let result = fixture.redo(&session_id).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Nothing to redo"));

    Ok(())
}

#[tokio::test]
async fn test_undo_twice_then_redo_once_restores_latest_undone_step() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    fixture.write_file("test.txt", "original")?;

    let session_id = fixture.create_session().await?;

    // Turn 1: original -> v1
    let user_msg_1 = fixture.add_user_message(&session_id, "Set v1").await?;
    let (turn1_id, pre1_snapshot) = fixture.take_pre_snapshot(&session_id).await?;
    fixture.write_file("test.txt", "v1")?;
    fixture
        .take_post_snapshot(&session_id, &turn1_id, &pre1_snapshot)
        .await?;

    // Turn 2: v1 -> v2
    let user_msg_2 = fixture.add_user_message(&session_id, "Set v2").await?;
    let (turn2_id, pre2_snapshot) = fixture.take_pre_snapshot(&session_id).await?;
    fixture.write_file("test.txt", "v2")?;
    fixture
        .take_post_snapshot(&session_id, &turn2_id, &pre2_snapshot)
        .await?;

    assert_eq!(fixture.read_file("test.txt")?, "v2");

    // Undo latest turn (v2 -> v1)
    fixture.undo(&session_id, &user_msg_2).await?;
    assert_eq!(fixture.read_file("test.txt")?, "v1");

    // Undo previous turn (v1 -> original)
    fixture.undo(&session_id, &user_msg_1).await?;
    assert_eq!(fixture.read_file("test.txt")?, "original");

    // Redo once should restore only the most recently undone step (original -> v1)
    let redo_result = fixture.redo(&session_id).await?;
    assert!(redo_result.restored);
    assert_eq!(fixture.read_file("test.txt")?, "v1");

    Ok(())
}

#[tokio::test]
async fn test_delete_two_files_full_undo_redo_cycle_tracks_stack_depth() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    fixture.write_file("first.txt", "first")?;
    fixture.write_file("second.txt", "second")?;

    let session_id = fixture.create_session().await?;

    let revert_stack_len = || async {
        let states = fixture
            .storage
            .session_store()
            .list_revert_states(&session_id)
            .await?;
        Ok::<usize, anyhow::Error>(states.len())
    };

    assert_eq!(revert_stack_len().await?, 0);

    // Turn 1: delete first.txt
    let user_msg_1 = fixture
        .add_user_message(&session_id, "delete first file")
        .await?;
    let (turn1_id, pre1_snapshot) = fixture.take_pre_snapshot(&session_id).await?;
    std::fs::remove_file(fixture.worktree.path().join("first.txt"))?;
    fixture
        .take_post_snapshot(&session_id, &turn1_id, &pre1_snapshot)
        .await?;

    // Turn 2: delete second.txt
    let user_msg_2 = fixture
        .add_user_message(&session_id, "delete second file")
        .await?;
    let (turn2_id, pre2_snapshot) = fixture.take_pre_snapshot(&session_id).await?;
    std::fs::remove_file(fixture.worktree.path().join("second.txt"))?;
    fixture
        .take_post_snapshot(&session_id, &turn2_id, &pre2_snapshot)
        .await?;

    assert!(!fixture.worktree.path().join("first.txt").exists());
    assert!(!fixture.worktree.path().join("second.txt").exists());
    assert_eq!(revert_stack_len().await?, 0);

    // Undo second deletion: second.txt restored, first.txt still deleted
    fixture.undo(&session_id, &user_msg_2).await?;
    assert!(!fixture.worktree.path().join("first.txt").exists());
    assert!(fixture.worktree.path().join("second.txt").exists());
    assert_eq!(fixture.read_file("second.txt")?, "second");
    assert_eq!(revert_stack_len().await?, 1);

    // Undo first deletion: both files restored
    fixture.undo(&session_id, &user_msg_1).await?;
    assert!(fixture.worktree.path().join("first.txt").exists());
    assert!(fixture.worktree.path().join("second.txt").exists());
    assert_eq!(fixture.read_file("first.txt")?, "first");
    assert_eq!(fixture.read_file("second.txt")?, "second");
    assert_eq!(revert_stack_len().await?, 2);

    // Redo once: re-apply first deletion only
    fixture.redo(&session_id).await?;
    assert!(!fixture.worktree.path().join("first.txt").exists());
    assert!(fixture.worktree.path().join("second.txt").exists());
    assert_eq!(fixture.read_file("second.txt")?, "second");
    assert_eq!(revert_stack_len().await?, 1);

    // Redo twice: re-apply second deletion, both files deleted again
    fixture.redo(&session_id).await?;
    assert!(!fixture.worktree.path().join("first.txt").exists());
    assert!(!fixture.worktree.path().join("second.txt").exists());
    assert_eq!(revert_stack_len().await?, 0);

    // Third redo should fail and keep stack empty
    let third_redo = fixture.redo(&session_id).await;
    assert!(third_redo.is_err());
    assert!(
        third_redo
            .unwrap_err()
            .to_string()
            .contains("Nothing to redo")
    );
    assert_eq!(revert_stack_len().await?, 0);

    Ok(())
}

// ==================== Regression tests: nested directory undo ====================

/// Regression: undo must work when the agent changed files in subdirectories.
///
/// Previously, `diff_tree_to_tree` returned intermediate directory paths (e.g.
/// "crates", "crates/agent") in addition to file paths. Those directory paths were
/// stored in `TurnSnapshotPatch.changed_paths` and then passed to `restore_paths`,
/// which tried to write a raw git tree object to an on-disk directory, erroring out
/// before restoring any files.
#[tokio::test]
async fn test_undo_nested_directory_file_changes() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;

    // Create nested directory structure
    let nested = fixture.worktree.path().join("src/deep");
    std::fs::create_dir_all(&nested)?;
    std::fs::write(nested.join("file.rs"), "original")?;
    fixture.write_file("root.txt", "root-original")?;

    let session_id = fixture.create_session().await?;
    let user_msg_id = fixture
        .add_user_message(&session_id, "Modify nested files")
        .await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&session_id).await?;

    // Modify both a nested file and a root file (triggers directory tree path changes)
    std::fs::write(nested.join("file.rs"), "modified")?;
    fixture.write_file("root.txt", "root-modified")?;
    fixture
        .take_post_snapshot(&session_id, &turn_id, &pre_snapshot)
        .await?;

    // Verify both files are modified
    assert_eq!(std::fs::read_to_string(nested.join("file.rs"))?, "modified");
    assert_eq!(fixture.read_file("root.txt")?, "root-modified");

    // Undo – regression: previously silently failed (warn log only) because
    // directory paths like "src" caused restore_paths to abort with an fs error.
    let result = fixture.undo(&session_id, &user_msg_id).await?;

    // Both files must be reverted
    assert_eq!(
        std::fs::read_to_string(nested.join("file.rs"))?,
        "original",
        "nested file must be reverted by undo"
    );
    assert_eq!(
        fixture.read_file("root.txt")?,
        "root-original",
        "root file must be reverted by undo"
    );
    // reverted_files should contain the actual file paths (no directory entries)
    assert!(
        result.reverted_files.len() >= 2,
        "expected at least 2 reverted files, got: {:?}",
        result.reverted_files
    );
    assert!(
        result
            .reverted_files
            .iter()
            .all(|p| std::path::Path::new(p).extension().is_some() || p.contains('.')),
        "reverted_files should not contain bare directory paths, got: {:?}",
        result.reverted_files
    );

    Ok(())
}

// ==================== Test Suite: cleanup_revert_on_prompt event journal pruning ====================

/// Helper: append a PromptReceived event to the journal and return its stream_seq.
async fn append_prompt_event(
    journal: &dyn EventJournal,
    session_id: &str,
    message_id: &str,
) -> i64 {
    journal
        .append_durable(&NewDurableEvent {
            session_seq: None,
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::PromptReceived {
                slash_command: None,
                content: "test prompt".to_string(),
                message_id: Some(message_id.to_string()),
            },
        })
        .await
        .expect("append prompt event")
        .stream_seq
}

/// Helper: append a generic non-prompt event and return its stream_seq.
async fn append_generic_event(journal: &dyn EventJournal, session_id: &str) -> i64 {
    journal
        .append_durable(&NewDurableEvent {
            session_seq: None,
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind: AgentEventKind::Cancelled,
        })
        .await
        .expect("append generic event")
        .stream_seq
}

/// After undo + new prompt, cleanup_revert_on_prompt must delete event journal
/// entries that belong to the undone turn so they do not reappear on page reload.
#[tokio::test]
async fn test_cleanup_revert_prunes_event_journal() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    // Turn 1: prompt + some events
    let msg1_id = fixture
        .add_user_message(&session_id, "first prompt")
        .await?;
    let seq_prompt1 = append_prompt_event(&*journal, &session_id, &msg1_id).await;
    let _seq_after1 = append_generic_event(&*journal, &session_id).await;

    // Turn 2 (this will be undone): prompt + some events that should be pruned
    let msg2_id = fixture
        .add_user_message(&session_id, "second prompt")
        .await?;
    let seq_prompt2 = append_prompt_event(&*journal, &session_id, &msg2_id).await;
    let seq_after2a = append_generic_event(&*journal, &session_id).await;
    let seq_after2b = append_generic_event(&*journal, &session_id).await;

    // Verify all 5 events are present before undo
    let before = journal.load_session_stream(&session_id, None, None).await?;
    assert_eq!(before.len(), 5, "expected 5 events before undo");

    // Undo turn 2 — this pushes a revert state with msg2_id as the frontier
    fixture.undo(&session_id, &msg2_id).await?;

    // Now simulate a new prompt being sent: cleanup_revert_on_prompt should
    // delete messages AND prune the event journal after the frontier seq.
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*fixture.storage.event_journal(),
        None,
        &session_id,
        None,
    )
    .await
    .expect("cleanup should succeed");

    // After cleanup: the frontier prompt_received event AND everything after
    // it must be gone — the undone turn's prompt and its follow-up events are
    // all pruned so they don't reappear on page reload.
    let after = journal.load_session_stream(&session_id, None, None).await?;

    // Turn-2 events (frontier prompt + two generic events) must all be pruned.
    assert!(
        !after.iter().any(|e| e.stream_seq == seq_prompt2),
        "frontier (turn-2) prompt event must be pruned (seq {})",
        seq_prompt2
    );
    assert!(
        !after.iter().any(|e| e.stream_seq == seq_after2a),
        "first event after turn-2 prompt must be pruned (seq {})",
        seq_after2a
    );
    assert!(
        !after.iter().any(|e| e.stream_seq == seq_after2b),
        "second event after turn-2 prompt must be pruned (seq {})",
        seq_after2b
    );

    // Turn-1 events must all be retained.
    assert!(
        after.iter().any(|e| e.stream_seq == seq_prompt1),
        "turn-1 prompt event must be retained (seq {})",
        seq_prompt1
    );

    // Exactly 2 events remain: turn-1 prompt and turn-1 generic.
    assert_eq!(
        after.len(),
        2,
        "expected 2 events after pruning, got {}",
        after.len()
    );

    Ok(())
}

/// Helper: append an arbitrary event and return its stream_seq.
async fn append_kind(journal: &dyn EventJournal, session_id: &str, kind: AgentEventKind) -> i64 {
    journal
        .append_durable(&NewDurableEvent {
            session_seq: None,
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            source_node_id: None,
            source_seq: None,
            kind,
        })
        .await
        .expect("append event")
        .stream_seq
}

fn run_started(run_id: &str) -> AgentEventKind {
    AgentEventKind::RunStarted {
        run_id: run_id.to_string(),
        origin: "interactive".to_string(),
    }
}

fn run_completed(run_id: &str) -> AgentEventKind {
    AgentEventKind::RunCompleted {
        run_id: run_id.to_string(),
        outcome: "completed".to_string(),
        stop_type: None,
        reason: None,
    }
}

/// Regression: the new prompt's run has already journaled its `run_started`
/// when cleanup runs. Pruning the undone turn must not touch the new run's
/// events, and must not leave the undone run open (start without completion).
#[tokio::test]
async fn test_cleanup_revert_keeps_run_lifecycle_and_the_new_runs_events() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    // Turn 1 in run "run-1".
    let msg1_id = fixture.add_user_message(&session_id, "first").await?;
    append_kind(&*journal, &session_id, run_started("run-1")).await;
    append_prompt_event(&*journal, &session_id, &msg1_id).await;
    let seq_end1 = append_kind(&*journal, &session_id, run_completed("run-1")).await;

    // Turn 2 in run "run-2" is undone.
    let msg2_id = fixture.add_user_message(&session_id, "second").await?;
    let seq_start2 = append_kind(&*journal, &session_id, run_started("run-2")).await;
    let seq_prompt2 = append_prompt_event(&*journal, &session_id, &msg2_id).await;
    let seq_work2 = append_generic_event(&*journal, &session_id).await;
    let seq_end2 = append_kind(&*journal, &session_id, run_completed("run-2")).await;
    fixture.undo(&session_id, &msg2_id).await?;

    // A new prompt: its run is started (and has emitted more) before cleanup.
    let seq_start3 = append_kind(&*journal, &session_id, run_started("run-3")).await;
    let seq_early3 = append_generic_event(&*journal, &session_id).await;

    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session_id,
        Some("run-3"),
    )
    .await
    .expect("cleanup should succeed");

    let after = journal.load_session_stream(&session_id, None, None).await?;
    let seqs: Vec<i64> = after.iter().map(|e| e.stream_seq).collect();
    assert!(!seqs.contains(&seq_prompt2), "undone prompt is pruned");
    assert!(!seqs.contains(&seq_work2), "undone work is pruned");
    for (seq, what) in [
        (seq_end1, "run-1 completion"),
        (seq_start2, "run-2 start"),
        (seq_end2, "run-2 completion"),
        (seq_start3, "the new run's start"),
        (seq_early3, "events the new run already wrote"),
    ] {
        assert!(seqs.contains(&seq), "{what} must survive (seq {seq})");
    }

    // Every started run other than the live one has been completed.
    let journal_ref = &*journal;
    assert_eq!(
        journal_ref.last_unfinished_run(&session_id).await?,
        Some("run-3".to_string()),
        "only the live run is open"
    );
    Ok(())
}

/// Without a journaled start for the new run there is no upper bound, but the
/// run lifecycle of the undone turn is still preserved.
#[tokio::test]
async fn test_cleanup_revert_without_current_run_start_keeps_lifecycle() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    let msg_id = fixture.add_user_message(&session_id, "only").await?;
    let seq_start = append_kind(&*journal, &session_id, run_started("run-1")).await;
    let seq_prompt = append_prompt_event(&*journal, &session_id, &msg_id).await;
    let seq_end = append_kind(&*journal, &session_id, run_completed("run-1")).await;
    fixture.undo(&session_id, &msg_id).await?;

    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session_id,
        Some("run-not-journaled"),
    )
    .await
    .expect("cleanup should succeed");

    let seqs: Vec<i64> = journal
        .load_session_stream(&session_id, None, None)
        .await?
        .iter()
        .map(|e| e.stream_seq)
        .collect();
    assert!(!seqs.contains(&seq_prompt));
    assert!(seqs.contains(&seq_start) && seqs.contains(&seq_end));
    assert_eq!(journal.last_unfinished_run(&session_id).await?, None);
    Ok(())
}

/// Journal that holds a session stream's writes back until a range delete
/// starts (or a short grace period passes), then persists them right before
/// the delete: the writer catching up between cleanup's journal read and its
/// prune, made deterministic.
struct GatedWriterJournal {
    inner: Arc<dyn EventJournal>,
    open: tokio::sync::watch::Sender<bool>,
    stream:
        std::sync::OnceLock<Arc<querymt_agent_services::session_event_stream::SessionEventStream>>,
}

impl GatedWriterJournal {
    /// Grace period after which held writes proceed anyway, so an explicit
    /// flush by the code under test completes instead of deadlocking.
    const GRACE: std::time::Duration = std::time::Duration::from_millis(200);
}

#[async_trait::async_trait]
impl EventJournal for GatedWriterJournal {
    async fn append_durable(
        &self,
        event: &NewDurableEvent,
    ) -> querymt_agent_core::session::error::SessionResult<querymt_agent_core::events::DurableEvent>
    {
        let mut open = self.open.subscribe();
        let _ = tokio::time::timeout(Self::GRACE, open.wait_for(|open| *open)).await;
        self.inner.append_durable(event).await
    }
    async fn max_session_seq(&self, session_id: &str) -> i64 {
        self.inner.max_session_seq(session_id).await
    }
    async fn load_session_stream(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> querymt_agent_core::session::error::SessionResult<
        Vec<querymt_agent_core::events::DurableEvent>,
    > {
        self.inner
            .load_session_stream(session_id, after_seq, limit)
            .await
    }
    async fn load_global_stream(
        &self,
        after_seq: Option<i64>,
        limit: Option<usize>,
    ) -> querymt_agent_core::session::error::SessionResult<
        Vec<querymt_agent_core::events::DurableEvent>,
    > {
        self.inner.load_global_stream(after_seq, limit).await
    }
    async fn delete_session_events_range(
        &self,
        session_id: &str,
        from_seq: i64,
        before_seq: Option<i64>,
    ) -> querymt_agent_core::session::error::SessionResult<usize> {
        self.open.send_replace(true);
        if let Some(stream) = self.stream.get() {
            stream.flush().await;
        }
        self.inner
            .delete_session_events_range(session_id, from_seq, before_seq)
            .await
    }
    async fn append_durable_from_source(
        &self,
        event: &NewDurableEvent,
    ) -> querymt_agent_core::session::error::SessionResult<
        Option<querymt_agent_core::events::DurableEvent>,
    > {
        self.inner.append_durable_from_source(event).await
    }
    async fn latest_source_seq(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> querymt_agent_core::session::error::SessionResult<Option<i64>> {
        self.inner
            .latest_source_seq(session_id, source_node_id)
            .await
    }
    async fn remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> querymt_agent_core::session::error::SessionResult<Option<i64>> {
        self.inner
            .remote_sync_cursor(session_id, source_node_id)
            .await
    }
    async fn advance_remote_sync_cursor(
        &self,
        session_id: &str,
        source_node_id: &str,
        source_seq: i64,
        complete: bool,
    ) -> querymt_agent_core::session::error::SessionResult<()> {
        self.inner
            .advance_remote_sync_cursor(session_id, source_node_id, source_seq, complete)
            .await
    }
    async fn load_remote_session_stream(
        &self,
        session_id: &str,
        source_node_id: &str,
    ) -> querymt_agent_core::session::error::SessionResult<
        Vec<querymt_agent_core::events::DurableEvent>,
    > {
        self.inner
            .load_remote_session_stream(session_id, source_node_id)
            .await
    }
    async fn max_stream_seq(
        &self,
        session_id: &str,
    ) -> querymt_agent_core::session::error::SessionResult<i64> {
        self.inner.max_stream_seq(session_id).await
    }
}

/// Regression: the new run's `run_started` is only queued on the session
/// stream (not yet persisted) when cleanup runs. Cleanup must flush the stream
/// so the prune is bounded by the new run's start and keeps its events, even
/// when the writer only persists them after cleanup read the journal.
#[tokio::test]
async fn test_cleanup_revert_flushes_queued_run_start_before_pruning() -> Result<()> {
    use querymt_agent_services::event_fanout::EventFanout;
    use querymt_agent_services::session_event_stream::SessionEventStream;

    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    let msg_id = fixture.add_user_message(&session_id, "only").await?;
    append_kind(&*journal, &session_id, run_started("run-1")).await;
    let seq_prompt = append_prompt_event(&*journal, &session_id, &msg_id).await;
    append_kind(&*journal, &session_id, run_completed("run-1")).await;
    fixture.undo(&session_id, &msg_id).await?;

    // The new run's events are queued, not awaited, and the writer lags.
    let gated = Arc::new(GatedWriterJournal {
        inner: journal.clone(),
        open: tokio::sync::watch::channel(false).0,
        stream: std::sync::OnceLock::new(),
    });
    let stream = SessionEventStream::new(&session_id, gated.clone(), Arc::new(EventFanout::new()));
    let _ = gated.stream.set(stream.clone());
    stream.emit(run_started("run-2"));
    stream.emit(AgentEventKind::QueuedInputStarted {
        input_id: "input-2".to_string(),
        run_id: "run-2".to_string(),
    });

    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*gated,
        Some(&stream),
        &session_id,
        Some("run-2"),
    )
    .await
    .expect("cleanup should succeed");
    stream.flush().await;

    let after = journal.load_session_stream(&session_id, None, None).await?;
    assert!(
        !after.iter().any(|e| e.stream_seq == seq_prompt),
        "undone prompt is pruned"
    );
    assert!(
        after.iter().any(|e| matches!(
            &e.kind,
            AgentEventKind::QueuedInputStarted { run_id, .. } if run_id == "run-2"
        )),
        "the new run's events must survive"
    );
    Ok(())
}

/// End to end through the session actor: prompt, prompt, undo the second,
/// prompt again. The third run journals its `run_started` before the actor
/// runs the undo cleanup, and the cleanup must neither drop it nor leave the
/// undone run open.
#[tokio::test]
async fn undo_then_new_prompt_keeps_every_runs_lifecycle_in_the_journal() -> Result<()> {
    use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider};

    let mut mock = MockLlmProvider::new();
    mock.expect_chat_with_tools()
        .returning(|_, _| Ok(MockChatResponse::text_only("done").into()));
    mock.expect_chat()
        .returning(|_| Ok(MockChatResponse::text_only("done").into()));
    let fixture =
        UndoTestFixture::with_provider_and_middleware(SharedLlmProvider::new(mock, vec![]), vec![])
            .await?;
    let session = fixture.create_session().await?;
    let store = fixture.storage.session_store();
    let llm_config = store
        .create_or_get_llm_config(
            &querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .await?;
    store
        .set_session_llm_config(&session, llm_config.id)
        .await?;

    fixture.prompt_to_completion(&session, "first").await?;
    fixture.prompt_to_completion(&session, "second").await?;
    let second_id = store
        .get_history(&session)
        .await?
        .into_iter()
        .filter(|m| m.role == querymt::chat::ChatRole::User)
        .nth(1)
        .map(|m| m.id)
        .expect("the second user message is stored");
    fixture.undo(&session, &second_id).await?;
    fixture.prompt_to_completion(&session, "third").await?;

    let events = fixture
        .storage
        .event_journal()
        .load_session_stream(&session, None, None)
        .await?;
    let started: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::RunStarted { run_id, .. } => Some(run_id.as_str()),
            _ => None,
        })
        .collect();
    let completed: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::RunCompleted { run_id, .. } => Some(run_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 3, "no run_started was pruned: {started:?}");
    for run in &started {
        assert!(
            completed.contains(run),
            "run {run} has a start but no completion"
        );
    }
    assert_eq!(
        fixture
            .storage
            .event_journal()
            .last_unfinished_run(&session)
            .await?,
        None,
        "no run is left open"
    );

    // The undone prompt is gone from the journal; the others remain.
    let prompts: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            AgentEventKind::PromptReceived { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, vec!["first", "third"]);
    Ok(())
}

/// The owner-side history epoch changes only when undo cleanup deletes journal
/// events; appends leave it alone. Remote peers use it to detect rewrites.
#[tokio::test]
async fn test_undo_cleanup_bumps_history_epoch_and_appends_do_not() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    let msg1_id = fixture.add_user_message(&session_id, "first").await?;
    append_prompt_event(&*journal, &session_id, &msg1_id).await;
    let msg2_id = fixture.add_user_message(&session_id, "second").await?;
    append_prompt_event(&*journal, &session_id, &msg2_id).await;
    append_generic_event(&*journal, &session_id).await;

    assert_eq!(journal.history_epoch(&session_id).await?, 0);
    append_generic_event(&*journal, &session_id).await;
    assert_eq!(
        journal.history_epoch(&session_id).await?,
        0,
        "appends must not change the epoch"
    );

    fixture.undo(&session_id, &msg2_id).await?;
    assert_eq!(
        journal.history_epoch(&session_id).await?,
        0,
        "undo alone keeps events until the next prompt"
    );
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*fixture.storage.event_journal(),
        None,
        &session_id,
        None,
    )
    .await
    .expect("cleanup should succeed");
    assert_eq!(journal.history_epoch(&session_id).await?, 1);

    append_generic_event(&*journal, &session_id).await;
    assert_eq!(journal.history_epoch(&session_id).await?, 1);

    // Another session is unaffected, and a delete that removes nothing is a no-op.
    assert_eq!(journal.history_epoch("other-session").await?, 0);
    journal
        .delete_session_events_from(&session_id, i64::MAX)
        .await?;
    assert_eq!(journal.history_epoch(&session_id).await?, 1);
    Ok(())
}

/// Rewinding past a turn that incurred charges prunes the conversation but
/// keeps the accounting ledger, so session spend is unchanged and journal
/// numbers are not reused or shifted.
#[tokio::test]
async fn test_cleanup_revert_keeps_accounting_for_undone_turn() -> Result<()> {
    use querymt_agent_core::accounting::samples::{sample_settlement, sample_start};
    use querymt_agent_services::accounting::{AttemptRecorder, SessionAccounting};
    use querymt_agent_services::event_fanout::EventFanout;
    use querymt_agent_services::session_event_stream::SessionEventStream;

    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    let accounting = SessionAccounting::hydrate(&session_id, journal.clone()).await?;
    let stream =
        SessionEventStream::new(&session_id, journal.clone(), Arc::new(EventFanout::new()));
    let recorder = AttemptRecorder::with_projection(stream.clone(), accounting.clone());

    // Turn 1, then turn 2 which is generated (and charged) and then undone.
    let msg1 = fixture.add_user_message(&session_id, "first").await?;
    stream
        .emit_persisted(AgentEventKind::PromptReceived {
            slash_command: None,
            content: "first".into(),
            message_id: Some(msg1),
        })
        .await?;
    let msg2 = fixture.add_user_message(&session_id, "second").await?;
    let prompt2 = stream
        .emit_persisted(AgentEventKind::PromptReceived {
            slash_command: None,
            content: "second".into(),
            message_id: Some(msg2.clone()),
        })
        .await?;
    let mut start = sample_start();
    start.attempt.session_id = session_id.clone();
    let handle = recorder
        .begin(start)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut settled = sample_settlement();
    settled.attempt.session_id = session_id.clone();
    recorder
        .settle(handle, settled)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let charged = accounting.read(|r| r.session().clone());
    assert!(charged.complete_total_usd().is_some());

    fixture.undo(&session_id, &msg2).await?;
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session_id,
        None,
    )
    .await
    .expect("cleanup should succeed");

    let after = journal.load_session_stream(&session_id, None, None).await?;
    assert!(
        !after.iter().any(|e| e.session_seq == prompt2.session_seq),
        "the undone turn's prompt is pruned"
    );
    let ledger: Vec<_> = after
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                AgentEventKind::LlmAttemptStarted { .. } | AgentEventKind::LlmAttemptSettled { .. }
            )
        })
        .collect();
    assert_eq!(ledger.len(), 2, "both ledger records survive the rewind");
    assert!(ledger.iter().all(|e| e.session_seq > prompt2.session_seq));

    // Reopening reproduces the same incurred totals from the journal alone.
    let reopened = SessionAccounting::hydrate(&session_id, journal.clone()).await?;
    assert_eq!(reopened.read(|r| r.session().clone()), charged);
    Ok(())
}

// ==================== Test Suite: cleanup must remove undone messages before history reload ====================

/// Regression test: after undo + cleanup_revert_on_prompt the message history
/// returned by `get_history` must NOT contain the undone turn's messages.
///
/// This is the exact bug that caused the LLM to still "see" undone user
/// messages — `cleanup_revert_on_prompt` was called AFTER the runtime context
/// had already loaded history, so the deleted messages were still in memory.
/// Additionally, `delete_messages_after` used `created_at > target` which
/// kept the frontier message itself (the undone user prompt) in the history.
///
/// The fix:
///   1. cleanup must run BEFORE `get_history` / `load_working_context`
///   2. `delete_messages_after` must delete the frontier message AND everything
///      after it (>= on internal id, not > on created_at)
#[tokio::test]
async fn test_cleanup_revert_removes_undone_messages_from_history() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let session_id = fixture.create_session().await?;
    let journal = fixture.storage.event_journal();

    // Turn 1: user message "hello"
    let msg1_id = fixture
        .add_user_message_at(&session_id, "hello", 1000)
        .await?;
    append_prompt_event(&*journal, &session_id, &msg1_id).await;
    // Simulate assistant response for turn 1
    fixture
        .add_assistant_message_at(&session_id, "Hi there!", 1001)
        .await?;
    append_generic_event(&*journal, &session_id).await;

    // Turn 2: user message "hallo" (will be undone)
    let msg2_id = fixture
        .add_user_message_at(&session_id, "hallo", 1002)
        .await?;
    append_prompt_event(&*journal, &session_id, &msg2_id).await;
    // Simulate assistant response for turn 2
    fixture
        .add_assistant_message_at(&session_id, "Hello again!", 1003)
        .await?;
    append_generic_event(&*journal, &session_id).await;

    // Verify both turns are in history before undo
    let history_before = fixture
        .storage
        .session_store()
        .get_history(&session_id)
        .await?;
    assert_eq!(
        history_before.len(),
        4,
        "expected 4 messages (2 user + 2 assistant) before undo"
    );

    // Undo turn 2
    fixture.undo(&session_id, &msg2_id).await?;

    // Simulate what execute_prompt_detached should do:
    // cleanup MUST run BEFORE loading history
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session_id,
        None,
    )
    .await
    .expect("cleanup should succeed");

    // Now get_history — the undone turn's messages must be gone
    let history_after = fixture
        .storage
        .session_store()
        .get_history(&session_id)
        .await?;

    // Only turn 1 messages should remain (user "hello" + assistant "Hi there!").
    // The frontier message (msg2_id = "hallo") AND its assistant response must
    // both be deleted — the user undid that turn, so neither the prompt nor the
    // response should appear in the LLM context on the next turn.
    assert_eq!(
        history_after.len(),
        2,
        "expected 2 messages after cleanup (turn 1 only), got {} — undone messages leaked into history",
        history_after.len()
    );

    // Verify the surviving messages are from turn 1
    let user_msgs: Vec<&str> = history_after
        .iter()
        .filter_map(|m| {
            m.parts.iter().find_map(|p| {
                if let querymt_agent_core::model::MessagePart::Text { content } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
        })
        .collect();
    assert!(
        user_msgs.contains(&"hello"),
        "turn-1 user message 'hello' must survive"
    );
    assert!(
        user_msgs.contains(&"Hi there!"),
        "turn-1 assistant message must survive"
    );
    assert!(
        !user_msgs.contains(&"hallo"),
        "undone user message 'hallo' must NOT survive"
    );
    assert!(
        !user_msgs.contains(&"Hello again!"),
        "undone assistant message must NOT survive"
    );

    Ok(())
}

// ==================== Test Suite: Inherited fork snapshots ====================
//
// Messages copied into a fork from a parent operating in a different
// workspace are context-only: their snapshot ids belong to the parent's
// snapshot repository and must never restore into the fork's checkout.
// Turns made inside the fork itself stay fully undoable.

#[tokio::test]
async fn test_fork_undo_treats_inherited_foreign_snapshots_as_context_only() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let fork_workspace = TempDir::new()?;

    fixture.write_file("a.txt", "a-original")?;

    let parent_id = fixture.create_session().await?;
    fixture.add_user_message(&parent_id, "Turn one").await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&parent_id).await?;
    fixture.write_file("a.txt", "a-turn-one")?;
    fixture
        .take_post_snapshot(&parent_id, &turn_id, &pre_snapshot)
        .await?;

    // Fork at the parent's last message so the copied prefix contains the
    // snapshot parts, then bind the fork to its own workspace the way
    // isolated forks do.
    let parent_history = fixture
        .storage
        .session_store()
        .get_history(&parent_id)
        .await?;
    let fork_target = parent_history.last().map(|m| m.id.clone()).unwrap();
    let child_id = fixture
        .storage
        .session_store()
        .fork_session(&parent_id, &fork_target, ForkOrigin::User)
        .await?;

    {
        let conn = fixture.storage.conn();
        let conn = conn.lock();
        conn.execute(
            "UPDATE sessions SET cwd = ? WHERE public_id = ?",
            rusqlite::params![
                fork_workspace.path().to_string_lossy().to_string(),
                child_id
            ],
        )?;
    }
    fixture
        .register_session_actor_with_cwd(&child_id, fork_workspace.path())
        .await;

    // Undo of the inherited parent turn inside the fork: context-only. No
    // files change anywhere and no error is reported.
    let child_history = fixture
        .storage
        .session_store()
        .get_history(&child_id)
        .await?;
    let inherited_target = child_history
        .iter()
        .find(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::TurnSnapshotPatch { .. }))
        })
        .map(|m| m.id.clone())
        .expect("fork copied the parent's patch message");

    let err = fixture
        .undo(&child_id, &inherited_target)
        .await
        .expect_err("a directly selected foreign snapshot must be rejected");
    assert!(
        err.to_string().contains("workspace") || format!("{err:?}").contains("WorkspaceMismatch"),
        "expected an explicit provenance error, got: {err:?}"
    );
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&child_id)
            .await?
            .is_none(),
        "a rejected undo must not record a redo frame"
    );
    assert_eq!(fixture.read_file("a.txt")?, "a-turn-one");
    assert_eq!(
        fixture.read_file_in(fork_workspace.path(), "a.txt").ok(),
        None,
        "nothing was restored into the fork workspace"
    );

    // A new turn made inside the fork remains fully undoable.
    fixture.write_file_in(fork_workspace.path(), "b.txt", "b-original")?;
    let child_user_msg = fixture.add_user_message(&child_id, "Child turn").await?;
    let (child_turn, child_pre) = fixture
        .take_pre_snapshot_with_cwd(&child_id, fork_workspace.path())
        .await?;
    fixture.write_file_in(fork_workspace.path(), "b.txt", "b-child")?;
    fixture
        .take_post_snapshot_with_cwd(&child_id, &child_turn, &child_pre, fork_workspace.path())
        .await?;

    let child_undo = fixture.undo(&child_id, &child_user_msg).await?;
    assert_eq!(child_undo.reverted_files, vec!["b.txt".to_string()]);
    assert_eq!(
        fixture.read_file_in(fork_workspace.path(), "b.txt")?,
        "b-original"
    );

    Ok(())
}

#[tokio::test]
async fn test_fork_undo_from_subdirectory_cwd_is_context_only() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let sub = fixture.worktree.path().join("sub");
    std::fs::create_dir_all(&sub)?;
    let fork_workspace = TempDir::new()?;

    // Parent session bound to a repository subdirectory.
    let parent_id = fixture.create_session_with_cwd(&sub).await?;
    fixture.write_file_in(&sub, "nested.txt", "original")?;
    fixture.add_user_message(&parent_id, "Edit nested").await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot_with_cwd(&parent_id, &sub).await?;
    fixture.write_file_in(&sub, "nested.txt", "edited")?;
    fixture
        .take_post_snapshot_with_cwd(&parent_id, &turn_id, &pre_snapshot, &sub)
        .await?;

    let parent_history = fixture
        .storage
        .session_store()
        .get_history(&parent_id)
        .await?;
    let fork_target = parent_history.last().map(|m| m.id.clone()).unwrap();
    let child_id = fixture
        .storage
        .session_store()
        .fork_session(&parent_id, &fork_target, ForkOrigin::User)
        .await?;

    {
        let conn = fixture.storage.conn();
        let conn = conn.lock();
        conn.execute(
            "UPDATE sessions SET cwd = ? WHERE public_id = ?",
            rusqlite::params![
                fork_workspace.path().to_string_lossy().to_string(),
                child_id
            ],
        )?;
    }
    fixture
        .register_session_actor_with_cwd(&child_id, fork_workspace.path())
        .await;

    let child_history = fixture
        .storage
        .session_store()
        .get_history(&child_id)
        .await?;
    let inherited_target = child_history
        .iter()
        .find(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::TurnSnapshotPatch { .. }))
        })
        .map(|m| m.id.clone())
        .expect("fork copied the parent's patch message");

    // The subdirectory cwd differs from the fork workspace root, so the
    // inherited snapshot is foreign here too and is rejected, unrestored.
    fixture
        .undo(&child_id, &inherited_target)
        .await
        .expect_err("a directly selected foreign snapshot must be rejected");
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&child_id)
            .await?
            .is_none(),
        "a rejected undo must not record a redo frame"
    );
    assert_eq!(fixture.read_file_in(&sub, "nested.txt")?, "edited");

    Ok(())
}

#[tokio::test]
async fn test_legacy_local_fork_undo_still_restores_inherited_history() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "a-original")?;

    let parent_id = fixture.create_session().await?;
    fixture.add_user_message(&parent_id, "Turn one").await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&parent_id).await?;
    fixture.write_file("a.txt", "a-turn-one")?;
    fixture
        .take_post_snapshot(&parent_id, &turn_id, &pre_snapshot)
        .await?;

    let parent_history = fixture
        .storage
        .session_store()
        .get_history(&parent_id)
        .await?;
    let fork_target = parent_history.last().map(|m| m.id.clone()).unwrap();

    // Legacy local fork: the child inherits the parent's cwd, so undo keeps
    // its existing behavior in the shared checkout.
    let child_id = fixture
        .storage
        .session_store()
        .fork_session(&parent_id, &fork_target, ForkOrigin::User)
        .await?;
    fixture
        .register_session_actor_with_cwd(&child_id, fixture.worktree.path())
        .await;

    let child_history = fixture
        .storage
        .session_store()
        .get_history(&child_id)
        .await?;
    // Undo the first copied user message: the patch scan aggregates the
    // following copied start/patch pair and restores the pre-turn snapshot
    // exactly as a legacy same-checkout fork always did.
    let inherited_target = child_history.first().map(|m| m.id.clone()).unwrap();

    let result = fixture.undo(&child_id, &inherited_target).await?;
    assert_eq!(result.reverted_files, vec!["a.txt".to_string()]);
    assert_eq!(fixture.read_file("a.txt")?, "a-original");

    Ok(())
}

#[tokio::test]
async fn test_undo_without_snapshot_backend_reports_unavailable_backend() -> Result<()> {
    use crate::agent::LocalAgentHandle;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::{McpToolState, SessionRuntime};
    use crate::agent::session_actor::SessionActor;
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use std::sync::Arc;

    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "a-original")?;

    let parent_id = fixture.create_session().await?;
    fixture.add_user_message(&parent_id, "Turn one").await?;
    let (turn_id, pre_snapshot) = fixture.take_pre_snapshot(&parent_id).await?;
    fixture.write_file("a.txt", "a-turn-one")?;
    fixture
        .take_post_snapshot(&parent_id, &turn_id, &pre_snapshot)
        .await?;

    let parent_history = fixture
        .storage
        .session_store()
        .get_history(&parent_id)
        .await?;
    let fork_target = parent_history.last().map(|m| m.id.clone()).unwrap();

    // Same-workspace legacy fork with snapshots disabled: undo must keep
    // reporting the existing unavailable-backend behavior instead of
    // substituting any inherited Git snapshot data.
    let child_id = fixture
        .storage
        .session_store()
        .fork_session(&parent_id, &fork_target, ForkOrigin::User)
        .await?;

    let (registry, _registry_dir) = crate::test_utils::empty_plugin_registry()?;
    let llm = LLMParams::new().provider("mock").model("mock");
    let config =
        Arc::new(AgentConfigBuilder::new(Arc::new(registry), fixture.storage.clone(), llm).build());
    let handle = LocalAgentHandle::from_config(config);

    let runtime = SessionRuntime::new(
        Some(fixture.worktree.path().to_path_buf()),
        std::collections::HashMap::new(),
        McpToolState::empty(),
    );
    let actor = SessionActor::new(handle.config.clone(), child_id.clone(), runtime);
    let actor_ref = SessionActor::spawn(actor);
    handle
        .registry
        .lock()
        .await
        .insert(child_id.clone(), actor_ref);

    let child_history = fixture
        .storage
        .session_store()
        .get_history(&child_id)
        .await?;
    let inherited_target = child_history
        .iter()
        .find(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::TurnSnapshotPatch { .. }))
        })
        .map(|m| m.id.clone())
        .expect("fork copied the parent's patch message");

    let err = handle
        .undo(&child_id, &inherited_target)
        .await
        .expect_err("undo must fail without a snapshot backend");
    // The no-backend guard is enforced at the session actor: the error may
    // surface directly or wrapped by the actor transport, but undo must keep
    // reporting the unavailable backend instead of substituting inherited
    // Git snapshot data.
    assert!(
        matches!(err, UndoError::NoSnapshotBackend)
            || matches!(err, UndoError::ActorSend(ref m) if m.contains("No snapshot backend")),
        "unexpected undo error without snapshot backend: {err:?}"
    );

    Ok(())
}

// ==================== workspace-snapshots scenarios ====================

use crate::test_utils::noise;

const LARGE: usize = 17 * 1024 * 1024;

/// One recorded turn: pre snapshot, `mutate`, post snapshot (with its patch).
async fn run_turn(
    fixture: &UndoTestFixture,
    session: &str,
    mutate: impl FnOnce(&std::path::Path) -> Result<()>,
) -> Result<()> {
    let (turn, pre) = fixture.take_pre_snapshot(session).await?;
    mutate(fixture.worktree.path())?;
    fixture.take_post_snapshot(session, &turn, &pre).await?;
    Ok(())
}

#[tokio::test]
async fn large_file_is_captured_and_undone() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let path = fixture.worktree.path().join("big.bin");
    let original = noise(LARGE, 1);
    std::fs::write(&path, &original)?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;

    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("big.bin"), noise(LARGE, 2))?;
        Ok(())
    })
    .await?;
    assert!(std::fs::read(&path)? != original);

    fixture.undo(&session, &target).await?;

    assert!(
        std::fs::read(&path)? == original,
        "the 17 MiB file must have its exact pre-turn content"
    );
    Ok(())
}

#[tokio::test]
async fn undo_and_redo_of_a_file_that_grew_restore_both_versions() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    let path = fixture.worktree.path().join("grow.bin");
    let small = noise(1024, 3);
    let large = noise(LARGE, 4);
    std::fs::write(&path, &small)?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "grow").await?;

    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("grow.bin"), noise(LARGE, 4))?;
        Ok(())
    })
    .await?;

    fixture.undo(&session, &target).await?;
    assert!(std::fs::read(&path)? == small, "1 KiB content after undo");

    let redo = fixture.redo(&session).await?;
    assert!(redo.restored);
    assert!(
        std::fs::read(&path)? == large,
        "exact 17 MiB content after redo"
    );
    Ok(())
}

#[tokio::test]
async fn undo_leaves_ignored_files_alone() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file(".gitignore", "ignored.log\n")?;
    fixture.write_file("kept.txt", "kept-original")?;
    fixture.write_file("ignored.log", "ignored-original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;

    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("kept.txt"), "kept-modified")?;
        std::fs::write(w.join("ignored.log"), "ignored-modified")?;
        Ok(())
    })
    .await?;

    fixture.undo(&session, &target).await?;

    assert_eq!(fixture.read_file("kept.txt")?, "kept-original");
    assert_eq!(fixture.read_file("ignored.log")?, "ignored-modified");
    Ok(())
}

#[tokio::test]
async fn undo_refuses_to_overwrite_a_file_ignored_since_the_turn() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("foo.txt", "original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;

    // The turn edits foo.txt and then ignores it: the post snapshot omits it,
    // and the pre-undo snapshot taken later cannot hold the edit either.
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("foo.txt"), "edited")?;
        std::fs::write(w.join(".gitignore"), "foo.txt\n")?;
        Ok(())
    })
    .await?;

    let error = fixture.undo(&session, &target).await.unwrap_err();

    assert!(error.to_string().contains("foo.txt"), "{error}");
    assert_eq!(
        fixture.read_file("foo.txt")?,
        "edited",
        "a rejected undo must not touch the file"
    );
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&session)
            .await?
            .is_none(),
        "a rejected undo leaves no frame behind"
    );
    Ok(())
}

#[tokio::test]
async fn failed_undo_keeps_history_but_its_frame_is_discarded_by_the_next_prompt() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "a-original")?;
    fixture.write_file("b.txt", "b-original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("a.txt"), "a-modified")?;
        std::fs::write(w.join("b.txt"), "b-modified")?;
        Ok(())
    })
    .await?;
    block_with_directory(&fixture, "b.txt")?;
    fixture.undo(&session, &target).await.unwrap_err();

    let journal = fixture.storage.event_journal();
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session,
        None,
    )
    .await
    .expect("cleanup should succeed");

    let history = fixture
        .storage
        .session_store()
        .get_history(&session)
        .await?;
    assert!(
        history.iter().any(|m| m.id == target),
        "a failed undo must not delete the message history"
    );
    assert!(
        fixture
            .storage
            .session_store()
            .list_revert_states(&session)
            .await?
            .is_empty(),
        "a new prompt discards the recovery frame so redo cannot overwrite its work"
    );
    std::fs::remove_dir_all(fixture.worktree.path().join("b.txt"))?;
    assert!(matches!(
        fixture.redo(&session).await,
        Err(crate::agent::undo::UndoError::NothingToRedo)
    ));
    Ok(())
}

#[tokio::test]
async fn failed_undo_then_successful_retry_leaves_no_stale_frame_after_the_next_prompt()
-> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "a-original")?;
    fixture.write_file("b.txt", "b-original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("a.txt"), "a-modified")?;
        std::fs::write(w.join("b.txt"), "b-modified")?;
        Ok(())
    })
    .await?;
    block_with_directory(&fixture, "b.txt")?;
    fixture.undo(&session, &target).await.unwrap_err();

    // The user fixes the cause and retries the undo, which now succeeds.
    std::fs::remove_dir_all(fixture.worktree.path().join("b.txt"))?;
    fixture.undo(&session, &target).await?;

    let journal = fixture.storage.event_journal();
    crate::agent::undo::cleanup_revert_on_prompt(
        &fixture.handle.config.provider,
        &*journal,
        None,
        &session,
        None,
    )
    .await
    .expect("cleanup should succeed");

    let store = fixture.storage.session_store();
    assert!(
        store.list_revert_states(&session).await?.is_empty(),
        "no stale failed frame may stay on the stack"
    );
    assert!(
        !store
            .get_history(&session)
            .await?
            .iter()
            .any(|m| m.id == target),
        "the completed undo still deletes its history"
    );
    assert!(matches!(
        fixture.redo(&session).await,
        Err(crate::agent::undo::UndoError::NothingToRedo)
    ));
    assert_eq!(fixture.read_file("b.txt")?, "b-original");
    Ok(())
}

#[tokio::test]
async fn undo_of_a_moved_file_restores_the_original_path() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "moved content")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "move").await?;

    run_turn(&fixture, &session, |w| {
        std::fs::rename(w.join("a.txt"), w.join("b.txt"))?;
        Ok(())
    })
    .await?;

    fixture.undo(&session, &target).await?;

    assert_eq!(fixture.read_file("a.txt")?, "moved content");
    assert!(!fixture.worktree.path().join("b.txt").exists());
    Ok(())
}

#[tokio::test]
async fn redo_of_a_moved_file_moves_it_back() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "moved content")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "move").await?;

    run_turn(&fixture, &session, |w| {
        std::fs::rename(w.join("a.txt"), w.join("b.txt"))?;
        Ok(())
    })
    .await?;
    fixture.undo(&session, &target).await?;

    let redo = fixture.redo(&session).await?;

    assert!(redo.restored);
    assert_eq!(fixture.read_file("b.txt")?, "moved content");
    assert!(!fixture.worktree.path().join("a.txt").exists());
    Ok(())
}

/// Replace file `name` with a directory that has content, so a restore of
/// `name` cannot rename onto it.
fn block_with_directory(fixture: &UndoTestFixture, name: &str) -> Result<()> {
    let path = fixture.worktree.path().join(name);
    std::fs::remove_file(&path)?;
    std::fs::create_dir(&path)?;
    std::fs::write(path.join("inner"), "x")?;
    Ok(())
}

#[tokio::test]
async fn undo_that_fails_while_restoring_can_be_redone() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("a.txt", "a-original")?;
    fixture.write_file("b.txt", "b-original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("a.txt"), "a-modified")?;
        std::fs::write(w.join("b.txt"), "b-modified")?;
        Ok(())
    })
    .await?;
    block_with_directory(&fixture, "b.txt")?;

    let error = fixture.undo(&session, &target).await.unwrap_err();

    assert!(error.to_string().contains("b.txt"), "{error}");
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&session)
            .await?
            .is_some(),
        "a failed undo still records a redo frame"
    );
    std::fs::remove_dir_all(fixture.worktree.path().join("b.txt"))?;
    let redo = fixture.redo(&session).await?;
    assert!(redo.restored);
    assert_eq!(
        fixture.read_file("a.txt")?,
        "a-modified",
        "redo returns a.txt to its content before the undo"
    );
    Ok(())
}

#[tokio::test]
async fn redo_that_fails_keeps_its_frame() -> Result<()> {
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("test.txt", "original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("test.txt"), "modified")?;
        Ok(())
    })
    .await?;
    fixture.undo(&session, &target).await?;
    block_with_directory(&fixture, "test.txt")?;

    let error = fixture.redo(&session).await.unwrap_err();

    assert!(error.to_string().contains("test.txt"), "{error}");
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&session)
            .await?
            .is_some(),
        "a failed redo keeps its frame"
    );
    std::fs::remove_dir_all(fixture.worktree.path().join("test.txt"))?;
    let redo = fixture.redo(&session).await?;
    assert!(redo.restored);
    assert_eq!(fixture.read_file("test.txt")?, "modified");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_delete_fails_the_restore() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Permission checks do not apply to root, so the failure cannot be
    // provoked there. Say so instead of passing silently.
    // SAFETY: `geteuid` has no preconditions, takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED failed_delete_fails_the_restore: running as root");
        return Ok(());
    }
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("keep.txt", "keep")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "create").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::create_dir(w.join("sub"))?;
        std::fs::write(w.join("sub/new.txt"), "new")?;
        Ok(())
    })
    .await?;
    let sub = fixture.worktree.path().join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555))?;

    let result = fixture.undo(&session, &target).await;
    // Always restore permissions so the temp dir can be removed.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755))?;

    let error = result.unwrap_err();
    assert!(error.to_string().contains("sub/new.txt"), "{error}");
    assert!(sub.join("new.txt").exists());
    Ok(())
}

fn gc_keep(n: usize) -> querymt_agent_services::snapshot::GcConfig {
    querymt_agent_services::snapshot::GcConfig {
        max_snapshots: std::num::NonZeroUsize::new(n),
        max_age_days: None,
    }
}

/// `count` extra changed snapshots, so earlier ones can be pruned.
async fn churn(fixture: &UndoTestFixture, count: usize) -> Result<()> {
    use querymt_agent_services::snapshot::SnapshotBackend;
    for i in 0..count {
        fixture.write_file("churn.txt", &format!("churn {i}"))?;
        fixture.backend.track(fixture.worktree.path()).await?;
    }
    Ok(())
}

fn workspace_bytes(fixture: &UndoTestFixture) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixture.worktree.path())? {
        let entry = entry?;
        out.push((
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read(entry.path())?,
        ));
    }
    out.sort();
    Ok(out)
}

#[tokio::test]
async fn undo_without_limits_reaches_the_first_turn() -> Result<()> {
    use querymt_agent_services::snapshot::SnapshotBackend;
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("f.txt", "v0")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "go").await?;

    for i in 1..=12 {
        run_turn(&fixture, &session, |w| {
            std::fs::write(w.join("f.txt"), format!("v{i}"))?;
            std::fs::write(w.join(format!("t{i}.txt")), "turn file")?;
            Ok(())
        })
        .await?;
        fixture
            .backend
            .gc(
                fixture.worktree.path(),
                &querymt_agent_services::snapshot::GcConfig::default(),
            )
            .await?;
    }

    fixture.undo(&session, &target).await?;

    assert_eq!(fixture.read_file("f.txt")?, "v0");
    for i in 1..=12 {
        assert!(!fixture.worktree.path().join(format!("t{i}.txt")).exists());
    }
    Ok(())
}

#[tokio::test]
async fn undo_to_a_pruned_snapshot_changes_no_files() -> Result<()> {
    use querymt_agent_services::snapshot::SnapshotBackend;
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("test.txt", "original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("test.txt"), "modified")?;
        Ok(())
    })
    .await?;
    churn(&fixture, 12).await?;
    let result = fixture
        .backend
        .gc(fixture.worktree.path(), &gc_keep(1))
        .await?;
    assert!(result.deleted_objects > 0, "content must really be deleted");
    let before = workspace_bytes(&fixture)?;

    let error = fixture.undo(&session, &target).await;

    assert!(error.is_err(), "undo past the retention window must fail");
    assert_eq!(before, workspace_bytes(&fixture)?);
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&session)
            .await?
            .is_none(),
        "an undo rejected before restoring records no redo frame"
    );
    Ok(())
}

#[tokio::test]
async fn redo_to_a_pruned_snapshot_keeps_its_frame() -> Result<()> {
    use querymt_agent_services::snapshot::SnapshotBackend;
    let fixture = UndoTestFixture::new().await?;
    fixture.write_file("test.txt", "original")?;
    let session = fixture.create_session().await?;
    let target = fixture.add_user_message(&session, "edit").await?;
    run_turn(&fixture, &session, |w| {
        std::fs::write(w.join("test.txt"), "modified")?;
        Ok(())
    })
    .await?;
    fixture.undo(&session, &target).await?;
    churn(&fixture, 12).await?;
    let result = fixture
        .backend
        .gc(fixture.worktree.path(), &gc_keep(1))
        .await?;
    assert!(result.deleted_objects > 0);
    let before = workspace_bytes(&fixture)?;

    let error = fixture.redo(&session).await;

    assert!(error.is_err(), "redo past the retention window must fail");
    assert_eq!(before, workspace_bytes(&fixture)?);
    assert!(
        fixture
            .storage
            .session_store()
            .peek_revert_state(&session)
            .await?
            .is_some(),
        "the revert frame must still be present"
    );
    Ok(())
}

// ==================== self-contained turn records ====================

/// Binds the mock LLM config to `session`, so a real prompt can run.
pub(crate) async fn bind_mock_llm(fixture: &UndoTestFixture, session: &str) -> Result<()> {
    let store = fixture.storage.session_store();
    let llm_config = store
        .create_or_get_llm_config(
            &querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .await?;
    store.set_session_llm_config(session, llm_config.id).await?;
    Ok(())
}

/// A mock that answers the first request with one `delete_file` call and
/// the request carrying the tool result with plain text.
pub(crate) fn deleting_provider(path: &str) -> crate::test_utils::SharedLlmProvider {
    use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider};
    use crate::tools::{Tool, builtins::DeleteFileTool};

    let arguments = serde_json::json!({ "path": path }).to_string();
    let respond = Arc::new(move |messages: &[querymt::chat::ChatMessage]| {
        let has_tool_result = messages.last().is_some_and(|message| {
            message
                .portable_input_parts()
                .iter()
                .any(|part| part.is_tool_result())
        });
        if has_tool_result {
            MockChatResponse::text_only("deleted")
        } else {
            MockChatResponse::with_tools(
                "deleting",
                vec![crate::test_utils::mock_querymt_tool_call(
                    "delete-1",
                    "delete_file",
                    &arguments,
                )],
            )
        }
    });
    let mut mock = MockLlmProvider::new();
    let with_tools = respond.clone();
    mock.expect_chat_with_tools()
        .returning(move |messages, _| Ok(with_tools(messages).into()));
    mock.expect_chat()
        .returning(move |messages| Ok(respond(messages).into()));
    SharedLlmProvider::new(mock, vec![DeleteFileTool::new().definition()])
}

/// Id of the first user message in `session`.
async fn first_user_message_id(fixture: &UndoTestFixture, session: &str) -> Result<String> {
    let history = fixture.storage.session_store().get_history(session).await?;
    history
        .iter()
        .find(|m| matches!(m.role, querymt::chat::ChatRole::User))
        .map(|m| m.id.clone())
        .ok_or_else(|| anyhow::anyhow!("no user message recorded"))
}

#[tokio::test]
async fn undo_of_a_prompt_that_deleted_a_file_restores_it() -> Result<()> {
    let fixture = UndoTestFixture::with_provider(deleting_provider("notes.txt")).await?;
    fixture.write_file("notes.txt", "original notes")?;
    let session = fixture.create_session().await?;
    bind_mock_llm(&fixture, &session).await?;

    fixture.prompt(&session, "delete the notes").await?;
    assert!(
        !fixture.worktree.path().join("notes.txt").exists(),
        "the prompt's tool call must have deleted the file"
    );
    let target = first_user_message_id(&fixture, &session).await?;

    let result = fixture.undo(&session, &target).await?;

    assert_eq!(
        std::fs::read_to_string(fixture.worktree.path().join("notes.txt")).ok(),
        Some("original notes".to_string()),
        "undo must restore the deleted file"
    );
    assert!(
        result.reverted_files.iter().any(|f| f == "notes.txt"),
        "reverted_files must contain notes.txt, got {:?}",
        result.reverted_files
    );
    Ok(())
}

// ==================== Turn record resolution (design D15-D17) ====================

mod turn_records {
    use super::*;
    use querymt::chat::ChatRole;
    use querymt_agent_core::model::{AgentMessage, TurnId};
    use querymt_agent_services::snapshot::{SnapshotBackend as _, SnapshotId};

    async fn record(fixture: &UndoTestFixture, session: &str, part: MessagePart) -> Result<()> {
        let mut message = AgentMessage::new(session.to_string(), ChatRole::Assistant);
        message.parts.push(part);
        fixture
            .storage
            .session_store()
            .add_message(session, message)
            .await?;
        Ok(())
    }

    async fn snapshot(fixture: &UndoTestFixture) -> Result<SnapshotId> {
        Ok(fixture.backend.track(fixture.worktree.path()).await?)
    }

    async fn start(
        fixture: &UndoTestFixture,
        session: &str,
        turn_id: TurnId,
        snapshot_id: &SnapshotId,
    ) -> Result<()> {
        record(
            fixture,
            session,
            MessagePart::TurnSnapshotStart {
                turn_id,
                snapshot_id: snapshot_id.clone(),
                workspace_root: Some(fixture.worktree.path().to_path_buf()),
                inherited: false,
            },
        )
        .await
    }

    async fn patch(
        fixture: &UndoTestFixture,
        session: &str,
        turn_id: TurnId,
        base: Option<&SnapshotId>,
        paths: &[&str],
    ) -> Result<()> {
        let post = snapshot(fixture).await?;
        record(
            fixture,
            session,
            MessagePart::TurnSnapshotPatch {
                turn_id,
                snapshot_id: post,
                base_snapshot_id: base.cloned(),
                changed_paths: paths.iter().map(|p| p.to_string()).collect(),
                workspace_root: Some(fixture.worktree.path().to_path_buf()),
                inherited: false,
            },
        )
        .await
    }

    #[tokio::test]
    async fn undo_works_without_the_turns_start_record() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        let pre = snapshot(&fixture).await?;
        fixture.write_file("a.txt", "after")?;
        patch(&fixture, &session, TurnId::new(), Some(&pre), &["a.txt"]).await?;

        let result = fixture.undo(&session, &target).await?;

        assert_eq!(fixture.read_file("a.txt")?, "before");
        assert_eq!(result.reverted_files, vec!["a.txt".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn earlier_format_turn_is_undone_through_its_start_record() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        let turn = TurnId::new();
        let pre = snapshot(&fixture).await?;
        start(&fixture, &session, turn, &pre).await?;
        fixture.write_file("a.txt", "after")?;
        patch(&fixture, &session, turn, None, &["a.txt"]).await?;

        fixture.undo(&session, &target).await?;

        assert_eq!(fixture.read_file("a.txt")?, "before");
        Ok(())
    }

    #[tokio::test]
    async fn turn_whose_records_disagree_on_turn_id_is_undone_by_position() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        let pre = snapshot(&fixture).await?;
        start(&fixture, &session, TurnId::new(), &pre).await?;
        fixture.write_file("a.txt", "after")?;
        // Different turn id, no base: only the record right before it can
        // name the pre-turn snapshot.
        patch(&fixture, &session, TurnId::new(), None, &["a.txt"]).await?;

        fixture.undo(&session, &target).await?;

        assert_eq!(fixture.read_file("a.txt")?, "before");
        Ok(())
    }

    #[tokio::test]
    async fn patch_with_its_own_base_ignores_an_unrelated_preceding_start() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let stale = snapshot(&fixture).await?;
        start(&fixture, &session, TurnId::new(), &stale).await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        let pre = snapshot(&fixture).await?;
        fixture.write_file("a.txt", "after")?;
        // Fresh turn id but its own base: the unclaimed start before the
        // target must not be paired with it.
        patch(&fixture, &session, TurnId::new(), Some(&pre), &["a.txt"]).await?;

        fixture.undo(&session, &target).await?;

        assert_eq!(fixture.read_file("a.txt")?, "before");
        Ok(())
    }

    #[tokio::test]
    async fn overlapping_turns_are_paired_by_turn_id() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "v0")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        let (turn_a, turn_b) = (TurnId::new(), TurnId::new());
        let snap_a = snapshot(&fixture).await?;
        start(&fixture, &session, turn_a, &snap_a).await?;
        fixture.write_file("a.txt", "v1")?;
        let snap_b = snapshot(&fixture).await?;
        start(&fixture, &session, turn_b, &snap_b).await?;
        fixture.write_file("a.txt", "v2")?;
        patch(&fixture, &session, turn_a, None, &["a.txt"]).await?;
        fixture.write_file("a.txt", "v3")?;
        patch(&fixture, &session, turn_b, None, &["a.txt"]).await?;

        fixture.undo(&session, &target).await?;

        // Reverse order: B restores v1, then A restores v0. Pairing by
        // position would pair A with B's snapshot and end on v1.
        assert_eq!(fixture.read_file("a.txt")?, "v0");
        Ok(())
    }

    #[tokio::test]
    async fn turn_without_a_pre_turn_record_fails_undo_without_changes() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "edit").await?;
        fixture.write_file("a.txt", "after")?;
        let turn = TurnId::new();
        patch(&fixture, &session, turn, None, &["a.txt"]).await?;

        let error = fixture.undo(&session, &target).await.unwrap_err();

        let UndoError::Snapshot(message) = &error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(message.contains(&turn.to_string()), "{message}");
        assert!(
            message.contains("pre-turn snapshot is not recorded"),
            "{message}"
        );
        assert_eq!(fixture.read_file("a.txt")?, "after");
        assert!(
            fixture
                .storage
                .session_store()
                .peek_revert_state(&session)
                .await?
                .is_none(),
            "a failed undo must not record a redo frame"
        );
        Ok(())
    }

    #[tokio::test]
    async fn undo_to_a_message_inside_a_turn_is_refused() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let prompt = fixture.add_user_message(&session, "edit").await?;
        let turn = TurnId::new();
        let pre = snapshot(&fixture).await?;
        start(&fixture, &session, turn, &pre).await?;
        let mut steering = AgentMessage::new(session.clone(), ChatRole::User);
        steering.parts.push(MessagePart::Steering {
            run_id: turn.to_string(),
            client_input_id: None,
            blocks: vec![],
        });
        let steering_id = steering.id.clone();
        fixture
            .storage
            .session_store()
            .add_message(&session, steering)
            .await?;
        fixture.write_file("a.txt", "after")?;
        patch(&fixture, &session, turn, Some(&pre), &["a.txt"]).await?;

        let error = fixture.undo(&session, &steering_id).await.unwrap_err();

        let UndoError::Snapshot(message) = &error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(message.contains("undo reverts whole turns"), "{message}");
        assert!(message.contains(&turn.to_string()), "{message}");
        assert_eq!(fixture.read_file("a.txt")?, "after");
        assert!(
            fixture
                .storage
                .session_store()
                .peek_revert_state(&session)
                .await?
                .is_none()
        );

        // The check only applies inside the turn.
        fixture.undo(&session, &prompt).await?;
        assert_eq!(fixture.read_file("a.txt")?, "before");
        Ok(())
    }

    #[tokio::test]
    async fn turn_that_changed_no_files_does_not_block_undo() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        fixture.write_file("a.txt", "before")?;
        let session = fixture.create_session().await?;
        let target = fixture.add_user_message(&session, "just chat").await?;
        let pre = snapshot(&fixture).await?;
        start(&fixture, &session, TurnId::new(), &pre).await?;

        let result = fixture.undo(&session, &target).await?;

        assert!(result.reverted_files.is_empty());
        assert_eq!(fixture.read_file("a.txt")?, "before");
        Ok(())
    }
}

// ==================== Test Suite: a rewind restores the todo list ====================

mod todo_rewind {
    use super::*;
    use crate::session::ViewStore as _;
    use crate::tools::builtins::todo::{TodoItem, TodoPriority, TodoStatus};
    use querymt_agent_core::events::EventEnvelope;
    use querymt_agent_core::protocol::PlanEntry;
    use std::time::Duration;

    fn todowrite(todos: &[(&str, &str)]) -> AgentEventKind {
        let todos: Vec<_> = todos
            .iter()
            .map(|(id, status)| {
                serde_json::json!({
                    "id": id, "content": id, "status": status, "priority": "medium"
                })
            })
            .collect();
        AgentEventKind::ToolCallStart {
            tool_call_id: "call".into(),
            tool_name: "todowrite".into(),
            arguments: serde_json::json!({ "todos": todos }).to_string(),
        }
    }

    fn item(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: id.into(),
            content: id.into(),
            status,
            priority: TodoPriority::Medium,
        }
    }

    /// One turn: a user message, its prompt event, then `events`. Returns the
    /// message id, which is the undo target.
    async fn turn(
        fixture: &UndoTestFixture,
        session: &str,
        prompt: &str,
        events: Vec<AgentEventKind>,
    ) -> Result<String> {
        let journal = fixture.storage.event_journal();
        let message_id = fixture.add_user_message(session, prompt).await?;
        append_prompt_event(&*journal, session, &message_id).await;
        for kind in events {
            append_kind(&*journal, session, kind).await;
        }
        Ok(message_id)
    }

    /// The plan `session/list` reports for the session.
    async fn listed_plan(fixture: &UndoTestFixture, session: &str) -> Option<Vec<PlanEntry>> {
        let view = fixture.storage.get_session_list_view(None).await.unwrap();
        view.groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .find(|item| item.session_id == session)
            .expect("the session is listed")
            .todo_plan
            .clone()
    }

    /// Undo `target`, then run the cleanup a new prompt triggers, announcing
    /// on the agent's event stream.
    async fn rewind(
        fixture: &UndoTestFixture,
        session: &str,
        target: &str,
    ) -> Result<Option<Vec<TodoItem>>> {
        fixture.undo(session, target).await?;
        let config = &fixture.handle.config;
        Ok(crate::agent::undo::cleanup_revert_on_prompt(
            &config.provider,
            &**config.event_sink.journal(),
            Some(&config.event_sink.session_stream(session)),
            session,
            None,
        )
        .await?)
    }

    /// The next `TodoPlanRestored` entries on the stream, `None` when the next
    /// event after a sentinel is the sentinel itself.
    async fn next_restored(
        events: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<EventEnvelope>>,
        fixture: &UndoTestFixture,
        session: &str,
    ) -> Option<Vec<PlanEntry>> {
        // Events arrive in order, so reaching the sentinel without a restored
        // event proves none was sent.
        fixture.handle.config.event_sink.emit_ephemeral(
            session,
            AgentEventKind::McpServerAuthRequired {
                server_name: "sentinel".into(),
                provider_id: "mcp:sentinel".into(),
                error: None,
            },
        );
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("an event on the stream")
                .expect("the stream is open");
            match envelope.kind() {
                AgentEventKind::TodoPlanRestored { entries } => return Some(entries.clone()),
                AgentEventKind::McpServerAuthRequired { .. } => return None,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn rewinding_past_a_todowrite_restores_the_earlier_list_everywhere() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        let session = fixture.create_session().await?;
        turn(
            &fixture,
            &session,
            "first",
            vec![todowrite(&[("a", "completed")])],
        )
        .await?;
        let second = turn(
            &fixture,
            &session,
            "second",
            vec![todowrite(&[("a", "completed"), ("b", "pending")])],
        )
        .await?;
        let mut events = fixture.handle.config.event_sink.fanout().subscribe();
        assert_eq!(listed_plan(&fixture, &session).await.unwrap().len(), 2);

        let restored = rewind(&fixture, &session, &second).await?;

        // The todo store `todoread` reads gets the earlier list, with its ids.
        assert_eq!(restored, Some(vec![item("a", TodoStatus::Completed)]));
        // The listed plan was recomputed with the prune.
        assert_eq!(listed_plan(&fixture, &session).await.unwrap().len(), 1);
        // Live observers are told the same plan.
        let announced = next_restored(&mut events, &fixture, &session)
            .await
            .expect("the rewind is announced");
        assert_eq!(Some(announced), listed_plan(&fixture, &session).await);
        Ok(())
    }

    #[tokio::test]
    async fn rewinding_past_the_only_todowrite_empties_the_list() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        let session = fixture.create_session().await?;
        turn(&fixture, &session, "first", vec![]).await?;
        let second = turn(
            &fixture,
            &session,
            "second",
            vec![todowrite(&[("a", "pending")])],
        )
        .await?;
        let mut events = fixture.handle.config.event_sink.fanout().subscribe();
        assert!(listed_plan(&fixture, &session).await.is_some());

        let restored = rewind(&fixture, &session, &second).await?;

        assert_eq!(restored, Some(Vec::new()));
        assert_eq!(listed_plan(&fixture, &session).await, None);
        assert_eq!(
            next_restored(&mut events, &fixture, &session).await,
            Some(Vec::new()),
            "an emptied plan is announced as empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rewinding_a_turn_without_a_todowrite_leaves_the_list_alone() -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        let session = fixture.create_session().await?;
        turn(
            &fixture,
            &session,
            "first",
            vec![todowrite(&[("a", "pending")])],
        )
        .await?;
        let second = turn(&fixture, &session, "second", vec![]).await?;
        let mut events = fixture.handle.config.event_sink.fanout().subscribe();

        let restored = rewind(&fixture, &session, &second).await?;

        assert_eq!(restored, None, "the todo store keeps its list");
        assert_eq!(listed_plan(&fixture, &session).await.unwrap().len(), 1);
        assert_eq!(next_restored(&mut events, &fixture, &session).await, None);
        Ok(())
    }

    #[tokio::test]
    async fn a_rewound_todowrite_that_changed_only_cancelled_items_resets_the_store_quietly()
    -> Result<()> {
        let fixture = UndoTestFixture::new().await?;
        let session = fixture.create_session().await?;
        turn(
            &fixture,
            &session,
            "first",
            vec![todowrite(&[("a", "pending")])],
        )
        .await?;
        // Cancelled items are not part of the plan: the listed plan is the same.
        let second = turn(
            &fixture,
            &session,
            "second",
            vec![todowrite(&[("a", "pending"), ("b", "cancelled")])],
        )
        .await?;
        let mut events = fixture.handle.config.event_sink.fanout().subscribe();

        let restored = rewind(&fixture, &session, &second).await?;

        assert_eq!(restored, Some(vec![item("a", TodoStatus::Pending)]));
        assert_eq!(next_restored(&mut events, &fixture, &session).await, None);
        Ok(())
    }
}
