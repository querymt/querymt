use super::capture::*;
use super::gc::{collect_garbage, collect_garbage_with, dir_size};
use super::*;
use std::fs;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

#[test]
fn test_snapshot_dir_deterministic() {
    let tmpbase = TempDir::new().unwrap();
    let path1 = PathBuf::from("/tmp/test-project");
    let path2 = PathBuf::from("/tmp/test-project");

    let backend1 = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let backend2 = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());

    let dir1 = backend1.snapshot_dir(&path1).unwrap();
    let dir2 = backend2.snapshot_dir(&path2).unwrap();

    assert_eq!(dir1, dir2, "Same path should produce same snapshot dir");
}

#[tokio::test]
async fn test_track_creates_snapshot() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot_id = backend.track(tmpdir.path()).await.unwrap();

    assert!(
        !snapshot_id.is_empty(),
        "Should return non-empty snapshot ID"
    );
    assert_eq!(snapshot_id.len(), 40, "Git SHA should be 40 hex characters");
}

#[tokio::test]
async fn test_diff_detects_changes() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let file_path = tmpdir.path().join("test.txt");

    fs::write(&file_path, "initial").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "modified").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    assert_eq!(diff.len(), 1, "Should detect one changed file");
    assert_eq!(
        diff[0],
        PathBuf::from("test.txt"),
        "Should identify test.txt as changed"
    );
}

#[tokio::test]
async fn test_restore_paths_works() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let file_path = tmpdir.path().join("test.txt");

    fs::write(&file_path, "original").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "modified").unwrap();
    backend
        .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("test.txt")])
        .await
        .unwrap();

    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "original", "File should be restored to original");
}

#[tokio::test]
async fn test_restore_full() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    // Modify both files
    fs::write(tmpdir.path().join("a.txt"), "AAA").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "BBB").unwrap();

    backend.restore(tmpdir.path(), &snapshot).await.unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "aaa"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
        "bbb"
    );
}

// ==================== Snapshot GC vs workspace references ====================

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must be available");
    assert!(status.success(), "git {args:?} failed");
}

/// Undo snapshot garbage collection must never remove the workspace
/// baseline/result references the worktree service keeps in the real
/// project repository: snapshot history and workspace refs are
/// independent lifecycles.
#[tokio::test]
async fn test_gc_leaves_workspace_baseline_refs_reachable() {
    // A real project repository.
    let project = TempDir::new().unwrap();
    run_git(project.path(), &["init", "-q"]);
    run_git(project.path(), &["config", "user.email", "qmt@example.com"]);
    run_git(project.path(), &["config", "user.name", "qmt"]);
    fs::write(project.path().join("app.txt"), "app").unwrap();
    run_git(project.path(), &["add", "."]);
    run_git(project.path(), &["commit", "-q", "-m", "init"]);

    // The baseline ref and a retained result ref the worktree service
    // would create for a workspace.
    let workspace_id = crate::worktree::WorkspaceId::generate();
    let baseline_ref = crate::worktree::baseline_ref_for(&workspace_id);
    run_git(project.path(), &["update-ref", &baseline_ref, "HEAD"]);
    let result_ref = format!("refs/qmt/results/{workspace_id}");
    run_git(project.path(), &["branch", "ws-result", "HEAD"]);
    run_git(project.path(), &["update-ref", &result_ref, "ws-result"]);

    // A linked worktree of that project with undo snapshot history.
    let linked = TempDir::new().unwrap();
    run_git(
        project.path(),
        &[
            "worktree",
            "add",
            "-b",
            "qmt/ws-gc-test",
            linked.path().to_str().unwrap(),
        ],
    );

    let snapshot_base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(snapshot_base.path());
    for i in 0..3 {
        fs::write(linked.path().join("scratch.txt"), format!("v{i}")).unwrap();
        backend.track(linked.path()).await.unwrap();
    }

    // Collect almost all snapshot history.
    let config = GcConfig {
        max_snapshots: NonZeroUsize::new(1),
        max_age_days: None,
    };
    let result = backend.gc(linked.path(), &config).await.unwrap();
    assert_eq!(result.remaining_count, 1);
    assert_eq!(result.removed_count, 2);

    // The project's workspace baseline ref and retained result ref are
    // still reachable.
    run_git(project.path(), &["rev-parse", "--verify", &baseline_ref]);
    run_git(project.path(), &["rev-parse", "--verify", &result_ref]);

    // The linked worktree still works: its administrative marker and
    // the shared repository metadata are intact.
    assert!(linked.path().join(".git").exists());
    run_git(linked.path(), &["status", "--porcelain"]);
}

// ==================== Test Suite 1.1: Track Idempotency & Determinism ====================

#[tokio::test]
async fn test_track_changes_after_modify() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let file_path = tmpdir.path().join("test.txt");

    fs::write(&file_path, "initial").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "modified").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    assert_ne!(
        snapshot1, snapshot2,
        "Track after modifications should return different snapshot ID"
    );
}

#[tokio::test]
async fn test_track_empty_directory() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot_id = backend.track(tmpdir.path()).await.unwrap();

    assert!(
        !snapshot_id.is_empty(),
        "Track empty directory should return valid snapshot ID"
    );
}

// ==================== Test Suite 1.2: Diff Detection ====================

#[tokio::test]
async fn test_diff_new_file_added() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    assert_eq!(diff.len(), 1, "Should detect one new file");
    assert!(
        diff.contains(&PathBuf::from("b.txt")),
        "Diff should contain b.txt"
    );
}

#[tokio::test]
async fn test_diff_file_deleted() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::remove_file(tmpdir.path().join("b.txt")).unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    assert_eq!(diff.len(), 1, "Should detect one deleted file");
    assert!(
        diff.contains(&PathBuf::from("b.txt")),
        "Diff should contain b.txt"
    );
}

#[tokio::test]
async fn test_diff_no_changes_empty() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    assert_eq!(
        diff.len(),
        0,
        "No modifications should result in empty diff"
    );
}

#[tokio::test]
async fn test_diff_nested_directory() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let nested_dir = tmpdir.path().join("src/foo");
    fs::create_dir_all(&nested_dir).unwrap();
    let nested_file = nested_dir.join("bar.rs");
    fs::write(&nested_file, "initial").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&nested_file, "modified").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    // Git's diff returns tree entries - may include directories
    // Verify the file is in the diff (it may report src, src/foo, and src/foo/bar.rs)
    assert!(
        diff.contains(&PathBuf::from("src/foo/bar.rs")),
        "Diff should return the modified file path"
    );
}

