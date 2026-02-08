//! Git-based snapshot backend using gitoxide (gix)
//!
//! This backend stores snapshots in a shadow git repository located at
//! `$HOME/.qmt/snapshots/<hash>/` where `<hash>` is derived from the
//! worktree path. This keeps the user's project directory untouched.

use super::backend::{GcConfig, GcResult, SnapshotBackend, SnapshotId};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Git-based snapshot backend using gitoxide (pure Rust, no git CLI dependency)
pub struct GitSnapshotBackend {
    #[cfg(test)]
    snapshot_base_override: Option<PathBuf>,
}

impl GitSnapshotBackend {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            snapshot_base_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_snapshot_base(base: PathBuf) -> Self {
        Self {
            snapshot_base_override: Some(base),
        }
    }

    /// Compute the snapshot repository directory for a given worktree
    fn snapshot_dir(&self, worktree: &Path) -> Result<PathBuf> {
        let canonical = worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf());
        let hash = crate::hash::RapidHash::new(canonical.to_string_lossy().as_bytes());

        #[cfg(test)]
        if let Some(ref base) = self.snapshot_base_override {
            return Ok(base.join(format!("{:016x}", hash.as_u64())));
        }

        let cache_dir = dirs::cache_dir().context("HOME directory must be set")?;
        Ok(cache_dir
            .join("querymt")
            .join("snapshots")
            .join(format!("{:016x}", hash.as_u64())))
    }

    /// Initialize or open the snapshot git repository.
    ///
    /// We use a bare repo with `core.worktree` pointing to the project directory.
    /// This keeps the project directory pristine (no .git folder).
    fn open_or_init(&self, worktree: &Path) -> Result<gix::Repository> {
        let git_dir = self.snapshot_dir(worktree)?;

        if git_dir.join("HEAD").exists() {
            // Open existing repository
            let repo = gix::open(&git_dir).context("Failed to open snapshot repository")?;
            Ok(repo)
        } else {
            // Initialize new bare repository
            fs::create_dir_all(&git_dir).context("Failed to create snapshot directory")?;

            let _repo =
                gix::init_bare(&git_dir).context("Failed to initialize snapshot repository")?;

            // Write worktree path as metadata file
            fs::write(
                git_dir.join("WORKTREE_PATH"),
                worktree.to_string_lossy().as_bytes(),
            )
            .context("Failed to write worktree metadata")?;

            // Set core.worktree and user identity in git config
            let config_path = git_dir.join("config");
            let config_content = fs::read_to_string(&config_path).unwrap_or_else(|_| String::new());
            let extra = format!(
                "\n[core]\n\tworktree = {}\n[user]\n\tname = qmt-snapshot\n\temail = snapshot@qmt.local\n",
                worktree.display()
            );
            fs::write(&config_path, format!("{}{}", config_content, extra))
                .context("Failed to write git config")?;

            // Re-open the repo so it picks up our config changes
            let repo = gix::open(&git_dir)
                .context("Failed to re-open snapshot repository after config")?;

            Ok(repo)
        }
    }

    /// Build a tree object from a sorted list of (relative_path, blob_oid, executable) entries.
    ///
    /// This constructs a `gix_object::Tree` with proper `Entry` objects and writes it to the ODB.
    fn build_tree_from_entries(
        repo: &gix::Repository,
        entries: &[(String, gix::ObjectId, bool)],
    ) -> Result<gix::ObjectId> {
        // Group entries by top-level directory component
        let mut blobs: Vec<(String, gix::ObjectId, bool)> = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<(String, gix::ObjectId, bool)>> = BTreeMap::new();

        for (path, oid, exec) in entries {
            if let Some(slash_pos) = path.find('/') {
                let dir_name = &path[..slash_pos];
                let rest = &path[slash_pos + 1..];
                subdirs.entry(dir_name.to_string()).or_default().push((
                    rest.to_string(),
                    *oid,
                    *exec,
                ));
            } else {
                blobs.push((path.clone(), *oid, *exec));
            }
        }

        // Build gix_object::Tree entries
        let mut tree_entries: Vec<gix::objs::tree::Entry> = Vec::new();

        // Add subdirectory trees (recurse)
        for (dir_name, sub_entries) in &subdirs {
            let sub_tree_id = Self::build_tree_from_entries(repo, sub_entries)?;
            tree_entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: dir_name.as_str().into(),
                oid: sub_tree_id,
            });
        }

        // Add blob entries
        for (name, oid, exec) in &blobs {
            let mode = if *exec {
                gix::objs::tree::EntryKind::BlobExecutable.into()
            } else {
                gix::objs::tree::EntryKind::Blob.into()
            };
            tree_entries.push(gix::objs::tree::Entry {
                mode,
                filename: name.as_str().into(),
                oid: *oid,
            });
        }

        // Sort entries (git requires specific ordering for trees)
        tree_entries.sort();

        let tree = gix::objs::Tree {
            entries: tree_entries,
        };

        let tree_id = repo
            .write_object(&tree)
            .context("Failed to write tree object")?;

        Ok(tree_id.detach())
    }

    /// Stage all files from the worktree and create a snapshot commit.
    ///
    /// This builds a tree from the worktree contents by:
    /// 1. Walking the worktree (respecting .gitignore)
    /// 2. Writing each file as a blob
    /// 3. Building a tree from those blobs
    /// 4. Creating a commit pointing to that tree
    fn create_snapshot(&self, worktree: &Path, message: &str) -> Result<SnapshotId> {
        let repo = self.open_or_init(worktree)?;

        // Collect all files from the worktree
        let mut entries: Vec<(String, gix::ObjectId, bool)> = Vec::new();

        for entry in ignore::WalkBuilder::new(worktree)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .build()
        {
            let entry = entry.context("Failed to walk directory")?;
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }

            let path = entry.path();
            let rel_path = path
                .strip_prefix(worktree)
                .context("Failed to compute relative path")?;

            // Skip hidden directories like .git
            let rel_str = rel_path.to_string_lossy();
            if rel_str.starts_with('.') || rel_str.contains("/.") {
                continue;
            }

            // Read file and create blob
            let content = fs::read(path).context("Failed to read file")?;
            let oid = repo
                .write_blob(&content)
                .context("Failed to write blob")?
                .detach();

            // Check if executable
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                let meta = fs::metadata(path).ok();
                meta.map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            };
            #[cfg(not(unix))]
            let executable = false;

            entries.push((rel_str.to_string(), oid, executable));
        }

        // Sort entries for deterministic tree creation
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        // Build tree from entries
        let tree_id = Self::build_tree_from_entries(&repo, &entries)?;

        // Get HEAD as parent (if exists)
        let parent_ids: Vec<gix::ObjectId> = match repo.head_commit() {
            Ok(commit) => vec![commit.id().detach()],
            Err(_) => vec![], // Empty repo, no parents
        };

        // Create commit using repo.commit() which reads author/committer from config
        // (we set user.name and user.email in the git config during init)
        let commit_id = repo
            .commit("HEAD", message, tree_id, parent_ids)
            .context("Failed to create commit")?;

        Ok(commit_id.detach().to_string())
    }

    /// Get diff between two commits by comparing their trees using `diff_tree_to_tree`
    fn diff_commits(repo: &gix::Repository, from: &str, to: &str) -> Result<Vec<PathBuf>> {
        let from_id =
            gix::ObjectId::from_hex(from.as_bytes()).context("Invalid 'from' commit ID")?;
        let to_id = gix::ObjectId::from_hex(to.as_bytes()).context("Invalid 'to' commit ID")?;

        let from_commit = repo
            .find_commit(from_id)
            .context("Failed to find 'from' commit")?;
        let to_commit = repo
            .find_commit(to_id)
            .context("Failed to find 'to' commit")?;

        let from_tree = from_commit.tree().context("Failed to get 'from' tree")?;
        let to_tree = to_commit.tree().context("Failed to get 'to' tree")?;

        // Use Repository::diff_tree_to_tree for convenience
        let changes = repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)
            .context("Failed to compute tree diff")?;

        let mut changed_files = Vec::new();
        for change in &changes {
            let location = change.location();
            if let Ok(path_str) = std::str::from_utf8(location.as_ref()) {
                changed_files.push(PathBuf::from(path_str));
            }
        }

        Ok(changed_files)
    }

    /// Restore files from a commit to the worktree
    fn checkout_paths(
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
        paths: &[PathBuf],
    ) -> Result<()> {
        let commit_id =
            gix::ObjectId::from_hex(commit_sha.as_bytes()).context("Invalid commit ID")?;
        let commit = repo
            .find_commit(commit_id)
            .context("Failed to find commit")?;
        let tree = commit.tree().context("Failed to get tree")?;

        for path in paths {
            let entry = tree
                .lookup_entry_by_path(path)
                .context("Failed to look up tree entry")?;

            if let Some(entry) = entry {
                let object = entry.object().context("Failed to get blob object")?;
                let data = object.data.clone();
                let full_path = worktree.join(path);

                // Create parent directories if needed
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).context("Failed to create parent directory")?;
                }

                fs::write(&full_path, &data).context("Failed to write file")?;
            } else {
                // File doesn't exist in this snapshot - remove it if it exists
                let full_path = worktree.join(path);
                if full_path.exists() {
                    let _ = fs::remove_file(&full_path);
                }
            }
        }

        Ok(())
    }

    /// Restore entire worktree from a commit
    fn checkout_all(repo: &gix::Repository, worktree: &Path, commit_sha: &str) -> Result<()> {
        let commit_id =
            gix::ObjectId::from_hex(commit_sha.as_bytes()).context("Invalid commit ID")?;
        let commit = repo
            .find_commit(commit_id)
            .context("Failed to find commit")?;
        let tree = commit.tree().context("Failed to get tree")?;

        // Collect all files from the tree recursively
        Self::restore_tree_recursive(repo, worktree, &tree, &PathBuf::new())?;

        Ok(())
    }

    /// Recursively restore a tree to the worktree
    #[allow(clippy::only_used_in_recursion)]
    fn restore_tree_recursive(
        repo: &gix::Repository,
        worktree: &Path,
        tree: &gix::Tree<'_>,
        prefix: &Path,
    ) -> Result<()> {
        for entry_result in tree.iter() {
            let entry_ref = entry_result.context("Failed to read tree entry")?;
            let name = std::str::from_utf8(entry_ref.filename().as_ref())
                .context("Invalid UTF-8 in filename")?;
            let entry_path = prefix.join(name);

            if entry_ref.mode().is_tree() {
                // Recurse into subtree
                let sub_object = entry_ref
                    .object()
                    .context("Failed to find subtree object")?;
                let sub_tree = sub_object
                    .try_into_tree()
                    .map_err(|_| anyhow!("Expected tree object"))?;
                Self::restore_tree_recursive(repo, worktree, &sub_tree, &entry_path)?;
            } else if entry_ref.mode().is_blob() || entry_ref.mode().is_blob_or_symlink() {
                // Restore blob
                let object = entry_ref.object().context("Failed to find blob object")?;
                let full_path = worktree.join(&entry_path);

                if let Some(parent) = full_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&full_path, &object.data);
            }
        }

        Ok(())
    }

    /// List all commits with timestamps by walking from HEAD
    fn list_commits(repo: &gix::Repository) -> Result<Vec<(String, i64)>> {
        let head = match repo.head_commit() {
            Ok(commit) => commit,
            Err(_) => return Ok(Vec::new()), // Empty repo
        };

        let mut commits = Vec::new();
        let mut current = Some(head.id().detach());

        while let Some(oid) = current {
            match repo.find_commit(oid) {
                Ok(commit) => {
                    let timestamp = commit.time().map(|t| t.seconds).unwrap_or(0);
                    commits.push((oid.to_string(), timestamp));

                    // Get first parent
                    current = commit.parent_ids().next().map(|id| id.detach());
                }
                Err(_) => break,
            }
        }

        Ok(commits)
    }
}

