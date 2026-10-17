//! Private-index baseline capture of a checkout's working state.
//!
//! Captures the *actual working contents* of a checkout — tracked working
//! files, tracked deletions, tracked hidden/ignored patterns, and
//! non-ignored untracked files — into a Git tree without touching the
//! source's HEAD, branch, index, stash, or files. This is repository state
//! materialization for isolated workspaces (design decision 4), not a
//! replacement for undo snapshots.
//!
//! Mechanics: a private temporary index is seeded from HEAD and then
//! reconciled with the working tree via `git add -A`, so Git itself handles
//! file modes, symlinks, binary contents, path exclusions (.gitignore), and
//! deletions. The staging partition is deliberately *not* propagated: a
//! partially staged file contributes its working content, and the real
//! index stays byte-for-byte unchanged. Ignored untracked files are
//! excluded and reported for disclosure.
//!
//! When the captured tree equals HEAD's tree, HEAD is reused as the
//! baseline (no private commit). Otherwise a private commit parented by
//! HEAD is created with an explicit internal identity via `commit-tree`,
//! which requires no user commit identity and never runs commit hooks.
//! Ref creation for the baseline lives with the caller (section 3.3).

use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::git::GitRunner;
use std::path::Path;

/// Author/committer identity for QueryMT's private commits. Private
/// commits never require the user's commit identity and never invoke user
/// commit hooks (`commit-tree` runs none).
pub const INTERNAL_COMMIT_NAME: &str = "QueryMT";
pub const INTERNAL_COMMIT_EMAIL: &str = "querymt@localhost";
const INTERNAL_COMMIT_MESSAGE: &str = "QueryMT private baseline";

/// Result of capturing a checkout's working state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedState {
    /// Full object id of the checkout HEAD at capture time.
    pub source_head: String,
    /// Captured working-state tree object id.
    pub tree: String,
    /// Immutable commit for the captured state. Equals `source_head` when
    /// the working state matched HEAD (no private commit created).
    pub commit: String,
    /// Whether a private commit was created (false when HEAD was reused).
    pub created_commit: bool,
    /// Ignored untracked files excluded from the capture (disclosed to the
    /// user, never silently copied).
    pub ignored_files: Vec<String>,
}

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
    git: &dyn GitRunner,
    checkout_root: &Path,
    policy: CaptureRetryPolicy,
) -> WorktreeResult<CapturedState> {
    let attempts = policy.max_attempts.max(1);
    let mut last_diagnostic = String::new();
    for attempt in 0..attempts {
        let before = source_manifest(git, checkout_root).await?;
        let captured = capture_working_state(git, checkout_root).await?;
        let after = source_manifest(git, checkout_root).await?;
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
    git: &dyn GitRunner,
    checkout_root: &Path,
) -> WorktreeResult<(String, String, Vec<u8>)> {
    // A private tree records contents, modes, symlink targets and untracked
    // files. Status labels cannot detect edits to an already-dirty path.
    let state = capture_working_state(git, checkout_root).await?;
    let args = ["ls-files", "--stage", "-z"];
    let index = git
        .run(checkout_root, &args)
        .await?
        .into_checked(&args)?
        .stdout;
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
    git: &dyn GitRunner,
    checkout_root: &Path,
) -> WorktreeResult<CapturedState> {
    validate_supported_layout(git, checkout_root).await?;

    let source_head = rev_parse(git, checkout_root, "HEAD").await?;

    // Private temporary index; removed on all exit paths.
    let index_file = std::env::temp_dir().join(format!(
        "qmt-baseline-index-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    let index_path = index_file.to_string_lossy().into_owned();
    let index_env = [("GIT_INDEX_FILE", index_path.as_str())];

    let captured = capture_with_private_index(git, checkout_root, &index_env, &source_head).await;
    let _ = std::fs::remove_file(&index_file);
    captured
}

async fn capture_with_private_index(
    git: &dyn GitRunner,
    checkout_root: &Path,
    index_env: &[(&str, &str)],
    source_head: &str,
) -> WorktreeResult<CapturedState> {
    // 1. Seed the private index from HEAD (design: capture reconciles *from*
    //    HEAD), then reconcile it with actual working contents. `add -A`
    //    covers tracked modifications/deletions and non-ignored untracked
    //    files, applies .gitignore exclusions, and preserves Git file
    //    semantics — all in the private index.
    let args = ["read-tree", "HEAD"];
    git.run_with_env(checkout_root, &args, index_env)
        .await?
        .into_checked(&args)?;

    let args = ["add", "-A"];
    git.run_with_env(checkout_root, &args, index_env)
        .await?
        .into_checked(&args)?;

    let args = ["write-tree"];
    let tree = git
        .run_with_env(checkout_root, &args, index_env)
        .await?
        .into_checked(&args)?
        .stdout_str()
        .trim()
        .to_string();

    // 2. Reuse HEAD when the working state matches it; otherwise create a
    //    private commit parented by source HEAD. `commit-tree` needs no
    //    user identity and runs no hooks.
    let head_tree = rev_parse(git, checkout_root, "HEAD^{tree}").await?;
    if tree == head_tree {
        return Ok(CapturedState {
            source_head: source_head.to_string(),
            tree,
            commit: source_head.to_string(),
            created_commit: false,
            ignored_files: list_ignored(git, checkout_root).await?,
        });
    }

    let commit_env = [
        ("GIT_AUTHOR_NAME", INTERNAL_COMMIT_NAME),
        ("GIT_AUTHOR_EMAIL", INTERNAL_COMMIT_EMAIL),
        ("GIT_COMMITTER_NAME", INTERNAL_COMMIT_NAME),
        ("GIT_COMMITTER_EMAIL", INTERNAL_COMMIT_EMAIL),
    ];
    let args = [
        "commit-tree",
        tree.as_str(),
        "-p",
        source_head,
        "-m",
        INTERNAL_COMMIT_MESSAGE,
    ];
    let commit = git
        .run_with_env(checkout_root, &args, &commit_env)
        .await?
        .into_checked(&args)?
        .stdout_str()
        .trim()
        .to_string();

    Ok(CapturedState {
        source_head: source_head.to_string(),
        tree,
        commit,
        created_commit: true,
        ignored_files: list_ignored(git, checkout_root).await?,
    })
}

/// Reject source layouts this first version cannot materialize faithfully.
///
/// Every rejection happens before any child workspaces launch; the error
/// names the unsupported state (spec: unsupported states fail explicitly
/// rather than omitting content silently).
async fn validate_supported_layout(
    git: &dyn GitRunner,
    checkout_root: &Path,
) -> WorktreeResult<()> {
    // Unborn HEAD has nothing to seed from or parent by.
    let probe = git
        .run(checkout_root, &["rev-parse", "--verify", "HEAD"])
        .await?;
    if !probe.success {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: checkout has no commits (unborn HEAD); \
             create an initial commit first"
                .to_string(),
        ));
    }

    // Unresolved merge conflicts cannot be represented faithfully: conflict
    // markers on disk would be captured as ordinary content.
    let args = ["ls-files", "-u"];
    let unmerged = git.run(checkout_root, &args).await?.into_checked(&args)?;
    if !unmerged.stdout_str().trim().is_empty() {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: source has unresolved index conflicts; \
             resolve them before requesting isolation"
                .to_string(),
        ));
    }

    // Submodules are unsupported in the first version.
    if checkout_root.join(".gitmodules").is_file() {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: submodules are unsupported in this version".to_string(),
        ));
    }

    // Sparse checkouts: tracked paths absent from disk would be captured as
    // deletions.
    let args = ["config", "--get", "core.sparseCheckout"];
    if let Ok(out) = git.run(checkout_root, &args).await
        && out.success
        && out.stdout_str().trim() == "true"
    {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: sparse checkouts are unsupported in this version".to_string(),
        ));
    }

    // LFS-managed content would be captured as pointer files.
    let attributes = checkout_root.join(".gitattributes");
    if let Ok(text) = std::fs::read_to_string(&attributes)
        && text.contains("filter=lfs")
    {
        return Err(WorktreeError::InvalidOperation(
            "cannot capture baseline: Git LFS content is unsupported in this version".to_string(),
        ));
    }

    Ok(())
}

