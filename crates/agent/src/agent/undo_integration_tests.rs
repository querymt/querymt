//! Integration tests for undo/redo with full agent setup
//!
//! These tests validate the complete undo flow including:
//! - Message history lookup
//! - Cross-session delegation awareness
//! - File restoration
//! - Proper reverted files list

use crate::events::{AgentEventKind, EventOrigin};
use crate::session::backend::StorageBackend;
use crate::session::projection::{EventJournal, NewDurableEvent};
use crate::test_utils::UndoTestFixture;
use anyhow::Result;

// ==================== Test Suite: Basic Undo with Message History ====================

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
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
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
            session_id: session_id.to_string(),
            origin: EventOrigin::Local,
            source_node: None,
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
