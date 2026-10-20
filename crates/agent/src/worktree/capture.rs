//! Private-index baseline capture of a checkout's working state.
//!
//! Captures the *actual working contents* of a checkout — tracked working
//! files, tracked deletions, tracked hidden/ignored patterns, and
//! non-ignored untracked files — into a Git tree without touching the
//! source's HEAD, branch, index, stash, or files. This is repository state
//! materialization for isolated workspaces (design decision 4), not a
//! replacement for undo snapshots.
//!
//! Mechanics: a private index is seeded from HEAD and then reconciled
//! with the working tree (native gix index/object primitives — the
//! equivalent of `git add -A` against a throwaway index), so file modes,
//! symlinks, binary contents, path exclusions (.gitignore), and deletions
//! are handled with Git semantics. The staging partition is deliberately
//! *not* propagated: a partially staged file contributes its working
//! content, and the real index stays byte-for-byte unchanged. Ignored
//! untracked files are excluded and reported for disclosure.
//!
//! When the captured tree equals HEAD's tree, HEAD is reused as the
//! baseline (no private commit). Otherwise a private commit parented by
//! HEAD is created with an explicit internal identity, which requires no
//! user commit identity and never runs commit hooks. Ref creation for the
//! baseline lives with the caller (section 3.3).

use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::repo_ops::{CapturedWorkingState, RepositoryOps};
use std::path::Path;

/// Result of capturing a checkout's working state.
///
/// Re-exported from the typed operations layer; fields and semantics are
/// identical.
pub use crate::worktree::repo_ops::CapturedWorkingState as CapturedState;

/// Author/committer identity for QueryMT's private commits. Private
/// commits never require the user's commit identity and never invoke user
/// commit hooks.
pub const INTERNAL_COMMIT_NAME: &str = "QueryMT";
pub const INTERNAL_COMMIT_EMAIL: &str = "querymt@localhost";

/// Bounded retry policy for validated capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRetryPolicy {
    /// Total attempts (first capture + retries). Minimum 1.
    pub max_attempts: u32,
}

impl Default for CaptureRetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3 }
    }
}

/// Validate-then-capture-then-validate with bounded retry.
///
/// The source manifest (HEAD, index entries and working tree) is captured before and after
/// each capture attempt. A mismatch means an external writer changed the
/// source mid-capture — external editors do not honor QueryMT's cooperative
/// gate — so the capture is retried against fresh state, and after the
/// bounded attempts it fails explicitly instead of launching child
/// workspaces from a known-inconsistent baseline. This is not a
/// transactional snapshot against arbitrary hostile writers: a write that
/// lands after the post-manifest is undetectable; the documented limitation
/// is surfaced through capture diagnostics instead (design decision 4).
pub async fn capture_working_state_validated(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
    policy: CaptureRetryPolicy,
) -> WorktreeResult<CapturedWorkingState> {
    let attempts = policy.max_attempts.max(1);
    let mut last_diagnostic = String::new();
    for attempt in 0..attempts {
        let before = source_manifest(ops, checkout_root).await?;
        let captured = capture_working_state(ops, checkout_root).await?;
        let after = source_manifest(ops, checkout_root).await?;
        if before == after && captured.source_head == before.0 && captured.tree == before.1 {
            return Ok(captured);
        }
        last_diagnostic = format!(
            "source changed during capture (attempt {attempt}/{attempts}): HEAD or the observed working state was concurrently modified"
        );
    }
    Err(WorktreeError::InvalidOperation(format!(
        "{last_diagnostic}; refusing to launch from a known-inconsistent baseline"
    )))
}

/// Content manifest of the source: HEAD, private working tree and real index entries.
async fn source_manifest(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
) -> WorktreeResult<(String, String, Vec<u8>)> {
    // A private tree records contents, modes, symlink targets and untracked
    // files. Status labels cannot detect edits to an already-dirty path.
    let state = capture_working_state(ops, checkout_root).await?;
    let index = ops.index_manifest(checkout_root).await?;
    Ok((state.source_head, state.tree, index))
}

/// Capture the working state of `checkout_root` into a private tree and
/// commit.
///
/// Supported layouts are ordinary non-bare repositories and linked
/// worktrees with a resolvable HEAD. Unresolved index conflicts, unborn
/// HEAD, sparse checkouts, submodules, and LFS-managed content are
/// rejected explicitly *before* capture rather than silently producing an
/// incomplete baseline.
pub async fn capture_working_state(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
) -> WorktreeResult<CapturedWorkingState> {
    validate_supported_layout(ops, checkout_root).await?;
    ops.capture_working_state(checkout_root).await
}

