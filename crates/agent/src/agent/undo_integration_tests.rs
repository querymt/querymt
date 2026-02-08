//! Integration tests for undo/redo with full agent setup
//!
//! These tests validate the complete undo flow including:
//! - Message history lookup
//! - Cross-session delegation awareness
//! - File restoration
//! - Proper reverted files list

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
    let result = fixture.agent.undo(&session_id, &user_msg_id).await?;

    // Verify reverted
    assert_eq!(fixture.read_file("test.txt")?, "original");
    assert_eq!(result.reverted_files.len(), 1);
    assert!(result.reverted_files.contains(&"test.txt".to_string()));

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
    let result = fixture.agent.undo(&session_id, &user_msg_id).await?;

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
    let _result = fixture.agent.undo(&session_id, &user_msg_id).await?;

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
    let result = fixture.agent.undo(&parent_id, &user_msg_id).await?;

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
    let result = fixture.agent.undo(&parent_id, &user_msg_id).await?;

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
    fixture.agent.undo(&session_id, &user_msg_id).await?;
    assert_eq!(fixture.read_file("test.txt")?, "original");

    // Redo - should restore to "modified"
    let redo_result = fixture.agent.redo(&session_id).await?;
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
    fixture.agent.undo(&session_id, &user_msg_id).await?;
    assert!(fixture.worktree.path().join("to_delete.txt").exists());
    assert_eq!(fixture.read_file("to_delete.txt")?, "content");

    // Redo - should delete the file again (THIS IS THE BUG FIX TEST)
    let redo_result = fixture.agent.redo(&session_id).await?;
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
    fixture.agent.undo(&session_id, &user_msg_id).await?;
    assert!(!fixture.worktree.path().join("new.txt").exists());

    // Redo - should recreate the file
    let redo_result = fixture.agent.redo(&session_id).await?;
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
    let result = fixture.agent.redo(&session_id).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Nothing to redo"));

    Ok(())
}