#[async_trait]
impl SnapshotBackend for GitSnapshotBackend {
    fn is_available(&self, worktree: &Path) -> bool {
        worktree.exists() && worktree.is_dir() && dirs::home_dir().is_some()
    }

    async fn track(&self, worktree: &Path) -> Result<SnapshotId> {
        let worktree = worktree.to_path_buf();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            backend.create_snapshot(&worktree, "snapshot")
        })
        .await
        .context("Task panicked")?
    }

    async fn diff(
        &self,
        worktree: &Path,
        pre: &SnapshotId,
        post: &SnapshotId,
    ) -> Result<Vec<PathBuf>> {
        let worktree = worktree.to_path_buf();
        let pre = pre.clone();
        let post = post.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::diff_commits(&repo, &pre, &post)
        })
        .await
        .context("Task panicked")?
    }

    async fn restore_paths(
        &self,
        worktree: &Path,
        snapshot: &SnapshotId,
        paths: &[PathBuf],
    ) -> Result<()> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        let paths = paths.to_vec();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::checkout_paths(&repo, &worktree, &snapshot, &paths)
        })
        .await
        .context("Task panicked")?
    }

    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> Result<()> {
        let worktree = worktree.to_path_buf();
        let snapshot = snapshot.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            Self::checkout_all(&repo, &worktree, &snapshot)
        })
        .await
        .context("Task panicked")?
    }

    async fn gc(&self, worktree: &Path, config: &GcConfig) -> Result<GcResult> {
        let worktree = worktree.to_path_buf();
        let config = config.clone();
        #[cfg(test)]
        let snapshot_base = self.snapshot_base_override.clone();

        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let backend = if let Some(base) = snapshot_base {
                GitSnapshotBackend::with_snapshot_base(base)
            } else {
                GitSnapshotBackend::new()
            };
            #[cfg(not(test))]
            let backend = GitSnapshotBackend::new();

            let repo = backend.open_or_init(&worktree)?;
            let commits = Self::list_commits(&repo)?;

            let total_count = commits.len();
            let now = OffsetDateTime::now_utc().unix_timestamp();

            // Filter by age
            let mut kept: Vec<_> = if let Some(max_age_days) = config.max_age_days {
                let cutoff = now - (max_age_days as i64 * 86400);
                commits
                    .into_iter()
                    .filter(|(_, timestamp)| *timestamp >= cutoff)
                    .collect()
            } else {
                commits
            };

            // Limit by count (keep most recent)
            if let Some(max_snapshots) = config.max_snapshots {
                kept.truncate(max_snapshots);
            }

            let removed = total_count.saturating_sub(kept.len());

            Ok(GcResult {
                removed_count: removed,
                remaining_count: kept.len(),
            })
        })
        .await
        .context("Task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_snapshot_dir_deterministic() {
        let tmpbase = TempDir::new().unwrap();
        let path1 = PathBuf::from("/tmp/test-project");
        let path2 = PathBuf::from("/tmp/test-project");

        let backend1 = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let backend2 = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

        let dir1 = backend1.snapshot_dir(&path1).unwrap();
        let dir2 = backend2.snapshot_dir(&path2).unwrap();

        assert_eq!(dir1, dir2, "Same path should produce same snapshot dir");
    }

    #[tokio::test]
    async fn test_track_creates_snapshot() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

    #[tokio::test]
    async fn test_gc_by_count() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

        // Create 5 snapshots
        for i in 0..5 {
            fs::write(tmpdir.path().join("test.txt"), format!("v{}", i)).unwrap();
            backend.track(tmpdir.path()).await.unwrap();
        }

        let config = GcConfig {
            max_snapshots: Some(3),
            max_age_days: None,
        };

        let result = backend.gc(tmpdir.path(), &config).await.unwrap();
        assert_eq!(result.remaining_count, 3);
        assert_eq!(result.removed_count, 2);
    }

    // ==================== Test Suite 1.1: Track Idempotency & Determinism ====================

    #[tokio::test]
    async fn test_track_idempotent_no_change() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        fs::write(tmpdir.path().join("test.txt"), "hello").unwrap();

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
        let snapshot1 = backend.track(tmpdir.path()).await.unwrap();

        // Note: Due to git timestamps, snapshot IDs will differ even with same content.
        // Instead, verify that both snapshots succeed and detect no differences.
        let snapshot2 = backend.track(tmpdir.path()).await.unwrap();

        // The snapshots will have different commit hashes due to timestamps,
        // but diff should show no changes
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
    async fn test_track_changes_after_modify() {
        let tmpdir = TempDir::new().unwrap();
        let tmpbase = TempDir::new().unwrap();
        let file_path = tmpdir.path().join("test.txt");

        fs::write(&file_path, "initial").unwrap();
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());

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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
}
