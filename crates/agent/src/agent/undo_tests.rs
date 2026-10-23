//! Simple integration tests for undo/redo functionality
//!
//! These tests focus on testing the undo logic with minimal setup.
//! They use real GitSnapshotBackend but mock the store interactions.

use crate::snapshot::backend::SnapshotBackend;
use crate::snapshot::git::GitSnapshotBackend;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ==================== Test Suite 1: Git Backend Integration ====================

#[tokio::test]
async fn test_git_backend_track_and_restore_paths() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "original").unwrap();
    let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
    let snap_pre = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("a.txt"), "modified").unwrap();
    let snap_post = backend.track(tmpdir.path()).await.unwrap();

    // Verify snapshots are different
    assert_ne!(snap_pre, snap_post);

    // Diff should detect the change
    let diff = backend
        .diff(tmpdir.path(), &snap_pre, &snap_post)
        .await
        .unwrap();
    assert_eq!(diff.len(), 1);
    assert!(diff.contains(&PathBuf::from("a.txt")));

    // Restore should revert the file
    backend
        .restore_paths(tmpdir.path(), &snap_pre, &[PathBuf::from("a.txt")])
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "original"
    );
}

#[tokio::test]
async fn test_git_backend_multiple_files() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "ccc").unwrap();

    let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
    let snap_pre = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("a.txt"), "AAA").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "BBB").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "CCC").unwrap();

    // Selective restore: only a.txt and b.txt
    backend
        .restore_paths(
            tmpdir.path(),
            &snap_pre,
            &[PathBuf::from("a.txt"), PathBuf::from("b.txt")],
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "aaa"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
        "bbb"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("c.txt")).unwrap(),
        "CCC"
    ); // Unchanged
}

#[tokio::test]
async fn test_git_backend_file_deletion() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("file.txt"), "content").unwrap();
    let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
    let snap_with_file = backend.track(tmpdir.path()).await.unwrap();

    // Simulate undo by restoring a path that didn't exist in pre-snapshot
    // First create a new file
    fs::write(tmpdir.path().join("new.txt"), "new content").unwrap();
    assert!(tmpdir.path().join("new.txt").exists());

    // Create snapshot without new.txt
    let snap_without = snap_with_file; // This snapshot doesn't have new.txt

    // Restore new.txt to state where it didn't exist
    backend
        .restore_paths(tmpdir.path(), &snap_without, &[PathBuf::from("new.txt")])
        .await
        .unwrap();

    // new.txt should be removed
    assert!(!tmpdir.path().join("new.txt").exists());
}

#[tokio::test]
async fn test_git_backend_full_restore() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "v1-a").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "v1-b").unwrap();

    let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
    let snap1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("a.txt"), "v2-a").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "v2-b").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "v2-c").unwrap();

    let _snap2 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("a.txt"), "v3-a").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "v3-b").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "v3-c").unwrap();
    let _snap3 = backend.track(tmpdir.path()).await.unwrap();

    // Restore to snap1
    backend.restore(tmpdir.path(), &snap1).await.unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "v1-a"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
        "v1-b"
    );

    // Bug #5: c.txt still exists after full restore
    // (This documents current behavior)
    assert!(tmpdir.path().join("c.txt").exists());
}

// Note: Full agent-level undo/redo tests would require complex setup with SessionProvider,
// PluginRegistry, etc. Those tests should be added as the codebase matures or when
// a proper test builder pattern is available.
//
// The tests above validate the core snapshot backend behavior which is the foundation
// for undo/redo functionality.

// ==================== Linked-worktree preservation ====================

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be available");
    assert!(status.success(), "git {args:?} failed");
}

fn git_for_each_ref(dir: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .args(["for-each-ref"])
        .current_dir(dir)
        .output()
        .expect("git must be available");
    String::from_utf8(output.stdout).expect("utf8 refs")
}