// ==================== Test Suite 1.3: Selective Restore (restore_paths) ====================

#[tokio::test]
async fn test_restore_paths_deletes_new_file() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
    assert!(tmpdir.path().join("b.txt").exists(), "b.txt should exist");

    backend
        .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("b.txt")])
        .await
        .unwrap();

    assert!(
        !tmpdir.path().join("b.txt").exists(),
        "b.txt should be removed after restore"
    );
}

#[tokio::test]
async fn test_restore_paths_selective() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "ccc").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("a.txt"), "AAA").unwrap();
    fs::write(tmpdir.path().join("b.txt"), "BBB").unwrap();
    fs::write(tmpdir.path().join("c.txt"), "CCC").unwrap();

    backend
        .restore_paths(
            tmpdir.path(),
            &snapshot,
            &[PathBuf::from("a.txt"), PathBuf::from("b.txt")],
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "aaa",
        "a.txt should be restored"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("b.txt")).unwrap(),
        "bbb",
        "b.txt should be restored"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("c.txt")).unwrap(),
        "CCC",
        "c.txt should remain untouched"
    );
}

#[tokio::test]
async fn test_restore_paths_nested() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let nested_dir = tmpdir.path().join("src/deep/nested");
    fs::create_dir_all(&nested_dir).unwrap();
    let nested_file = nested_dir.join("file.rs");
    fs::write(&nested_file, "original").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&nested_file, "modified").unwrap();

    backend
        .restore_paths(
            tmpdir.path(),
            &snapshot,
            &[PathBuf::from("src/deep/nested/file.rs")],
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(&nested_file).unwrap(),
        "original",
        "Nested file should be restored"
    );
}

#[tokio::test]
async fn test_restore_paths_empty_list() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("test.txt"), "modified").unwrap();

    backend
        .restore_paths(tmpdir.path(), &snapshot, &[])
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(tmpdir.path().join("test.txt")).unwrap(),
        "modified",
        "File should remain modified after empty restore"
    );
}

// ==================== Test Suite 1.4: Full Restore & Edge Cases ====================

#[tokio::test]
async fn test_restore_full_does_not_remove_new_files() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(tmpdir.path().join("b.txt"), "bbb").unwrap();

    backend.restore(tmpdir.path(), &snapshot).await.unwrap();

    assert!(
        tmpdir.path().join("a.txt").exists(),
        "a.txt should still exist"
    );
    assert_eq!(
        fs::read_to_string(tmpdir.path().join("a.txt")).unwrap(),
        "aaa",
        "a.txt should be restored"
    );

    // Bug #5: This documents the current limitation - b.txt is NOT removed
    assert!(
        tmpdir.path().join("b.txt").exists(),
        "BUG: b.txt still exists after full restore (documents Bug #5)"
    );
}

#[tokio::test]
async fn test_multi_snapshot_chain_restore() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());

    let file_path = tmpdir.path().join("test.txt");

    // Create 5 snapshots with progressive changes
    fs::write(&file_path, "v1").unwrap();
    let _snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "v2").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "v3").unwrap();
    let _snapshot3 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "v4").unwrap();
    let _snapshot4 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(&file_path, "v5").unwrap();
    let _snapshot5 = backend.track(tmpdir.path()).await.unwrap();

    // Restore to snapshot #2
    backend.restore(tmpdir.path(), &snapshot2).await.unwrap();

    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "v2",
        "Should restore to snapshot #2 state"
    );
}

#[tokio::test]
async fn test_restore_binary_file() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let binary_data: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let file_path = tmpdir.path().join("test.bin");
    fs::write(&file_path, &binary_data).unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    let modified_data: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0xFF];
    fs::write(&file_path, &modified_data).unwrap();

    backend
        .restore_paths(tmpdir.path(), &snapshot, &[PathBuf::from("test.bin")])
        .await
        .unwrap();

    let restored_data = fs::read(&file_path).unwrap();
    assert_eq!(
        restored_data, binary_data,
        "Binary content should roundtrip correctly"
    );
}

// ==================== Regression tests: directory path handling ====================

/// Regression: diff() must return only file paths, not intermediate directory paths.
///
/// gix's diff_tree_to_tree emits change entries for every ancestor tree whose
/// hash changed (e.g. "src", "src/foo") in addition to the actual file
/// ("src/foo/bar.rs"). Passing those directory paths to restore_paths causes it
/// to try to write raw git tree object bytes to a directory path on disk, which
/// fails and aborts the entire undo operation.
#[tokio::test]
async fn test_diff_nested_returns_only_files() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let nested_dir = tmpdir.path().join("src/foo");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::write(nested_dir.join("bar.rs"), "initial").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

    fs::write(nested_dir.join("bar.rs"), "modified").unwrap();
    let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

    let diff = backend
        .diff(tmpdir.path(), &snapshot1, &snapshot2)
        .await
        .unwrap();

    // Must only contain the file path, not intermediate directories "src" or "src/foo"
    assert_eq!(
        diff.len(),
        1,
        "diff should return exactly one entry (the file), got: {:?}",
        diff
    );
    assert_eq!(
        diff[0],
        PathBuf::from("src/foo/bar.rs"),
        "diff should identify src/foo/bar.rs"
    );
}

/// Regression: restore_paths must not error when directory paths are included.
///
/// Even after the fix to diff(), external callers could pass directory paths.
/// The defensive check in checkout_paths must skip them gracefully so that
/// actual file paths in the same batch are still restored.
#[tokio::test]
async fn test_restore_paths_with_directory_path_does_not_error() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();

    let src_dir = tmpdir.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("main.rs"), "original").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let snapshot = backend.track(tmpdir.path()).await.unwrap();

    fs::write(src_dir.join("main.rs"), "modified").unwrap();

    // Pass both the directory path "src" AND the file path – previously this
    // caused restore_paths to error on "src" before reaching "src/main.rs".
    backend
        .restore_paths(
            tmpdir.path(),
            &snapshot,
            &[PathBuf::from("src"), PathBuf::from("src/main.rs")],
        )
        .await
        .unwrap();

    assert_eq!(
        fs::read_to_string(src_dir.join("main.rs")).unwrap(),
        "original",
        "file should be restored even when directory paths are included in the list"
    );
}

// ==================== Stat cache, streaming, size limit ====================