async fn rev_parse(git: &dyn GitRunner, repo_root: &Path, expr: &str) -> WorktreeResult<String> {
    let args = ["rev-parse", expr];
    git.run(repo_root, &args)
        .await?
        .into_checked(&args)
        .map(|out| out.stdout_str().trim().to_string())
}

/// Ignored untracked files in the checkout, for disclosure.
async fn list_ignored(git: &dyn GitRunner, checkout_root: &Path) -> WorktreeResult<Vec<String>> {
    let args = ["status", "--porcelain=v1", "--ignored=matching"];
    let out = git
        .run(checkout_root, &args)
        .await?
        .into_checked(&args)?
        .stdout_str();
    Ok(out
        .lines()
        .filter(|line| line.len() >= 4 && line.starts_with("!!"))
        .map(|line| line[3..].to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::git::{CliGit, GitOutput};
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
        capture_working_state(&CliGit::new(), root).await
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
        commit_all(repo.path(), "init");

        let err = capture(repo.path()).await.unwrap_err();
        assert!(err.to_string().contains("LFS"), "error: {err}");
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

    /// Real-git runner that simulates an external editor writing to the
    /// checkout right after the first `git add -A` (mid-capture).
    struct FlakyWriterGit {
        inner: CliGit,
        remaining_mutations: std::sync::atomic::AtomicU32,
        mutations: std::sync::atomic::AtomicU32,
    }

    impl FlakyWriterGit {
        fn flaky_once() -> Self {
            Self {
                inner: CliGit::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(1),
                mutations: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn always_flaky() -> Self {
            Self {
                inner: CliGit::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(u32::MAX),
                mutations: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn mutation_count(&self) -> u32 {
            self.mutations.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl GitRunner for FlakyWriterGit {
        async fn run_with_env(
            &self,
            repo_root: &Path,
            args: &[&str],
            extra_env: &[(&str, &str)],
        ) -> WorktreeResult<GitOutput> {
            let out = self.inner.run_with_env(repo_root, args, extra_env).await?;
            if args.first() == Some(&"add")
                && self
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
                // Content-only changes retain identical porcelain status.
                std::fs::write(
                    repo_root.join("race.txt"),
                    format!("race-{}", self.mutation_count()),
                )
                .unwrap();
            }
            Ok(out)
        }
    }

    #[tokio::test]
    async fn validated_capture_retries_when_source_changes_mid_capture() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let git = FlakyWriterGit::flaky_once();
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

        let git = FlakyWriterGit::always_flaky();
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
            capture_working_state_validated(&CliGit::new(), repo.path(), Default::default())
                .await
                .unwrap();
        assert_eq!(captured.commit, captured.source_head);
    }

    /// Silence unused-variable lint for helpers used only under cfg(unix).
    #[allow(dead_code)]
    fn unused(_p: PathBuf) {}
}