/// Undo/redo operate inside a linked Git worktree — the shape of a managed
/// workspace checkout — using only the configured `GitSnapshotBackend`.
/// Restores must preserve sibling files, project refs, and Git's own
/// administrative markers.
#[tokio::test]
async fn linked_worktree_undo_redo_preserves_siblings_refs_and_git_markers() {
    let repo = TempDir::new().unwrap();
    let linked = TempDir::new().unwrap();
    run_git(repo.path(), &["init", "-q", "-b", "main"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["config", "user.email", "test@example.com"]);
    fs::write(repo.path().join("shared.txt"), "shared").unwrap();
    fs::write(repo.path().join("sibling.txt"), "sibling").unwrap();
    run_git(repo.path(), &["add", "."]);
    run_git(repo.path(), &["commit", "-q", "-m", "init"]);
    run_git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "undo-fixture",
            linked.path().to_str().unwrap(),
        ],
    );

    let refs_before = git_for_each_ref(repo.path());

    // The single configured backend, with its snapshot base outside the
    // repository: undo adds no second snapshot implementation and no
    // project refs.
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::with_snapshot_base(base.path().to_path_buf());

    // Turn-style capture inside the linked worktree.
    let snap_pre = backend.track(linked.path()).await.unwrap();
    fs::write(linked.path().join("shared.txt"), "edited").unwrap();
    fs::write(linked.path().join("new.txt"), "added").unwrap();
    let snap_post = backend.track(linked.path()).await.unwrap();
    let diff = backend
        .diff(linked.path(), &snap_pre, &snap_post)
        .await
        .unwrap();
    assert!(diff.contains(&PathBuf::from("shared.txt")));
    assert!(diff.contains(&PathBuf::from("new.txt")));

    // Undo: selective restore touches only the listed paths.
    backend
        .restore_paths(linked.path(), &snap_pre, &[PathBuf::from("shared.txt")])
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(linked.path().join("shared.txt")).unwrap(),
        "shared"
    );
    assert_eq!(
        fs::read_to_string(linked.path().join("sibling.txt")).unwrap(),
        "sibling",
        "sibling files are preserved by undo"
    );
    assert!(
        linked.path().join("new.txt").exists(),
        "unlisted files are not removed by a selective restore"
    );

    // Redo: whole-tree restore back to the post snapshot.
    backend.restore(linked.path(), &snap_post).await.unwrap();
    assert_eq!(
        fs::read_to_string(linked.path().join("shared.txt")).unwrap(),
        "edited"
    );
    assert_eq!(
        fs::read_to_string(linked.path().join("new.txt")).unwrap(),
        "added"
    );
    assert_eq!(
        fs::read_to_string(linked.path().join("sibling.txt")).unwrap(),
        "sibling",
        "sibling files are preserved by redo"
    );

    // Project refs are untouched: the backend stores its snapshots outside
    // the repository and restores never register refs in the project.
    assert_eq!(
        git_for_each_ref(repo.path()),
        refs_before,
        "undo/redo must not modify project refs"
    );

    // Git administrative markers survive: the linked worktree's `.git`
    // file still points at its gitdir and both trees remain usable.
    let git_file = fs::read_to_string(linked.path().join(".git")).unwrap();
    assert!(
        git_file.starts_with("gitdir:"),
        "linked worktree .git marker must remain a gitdir pointer"
    );
    assert!(repo.path().join(".git").is_dir());
    run_git(linked.path(), &["status", "--porcelain"]);
    run_git(repo.path(), &["status", "--porcelain"]);
}

// ==================== Workspace provenance ====================

fn revert_frame(workspace_root: Option<&std::path::Path>) -> crate::session::domain::RevertState {
    crate::session::domain::RevertState {
        public_id: "rev-1".to_string(),
        session_id: "sess-1".to_string(),
        message_id: "msg-1".to_string(),
        snapshot_id: "snap-1".to_string(),
        backend_id: "git".to_string(),
        workspace_root: workspace_root.map(std::path::Path::to_path_buf),
        created_at: time::OffsetDateTime::now_utc(),
    }
}

#[test]
fn redo_rejects_cross_workspace_frames_without_restoring() {
    let dir = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();

    let err = crate::agent::undo::validate_revert_workspace(
        &revert_frame(Some(other.path())),
        dir.path(),
    )
    .expect_err("cross-workspace frame must be rejected");
    assert!(
        matches!(err, crate::agent::undo::UndoError::WorkspaceMismatch { .. }),
        "error: {err:?}"
    );
    assert!(
        err.to_string()
            .contains("refusing to restore across workspaces"),
        "error: {err}"
    );
}

#[test]
fn redo_accepts_same_workspace_and_legacy_frames() {
    let dir = TempDir::new().unwrap();

    // Same-workspace frame (canonicalized: symlink prefixes must not
    // cause rejection).
    crate::agent::undo::validate_revert_workspace(&revert_frame(Some(dir.path())), dir.path())
        .expect("same-workspace frame is accepted");

    // Legacy frame recorded before provenance existed keeps undo history.
    crate::agent::undo::validate_revert_workspace(&revert_frame(None), dir.path())
        .expect("legacy frames keep working");
}