/// Backdate a file's mtime so it is outside the "racily clean" window and
/// therefore eligible for the stat cache.
fn backdate(path: &Path) {
    let old = SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

fn track_stats(backend: &GitSnapshotBackend, worktree: &Path) -> SnapshotStats {
    backend.create_snapshot(worktree, "snapshot").unwrap().1
}

#[test]
fn unchanged_file_is_not_read_again() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let a = tmpdir.path().join("a.txt");
    let b = tmpdir.path().join("b.txt");
    fs::write(&a, "aaa").unwrap();
    fs::write(&b, "bbb").unwrap();
    backdate(&a);
    backdate(&b);

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let first = track_stats(&backend, tmpdir.path());
    assert_eq!((first.hashed_files, first.cached_files), (2, 0));
    assert_eq!(first.bytes_hashed, 6);

    let second = track_stats(&backend, tmpdir.path());
    assert_eq!(second.files, 2);
    assert_eq!(
        second.cached_files, 2,
        "unchanged files must not be re-read"
    );
    assert_eq!(second.hashed_files, 0);
    assert_eq!(second.bytes_hashed, 0);
}

#[test]
fn test_stat_cache_rehashes_modified_file() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let a = tmpdir.path().join("a.txt");
    let b = tmpdir.path().join("b.txt");
    fs::write(&a, "aaa").unwrap();
    fs::write(&b, "bbb").unwrap();
    backdate(&a);
    backdate(&b);

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let (id1, _) = backend.create_snapshot(tmpdir.path(), "s").unwrap();

    // Same length, new mtime: must be detected via mtime.
    fs::write(&a, "AAA").unwrap();
    let (id2, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
    assert_eq!(stats.hashed_files, 1);
    assert_eq!(stats.cached_files, 1);

    let repo = gix::open(
        backend
            .snapshot_dir(tmpdir.path())
            .expect("snapshot dir resolves"),
    )
    .unwrap();
    let changed = GitSnapshotBackend::diff_commits(&repo, &id1, &id2).unwrap();
    assert_eq!(changed, vec![PathBuf::from("a.txt")]);
}

#[test]
fn test_racily_clean_files_are_not_cached() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    // Freshly written: mtime is inside the racy window.
    fs::write(tmpdir.path().join("a.txt"), "aaa").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    track_stats(&backend, tmpdir.path());
    let second = track_stats(&backend, tmpdir.path());
    assert_eq!(second.cached_files, 0);
    assert_eq!(second.hashed_files, 1);
}

#[test]
fn test_corrupt_or_stale_stat_cache_is_harmless() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let a = tmpdir.path().join("a.txt");
    fs::write(&a, "aaa").unwrap();
    backdate(&a);

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    track_stats(&backend, tmpdir.path());

    let cache_path = backend
        .snapshot_dir(tmpdir.path())
        .unwrap()
        .join(STAT_CACHE_FILE);
    assert!(cache_path.exists(), "cache must be persisted");

    fs::write(&cache_path, b"{ not json").unwrap();
    let stats = track_stats(&backend, tmpdir.path());
    assert_eq!(stats.hashed_files, 1, "corrupt cache falls back to hashing");
    assert_eq!(stats.files, 1);
}

#[test]
fn test_cache_entry_with_missing_object_is_rehashed() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let a = tmpdir.path().join("a.txt");
    fs::write(&a, "aaa").unwrap();
    backdate(&a);

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    track_stats(&backend, tmpdir.path());

    // Point the cache at an object that does not exist.
    let cache_path = backend
        .snapshot_dir(tmpdir.path())
        .unwrap()
        .join(STAT_CACHE_FILE);
    let mut cache: StatCache = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    for entry in cache.entries.values_mut() {
        entry.oid = "1".repeat(40);
    }
    fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();

    let stats = track_stats(&backend, tmpdir.path());
    assert_eq!(stats.hashed_files, 1);
    assert_eq!(stats.cached_files, 0);
}

#[test]
fn dot_entries_are_not_captured() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    fs::create_dir_all(tmpdir.path().join(".venv/lib")).unwrap();
    fs::write(tmpdir.path().join(".venv/lib/x.py"), "x").unwrap();
    fs::write(tmpdir.path().join(".hidden"), "h").unwrap();
    fs::create_dir_all(tmpdir.path().join("src/.cache")).unwrap();
    fs::write(tmpdir.path().join("src/.cache/y"), "y").unwrap();
    fs::write(tmpdir.path().join("src/lib.rs"), "fn main() {}").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let stats = track_stats(&backend, tmpdir.path());
    assert_eq!(stats.files, 1, "only src/lib.rs belongs in the snapshot");
}

#[test]
fn large_file_round_trips_through_restore() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let size = (Limits::default().stream_bytes + 1024 * 1024) as usize;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let big = tmpdir.path().join("big.bin");
    fs::write(&big, &data).unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let (id, stats) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
    assert_eq!(stats.bytes_hashed, size as u64);

    // Overwrite, then restore from the streamed blob.
    fs::write(&big, b"clobbered").unwrap();
    let repo = gix::open(backend.snapshot_dir(tmpdir.path()).unwrap()).unwrap();
    let restored = backend
        .checkout_paths(&repo, tmpdir.path(), &id, &[PathBuf::from("big.bin")])
        .unwrap();
    assert_eq!(restored.streamed_files, 1);
    assert!(
        fs::read(&big).unwrap() == data,
        "streamed blob must round-trip byte for byte"
    );
}

#[test]
fn test_streamed_blob_matches_in_memory_hash() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let repo = backend.open_or_init(tmpdir.path()).unwrap();

    let size = Limits::default().stream_bytes + 17;
    let path = tmpdir.path().join("blob.bin");
    fs::write(&path, vec![42u8; size as usize]).unwrap();

    let listed = ListedFile {
        size,
        stamp: FileStamp::from_metadata(&fs::metadata(&path).unwrap()),
    };
    let CaptureOutcome::Stored {
        oid: streamed,
        bytes,
    } = GitSnapshotBackend::write_file_blob(&repo, &path, listed, Limits::default().stream_bytes)
        .unwrap()
    else {
        panic!("file is stable");
    };
    let expected = gix::objs::compute_hash(
        repo.object_hash(),
        gix::objs::Kind::Blob,
        &fs::read(&path).unwrap(),
    )
    .unwrap();
    assert_eq!(streamed, expected);
    assert_eq!(bytes, size, "bytes is what was read");
    assert!(repo.has_object(streamed));
}

// ==================== Review fixes ====================

fn repo_for(backend: &GitSnapshotBackend, worktree: &Path) -> gix::Repository {
    gix::open(backend.snapshot_dir(worktree).unwrap()).unwrap()
}