/// Reject source layouts this first version cannot materialize faithfully.
///
/// Every rejection happens before any child workspaces launch; the error
/// names the unsupported state (spec: unsupported states fail explicitly
/// rather than omitting content silently).
async fn validate_supported_layout(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
) -> WorktreeResult<()> {
    // Unborn HEAD has nothing to seed from or parent by.
    if ops.head_commit(checkout_root).await?.is_none() {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: checkout has no commits (unborn HEAD); \
             create an initial commit first"
                .to_string(),
        ));
    }

    // Unresolved merge conflicts cannot be represented faithfully: conflict
    // markers on disk would be captured as ordinary content.
    if ops.has_unmerged_entries(checkout_root).await? {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: source has unresolved index conflicts; \
             resolve them before requesting isolation"
                .to_string(),
        ));
    }

    // Sparse checkouts: tracked paths absent from disk would be captured as
    // deletions.
    if ops.sparse_checkout_enabled(checkout_root).await? {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: sparse checkouts are unsupported in this version".to_string(),
        ));
    }

    // Submodules and externally filtered content (Git LFS, custom
    // clean/smudge processes) are rejected inside the native capture itself,
    // from HEAD/index gitlink entries and *effective* Git attributes
    // (nested `.gitattributes`, Git-directory attributes, macros) — not
    // from a `.gitmodules` presence check or a root-file text scan.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::repo_ops::{CapturedWorkingState, GixRepoOps, RepositoryOps};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::TempDir;

    // -- CLI fixtures (direct `git`, not the adapter, for setup ease) -------

    fn run_git(repo: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .expect("spawn git")
            .success()
    }

    fn run_git_ok(repo: &Path, args: &[&str]) {
        assert!(run_git(repo, args), "git {args:?} failed");
    }

    fn out_git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        run_git_ok(tmp.path(), &["init", "-q"]);
        tmp
    }

    fn commit_all(repo: &Path, message: &str) {
        run_git_ok(repo, &["add", "-A"]);
        run_git_ok(repo, &["commit", "-q", "-m", message]);
    }

    fn head(repo: &Path) -> String {
        out_git(repo, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn tree_paths(repo: &Path, tree: &str) -> Vec<(String, String)> {
        // (path, mode) listing of a tree. `ls-tree` separates metadata and
        // path with a single tab, so split on that (not whitespace — paths
        // may contain spaces).
        let out = out_git(repo, &["ls-tree", "-r", tree]);
        out.lines()
            .filter_map(|line| {
                let (meta, path) = line.split_once('\t')?;
                let mode = meta.split_whitespace().next()?.to_string();
                Some((path.to_string(), mode))
            })
            .collect()
    }

    fn blob_of(repo: &Path, tree: &str, path: &str) -> String {
        out_git(repo, &["rev-parse", &format!("{tree}:{path}")])
            .trim()
            .to_string()
    }

    fn blob_of_oid(repo: &Path, oid: &str) -> Vec<u8> {
        Command::new("git")
            .args(["cat-file", "blob", oid])
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
            .stdout
    }

    async fn capture(root: &Path) -> WorktreeResult<CapturedState> {
        capture_working_state(&GixRepoOps::new(), root).await
    }

    // -- Clean and dirty captures ---------------------------------------------

    #[tokio::test]
    async fn clean_checkout_reuses_head_without_private_commit() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let captured = capture(repo.path()).await.unwrap();

        assert_eq!(captured.commit, head(repo.path()));
        assert!(!captured.created_commit);
        assert_eq!(
            captured.tree,
            out_git(repo.path(), &["rev-parse", "HEAD^{tree}"]).trim()
        );
        assert!(captured.ignored_files.is_empty());
    }

    #[tokio::test]
    async fn partial_staging_captures_working_content_and_leaves_index_unchanged() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "base").unwrap();
        commit_all(repo.path(), "base");

        // Stage one content, then change the working file to another.
        fs::write(repo.path().join("file.txt"), "staged").unwrap();
        run_git_ok(repo.path(), &["add", "file.txt"]);
        fs::write(repo.path().join("file.txt"), "working").unwrap();

        let index_before = out_git(repo.path(), &["ls-files", "-s"]);
        let status_before = out_git(repo.path(), &["status", "--porcelain"]);

        let captured = capture(repo.path()).await.unwrap();

        // The baseline carries the *working* content.
        let working_blob = out_git(repo.path(), &["hash-object", "file.txt"])
            .trim()
            .to_string();
        assert_eq!(
            blob_of(repo.path(), &captured.tree, "file.txt"),
            working_blob
        );
        assert!(captured.created_commit);
        // Parented by source HEAD.
        assert_eq!(
            out_git(
                repo.path(),
                &["rev-parse", &format!("{}^", captured.commit)]
            )
            .trim(),
            captured.source_head
        );

        // The real index and observed status are untouched.
        assert_eq!(out_git(repo.path(), &["ls-files", "-s"]), index_before);
        assert_eq!(
            out_git(repo.path(), &["status", "--porcelain"]),
            status_before
        );
        // And the staged blob is still the staged one (differs from working).
        assert_ne!(
            blob_of(repo.path(), "HEAD", "file.txt"),
            working_blob,
            "HEAD still holds the committed content, not the working copy"
        );
    }

    #[tokio::test]
    async fn tracked_deletions_are_reflected_in_the_baseline() {
        let repo = init_repo();
        fs::write(repo.path().join("gone.txt"), "x").unwrap();
        fs::write(repo.path().join("stay.txt"), "y").unwrap();
        commit_all(repo.path(), "init");
        fs::remove_file(repo.path().join("gone.txt")).unwrap();

        let captured = capture(repo.path()).await.unwrap();
        let paths: Vec<String> = tree_paths(repo.path(), &captured.tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(!paths.iter().any(|p| p == "gone.txt"));
        assert!(paths.iter().any(|p| p == "stay.txt"));
    }

    #[tokio::test]
    async fn untracked_files_are_included_and_ignored_files_excluded_and_disclosed() {
        let repo = init_repo();
        fs::write(repo.path().join(".gitignore"), "ign*\n").unwrap();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        fs::write(repo.path().join("ign.env"), "secret").unwrap();
        fs::write(repo.path().join("new.txt"), "untracked").unwrap();

        let captured = capture(repo.path()).await.unwrap();

        let paths: Vec<String> = tree_paths(repo.path(), &captured.tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(paths.iter().any(|p| p == "new.txt"), "paths: {paths:?}");
        assert!(!paths.iter().any(|p| p == "ign.env"));
        assert!(
            captured.ignored_files.iter().any(|p| p.contains("ign.env")),
            "ignored: {:?}",
            captured.ignored_files
        );
    }

    #[tokio::test]
    async fn tracked_ignored_patterns_and_dotfiles_are_still_captured() {
        let repo = init_repo();
        fs::write(repo.path().join(".gitignore"), "secret*\n").unwrap();
        fs::write(repo.path().join(".env.example"), "A=1").unwrap();
        commit_all(repo.path(), "init");
        // A tracked file that matches the ignore pattern (added explicitly).
        fs::write(repo.path().join("secret.key"), "key").unwrap();
        run_git_ok(repo.path(), &["add", "-f", "secret.key"]);
        commit_all(repo.path(), "track secret");
        // A non-ignored untracked dotfile.
        fs::write(repo.path().join(".hidden"), "dot").unwrap();

        let captured = capture(repo.path()).await.unwrap();
        let paths: Vec<String> = tree_paths(repo.path(), &captured.tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(paths.iter().any(|p| p == "secret.key"));
        assert!(paths.iter().any(|p| p == ".env.example"));
        assert!(paths.iter().any(|p| p == ".hidden"));
    }

    #[tokio::test]
    async fn executable_bits_symlinks_and_binary_contents_are_preserved() {
        let repo = init_repo();
        fs::write(repo.path().join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                repo.path().join("script.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        fs::write(repo.path().join("target.txt"), "pointed").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("target.txt", repo.path().join("link")).unwrap();
        let binary: Vec<u8> = vec![0x00, 0x01, 0xff, 0xfe, 0x80];
        fs::write(repo.path().join("blob.bin"), &binary).unwrap();
        commit_all(repo.path(), "assets");

        let captured = capture(repo.path()).await.unwrap();
        let entries = tree_paths(repo.path(), &captured.tree);

        #[cfg(unix)]
        {
            let mode_of = |path: &str| {
                entries
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, m)| m.clone())
                    .unwrap()
            };
            assert_eq!(mode_of("script.sh"), "100755");
            assert_eq!(mode_of("target.txt"), "100644");
            assert_eq!(mode_of("link"), "120000");
            let link_target =
                blob_of_oid(repo.path(), &blob_of(repo.path(), &captured.tree, "link"));
            assert_eq!(link_target, b"target.txt");
        }

        let bin_blob = blob_of_oid(
            repo.path(),
            &blob_of(repo.path(), &captured.tree, "blob.bin"),
        );
        assert_eq!(bin_blob, binary, "binary content must be byte-exact");
    }

    #[tokio::test]
    async fn capture_leaves_head_index_and_stash_unchanged() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        // A pre-existing stash entry must survive capture untouched.
        fs::write(repo.path().join("file.txt"), "stashed").unwrap();
        run_git_ok(repo.path(), &["stash", "push", "-q", "-m", "wip"]);
        fs::write(repo.path().join("file.txt"), "edited").unwrap();
        fs::write(repo.path().join("new.txt"), "untracked").unwrap();

        let head_before = head(repo.path());
        let index_before = out_git(repo.path(), &["ls-files", "-s"]);
        let status_before = out_git(repo.path(), &["status", "--porcelain"]);
        let stash_before = out_git(repo.path(), &["stash", "list"]);
        let stash_commit_before = out_git(repo.path(), &["rev-parse", "stash@{0}"])
            .trim()
            .to_string();

        let captured = capture(repo.path()).await.unwrap();

        assert_eq!(head(repo.path()), head_before);
        assert_eq!(out_git(repo.path(), &["ls-files", "-s"]), index_before);
        assert_eq!(
            out_git(repo.path(), &["status", "--porcelain"]),
            status_before
        );
        assert_eq!(out_git(repo.path(), &["stash", "list"]), stash_before);
        assert_eq!(
            out_git(repo.path(), &["rev-parse", "stash@{0}"]).trim(),
            stash_commit_before,
            "stash entry itself unchanged"
        );
        assert!(captured.created_commit);
    }

    // -- Unsupported layouts fail before capture ------------------------------

    #[tokio::test]
    async fn unborn_head_fails_explicitly() {
        let repo = init_repo();
        let err = capture(repo.path()).await.unwrap_err();
        assert!(err.to_string().contains("unborn HEAD"), "error: {err}");
    }

    #[tokio::test]
    async fn unresolved_conflicts_fail_explicitly() {
        let repo = init_repo();
        run_git_ok(repo.path(), &["checkout", "-q", "-b", "main"]);
        fs::write(repo.path().join("file.txt"), "base").unwrap();
        commit_all(repo.path(), "base");
        run_git_ok(repo.path(), &["checkout", "-q", "-b", "feature"]);
        fs::write(repo.path().join("file.txt"), "feature").unwrap();
        commit_all(repo.path(), "feature");
        run_git_ok(repo.path(), &["checkout", "-q", "main"]);
        fs::write(repo.path().join("file.txt"), "main").unwrap();
        commit_all(repo.path(), "main");
        // Merge conflict (expected failure).
        let _ = run_git(repo.path(), &["merge", "--no-edit", "feature"]);

        let err = capture(repo.path()).await.unwrap_err();
        assert!(err.to_string().contains("conflicts"), "error: {err}");
    }

    #[tokio::test]
    async fn submodules_fail_explicitly() {
        // Child repository to embed as a submodule.
        let child = init_repo();
        fs::write(child.path().join("c.txt"), "child").unwrap();
        commit_all(child.path(), "child init");

        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        let child_url = format!("file://{}", child.path().display());
        run_git_ok(
            repo.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child_url.as_str(),
                "child",
            ],
        );
        commit_all(repo.path(), "add submodule");

        let err = capture(repo.path()).await.unwrap_err();
        assert!(err.to_string().contains("submodules"), "error: {err}");
    }

    #[tokio::test]
    async fn sparse_checkouts_fail_explicitly() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::create_dir_all(repo.path().join("sub")).unwrap();
        fs::write(repo.path().join("sub/deep.txt"), "deep").unwrap();
        commit_all(repo.path(), "init");
        run_git_ok(repo.path(), &["sparse-checkout", "init", "--cone"]);

        let err = capture(repo.path()).await.unwrap_err();
        assert!(err.to_string().contains("sparse"), "error: {err}");
    }

    #[tokio::test]
    async fn lfs_managed_content_fails_explicitly() {
        let repo = init_repo();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        // LFS-managed content must be present: a filter configuration
        // without matching paths is not itself a blocker (effective
        // attribute resolution is path-based, not a config text scan).
        fs::write(repo.path().join("media.bin"), "lfs pointer").unwrap();
        commit_all(repo.path(), "init");

        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LFS"), "error: {msg}");
        assert!(msg.contains("media.bin"), "names the path: {msg}");
    }

    // -- Newly staged tracked files (F4) ---------------------------------------

    /// A file force-added to the real index is tracked for capture even
    /// though ignore rules would exclude it, subsequent working edits are
    /// captured, genuinely untracked ignored files stay excluded, and the
    /// real index remains byte-for-byte unchanged (13.1).
    #[tokio::test]
    async fn newly_staged_ignored_file_is_captured_and_untracked_ignored_excluded() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::write(repo.path().join(".gitignore"), "secret*\n").unwrap();
        commit_all(repo.path(), "init");

        fs::write(repo.path().join("secret-staged.txt"), "staged version").unwrap();
        run_git_ok(repo.path(), &["add", "-f", "secret-staged.txt"]);
        fs::write(repo.path().join("secret-untracked.txt"), "never captured").unwrap();

        let ops = GixRepoOps::new();
        let index_before = ops.index_manifest(repo.path()).await.unwrap();

        let captured = capture(repo.path()).await.unwrap();

        // The staged ignored file is captured with its working content.
        let staged_blob = out_git(repo.path(), &["hash-object", "secret-staged.txt"])
            .trim()
            .to_string();
        assert_eq!(
            blob_of(repo.path(), &captured.tree, "secret-staged.txt"),
            staged_blob
        );
        // The genuinely untracked ignored file is excluded and disclosed.
        assert!(
            tree_paths(repo.path(), &captured.tree)
                .iter()
                .all(|(p, _)| p != "secret-untracked.txt")
        );
        assert!(
            captured
                .ignored_files
                .iter()
                .any(|p| p.contains("secret-untracked.txt")),
            "ignored files are disclosed: {:?}",
            captured.ignored_files
        );

        // Subsequent working edits to the newly staged file are captured.
        fs::write(
            repo.path().join("secret-staged.txt"),
            "edited after staging",
        )
        .unwrap();
        let recaptured = capture(repo.path()).await.unwrap();
        let edited_blob = out_git(repo.path(), &["hash-object", "secret-staged.txt"])
            .trim()
            .to_string();
        assert_eq!(
            blob_of(repo.path(), &recaptured.tree, "secret-staged.txt"),
            edited_blob
        );

        // The real index stays byte-for-byte unchanged across captures.
        let index_after = ops.index_manifest(repo.path()).await.unwrap();
        assert_eq!(index_before, index_after);
    }

    // -- Repository-layout normalization (F5) ----------------------------------

    /// Active submodules are rejected from gitlink entries in the real index
    /// or the HEAD tree — no `.gitmodules` file required — with the path
    /// named in the diagnostic (13.3).
    #[tokio::test]
    async fn gitlinks_in_index_or_head_fail_even_without_gitmodules() {
        // A gitlink in the real index — no `.gitmodules` anywhere.
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        let sha = out_git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        run_git_ok(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},vendored"),
            ],
        );
        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("submodules"), "{msg}");
        assert!(msg.contains("vendored"), "diagnostic names the path: {msg}");

        // The same for a gitlink committed into HEAD.
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        let sha = out_git(repo.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_string();
        run_git_ok(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sha},vendored"),
            ],
        );
        run_git_ok(repo.path(), &["commit", "-m", "add gitlink"]);
        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("submodules") && msg.contains("vendored"),
            "{msg}"
        );
    }

    /// Capture normalizes to the resolved checkout root: a session whose
    /// cwd is a subdirectory still captures the repository-wide baseline.
    #[tokio::test]
    async fn subdirectory_sessions_capture_the_repository_wide_baseline() {
        let repo = init_repo();
        fs::write(repo.path().join("root.txt"), "root").unwrap();
        fs::create_dir_all(repo.path().join("crates/sub")).unwrap();
        fs::write(repo.path().join("crates/sub/lib.rs"), "sub").unwrap();
        commit_all(repo.path(), "init");

        let resolved =
            crate::worktree::identity::resolve_repository(&repo.path().join("crates/sub")).unwrap();
        let captured = capture(&resolved.checkout.root).await.unwrap();
        let captured_paths = tree_paths(repo.path(), &captured.tree);
        let paths: Vec<&str> = captured_paths.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"root.txt"), "{paths:?}");
        assert!(paths.contains(&"crates/sub/lib.rs"), "{paths:?}");
    }

    /// Root-level sparse-checkout configuration is detected no matter which
    /// subdirectory the session started in.
    #[tokio::test]
    async fn sparse_checkouts_fail_even_from_subdirectory() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::create_dir_all(repo.path().join("deep/sub")).unwrap();
        fs::write(repo.path().join("deep/sub/leaf.txt"), "leaf").unwrap();
        commit_all(repo.path(), "init");
        run_git_ok(repo.path(), &["config", "core.sparseCheckout", "true"]);

        let resolved =
            crate::worktree::identity::resolve_repository(&repo.path().join("deep/sub")).unwrap();
        let err = capture(&resolved.checkout.root).await.unwrap_err();
        assert!(err.to_string().contains("sparse"), "{err}");
    }

    // -- Effective attribute resolution (F5) -----------------------------------

    /// LFS is detected through *effective* attributes: a nested
    /// `.gitattributes` selects the filter, and the diagnostic names the
    /// affected path (13.4).
    #[tokio::test]
    async fn lfs_via_nested_gitattributes_fails_with_path_diagnostic() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::create_dir_all(repo.path().join("assets")).unwrap();
        fs::write(
            repo.path().join("assets/.gitattributes"),
            "*.psd filter=lfs\n",
        )
        .unwrap();
        fs::write(repo.path().join("assets/art.psd"), "fake lfs content").unwrap();
        commit_all(repo.path(), "init");

        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LFS"), "{msg}");
        assert!(msg.contains("assets/art.psd"), "names the path: {msg}");
    }

    /// Git-directory attributes (`$GIT_DIR/info/attributes`) participate in
    /// effective attribute resolution.
    #[tokio::test]
    async fn lfs_via_git_directory_attributes_fails_with_path_diagnostic() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::write(repo.path().join("media.bin"), "binary").unwrap();
        commit_all(repo.path(), "init");
        fs::create_dir_all(repo.path().join(".git/info")).unwrap();
        fs::write(
            repo.path().join(".git/info/attributes"),
            "*.bin filter=lfs\n",
        )
        .unwrap();

        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LFS") && msg.contains("media.bin"), "{msg}");
    }

    /// `[attr]` macros expand during effective attribute resolution; an LFS
    /// filter reached only through a macro is still rejected with a path
    /// diagnostic rather than accepted via a text scan miss.
    #[tokio::test]
    async fn lfs_via_attribute_macro_fails_with_path_diagnostic() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::write(repo.path().join("model.big"), "big").unwrap();
        commit_all(repo.path(), "init");
        fs::write(
            repo.path().join(".gitattributes"),
            "[attr]lfs-heavy filter=lfs\n*.big lfs-heavy\n",
        )
        .unwrap();

        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LFS") && msg.contains("model.big"), "{msg}");
    }

    /// Any external filter process (not just LFS) is rejected with the
    /// filter name and path before child execution.
    #[tokio::test]
    async fn custom_external_filter_fails_with_filter_name_and_path() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        fs::write(repo.path().join("arch.zip"), "zip").unwrap();
        commit_all(repo.path(), "init");
        fs::write(repo.path().join(".gitattributes"), "*.zip filter=zipper\n").unwrap();

        let err = capture(repo.path()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("external filter") && msg.contains("zipper") && msg.contains("arch.zip"),
            "{msg}"
        );
    }

    // -- Validated capture with changing candidate sets (F4/F5) ----------------

    /// An external writer staging a new tracked file mid-capture changes the
    /// candidate set; the validated capture detects the index change and
    /// retries against fresh state instead of launching from an inconsistent
    /// baseline (13.5).
    #[tokio::test]
    async fn externally_staged_new_file_mid_capture_triggers_bounded_retry() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let ops = FlakyWriterOps::stage_flaky_once();
        let captured =
            capture_working_state_validated(&ops, repo.path(), CaptureRetryPolicy::default())
                .await
                .unwrap();
        assert!(captured.created_commit, "the staged file changed the tree");
        let late_paths = tree_paths(repo.path(), &captured.tree);
        let paths: Vec<&str> = late_paths.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"late.txt"), "{paths:?}");
    }

    // -- Private commit identity and hooks ------------------------------------

    #[tokio::test]
    async fn capture_uses_internal_identity_without_user_commit_config() {
        let repo = init_repo();
        // No user.name/email configured in this fresh repository, and the
        // adapter strips the environment anyway — capture must not depend
        // on any user commit identity.
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        fs::write(repo.path().join("file.txt"), "dirty").unwrap();

        let captured = capture(repo.path()).await.unwrap();

        assert!(captured.created_commit);
        let author = out_git(
            repo.path(),
            &["log", "-1", "--format=%an <%ae>", &captured.commit],
        );
        assert_eq!(author.trim(), "QueryMT <querymt@localhost>");
        let committer = out_git(
            repo.path(),
            &["log", "-1", "--format=%cn <%ce>", &captured.commit],
        );
        assert_eq!(committer.trim(), "QueryMT <querymt@localhost>");
    }

    #[tokio::test]
    async fn capture_never_invokes_commit_hooks() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");
        fs::write(repo.path().join("file.txt"), "dirty").unwrap();

        // Hostile hooks that would fail any real commit and leave a marker.
        let hooks = repo.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let marker = repo.path().join("hook-ran.marker");
        let marker_path = marker.display().to_string();
        for hook in ["pre-commit", "commit-msg", "pre-commit-core", "post-commit"] {
            fs::write(
                hooks.join(hook),
                format!("#!/bin/sh\ntouch '{marker_path}'\nexit 1\n"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(hooks.join(hook), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        let captured = capture(repo.path()).await.unwrap();

        assert!(captured.created_commit, "capture succeeds despite hooks");
        assert!(
            !marker.exists(),
            "no commit hook may run during baseline capture"
        );
    }

    // -- Validated capture: manifest check + bounded retry ---------------------

    /// Typed-ops wrapper that simulates an external editor writing to the
    /// checkout right after the first capture attempt begins (mid-capture).
    /// It delegates to the real native implementation — no argv, no
    /// fabricated output.
    struct FlakyWriterOps {
        inner: GixRepoOps,
        remaining_mutations: std::sync::atomic::AtomicU32,
        mutations: std::sync::atomic::AtomicU32,
        stage_once: std::sync::atomic::AtomicBool,
    }

    impl FlakyWriterOps {
        fn flaky_once() -> Self {
            Self {
                inner: GixRepoOps::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(1),
                mutations: std::sync::atomic::AtomicU32::new(0),
                stage_once: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn always_flaky() -> Self {
            Self {
                inner: GixRepoOps::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(u32::MAX),
                mutations: std::sync::atomic::AtomicU32::new(0),
                stage_once: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// No content mutation, but an external writer stages a new tracked
        /// file once — changing the candidate set itself mid-capture.
        fn stage_flaky_once() -> Self {
            Self {
                inner: GixRepoOps::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(0),
                mutations: std::sync::atomic::AtomicU32::new(0),
                stage_once: std::sync::atomic::AtomicBool::new(true),
            }
        }

        fn mutation_count(&self) -> u32 {
            self.mutations.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl RepositoryOps for FlakyWriterOps {
        async fn capture_working_state(
            &self,
            checkout: &Path,
        ) -> WorktreeResult<CapturedWorkingState> {
            let out = self.inner.capture_working_state(checkout).await?;
            if self
                .remaining_mutations
                .try_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                self.mutations
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Content-only changes keep the real index untouched while
                // the captured tree changes.
                std::fs::write(
                    checkout.join("race.txt"),
                    format!("race-{}", self.mutation_count()),
                )
                .unwrap();
            }
            if self
                .stage_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                // A tracked-file addition lands in the real index while the
                // capture is running: the candidate set itself changes.
                std::fs::write(checkout.join("late.txt"), "staged mid capture").unwrap();
                run_git_ok(checkout, &["add", "late.txt"]);
            }
            Ok(out)
        }

        async fn resolve_commit(
            &self,
            common_dir: &Path,
            expr: &str,
        ) -> WorktreeResult<Option<String>> {
            self.inner.resolve_commit(common_dir, expr).await
        }

        async fn index_manifest(&self, checkout: &Path) -> WorktreeResult<Vec<u8>> {
            self.inner.index_manifest(checkout).await
        }

        async fn head_commit(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
            self.inner.head_commit(checkout).await
        }

        async fn has_unmerged_entries(&self, checkout: &Path) -> WorktreeResult<bool> {
            self.inner.has_unmerged_entries(checkout).await
        }

        async fn sparse_checkout_enabled(&self, checkout: &Path) -> WorktreeResult<bool> {
            self.inner.sparse_checkout_enabled(checkout).await
        }

        async fn branch_tip(
            &self,
            common_dir: &Path,
            branch: &crate::worktree::domain::ManagedBranch,
        ) -> WorktreeResult<Option<String>> {
            self.inner.branch_tip(common_dir, branch).await
        }

        async fn create_branch(
            &self,
            common_dir: &Path,
            branch: &crate::worktree::domain::ManagedBranch,
            target: &str,
        ) -> WorktreeResult<()> {
            self.inner.create_branch(common_dir, branch, target).await
        }

        async fn delete_branch(
            &self,
            common_dir: &Path,
            branch: &crate::worktree::domain::ManagedBranch,
        ) -> WorktreeResult<()> {
            self.inner.delete_branch(common_dir, branch).await
        }

        async fn delete_branch_if_unchanged(
            &self,
            common_dir: &Path,
            branch: &crate::worktree::domain::ManagedBranch,
            expected_target: &str,
        ) -> WorktreeResult<bool> {
            self.inner
                .delete_branch_if_unchanged(common_dir, branch, expected_target)
                .await
        }

        async fn set_ref(
            &self,
            common_dir: &Path,
            ref_name: &str,
            commit: &str,
        ) -> WorktreeResult<()> {
            self.inner.set_ref(common_dir, ref_name, commit).await
        }

        async fn create_ref_if_absent(
            &self,
            common_dir: &Path,
            ref_name: &str,
            commit: &str,
        ) -> WorktreeResult<()> {
            self.inner
                .create_ref_if_absent(common_dir, ref_name, commit)
                .await
        }

        async fn list_worktrees(
            &self,
            common_dir: &Path,
        ) -> WorktreeResult<Vec<crate::worktree::RegisteredWorktree>> {
            self.inner.list_worktrees(common_dir).await
        }

        async fn add_worktree(
            &self,
            common_dir: &Path,
            path: &Path,
            branch: &crate::worktree::domain::ManagedBranch,
        ) -> WorktreeResult<()> {
            self.inner.add_worktree(common_dir, path, branch).await
        }

        async fn remove_worktree(&self, common_dir: &Path, path: &Path) -> WorktreeResult<()> {
            self.inner.remove_worktree(common_dir, path).await
        }

        async fn prune_worktrees(&self, common_dir: &Path) -> WorktreeResult<()> {
            self.inner.prune_worktrees(common_dir).await
        }

        async fn checkout_status(
            &self,
            checkout: &Path,
        ) -> WorktreeResult<crate::worktree::CheckoutStatus> {
            self.inner.checkout_status(checkout).await
        }

        async fn current_branch(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
            self.inner.current_branch(checkout).await
        }

        async fn commits_between_count(
            &self,
            common_dir: &Path,
            base: &str,
            tip: &str,
        ) -> WorktreeResult<usize> {
            self.inner
                .commits_between_count(common_dir, base, tip)
                .await
        }

        async fn commits_between(
            &self,
            common_dir: &Path,
            base: &str,
            tip: &str,
        ) -> WorktreeResult<Vec<String>> {
            self.inner.commits_between(common_dir, base, tip).await
        }

        async fn is_ancestor(
            &self,
            common_dir: &Path,
            ancestor: &str,
            descendant: &str,
        ) -> WorktreeResult<bool> {
            self.inner
                .is_ancestor(common_dir, ancestor, descendant)
                .await
        }

        async fn changed_paths_between(
            &self,
            common_dir: &Path,
            base: &str,
            tip: &str,
        ) -> WorktreeResult<Vec<crate::worktree::ChangedPath>> {
            self.inner
                .changed_paths_between(common_dir, base, tip)
                .await
        }
    }

    #[tokio::test]
    async fn validated_capture_retries_when_source_changes_mid_capture() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let git = FlakyWriterOps::flaky_once();
        let captured =
            capture_working_state_validated(&git, repo.path(), CaptureRetryPolicy::default())
                .await
                .unwrap();

        assert_eq!(git.mutation_count(), 1, "the injected race fired once");
        // The retry captured the post-race state: race.txt is included and
        // the baseline is consistent with the observed working state.
        let paths: Vec<String> = tree_paths(repo.path(), &captured.tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(paths.iter().any(|p| p == "race.txt"), "paths: {paths:?}");
        assert!(captured.created_commit);
    }

    #[tokio::test]
    async fn validated_capture_fails_explicitly_after_bounded_attempts() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let git = FlakyWriterOps::always_flaky();
        let policy = CaptureRetryPolicy { max_attempts: 2 };
        let err = capture_working_state_validated(&git, repo.path(), policy)
            .await
            .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("changed during capture"),
            "error: {message}"
        );
        assert!(message.contains("known-inconsistent"), "error: {message}");
        assert_eq!(
            git.mutation_count(),
            6,
            "all three captures in every attempt raced"
        );
    }

    #[tokio::test]
    async fn validated_capture_succeeds_first_try_without_races() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let captured =
            capture_working_state_validated(&GixRepoOps::new(), repo.path(), Default::default())
                .await
                .unwrap();
        assert_eq!(captured.commit, captured.source_head);
    }

    /// Silence unused-variable lint for helpers used only under cfg(unix).
    #[allow(dead_code)]
    fn unused(_p: PathBuf) {}
}
