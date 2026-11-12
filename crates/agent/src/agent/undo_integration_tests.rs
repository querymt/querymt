//! Integration tests for undo/redo with full agent setup
//!
//! These tests validate the complete undo flow including:
//! - Message history lookup
//! - Cross-session delegation awareness
//! - File restoration
//! - Proper reverted files list

use crate::agent::undo::UndoError;
use crate::events::{AgentEventKind, EventOrigin};
use crate::model::MessagePart;
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::projection::{EventJournal, NewDurableEvent};
use crate::test_utils::UndoTestFixture;
use anyhow::Result;
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
    let mut message =
        crate::model::AgentMessage::new(session.clone(), querymt::chat::ChatRole::Assistant);
    message.parts.push(MessagePart::TurnSnapshotStart {
        turn_id: "foreign".into(),
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
        &session_id,
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
        &session_id,
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
    use crate::accounting::tests::{sample_settlement, sample_start};
    use crate::accounting::{AttemptRecorder, SessionAccounting};
    use crate::event_fanout::EventFanout;
    use crate::session_event_stream::SessionEventStream;

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
            content: "first".into(),
            message_id: Some(msg1),
        })
        .await?;
    let msg2 = fixture.add_user_message(&session_id, "second").await?;
    let prompt2 = stream
        .emit_persisted(AgentEventKind::PromptReceived {
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
        &session_id,
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
        &session_id,
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
                if let crate::model::MessagePart::Text { content } = p {
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
        let conn = conn.lock().unwrap();
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
        let conn = conn.lock().unwrap();
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

fn gc_keep(n: usize) -> crate::snapshot::GcConfig {
    crate::snapshot::GcConfig {
        max_snapshots: std::num::NonZeroUsize::new(n),
        max_age_days: None,
    }
}

/// `count` extra changed snapshots, so earlier ones can be pruned.
async fn churn(fixture: &UndoTestFixture, count: usize) -> Result<()> {
    use crate::snapshot::SnapshotBackend;
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
    use crate::snapshot::SnapshotBackend;
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
                &crate::snapshot::GcConfig::default(),
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
    use crate::snapshot::SnapshotBackend;
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
    use crate::snapshot::SnapshotBackend;
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