/// Rewriting a file with the same size and mtime (`cp -p`, `touch -r`) is
/// still noticed because ctime / inode change.
#[cfg(unix)]
#[test]
fn rewrite_with_same_size_and_mtime_is_detected() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let f = tmpdir.path().join("a.txt");
    fs::write(&f, "aaa").unwrap();
    backdate(&f);
    let mtime = fs::metadata(&f).unwrap().modified().unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    let (id1, first) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
    assert_eq!(first.hashed_files, 1);

    // Same length, mtime restored to the exact previous value.
    // Sleep so the ctime of the rewrite differs on coarse-grained filesystems.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&f, "bbb").unwrap();
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    let (id2, second) = backend.create_snapshot(tmpdir.path(), "s").unwrap();
    assert_eq!(
        second.hashed_files, 1,
        "ctime change must invalidate the cache"
    );
    let repo = repo_for(&backend, tmpdir.path());
    assert_eq!(
        GitSnapshotBackend::diff_commits(&repo, &id1, &id2).unwrap(),
        vec![PathBuf::from("a.txt")]
    );
}

#[test]
fn test_unchanged_cache_is_not_rewritten() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    let a = tmpdir.path().join("a.txt");
    fs::write(&a, "aaa").unwrap();
    backdate(&a);

    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    track_stats(&backend, tmpdir.path());
    let cache_path = backend
        .snapshot_dir(tmpdir.path())
        .unwrap()
        .join(STAT_CACHE_FILE);
    let before = fs::metadata(&cache_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    track_stats(&backend, tmpdir.path());
    let after = fs::metadata(&cache_path).unwrap().modified().unwrap();
    assert_eq!(
        before, after,
        "cache file must not be rewritten when unchanged"
    );
}

#[test]
fn test_concurrent_snapshots_leave_valid_cache_and_no_temp_files() {
    let tmpdir = TempDir::new().unwrap();
    let tmpbase = TempDir::new().unwrap();
    for i in 0..20 {
        let f = tmpdir.path().join(format!("f{i}.txt"));
        fs::write(&f, format!("{i}")).unwrap();
        backdate(&f);
    }
    let backend = GitSnapshotBackend::new().with_snapshot_base(tmpbase.path());
    track_stats(&backend, tmpdir.path());
    let git_dir = backend.snapshot_dir(tmpdir.path()).unwrap();
    // Force rewrites: invalidate the cache so every run saves it.
    let repo = repo_for(&backend, tmpdir.path());
    let cache = GitSnapshotBackend::load_stat_cache(&repo);
    assert_eq!(cache.entries.len(), 20);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let git_dir = git_dir.clone();
        handles.push(std::thread::spawn(move || {
            let repo = gix::open(&git_dir).unwrap();
            for _ in 0..25 {
                GitSnapshotBackend::save_stat_cache(&repo, &StatCache::empty());
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let leftovers = fs::read_dir(&git_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0, "no temp files may be left behind");
    // `load_stat_cache` falls back to an empty cache on corruption, so parse
    // the persisted file directly.
    let bytes = fs::read(git_dir.join(STAT_CACHE_FILE)).unwrap();
    let persisted: StatCache =
        serde_json::from_slice(&bytes).expect("concurrent saves leave valid cache JSON");
    assert_eq!(persisted.version, STAT_CACHE_VERSION);
}

// ==================== Scenario tests (workspace-snapshots) ====================

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::test_utils::noise;

/// Shared in-memory sink for captured tracing output.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `f` and return its result with the WARN-and-above tracing output.
fn capture_warnings<T>(f: impl FnOnce() -> T) -> (T, String) {
    let buffer = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    (out, text)
}

/// Every file path stored in snapshot `id`.
fn snapshot_files(repo: &gix::Repository, id: &str) -> BTreeSet<String> {
    let commit = repo
        .find_commit(gix::ObjectId::from_hex(id.as_bytes()).unwrap())
        .unwrap();
    let tree = commit.tree().unwrap();
    repo.diff_tree_to_tree(None, Some(&tree), Some(gix::diff::Options::default()))
        .unwrap()
        .iter()
        .filter(|c| !c.entry_mode().is_tree())
        .map(|c| String::from_utf8(c.location().to_vec()).unwrap())
        .collect()
}

fn blob_id(repo: &gix::Repository, id: &str, path: &str) -> gix::ObjectId {
    let commit = repo
        .find_commit(gix::ObjectId::from_hex(id.as_bytes()).unwrap())
        .unwrap();
    commit
        .tree()
        .unwrap()
        .lookup_entry_by_path(path)
        .unwrap()
        .expect("path is in the snapshot")
        .object_id()
}

/// Relative paths of every file below `dir`.
fn files_below(dir: &Path) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                out.insert(path.strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    out
}

fn small_limits() -> Limits {
    Limits {
        stream_bytes: 1024,
        ..Limits::default()
    }
}

// ---- backend configuration ----

#[tokio::test]
async fn trait_methods_use_the_configured_snapshot_base() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let file = work.path().join("a.txt");
    fs::write(&file, "one").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());

    let first = backend.track(work.path()).await.unwrap();
    fs::write(&file, "two").unwrap();
    let second = backend.track(work.path()).await.unwrap();
    let changed = backend.diff(work.path(), &first, &second).await.unwrap();
    assert_eq!(changed, vec![PathBuf::from("a.txt")]);
    backend
        .restore_paths(work.path(), &first, &changed)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "one");
    backend
        .gc(
            work.path(),
            &GcConfig {
                max_snapshots: NonZeroUsize::new(1),
                max_age_days: None,
            },
        )
        .await
        .unwrap();

    assert!(
        !base.path().read_dir().unwrap().next().is_none(),
        "the configured base holds the repository"
    );
    let default_dir = GitSnapshotBackend::new().snapshot_dir(work.path()).unwrap();
    assert!(
        !default_dir.exists(),
        "nothing may be written under the default cache dir: {}",
        default_dir.display()
    );
}

// ---- capture completeness ----

#[test]
fn file_that_shrinks_during_capture_is_captured() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(small_limits());
    let repo = backend.open_or_init(work.path()).unwrap();
    let path = work.path().join("f.bin");
    let data = noise(4096, 1);
    fs::write(&path, &data).unwrap();

    // The walk listed the file as longer than it is at read time.
    let mut stamp = FileStamp::from_metadata(&fs::metadata(&path).unwrap()).unwrap();
    stamp.size += 512;
    let captured = backend
        .capture_file(
            &repo,
            &path,
            ListedFile {
                size: stamp.size,
                stamp: Some(stamp),
            },
        )
        .unwrap();

    assert!(captured.copied, "a racing file is captured from a copy");
    assert_eq!(captured.bytes, 4096);
    assert_eq!(repo.find_object(captured.oid).unwrap().data, data);
    let leftovers = fs::read_dir(repo.git_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(COPY_DIR_PREFIX))
        .count();
    assert_eq!(leftovers, 0, "the copy directory must be removed");
}

