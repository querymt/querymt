//! Git-based snapshot backend using gitoxide (gix)
//!
//! This backend stores snapshots in a shadow git repository located at
//! `$HOME/.qmt/snapshots/<hash>/` where `<hash>` is derived from the
//! worktree path. This keeps the user's project directory untouched.

use super::backend::{
    GcConfig, GcResult, SnapshotBackend, SnapshotError, SnapshotId, SnapshotResult,
};
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
    fn snapshot_dir(&self, worktree: &Path) -> SnapshotResult<PathBuf> {
        let canonical = worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf());
        let hash = crate::hash::RapidHash::new(canonical.to_string_lossy().as_bytes());

        #[cfg(test)]
        if let Some(ref base) = self.snapshot_base_override {
            return Ok(base.join(format!("{:016x}", hash.as_u64())));
        }

        let cache_dir = dirs::cache_dir()
            .ok_or_else(|| SnapshotError::Filesystem("HOME directory must be set".to_string()))?;
        Ok(cache_dir
            .join("querymt")
            .join("snapshots")
            .join(format!("{:016x}", hash.as_u64())))
    }

    /// Initialize or open the snapshot git repository.
    ///
    /// We use a bare repo with `core.worktree` pointing to the project directory.
    /// This keeps the project directory pristine (no .git folder).
    fn open_or_init(&self, worktree: &Path) -> SnapshotResult<gix::Repository> {
        let git_dir = self.snapshot_dir(worktree)?;

        if git_dir.join("HEAD").exists() {
            // Open existing repository
            gix::open(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!("Failed to open snapshot repository: {}", e))
            })
        } else {
            // Initialize new bare repository
            fs::create_dir_all(&git_dir).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to create snapshot directory: {}", e))
            })?;

            gix::init_bare(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!(
                    "Failed to initialize snapshot repository: {}",
                    e
                ))
            })?;

            // Write worktree path as metadata file
            fs::write(
                git_dir.join("WORKTREE_PATH"),
                worktree.to_string_lossy().as_bytes(),
            )
            .map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to write worktree metadata: {}", e))
            })?;

            // Set core.worktree and user identity in git config
            let config_path = git_dir.join("config");
            let config_content = fs::read_to_string(&config_path).unwrap_or_else(|_| String::new());
            let extra = format!(
                "\n[core]\n\tworktree = {}\n[user]\n\tname = qmt-snapshot\n\temail = snapshot@qmt.local\n",
                worktree.display()
            );
            fs::write(&config_path, format!("{}{}", config_content, extra)).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to write git config: {}", e))
            })?;

            // Re-open the repo so it picks up our config changes
            gix::open(&git_dir).map_err(|e| {
                SnapshotError::Repository(format!(
                    "Failed to re-open snapshot repository after config: {}",
                    e
                ))
            })
        }
    }

    /// Build a tree object from a sorted list of (relative_path, blob_oid, executable) entries.
    ///
    /// This constructs a `gix_object::Tree` with proper `Entry` objects and writes it to the ODB.
    fn build_tree_from_entries(
        repo: &gix::Repository,
        entries: &[(String, gix::ObjectId, bool)],
    ) -> SnapshotResult<gix::ObjectId> {
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

        let tree_id = repo.write_object(&tree).map_err(|e| {
            SnapshotError::Repository(format!("Failed to write tree object: {}", e))
        })?;

        Ok(tree_id.detach())
    }

    /// Stage all files from the worktree and create a snapshot commit.
    ///
    /// This builds a tree from the worktree contents by:
    /// 1. Walking the worktree (respecting .gitignore)
    /// 2. Writing each file as a blob
    /// 3. Building a tree from those blobs
    /// 4. Creating a commit pointing to that tree
    fn create_snapshot(&self, worktree: &Path, message: &str) -> SnapshotResult<SnapshotId> {
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
            let entry = entry.map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to walk directory: {}", e))
            })?;
            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                continue;
            }

            let path = entry.path();
            let rel_path = path.strip_prefix(worktree).map_err(|e| {
                SnapshotError::Filesystem(format!("Failed to compute relative path: {}", e))
            })?;

            // Skip hidden directories like .git
            let rel_str = rel_path.to_string_lossy();
            if rel_str.starts_with('.') || rel_str.contains("/.") {
                continue;
            }

            // Read file and create blob
            let content = fs::read(path)
                .map_err(|e| SnapshotError::Filesystem(format!("Failed to read file: {}", e)))?;
            let oid = repo
                .write_blob(&content)
                .map_err(|e| SnapshotError::Repository(format!("Failed to write blob: {}", e)))?
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
            .map_err(|e| SnapshotError::Repository(format!("Failed to create commit: {}", e)))?;

        Ok(commit_id.detach().to_string())
    }

    /// Get diff between two commits by comparing their trees using `diff_tree_to_tree`
    fn diff_commits(repo: &gix::Repository, from: &str, to: &str) -> SnapshotResult<Vec<PathBuf>> {
        let from_id = gix::ObjectId::from_hex(from.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(from.to_string()))?;
        let to_id = gix::ObjectId::from_hex(to.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(to.to_string()))?;

        let from_commit = repo
            .find_commit(from_id)
            .map_err(|e| SnapshotError::NotFound(format!("'from' commit {}: {}", from, e)))?;
        let to_commit = repo
            .find_commit(to_id)
            .map_err(|e| SnapshotError::NotFound(format!("'to' commit {}: {}", to, e)))?;

        let from_tree = from_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'from' tree: {}", e)))?;
        let to_tree = to_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'to' tree: {}", e)))?;

        // Use Repository::diff_tree_to_tree for convenience
        let changes = repo
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)
            .map_err(|e| {
                SnapshotError::Repository(format!("Failed to compute tree diff: {}", e))
            })?;

        let mut changed_files = Vec::new();
        for change in &changes {
            // Skip tree (directory) entries – we only care about file-level changes.
            // gix's diff_tree_to_tree emits tree modifications for every ancestor
            // directory whose hash changed; passing those paths to `restore_paths`
            // would cause it to try to write a raw git tree object to a directory
            // path on disk, which errors and aborts the entire restore.
            if change.entry_mode().is_tree() {
                continue;
            }
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
    ) -> SnapshotResult<()> {
        let commit_id = gix::ObjectId::from_hex(commit_sha.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(commit_sha.to_string()))?;
        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| SnapshotError::NotFound(format!("commit {}: {}", commit_sha, e)))?;
        let tree = commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {}", e)))?;

        for path in paths {
            let entry = tree.lookup_entry_by_path(path).map_err(|e| {
                SnapshotError::Repository(format!("Failed to look up tree entry: {}", e))
            })?;

            if let Some(entry) = entry {
                // Defense-in-depth: skip tree (directory) entries.
                // `diff_commits` already filters these out, but callers may pass
                // directory paths directly; trying to write a tree object to a
                // directory path on disk would fail and abort the whole restore.
                if entry.mode().is_tree() {
                    continue;
                }
                let object = entry.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to get blob object: {}", e))
                })?;
                let data = object.data.clone();
                let full_path = worktree.join(path);

                // Create parent directories if needed
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        SnapshotError::Filesystem(format!(
                            "Failed to create parent directory: {}",
                            e
                        ))
                    })?;
                }

                fs::write(&full_path, &data).map_err(|e| {
                    SnapshotError::Filesystem(format!("Failed to write file: {}", e))
                })?;
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
    fn checkout_all(
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
    ) -> SnapshotResult<()> {
        let commit_id = gix::ObjectId::from_hex(commit_sha.as_bytes())
            .map_err(|_| SnapshotError::InvalidSnapshotId(commit_sha.to_string()))?;
        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| SnapshotError::NotFound(format!("commit {}: {}", commit_sha, e)))?;
        let tree = commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {}", e)))?;

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
    ) -> SnapshotResult<()> {
        for entry_result in tree.iter() {
            let entry_ref = entry_result.map_err(|e| {
                SnapshotError::Repository(format!("Failed to read tree entry: {}", e))
            })?;
            let name = std::str::from_utf8(entry_ref.filename().as_ref()).map_err(|e| {
                SnapshotError::Repository(format!("Invalid UTF-8 in filename: {}", e))
            })?;
            let entry_path = prefix.join(name);

            if entry_ref.mode().is_tree() {
                // Recurse into subtree
                let sub_object = entry_ref.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to find subtree object: {}", e))
                })?;
                let sub_tree = sub_object
                    .try_into_tree()
                    .map_err(|_| SnapshotError::Repository("Expected tree object".to_string()))?;
                Self::restore_tree_recursive(repo, worktree, &sub_tree, &entry_path)?;
            } else if entry_ref.mode().is_blob() || entry_ref.mode().is_blob_or_symlink() {
                // Restore blob
                let object = entry_ref.object().map_err(|e| {
                    SnapshotError::Repository(format!("Failed to find blob object: {}", e))
                })?;
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
    fn list_commits(repo: &gix::Repository) -> SnapshotResult<Vec<(String, i64)>> {
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

    async fn track(&self, worktree: &Path) -> SnapshotResult<SnapshotId> {
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
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn diff(
        &self,
        worktree: &Path,
        pre: &SnapshotId,
        post: &SnapshotId,
    ) -> SnapshotResult<Vec<PathBuf>> {
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
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore_paths(
        &self,
        worktree: &Path,
        snapshot: &SnapshotId,
        paths: &[PathBuf],
    ) -> SnapshotResult<()> {
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
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> SnapshotResult<()> {
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
        .map_err(|_| SnapshotError::TaskPanicked)?
    }

    async fn gc(&self, worktree: &Path, config: &GcConfig) -> SnapshotResult<GcResult> {
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
        .map_err(|_| SnapshotError::TaskPanicked)?
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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

        let backend = GitSnapshotBackend::with_snapshot_base(tmpbase.path().to_path_buf());
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
}