#[test]
fn file_that_grows_during_capture_is_captured_whole() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(small_limits());
    let repo = backend.open_or_init(work.path()).unwrap();
    let path = work.path().join("f.bin");
    let data = noise(4096, 2);
    fs::write(&path, &data).unwrap();

    // The walk listed the file as shorter than it is at read time.
    let mut stamp = FileStamp::from_metadata(&fs::metadata(&path).unwrap()).unwrap();
    stamp.size = 2048;
    let captured = backend
        .capture_file(
            &repo,
            &path,
            ListedFile {
                size: 2048,
                stamp: Some(stamp),
            },
        )
        .unwrap();

    assert!(captured.copied);
    assert_eq!(
        repo.find_object(captured.oid).unwrap().data,
        data,
        "every byte is captured, not only the listed length"
    );
    let leftovers = fs::read_dir(repo.git_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(COPY_DIR_PREFIX))
        .count();
    assert_eq!(leftovers, 0);
}

// ---- snapshot scope ----

#[tokio::test]
async fn gitignored_file_in_a_git_repository_is_not_captured() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    run_git(work.path(), &["init", "-q"]);
    fs::write(work.path().join(".gitignore"), "model.bin\n").unwrap();
    fs::write(work.path().join("model.bin"), "weights").unwrap();
    fs::write(work.path().join("main.rs"), "fn main() {}").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    let files = snapshot_files(&repo_for(&backend, work.path()), &id);
    assert_eq!(files, BTreeSet::from(["main.rs".to_string()]));
}

#[tokio::test]
async fn gitignore_applies_outside_a_git_repository() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    assert!(!work.path().join(".git").exists());
    fs::write(work.path().join(".gitignore"), "data/\n").unwrap();
    fs::create_dir_all(work.path().join("data/deep")).unwrap();
    fs::write(work.path().join("data/deep/blob"), "x").unwrap();
    fs::write(work.path().join("keep.txt"), "k").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    let files = snapshot_files(&repo_for(&backend, work.path()), &id);
    assert_eq!(files, BTreeSet::from(["keep.txt".to_string()]));
}

#[tokio::test]
async fn ignore_file_excludes_paths() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join(".ignore"), "*.gguf\n").unwrap();
    fs::write(work.path().join("llama.gguf"), "w").unwrap();
    fs::write(work.path().join("notes.md"), "n").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    let files = snapshot_files(&repo_for(&backend, work.path()), &id);
    assert_eq!(files, BTreeSet::from(["notes.md".to_string()]));
}

#[tokio::test]
async fn git_info_exclude_is_honoured() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    run_git(work.path(), &["init", "-q"]);
    fs::create_dir_all(work.path().join(".git/info")).unwrap();
    fs::write(work.path().join(".git/info/exclude"), "scratch.txt\n").unwrap();
    fs::write(work.path().join("scratch.txt"), "s").unwrap();
    fs::write(work.path().join("real.txt"), "r").unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    let files = snapshot_files(&repo_for(&backend, work.path()), &id);
    assert_eq!(files, BTreeSet::from(["real.txt".to_string()]));
}

// ---- reuse ----

#[tokio::test]
async fn unchanged_workspace_returns_the_previous_snapshot_id() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "hello").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());

    let first = backend.track(work.path()).await.unwrap();
    let second = backend.track(work.path()).await.unwrap();

    assert_eq!(first, second);
    let commits = GitSnapshotBackend::list_commits(&repo_for(&backend, work.path())).unwrap();
    assert_eq!(commits.len(), 1, "no new commit may be recorded");
}

#[tokio::test]
async fn changed_workspace_returns_a_new_snapshot_id() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "hello").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());

    let first = backend.track(work.path()).await.unwrap();
    fs::write(work.path().join("a.txt"), "changed").unwrap();
    let second = backend.track(work.path()).await.unwrap();

    assert_ne!(first, second);
    let commits = GitSnapshotBackend::list_commits(&repo_for(&backend, work.path())).unwrap();
    assert_eq!(commits.len(), 2);
}

// ---- large-file reporting ----

#[test]
fn large_file_warnings_names_at_most_ten() {
    let files: Vec<(String, u64)> = (0..12).map(|i| (format!("f{i}"), 100)).collect();
    let (named, remaining) = large_file_warnings(&files);
    assert_eq!(named.len(), 10);
    assert_eq!(remaining, 2);
    let (named, remaining) = large_file_warnings(&files[..3]);
    assert_eq!((named.len(), remaining), (3, 0));
}

#[test]
fn large_file_entering_snapshots_is_reported_once() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(Limits {
            large_warn_bytes: 1024,
            ..Limits::default()
        });
    let big = work.path().join("huge.bin");
    fs::write(&big, noise(2048, 3)).unwrap();

    let ((), logs) = capture_warnings(|| {
        backend.create_snapshot(work.path(), "s").unwrap();
        fs::write(&big, noise(2048, 4)).unwrap();
        backend.create_snapshot(work.path(), "s").unwrap();
    });

    let warnings: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("large file entering undo snapshots"))
        .collect();
    assert_eq!(warnings.len(), 1, "logs:\n{logs}");
    assert!(warnings[0].contains("huge.bin"), "{}", warnings[0]);
    assert!(warnings[0].contains(".ignore"), "{}", warnings[0]);
}

#[test]
fn many_large_files_are_summarised() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(Limits {
            large_warn_bytes: 1024,
            ..Limits::default()
        });
    for i in 0..12 {
        fs::write(
            work.path().join(format!("big{i:02}.bin")),
            noise(2048, i + 10),
        )
        .unwrap();
    }

    let ((), logs) = capture_warnings(|| {
        backend.create_snapshot(work.path(), "s").unwrap();
    });

    let named = logs
        .lines()
        .filter(|l| l.contains("large file entering undo snapshots"))
        .count();
    let summary: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("more large files entering undo snapshots"))
        .collect();
    assert_eq!(named, 10, "logs:\n{logs}");
    assert_eq!(summary.len(), 1, "logs:\n{logs}");
    assert!(summary[0].contains("count=2"), "{}", summary[0]);
}

// ---- storage ----

#[tokio::test]
async fn large_file_is_stored_uncompressed_and_readable_by_git() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let size = 17 * 1024 * 1024;
    let data = noise(size, 5);
    fs::write(work.path().join("big.bin"), &data).unwrap();

    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    let repo = repo_for(&backend, work.path());
    let blob = blob_id(&repo, &id, "big.bin");

    let hex = blob.to_hex().to_string();
    let loose = repo
        .git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    assert!(
        fs::metadata(&loose).unwrap().len() >= size as u64,
        "the object must be stored without compression"
    );
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo.git_dir())
        .args(["cat-file", "blob", &hex])
        .output()
        .expect("git must be available");
    assert!(out.status.success());
    assert!(out.stdout == data, "git must read the exact content");
}

#[cfg(unix)]
#[test]
fn streamed_restore_rejects_a_corrupt_object() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(small_limits());
    let target = work.path().join("big.bin");
    fs::write(&target, noise(4096, 6)).unwrap();
    let (id, _) = backend.create_snapshot(work.path(), "s").unwrap();
    let repo = repo_for(&backend, work.path());

    let hex = blob_id(&repo, &id, "big.bin").to_hex().to_string();
    let loose = repo
        .git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
    let mut bytes = fs::read(&loose).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    fs::write(&loose, bytes).unwrap();

    fs::write(&target, b"current").unwrap();
    let result = backend.checkout_paths(&repo, work.path(), &id, &[PathBuf::from("big.bin")]);
    assert!(result.is_err(), "a corrupt object must fail the restore");
    assert_eq!(fs::read(&target).unwrap(), b"current");
    let leftovers: Vec<_> = files_below(work.path()).into_iter().collect();
    assert_eq!(leftovers, vec![PathBuf::from("big.bin")], "no temp file");
}

// ---- moves ----

#[tokio::test]
async fn diff_of_a_move_reports_both_paths() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "some content that is renamed").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let before = backend.track(work.path()).await.unwrap();
    fs::rename(work.path().join("a.txt"), work.path().join("b.txt")).unwrap();
    let after = backend.track(work.path()).await.unwrap();

    let changed = backend.diff(work.path(), &before, &after).await.unwrap();
    assert_eq!(
        changed,
        vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")]
    );
}

// ---- atomic restore ----

#[cfg(unix)]
#[tokio::test]
async fn executable_bit_follows_the_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let script = work.path().join("run.sh");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&script, "changed").unwrap();
    backend
        .restore_paths(work.path(), &id, &[PathBuf::from("run.sh")])
        .await
        .unwrap();

    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "mode is {mode:o}");
    assert_eq!(fs::read_to_string(&script).unwrap(), "#!/bin/sh\n");
}

#[cfg(unix)]
#[tokio::test]
async fn restore_keeps_a_private_file_private() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let secret = work.path().join("secret.txt");
    fs::write(&secret, "v1").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    fs::write(&secret, "v2").unwrap();
    backend
        .restore_paths(work.path(), &id, &[PathBuf::from("secret.txt")])
        .await
        .unwrap();

    assert_eq!(fs::read_to_string(&secret).unwrap(), "v1");
    let mode = fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode is {mode:o}");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_restore_leaves_no_temporary_files() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "file").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    // The path is now a non-empty directory, so the rename must fail.
    fs::remove_file(work.path().join("a.txt")).unwrap();
    fs::create_dir(work.path().join("a.txt")).unwrap();
    fs::write(work.path().join("a.txt/inner"), "x").unwrap();
    let before = files_below(work.path());
    let entries_before: BTreeSet<_> = fs::read_dir(work.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();

    let error = backend
        .restore_paths(work.path(), &id, &[PathBuf::from("a.txt")])
        .await
        .unwrap_err();

    assert!(error.to_string().contains("a.txt"), "{error}");
    let entries_after: BTreeSet<_> = fs::read_dir(work.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(entries_before, entries_after);
    assert_eq!(before, files_below(work.path()));
}

#[cfg(unix)]
#[tokio::test]
async fn full_restore_reports_a_write_failure() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "file").unwrap();
    fs::write(work.path().join("b.txt"), "other").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    fs::remove_file(work.path().join("b.txt")).unwrap();
    fs::create_dir(work.path().join("b.txt")).unwrap();
    fs::write(work.path().join("b.txt/inner"), "x").unwrap();

    let error = backend.restore(work.path(), &id).await.unwrap_err();
    assert!(error.to_string().contains("b.txt"), "{error}");
}

// ---- locking ----

#[test]
fn exclusive_lock_is_refused_while_shared_is_held() {
    let dir = TempDir::new().unwrap();
    let shared = SnapshotLock::shared(dir.path()).unwrap();
    assert!(
        SnapshotLock::try_exclusive(dir.path()).unwrap().is_none(),
        "exclusive must be refused while a shared lock is held"
    );
    drop(shared);
    assert!(SnapshotLock::try_exclusive(dir.path()).unwrap().is_some());
}

// ---- garbage collection ----

/// `count` snapshots; every one changes `f.txt` first, because unchanged
/// snapshots are reused.
async fn make_snapshots(backend: &GitSnapshotBackend, work: &Path, count: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..count {
        fs::write(work.join("f.txt"), format!("version {i}")).unwrap();
        ids.push(backend.track(work).await.unwrap());
    }
    ids
}

fn keep_last(n: usize) -> GcConfig {
    GcConfig {
        max_snapshots: NonZeroUsize::new(n),
        max_age_days: None,
    }
}

#[tokio::test]
async fn garbage_collection_without_limits_deletes_nothing() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    make_snapshots(&backend, work.path(), 15).await;
    let objects = repo_for(&backend, work.path()).git_dir().join("objects");
    let before = files_below(&objects);

    let result = backend.gc(work.path(), &GcConfig::default()).await.unwrap();

    assert_eq!(result.removed_count, 0);
    assert_eq!(result.remaining_count, 15);
    assert_eq!(result.deleted_objects, 0);
    assert_eq!(before, files_below(&objects));
}

#[tokio::test]
async fn snapshots_beyond_max_snapshots_are_pruned() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    make_snapshots(&backend, work.path(), 15).await;

    let result = backend.gc(work.path(), &keep_last(3)).await.unwrap();

    assert_eq!(result.removed_count, 12);
    assert_eq!(result.remaining_count, 3);
    let commits = GitSnapshotBackend::list_commits(&repo_for(&backend, work.path())).unwrap();
    assert_eq!(
        commits.len(),
        3,
        "the pruned commits are gone from the chain"
    );
}

#[tokio::test]
async fn snapshots_older_than_max_age_days_are_pruned() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    make_snapshots(&backend, work.path(), 3).await;
    let repo = repo_for(&backend, work.path());
    let cfg = GcConfig {
        max_snapshots: None,
        max_age_days: NonZeroU64::new(30),
    };

    let later = SystemTime::now() + std::time::Duration::from_secs(31 * 86_400);
    let result = collect_garbage(&repo, &cfg, later).unwrap();

    assert_eq!(result.remaining_count, 1, "only the most recent remains");
    assert_eq!(result.removed_count, 2);
    let within = collect_garbage(&repo, &cfg, SystemTime::now()).unwrap();
    assert_eq!(within.removed_count, 0, "young snapshots are kept");
}

#[tokio::test]
async fn latest_snapshot_is_kept_regardless_of_age() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    make_snapshots(&backend, work.path(), 1).await;
    let repo = repo_for(&backend, work.path());
    let cfg = GcConfig {
        max_snapshots: None,
        max_age_days: NonZeroU64::new(30),
    };

    let later = SystemTime::now() + std::time::Duration::from_secs(400 * 86_400);
    let result = collect_garbage(&repo, &cfg, later).unwrap();

    assert_eq!((result.removed_count, result.remaining_count), (0, 1));
}

#[tokio::test]
async fn pruned_snapshot_content_is_deleted() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let ids = make_snapshots(&backend, work.path(), 15).await;
    let repo = repo_for(&backend, work.path());
    let pruned_blobs: Vec<_> = ids[..12]
        .iter()
        .map(|id| blob_id(&repo, id, "f.txt"))
        .collect();
    let objects = repo.git_dir().join("objects");
    let size_before = dir_size(&objects);

    let result = backend.gc(work.path(), &keep_last(3)).await.unwrap();

    assert!(result.deleted_objects > 0);
    let fresh = repo_for(&backend, work.path());
    for blob in pruned_blobs {
        assert!(!fresh.has_object(blob), "{blob} must be deleted");
    }
    assert!(dir_size(&objects) < size_before, "the store must shrink");
}

#[tokio::test]
async fn retained_snapshot_content_survives_pruning() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let mut expected = Vec::new();
    let mut ids = Vec::new();
    for i in 0..15 {
        let a = format!("a version {i}");
        let b = format!("b version {}", i / 2);
        fs::write(work.path().join("a.txt"), &a).unwrap();
        fs::create_dir_all(work.path().join("sub")).unwrap();
        fs::write(work.path().join("sub/b.txt"), &b).unwrap();
        ids.push(backend.track(work.path()).await.unwrap());
        expected.push((a, b));
    }

    let result = backend.gc(work.path(), &keep_last(3)).await.unwrap();
    assert!(result.deleted_objects > 0);

    for (id, (a, b)) in ids.iter().zip(&expected).skip(12) {
        fs::write(work.path().join("a.txt"), "clobbered").unwrap();
        fs::write(work.path().join("sub/b.txt"), "clobbered").unwrap();
        backend.restore(work.path(), id).await.unwrap();
        assert_eq!(&fs::read_to_string(work.path().join("a.txt")).unwrap(), a);
        assert_eq!(
            &fs::read_to_string(work.path().join("sub/b.txt")).unwrap(),
            b
        );
    }
}

#[tokio::test]
async fn garbage_collection_skips_while_a_snapshot_is_in_progress() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    make_snapshots(&backend, work.path(), 15).await;
    let repo = repo_for(&backend, work.path());
    let objects = repo.git_dir().join("objects");
    let before = files_below(&objects);

    let held = SnapshotLock::shared(repo.git_dir()).unwrap();
    let result = backend.gc(work.path(), &keep_last(3)).await.unwrap();
    assert_eq!(result.deleted_objects, 0);
    assert_eq!(before, files_below(&objects), "nothing may be deleted");

    fs::write(work.path().join("f.txt"), "one more").unwrap();
    backend
        .track(work.path())
        .await
        .expect("a snapshot completes while the lock is held");
    drop(held);
}

#[test]
fn snapshot_committed_before_the_sweep_lock_survives() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    for i in 0..15 {
        fs::write(work.path().join("f.txt"), format!("version {i}")).unwrap();
        backend.create_snapshot(work.path(), "s").unwrap();
    }
    let repo = repo_for(&backend, work.path());

    // Another session commits a snapshot after the unlocked estimate but
    // before the sweep takes its exclusive lock.
    let mut newest = None;
    let result = collect_garbage_with(&repo, &keep_last(3), SystemTime::now(), || {
        fs::write(work.path().join("f.txt"), "committed during gc").unwrap();
        newest = Some(backend.create_snapshot(work.path(), "s").unwrap().0);
    })
    .unwrap();

    let newest = newest.expect("the hook ran");
    assert!(result.deleted_objects > 0, "the sweep must have run");
    assert_eq!(result.remaining_count, 3);
    let fresh = repo_for(&backend, work.path());
    let commits = GitSnapshotBackend::list_commits(&fresh).unwrap();
    assert_eq!(commits[0].0, newest, "HEAD is the new snapshot");
    fs::write(work.path().join("f.txt"), "clobbered").unwrap();
    backend.checkout_all(&fresh, work.path(), &newest).unwrap();
    assert_eq!(
        fs::read_to_string(work.path().join("f.txt")).unwrap(),
        "committed during gc"
    );
    fs::write(work.path().join("f.txt"), "one more").unwrap();
    backend
        .create_snapshot(work.path(), "s")
        .expect("tracking still works after the sweep");
}

// ---- store-size gauge ----

fn gauge_limits() -> Limits {
    Limits {
        store_warn_bytes: 4 * 1024,
        store_measure_every_bytes: 1024,
        ..Limits::default()
    }
}

#[test]
fn large_snapshot_store_is_reported_once() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(gauge_limits());
    fs::write(work.path().join("blob.bin"), noise(8 * 1024, 7)).unwrap();

    let ((), logs) = capture_warnings(|| {
        backend.create_snapshot(work.path(), "s").unwrap();
        for seed in 8..10 {
            fs::write(work.path().join("blob.bin"), noise(8 * 1024, seed)).unwrap();
            backend.create_snapshot(work.path(), "s").unwrap();
        }
    });

    let warnings: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("snapshot store is large"))
        .collect();
    assert_eq!(warnings.len(), 1, "logs:\n{logs}");
    let git_dir = backend.snapshot_dir(work.path()).unwrap();
    assert!(
        warnings[0].contains(&git_dir.display().to_string()),
        "{}",
        warnings[0]
    );
    assert!(warnings[0].contains("size_bytes="), "{}", warnings[0]);
    assert!(warnings[0].contains(".ignore"), "{}", warnings[0]);
}

#[test]
fn small_snapshot_store_is_not_reported() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(gauge_limits());
    fs::write(work.path().join("small.txt"), "tiny").unwrap();

    let ((), logs) = capture_warnings(|| {
        backend.create_snapshot(work.path(), "s").unwrap();
    });

    assert!(!logs.contains("snapshot store is large"), "logs:\n{logs}");
}

// ---- restore hardening ----

#[tokio::test]
async fn restore_rejects_paths_that_escape_the_worktree() {
    let parent = TempDir::new().unwrap();
    let work = parent.path().join("work");
    fs::create_dir(&work).unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.join("a.txt"), "inside").unwrap();
    let outside = parent.path().join("victim.txt");
    fs::write(&outside, "must survive").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(&work).await.unwrap();

    for bad in ["../victim.txt", "/etc/hostname", "", "sub/../../victim.txt"] {
        let error = backend
            .restore_paths(&work, &id, &[PathBuf::from(bad)])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("refusing"), "{bad:?}: {error}");
    }
    assert_eq!(fs::read_to_string(&outside).unwrap(), "must survive");
}

#[cfg(unix)]
#[tokio::test]
async fn restore_does_not_keep_setuid_bits() {
    use std::os::unix::fs::PermissionsExt;
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let file = work.path().join("tool");
    fs::write(&file, "v1").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    fs::write(&file, "v2").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).unwrap();
    backend
        .restore_paths(work.path(), &id, &[PathBuf::from("tool")])
        .await
        .unwrap();

    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755, "mode is {mode:o}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
}

#[test]
fn restored_mode_never_has_special_bits() {
    #[cfg(unix)]
    for executable in [false, true] {
        let mode = super::restore::restored_mode(Some(0o7777), executable);
        assert_eq!(mode & 0o7000, 0, "{mode:o}");
    }
}

#[test]
fn capture_of_a_missing_file_is_reported_as_a_read_error() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let repo = backend.open_or_init(work.path()).unwrap();

    let error = backend
        .capture_file(
            &repo,
            &work.path().join("gone.txt"),
            ListedFile {
                size: 3,
                stamp: None,
            },
        )
        .unwrap_err();

    match error {
        SnapshotError::Io { op, source, .. } => {
            assert_eq!(op, "read");
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn stream_capture_reports_the_bytes_it_read() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(small_limits());
    let repo = backend.open_or_init(work.path()).unwrap();
    let path = work.path().join("grown.bin");
    fs::write(&path, noise(4096, 11)).unwrap();

    // The listing is stale: the file grew after it was listed.
    let captured = backend
        .capture_file(
            &repo,
            &path,
            ListedFile {
                size: 3000,
                stamp: None,
            },
        )
        .unwrap();

    assert_eq!(captured.bytes, 4096, "the real size, not the listed one");
}

#[cfg(unix)]
#[tokio::test]
async fn restore_does_not_follow_a_symlinked_parent_out_of_the_worktree() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(work.path().join("keep.txt"), "k").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    // A snapshot that lacks `sub/file.txt`: restoring it deletes that path.
    let without = backend.track(work.path()).await.unwrap();
    fs::create_dir(work.path().join("sub")).unwrap();
    fs::write(work.path().join("sub/file.txt"), "inside").unwrap();
    let with = backend.track(work.path()).await.unwrap();

    // The directory is swapped for a symlink after the snapshots.
    fs::remove_dir_all(work.path().join("sub")).unwrap();
    std::os::unix::fs::symlink(outside.path(), work.path().join("sub")).unwrap();
    fs::write(outside.path().join("file.txt"), "outside").unwrap();

    let paths = [PathBuf::from("sub/file.txt")];
    assert!(
        backend
            .restore_paths(work.path(), &with, &paths)
            .await
            .is_err()
    );
    assert!(
        backend
            .restore_paths(work.path(), &without, &paths)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("file.txt")).unwrap(),
        "outside",
        "neither write nor delete may escape through the symlink"
    );
}

#[tokio::test]
async fn full_restore_rewrites_nested_files_through_the_confined_root() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::create_dir(work.path().join("d")).unwrap();
    fs::write(work.path().join("d/a.txt"), "one").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    fs::write(work.path().join("d/a.txt"), "two").unwrap();

    backend.restore(work.path(), &id).await.unwrap();

    assert_eq!(
        fs::read_to_string(work.path().join("d/a.txt")).unwrap(),
        "one"
    );
}

#[tokio::test]
async fn restore_paths_rejects_parent_traversal() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "a").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    let result = backend
        .restore_paths(work.path(), &id, &[PathBuf::from("../escape.txt")])
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn uncaptured_paths_reports_files_outside_the_snapshot() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("kept.txt"), "k").unwrap();
    fs::write(work.path().join(".ignore"), "ignored.txt\n").unwrap();
    fs::write(work.path().join("ignored.txt"), "i").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();

    let uncaptured = backend
        .uncaptured_paths(
            work.path(),
            &id,
            &[
                PathBuf::from("kept.txt"),
                PathBuf::from("ignored.txt"),
                PathBuf::from("gone.txt"),
            ],
        )
        .await
        .unwrap();

    assert_eq!(uncaptured, vec![PathBuf::from("ignored.txt")]);
}

#[tokio::test]
async fn restore_batch_with_a_missing_snapshot_changes_no_files() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    fs::write(work.path().join("a.txt"), "one").unwrap();
    let backend = GitSnapshotBackend::new().with_snapshot_base(base.path());
    let id = backend.track(work.path()).await.unwrap();
    fs::write(work.path().join("a.txt"), "two").unwrap();

    let missing = "0".repeat(40);
    let result = backend
        .restore_batch(
            work.path(),
            &[
                (id, vec![PathBuf::from("a.txt")]),
                (missing, vec![PathBuf::from("a.txt")]),
            ],
        )
        .await;

    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(work.path().join("a.txt")).unwrap(),
        "two",
        "every snapshot is resolved before the first file changes"
    );
}

#[tokio::test]
async fn small_file_that_no_longer_matches_its_listing_is_captured_from_a_copy() {
    let work = TempDir::new().unwrap();
    let base = TempDir::new().unwrap();
    let backend = GitSnapshotBackend::new()
        .with_snapshot_base(base.path())
        .with_limits(small_limits());
    let repo = backend.open_or_init(work.path()).unwrap();
    let path = work.path().join("small.txt");
    fs::write(&path, "twenty bytes of data").unwrap();

    let captured = backend
        .capture_file(
            &repo,
            &path,
            ListedFile {
                size: 5,
                stamp: None,
            },
        )
        .unwrap();

    assert!(captured.copied, "a stale listing must not be trusted");
    assert_eq!(captured.bytes, 20);
}
