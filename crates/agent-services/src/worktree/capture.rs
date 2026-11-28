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

use crate::worktree::capture_scan::{
    Fingerprints, Verification, changed_during_capture_detail, fingerprint_source,
    verify_captured_tree,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::repo_ops::{CapturedWorkingState, RepositoryOps, native_err};
use std::path::Path;
use std::time::SystemTime;
use tracing::Instrument;

/// Result of capturing a checkout's working state.
///
/// Re-exported from the typed operations layer; fields and semantics are
/// identical.
pub use crate::worktree::repo_ops::CapturedWorkingState as CapturedState;

/// What one capture did; see [`RepositoryOps::capture_working_state_with_stats`].
pub use crate::worktree::repo_ops::CaptureStats;

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
/// The source manifest (HEAD, index entries and stat fingerprints of every
/// tracked and non-ignored untracked path) is taken before and after each
/// capture attempt. A mismatch means an external writer changed the source
/// mid-capture — external editors do not honor QueryMT's cooperative gate —
/// so the capture is retried against fresh state, and after the bounded
/// attempts it fails explicitly instead of launching child workspaces from a
/// known-inconsistent baseline. An attempt is also rejected when the captured
/// tree's paths differ from the observed ones or when a captured file
/// modified within the racy window of the attempt's start no longer hashes
/// to its captured content. This is not a transactional snapshot against
/// arbitrary hostile writers: a write that lands after the last observation
/// of a path (its post-manifest fingerprint, or its re-hash when racy) is
/// undetectable; the documented limitation is surfaced through capture
/// diagnostics instead (design decision 4).
///
/// Records a `worktree.capture` span (and one `worktree.capture.manifest`
/// span per manifest) with the attempt count and the summed [`CaptureStats`].
pub async fn capture_working_state_validated(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
    policy: CaptureRetryPolicy,
    commit_message: &str,
) -> WorktreeResult<CapturedWorkingState> {
    let attempts = policy.max_attempts.max(1);
    let span = tracing::info_span!(
        "worktree.capture",
        attempts = tracing::field::Empty,
        files_read = tracing::field::Empty,
        bytes_read = tracing::field::Empty,
        objects_written = tracing::field::Empty,
        reused_index_entries = tracing::field::Empty,
        racy_rehashed = tracing::field::Empty,
    );
    let mut totals = CaptureTotals::default();
    let outcome = run_attempts(ops, checkout_root, attempts, commit_message, &mut totals)
        .instrument(span.clone())
        .await;
    span.record("attempts", totals.attempts);
    span.record("files_read", totals.stats.files_read);
    span.record("bytes_read", totals.stats.bytes_read);
    span.record("objects_written", totals.stats.objects_written);
    span.record("reused_index_entries", totals.stats.reused_index_entries);
    span.record("racy_rehashed", totals.racy_rehashed);
    outcome
}

/// Counters summed over the attempts of one validated capture.
#[derive(Default)]
struct CaptureTotals {
    attempts: u64,
    stats: CaptureStats,
    racy_rehashed: u64,
}

async fn run_attempts(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
    attempts: u32,
    commit_message: &str,
    totals: &mut CaptureTotals,
) -> WorktreeResult<CapturedWorkingState> {
    let mut last_diagnostic = String::new();
    for attempt in 0..attempts {
        totals.attempts += 1;
        validate_supported_layout(ops, checkout_root).await?;
        let capture_start = SystemTime::now();
        let before = source_manifest(ops, checkout_root).await?;
        let (captured, stats) = match ops
            .capture_working_state_with_stats(checkout_root, commit_message)
            .await
        {
            Ok(done) => done,
            // A file that vanished or changed between its two reads.
            Err(err) => match changed_during_capture_detail(&err) {
                Some(detail) => {
                    last_diagnostic = format!(
                        "source changed during capture (attempt {attempt}/{attempts}): {detail}"
                    );
                    continue;
                }
                None => return Err(err),
            },
        };
        totals.stats.add(&stats);
        let after = source_manifest(ops, checkout_root).await?;
        if before != after || captured.source_head != before.head {
            last_diagnostic = format!(
                "source changed during capture (attempt {attempt}/{attempts}): HEAD or the observed working state was concurrently modified"
            );
            continue;
        }
        let verification = verify(checkout_root, &captured.tree, &after, capture_start).await?;
        totals.racy_rehashed += verification.rehashed;
        match verification.mismatch {
            None => return Ok(captured),
            Some(reason) => {
                last_diagnostic = format!(
                    "source changed during capture (attempt {attempt}/{attempts}): {reason}"
                );
            }
        }
    }
    Err(WorktreeError::InvalidOperation(format!(
        "{last_diagnostic}; refusing to launch from a known-inconsistent baseline"
    )))
}

async fn verify(
    checkout_root: &Path,
    tree: &str,
    after: &SourceManifest,
    capture_start: SystemTime,
) -> WorktreeResult<Verification> {
    let checkout = checkout_root.to_path_buf();
    let tree = tree.to_string();
    let paths = after.paths.clone();
    tokio::task::spawn_blocking(move || {
        verify_captured_tree(&checkout, &tree, &paths, capture_start)
    })
    .await
    .map_err(native_err)?
}

/// What validation compares before and after a capture attempt: HEAD, the
/// real index entries and the stat fingerprints of every tracked and
/// non-ignored untracked path. Unlike a content manifest it reads no file.
///
/// HEAD and the index manifest come through [`RepositoryOps`]; the
/// fingerprints and the racy re-hash ([`verify`]) open the checkout with
/// `gix` directly, because they describe the on-disk source rather than a
/// repository operation. A `RepositoryOps` test double used with the
/// validated capture must therefore point at a real checkout.
#[derive(Debug, PartialEq, Eq)]
struct SourceManifest {
    head: String,
    index: Vec<u8>,
    paths: Fingerprints,
}

async fn source_manifest(
    ops: &dyn RepositoryOps,
    checkout_root: &Path,
) -> WorktreeResult<SourceManifest> {
    let span = tracing::info_span!("worktree.capture.manifest", paths = tracing::field::Empty);
    async {
        let head = ops.head_commit(checkout_root).await?.ok_or_else(|| {
            WorktreeError::InvalidOperation(
                "cannot capture baseline: checkout has no commits (unborn HEAD)".to_string(),
            )
        })?;
        let index = ops.index_manifest(checkout_root).await?;
        let checkout = checkout_root.to_path_buf();
        let paths = tokio::task::spawn_blocking(move || fingerprint_source(&checkout))
            .await
            .map_err(native_err)??;
        tracing::Span::current().record("paths", paths.len());
        Ok(SourceManifest { head, index, paths })
    }
    .instrument(span)
    .await
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
    commit_message: &str,
) -> WorktreeResult<CapturedWorkingState> {
    validate_supported_layout(ops, checkout_root).await?;
    ops.capture_working_state(checkout_root, commit_message)
        .await
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
    use crate::worktree::repo_ops::{
        BASELINE_COMMIT_MESSAGE, CapturedWorkingState, GixRepoOps, RepositoryOps,
    };
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .stdout
    }

    async fn capture(root: &Path) -> WorktreeResult<CapturedState> {
        capture_working_state(&GixRepoOps::new(), root, BASELINE_COMMIT_MESSAGE).await
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

    /// `foo.c` must precede the directory `foo` (Git compares the directory
    /// as `foo/`). A rewritten tree with both must pass `fsck --strict`.
    #[tokio::test]
    async fn captured_trees_use_gits_directory_aware_entry_order() {
        let repo = init_repo();
        fs::create_dir_all(repo.path().join("foo")).unwrap();
        fs::write(repo.path().join("foo/a"), "a").unwrap();
        fs::write(repo.path().join("foo.c"), "c").unwrap();
        commit_all(repo.path(), "init");

        // Clean checkout: reuses HEAD's tree.
        let clean = capture(repo.path()).await.unwrap();
        // Dirty checkout: the root tree is rewritten by the native writer.
        fs::write(repo.path().join("foo.c"), "changed").unwrap();
        fs::write(repo.path().join("foo/b"), "new").unwrap();
        let dirty = capture(repo.path()).await.unwrap();
        assert_ne!(clean.tree, dirty.tree);

        let listing = out_git(repo.path(), &["ls-tree", &dirty.tree]);
        let names: Vec<&str> = listing
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .collect();
        assert_eq!(names, vec!["foo.c", "foo"], "Git-canonical order");
        run_git_ok(repo.path(), &["fsck", "--strict", "--no-dangling"]);
    }

    /// A file staged (or intent-to-add) and then deleted from disk is a
    /// working-tree deletion: it must be omitted, never serialized with a
    /// placeholder object id, and the real index stays untouched.
    #[tokio::test]
    async fn staged_additions_deleted_from_disk_are_omitted() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        fs::write(repo.path().join("staged-gone.txt"), "x").unwrap();
        run_git_ok(repo.path(), &["add", "staged-gone.txt"]);
        fs::write(repo.path().join("intent-gone.txt"), "y").unwrap();
        run_git_ok(repo.path(), &["add", "-N", "intent-gone.txt"]);
        fs::write(repo.path().join("staged-kept.txt"), "z").unwrap();
        run_git_ok(repo.path(), &["add", "staged-kept.txt"]);
        fs::remove_file(repo.path().join("staged-gone.txt")).unwrap();
        fs::remove_file(repo.path().join("intent-gone.txt")).unwrap();

        let ops = GixRepoOps::new();
        let index_before = ops.index_manifest(repo.path()).await.unwrap();
        let captured = capture(repo.path()).await.unwrap();

        let paths: Vec<String> = tree_paths(repo.path(), &captured.tree)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(!paths.iter().any(|p| p == "staged-gone.txt"), "{paths:?}");
        assert!(!paths.iter().any(|p| p == "intent-gone.txt"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "staged-kept.txt"), "{paths:?}");
        // The tree is intact: every referenced blob exists.
        run_git_ok(repo.path(), &["fsck", "--strict", "--no-dangling"]);
        assert_eq!(index_before, ops.index_manifest(repo.path()).await.unwrap());
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
        let captured = capture_working_state_validated(
            &ops,
            repo.path(),
            CaptureRetryPolicy::default(),
            BASELINE_COMMIT_MESSAGE,
        )
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

    type CaptureHook = Box<dyn Fn(&Path, u32) + Send + Sync>;

    /// Typed-ops wrapper that simulates an external editor writing to the
    /// checkout right after the first capture attempt begins (mid-capture).
    /// It delegates to the real native implementation — no argv, no
    /// fabricated output.
    struct FlakyWriterOps {
        inner: GixRepoOps,
        remaining_mutations: std::sync::atomic::AtomicU32,
        mutations: std::sync::atomic::AtomicU32,
        stage_once: std::sync::atomic::AtomicBool,
        /// Runs after each capture returns, with the checkout and the
        /// zero-based capture call index.
        hook: Option<CaptureHook>,
        calls: std::sync::atomic::AtomicU32,
    }

    impl FlakyWriterOps {
        fn new(remaining: u32, stage_once: bool) -> Self {
            Self {
                inner: GixRepoOps::new(),
                remaining_mutations: std::sync::atomic::AtomicU32::new(remaining),
                mutations: std::sync::atomic::AtomicU32::new(0),
                stage_once: std::sync::atomic::AtomicBool::new(stage_once),
                hook: None,
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn flaky_once() -> Self {
            Self::new(1, false)
        }

        fn always_flaky() -> Self {
            Self::new(u32::MAX, false)
        }

        /// No content mutation, but an external writer stages a new tracked
        /// file once — changing the candidate set itself mid-capture.
        fn stage_flaky_once() -> Self {
            Self::new(0, true)
        }

        /// An external writer acting right after each capture returns.
        fn with_hook(hook: impl Fn(&Path, u32) + Send + Sync + 'static) -> Self {
            let mut ops = Self::new(0, false);
            ops.hook = Some(Box::new(hook));
            ops
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
            commit_message: &str,
        ) -> WorktreeResult<CapturedWorkingState> {
            let out = self
                .inner
                .capture_working_state(checkout, commit_message)
                .await?;
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

        async fn capture_working_state_with_stats(
            &self,
            checkout: &Path,
            commit_message: &str,
        ) -> WorktreeResult<(CapturedWorkingState, CaptureStats)> {
            let Some(hook) = &self.hook else {
                // Mutation-style wrappers act inside `capture_working_state`.
                return self
                    .capture_working_state(checkout, commit_message)
                    .await
                    .map(|state| (state, CaptureStats::default()));
            };
            // Keep the counters: capture through the real implementation,
            // then let the writer act.
            let (state, stats) = self
                .inner
                .capture_working_state_with_stats(checkout, commit_message)
                .await?;
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            hook(checkout, call);
            Ok((state, stats))
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

        async fn unpreserved_staged_paths(
            &self,
            checkout: &Path,
            tree: &str,
        ) -> WorktreeResult<Vec<String>> {
            self.inner.unpreserved_staged_paths(checkout, tree).await
        }

        async fn sparse_checkout_enabled(&self, checkout: &Path) -> WorktreeResult<bool> {
            self.inner.sparse_checkout_enabled(checkout).await
        }

        async fn prepare_integration(
            &self,
            request: crate::worktree::integrate::PrepareRequest,
        ) -> WorktreeResult<crate::worktree::integrate::PrepareOutcome> {
            self.inner.prepare_integration(request).await
        }

        async fn apply_integration(
            &self,
            prepared: crate::worktree::integrate::PreparedIntegration,
        ) -> WorktreeResult<crate::worktree::integrate::ApplyOutcome> {
            self.inner.apply_integration(prepared).await
        }

        async fn run_integration_hooks(
            &self,
            request: crate::worktree::hooks::HookRequest,
        ) -> WorktreeResult<crate::worktree::hooks::HookOutcome> {
            self.inner.run_integration_hooks(request).await
        }

        async fn finalize_integration(
            &self,
            request: crate::worktree::integrate::FinalizeRequest,
        ) -> WorktreeResult<crate::worktree::integrate::FinalizeOutcome> {
            self.inner.finalize_integration(request).await
        }

        async fn recover_integration(
            &self,
            checkout: &Path,
        ) -> WorktreeResult<crate::worktree::integrate::RecoveryOutcome> {
            self.inner.recover_integration(checkout).await
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

        async fn remove_worktree(
            &self,
            common_dir: &Path,
            path: &Path,
            mode: crate::worktree::repo_ops::RemoveCheckout,
        ) -> WorktreeResult<()> {
            self.inner.remove_worktree(common_dir, path, mode).await
        }

        async fn commit_tree(&self, common_dir: &Path, commit: &str) -> Option<String> {
            self.inner.commit_tree(common_dir, commit).await
        }

        async fn prune_worktrees(&self, common_dir: &Path, checkout: &Path) -> WorktreeResult<()> {
            self.inner.prune_worktrees(common_dir, checkout).await
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
        let captured = capture_working_state_validated(
            &git,
            repo.path(),
            CaptureRetryPolicy::default(),
            BASELINE_COMMIT_MESSAGE,
        )
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
        let err =
            capture_working_state_validated(&git, repo.path(), policy, BASELINE_COMMIT_MESSAGE)
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
            2,
            "the single capture of every attempt raced (manifests no longer capture)"
        );
    }

    #[tokio::test]
    async fn validated_capture_succeeds_first_try_without_races() {
        let repo = init_repo();
        fs::write(repo.path().join("file.txt"), "one").unwrap();
        commit_all(repo.path(), "init");

        let captured = capture_working_state_validated(
            &GixRepoOps::new(),
            repo.path(),
            Default::default(),
            BASELINE_COMMIT_MESSAGE,
        )
        .await
        .unwrap();
        assert_eq!(captured.commit, captured.source_head);
    }

    // -- Selective, hash-first capture and stat-fingerprint validation ---------

    use std::collections::HashMap;
    use std::fs::File;
    use std::time::Duration;

    /// In-memory record of `tracing` spans and their recorded fields, in
    /// creation order.
    #[derive(Clone, Default)]
    struct SpanLog(std::sync::Arc<std::sync::Mutex<Vec<SpanRecord>>>);

    type SpanRecord = (String, HashMap<String, String>);

    struct SpanIdx(usize);
    struct Fields<'a>(&'a mut HashMap<String, String>);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for SpanLog
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = HashMap::new();
            attrs.record(&mut Fields(&mut fields));
            let mut spans = self.0.lock().unwrap();
            spans.push((attrs.metadata().name().to_string(), fields));
            ctx.span(id)
                .unwrap()
                .extensions_mut()
                .insert(SpanIdx(spans.len() - 1));
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).unwrap();
            let idx = span.extensions().get::<SpanIdx>().unwrap().0;
            values.record(&mut Fields(&mut self.0.lock().unwrap()[idx].1));
        }
    }

    impl SpanLog {
        fn install() -> (Self, tracing::subscriber::DefaultGuard) {
            use tracing_subscriber::layer::SubscriberExt;
            let log = Self::default();
            let subscriber = tracing_subscriber::registry().with(log.clone());
            (log, tracing::subscriber::set_default(subscriber))
        }

        /// The recorded fields of the only span called `name`.
        fn only(&self, name: &str) -> HashMap<String, String> {
            let spans = self.0.lock().unwrap();
            let mut found = spans.iter().filter(|(n, _)| n == name);
            let (_, fields) = found.next().unwrap_or_else(|| panic!("no `{name}` span"));
            assert!(found.next().is_none(), "more than one `{name}` span");
            fields.clone()
        }

        fn count(&self, name: &str) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .count()
        }
    }

    fn set_mtime(path: &Path, age_secs: u64) {
        set_mtime_to(path, SystemTime::now() - Duration::from_secs(age_secs));
    }

    fn set_mtime_to(path: &Path, mtime: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Move the mtimes of every regular file below `dir` (but `.git`) back.
    fn age_tree(dir: &Path, age_secs: u64) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    age_tree(&path, age_secs);
                }
            } else if kind.is_file() {
                set_mtime(&path, age_secs);
            }
        }
    }

    fn write_file(repo: &Path, rel: &str, content: &str) {
        let path = repo.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A repository with `files` committed, all mtimes an hour old, so the
    /// index was written in a later tick than any file and no entry is racy.
    fn aged_repo(files: &[(&str, &str)]) -> TempDir {
        let repo = init_repo();
        for (rel, content) in files {
            write_file(repo.path(), rel, content);
        }
        age_tree(repo.path(), 3600);
        commit_all(repo.path(), "init");
        repo
    }

    async fn capture_stats(ops: &GixRepoOps, root: &Path) -> (CapturedState, CaptureStats) {
        ops.capture_working_state_with_stats(root, BASELINE_COMMIT_MESSAGE)
            .await
            .unwrap()
    }

    fn blob_bytes(repo: &Path, tree: &str, path: &str) -> Vec<u8> {
        blob_of_oid(repo, &blob_of(repo, tree, path))
    }

    fn loose_object_count(repo: &Path) -> u64 {
        out_git(repo, &["count-objects"])
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn validated(ops: &dyn RepositoryOps, root: &Path) -> WorktreeResult<CapturedState> {
        capture_working_state_validated(
            ops,
            root,
            CaptureRetryPolicy::default(),
            BASELINE_COMMIT_MESSAGE,
        )
        .await
    }

    #[tokio::test]
    async fn clean_checkout_reads_no_file_content() {
        let repo = aged_repo(&[("a.txt", "one"), ("dir/b.txt", "two"), ("c.txt", "three")]);

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(stats.files_read, 0);
        assert_eq!(stats.bytes_read, 0);
        assert_eq!(stats.reused_index_entries, 3);
        assert_eq!(captured.commit, head(repo.path()));
        assert!(!captured.created_commit);
    }

    #[tokio::test]
    async fn one_edited_file_is_the_only_file_read() {
        let files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("d{}/f{i}.txt", i % 4), format!("content {i}")))
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let repo = aged_repo(&borrowed);
        write_file(repo.path(), "d3/f7.txt", "edited content");

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(stats.files_read, 1);
        assert_eq!(stats.bytes_read, "edited content".len() as u64);
        assert_eq!(stats.reused_index_entries, 19);
        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "d3/f7.txt"),
            b"edited content"
        );
    }

    #[tokio::test]
    async fn staged_then_edited_file_keeps_working_contents() {
        let repo = aged_repo(&[("file.txt", "base"), ("other.txt", "other")]);
        write_file(repo.path(), "file.txt", "staged");
        run_git_ok(repo.path(), &["add", "file.txt"]);
        write_file(repo.path(), "file.txt", "working");
        let index_before = fs::read(repo.path().join(".git/index")).unwrap();

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "file.txt"),
            b"working"
        );
        assert_eq!(stats.files_read, 1);
        assert_eq!(
            fs::read(repo.path().join(".git/index")).unwrap(),
            index_before,
            "the real index is byte-for-byte unchanged"
        );
    }

    #[tokio::test]
    async fn racily_clean_entry_is_read() {
        let repo = init_repo();
        run_git_ok(repo.path(), &["config", "core.trustctime", "false"]);
        write_file(repo.path(), "racy.txt", "aaaa");
        write_file(repo.path(), "calm.txt", "calm");
        set_mtime(&repo.path().join("racy.txt"), 3600);
        set_mtime(&repo.path().join("calm.txt"), 7200);
        commit_all(repo.path(), "init");
        // The index was written no later than the recorded mtime ...
        let recorded = fs::metadata(repo.path().join("racy.txt"))
            .unwrap()
            .modified()
            .unwrap();
        File::options()
            .write(true)
            .open(repo.path().join(".git/index"))
            .unwrap()
            .set_modified(recorded)
            .unwrap();
        // ... and the file is rewritten with the same size and mtime.
        write_file(repo.path(), "racy.txt", "bbbb");
        File::options()
            .write(true)
            .open(repo.path().join("racy.txt"))
            .unwrap()
            .set_modified(recorded)
            .unwrap();

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(blob_bytes(repo.path(), &captured.tree, "racy.txt"), b"bbbb");
        assert_eq!(stats.files_read, 1);
        assert_eq!(
            stats.reused_index_entries, 1,
            "only the calm entry is reused"
        );
    }

    #[tokio::test]
    async fn assume_unchanged_file_edited_is_captured() {
        let repo = aged_repo(&[("au.txt", "aaaa"), ("other.txt", "other")]);
        run_git_ok(repo.path(), &["config", "core.trustctime", "false"]);
        run_git_ok(
            repo.path(),
            &["update-index", "--assume-unchanged", "au.txt"],
        );
        // Same size and mtime: only the flag can reveal the edit.
        let recorded = fs::metadata(repo.path().join("au.txt"))
            .unwrap()
            .modified()
            .unwrap();
        write_file(repo.path(), "au.txt", "bbbb");
        File::options()
            .write(true)
            .open(repo.path().join("au.txt"))
            .unwrap()
            .set_modified(recorded)
            .unwrap();

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(blob_bytes(repo.path(), &captured.tree, "au.txt"), b"bbbb");
        assert_eq!(stats.files_read, 1);
    }

    #[tokio::test]
    async fn intent_to_add_file_is_read() {
        let repo = aged_repo(&[("base.txt", "base")]);
        write_file(repo.path(), "added.txt", "has content");
        run_git_ok(repo.path(), &["add", "-N", "added.txt"]);

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "added.txt"),
            b"has content"
        );
        assert_eq!(stats.files_read, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_replaced_by_a_symlink_is_read() {
        let repo = aged_repo(&[("f.txt", "12345"), ("target.txt", "t")]);
        fs::remove_file(repo.path().join("f.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", repo.path().join("f.txt")).unwrap();

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        let listing = tree_paths(repo.path(), &captured.tree);
        assert!(listing.contains(&("f.txt".to_string(), "120000".to_string())));
        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "f.txt"),
            b"target.txt"
        );
        assert_eq!(stats.files_read, 1);
    }

    #[tokio::test]
    async fn untracked_files_are_captured_when_status_hides_them() {
        let repo = aged_repo(&[("tracked.txt", "t")]);
        run_git_ok(repo.path(), &["config", "status.showUntrackedFiles", "no"]);
        write_file(repo.path(), "new/untracked.txt", "fresh");

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "new/untracked.txt"),
            b"fresh"
        );
        assert_eq!(stats.files_read, 1);
    }

    #[tokio::test]
    async fn crlf_converted_file_keeps_working_bytes() {
        let repo = init_repo();
        write_file(repo.path(), ".gitattributes", "*.txt text eol=crlf\n");
        write_file(repo.path(), "a.txt", "one\r\ntwo\r\n");
        write_file(repo.path(), "plain.bin", "bin");
        age_tree(repo.path(), 3600);
        commit_all(repo.path(), "init");
        // The index holds the LF-normalized blob, the file the CRLF bytes.
        assert_eq!(blob_bytes(repo.path(), "HEAD", "a.txt"), b"one\ntwo\n");

        let (captured, stats) = capture_stats(&GixRepoOps::new(), repo.path()).await;

        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "a.txt"),
            b"one\r\ntwo\r\n"
        );
        assert_eq!(stats.files_read, 1, "only the converted file is read");
    }

    #[tokio::test]
    async fn repeated_delegation_from_the_same_dirty_state() {
        let repo = aged_repo(&[("a.txt", "one"), ("dir/b.txt", "two")]);
        write_file(repo.path(), "a.txt", "dirty a");
        write_file(repo.path(), "dir/b.txt", "dirty b");
        let ops = GixRepoOps::new();

        let (first, first_stats) = capture_stats(&ops, repo.path()).await;
        let objects_after_first = loose_object_count(repo.path());
        let (second, second_stats) = capture_stats(&ops, repo.path()).await;

        assert!(first_stats.objects_written >= 4, "{first_stats:?}");
        assert_eq!(second.tree, first.tree);
        // Only the baseline commit, which carries the capture time, may be new.
        assert!(second_stats.objects_written <= 1, "{second_stats:?}");
        assert!(loose_object_count(repo.path()) - objects_after_first <= 1);
    }

    #[tokio::test]
    async fn new_untracked_file_is_written_once() {
        let repo = aged_repo(&[("a.txt", "one")]);
        write_file(repo.path(), "new.txt", "a brand new file");
        let blob = out_git(repo.path(), &["hash-object", "new.txt"])
            .trim()
            .to_string();
        assert!(!run_git(repo.path(), &["cat-file", "-e", &blob]));
        let ops = GixRepoOps::new();

        let (first, first_stats) = capture_stats(&ops, repo.path()).await;
        let (_, second_stats) = capture_stats(&ops, repo.path()).await;

        // The new blob, the changed root tree and the baseline commit.
        assert_eq!(first_stats.objects_written, 3, "{first_stats:?}");
        assert!(run_git(repo.path(), &["cat-file", "-e", &blob]));
        assert_eq!(
            blob_bytes(repo.path(), &first.tree, "new.txt"),
            b"a brand new file"
        );
        assert!(second_stats.objects_written <= 1, "{second_stats:?}");
    }

    #[tokio::test]
    async fn repeated_delegation_with_a_large_dirty_file_writes_nothing() {
        let repo = aged_repo(&[("small.txt", "small"), ("big.bin", "x")]);
        let big: String = (0..4096)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        write_file(repo.path(), "big.bin", &big);
        let capture = || {
            crate::worktree::repo_ops::capture_with_threshold(
                repo.path(),
                BASELINE_COMMIT_MESSAGE,
                1024,
            )
        };

        let (first, first_stats) = capture().await.unwrap();
        let (second, second_stats) = capture().await.unwrap();

        assert!(first_stats.objects_written >= 3, "{first_stats:?}");
        // New to the repository: hashed, then read again to be written.
        assert_eq!(first_stats.bytes_read, 2 * big.len() as u64);
        assert_eq!(
            blob_bytes(repo.path(), &first.tree, "big.bin"),
            big.as_bytes()
        );
        assert_eq!(second.tree, first.tree);
        assert!(second_stats.objects_written <= 1, "{second_stats:?}");
        assert_eq!(second_stats.files_read, 1);
        assert_eq!(
            second_stats.bytes_read,
            big.len() as u64,
            "hashed once, never re-read"
        );
    }

    /// Tree of the working state built by reading every candidate file raw
    /// into a private index with its disk mode.
    #[cfg(unix)]
    fn full_read_tree(root: &Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let repo = gix::open(root).unwrap();
        let listing = out_git(root, &["ls-files", "-z", "-c", "-o", "--exclude-standard"]);
        let candidates: std::collections::BTreeSet<&str> =
            listing.split('\0').filter(|p| !p.is_empty()).collect();
        let mut index = gix::index::State::new(gix::hash::Kind::Sha1);
        let mut kept = std::collections::HashSet::new();
        for path in candidates {
            let abs = root.join(path);
            let Ok(md) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if md.is_dir() {
                continue;
            }
            let (bytes, mode) = if md.file_type().is_symlink() {
                (
                    fs::read_link(&abs)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                    gix::index::entry::Mode::SYMLINK,
                )
            } else if md.permissions().mode() & 0o111 != 0 {
                (
                    fs::read(&abs).unwrap(),
                    gix::index::entry::Mode::FILE_EXECUTABLE,
                )
            } else {
                (fs::read(&abs).unwrap(), gix::index::entry::Mode::FILE)
            };
            let id = repo.write_blob(&bytes).unwrap().detach();
            index.dangerously_push_entry(
                gix::index::entry::Stat::default(),
                id,
                gix::index::entry::Flags::empty(),
                mode,
                path.into(),
            );
            kept.insert(gix::bstr::BString::from(path));
        }
        index.sort_entries();
        crate::worktree::repo_ops::write_tree_from_index(&repo, &index, &kept)
            .unwrap()
            .to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selective_capture_matches_a_full_read() {
        use std::os::unix::fs::PermissionsExt;
        let repo = init_repo();
        let root = repo.path();
        write_file(root, ".gitignore", "*.log\n");
        for (rel, content) in [
            ("del.txt", "to delete"),
            ("edit.txt", "to edit"),
            ("run.sh", "#!/bin/sh\n"),
            ("au.txt", "assume"),
            ("keep.txt", "keep"),
            ("target.txt", "target"),
        ] {
            write_file(root, rel, content);
        }
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("target.txt", root.join("link")).unwrap();
        age_tree(root, 3600);
        commit_all(root, "init");

        fs::remove_file(root.join("del.txt")).unwrap();
        write_file(root, "edit.txt", "edited content");
        write_file(root, "staged.txt", "staged only");
        run_git_ok(root, &["add", "staged.txt"]);
        run_git_ok(root, &["update-index", "--assume-unchanged", "au.txt"]);
        write_file(root, "au.txt", "assume edited");
        write_file(root, "new.txt", "untracked");
        write_file(root, "ignored.log", "ignored");

        let (captured, stats) = capture_stats(&GixRepoOps::new(), root).await;

        assert!(stats.reused_index_entries > 0, "{stats:?}");
        assert_eq!(captured.tree, full_read_tree(root));

        // Git's own `add -A`, with the assume-unchanged bit cleared in a
        // temporary copy of the index.
        let temp = TempDir::new().unwrap();
        let index_file = temp.path().join("index");
        fs::copy(root.join(".git/index"), &index_file).unwrap();
        let git_with_index = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_INDEX_FILE", &index_file)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git_with_index(&["update-index", "--no-assume-unchanged", "au.txt"]);
        git_with_index(&["add", "-A"]);
        assert_eq!(captured.tree, git_with_index(&["write-tree"]).trim());
    }

    #[tokio::test]
    async fn quiet_checkout_needs_one_attempt() {
        let repo = aged_repo(&[
            ("a.txt", "one"),
            ("b.txt", "two"),
            ("c.txt", "three"),
            ("d.txt", "four"),
        ]);
        // Changed files keep old mtimes, outside the racy window.
        write_file(repo.path(), "a.txt", "edited a");
        write_file(repo.path(), "b.txt", "edited b");
        write_file(repo.path(), "new.txt", "untracked");
        for name in ["a.txt", "b.txt", "new.txt"] {
            set_mtime(&repo.path().join(name), 7200);
        }
        let (log, _guard) = SpanLog::install();

        let captured = validated(&GixRepoOps::new(), repo.path()).await.unwrap();

        let span = log.only("worktree.capture");
        assert_eq!(span["attempts"], "1");
        assert_eq!(span["racy_rehashed"], "0");
        assert_eq!(span["files_read"], "3");
        assert_eq!(span["reused_index_entries"], "2");
        assert_eq!(log.count("worktree.capture.manifest"), 2);
        assert_eq!(
            log.0
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _)| n == "worktree.capture.manifest")
                .unwrap()
                .1["paths"],
            "5"
        );
        assert_eq!(
            blob_bytes(repo.path(), &captured.tree, "a.txt"),
            b"edited a"
        );
    }

    /// The second attempt is the only one that can succeed when the writer
    /// acts on the first capture only; a writer acting on every capture
    /// exhausts the attempts without a baseline.
    async fn assert_retried_then_bounded_failure(
        repo: &Path,
        once: FlakyWriterOps,
        always: FlakyWriterOps,
        expected_path: &str,
        expected_content: &[u8],
    ) {
        let (log, guard) = SpanLog::install();
        let captured = validated(&once, repo).await.unwrap();
        assert_eq!(log.only("worktree.capture")["attempts"], "2");
        assert_eq!(
            blob_bytes(repo, &captured.tree, expected_path),
            expected_content
        );
        drop(guard);

        let err = validated(&always, repo).await.unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("source changed during capture")
                && message.contains("known-inconsistent"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn external_edit_to_an_already_dirty_file_during_capture() {
        let repo = aged_repo(&[("f.txt", "base"), ("g.txt", "g")]);
        write_file(repo.path(), "f.txt", "dirty one");
        let edit = |root: &Path, call: u32| {
            write_file(
                root,
                "f.txt",
                &format!("dirty {}", "x".repeat(call as usize + 3)),
            );
        };
        assert_retried_then_bounded_failure(
            repo.path(),
            FlakyWriterOps::with_hook(move |root, call| {
                if call == 0 {
                    edit(root, 7)
                }
            }),
            FlakyWriterOps::with_hook(edit),
            "f.txt",
            b"dirty xxxxxxxxxx",
        )
        .await;
    }

    #[tokio::test]
    async fn untracked_file_created_during_capture() {
        let repo = aged_repo(&[("f.txt", "base")]);
        assert_retried_then_bounded_failure(
            repo.path(),
            FlakyWriterOps::with_hook(|root, call| {
                if call == 0 {
                    write_file(root, "late.txt", "appeared");
                }
            }),
            FlakyWriterOps::with_hook(|root, call| {
                write_file(root, &format!("late-{call}.txt"), "appeared");
            }),
            "late.txt",
            b"appeared",
        )
        .await;
    }

    #[tokio::test]
    async fn same_size_rewrite_after_the_read_is_detected() {
        let repo = aged_repo(&[("f.txt", "aaaa"), ("g.txt", "g")]);
        run_git_ok(repo.path(), &["config", "core.trustctime", "false"]);
        // Dirty with the same size. Its mtime lies in the future, so it stays
        // inside the racy window of every attempt however slow the test runs.
        write_file(repo.path(), "f.txt", "bbbb");
        let mtime = SystemTime::now() + Duration::from_secs(3600);
        set_mtime_to(&repo.path().join("f.txt"), mtime);
        // Rewritten with the same size after the read, previous mtime restored:
        // inode, size and mtime are unchanged, so only the re-hash can tell.
        let rewrite = move |root: &Path, content: &str| {
            write_file(root, "f.txt", content);
            set_mtime_to(&root.join("f.txt"), mtime);
        };
        let (log, guard) = SpanLog::install();
        let ops = FlakyWriterOps::with_hook(move |root, call| {
            if call == 0 {
                rewrite(root, "cccc");
            }
        });

        let captured = validated(&ops, repo.path()).await.unwrap();

        let span = log.only("worktree.capture");
        assert_eq!(span["attempts"], "2");
        // One re-hash per attempt: the first attempt passed the fingerprint
        // comparison and only its re-hash rejected it.
        assert_eq!(span["racy_rehashed"], "2");
        assert_eq!(blob_bytes(repo.path(), &captured.tree, "f.txt"), b"cccc");
        drop(guard);

        let always = FlakyWriterOps::with_hook(move |root, call| {
            rewrite(root, if call % 2 == 0 { "dddd" } else { "eeee" });
        });
        let err = validated(&always, repo.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("source changed during capture"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn capture_span_explains_a_slow_capture() {
        let repo = aged_repo(&[("seed.txt", "seed")]);
        let block = "x".repeat(100 * 1024);
        for i in 0..100 {
            let rel = format!("bulk/{i:03}.bin");
            write_file(repo.path(), &rel, &block);
            set_mtime(&repo.path().join(rel), 3600);
        }
        let (log, _guard) = SpanLog::install();

        validated(&GixRepoOps::new(), repo.path()).await.unwrap();

        let span = log.only("worktree.capture");
        assert_eq!(span["attempts"], "1");
        assert_eq!(span["files_read"], "100");
        assert_eq!(span["bytes_read"], "10240000");
    }

    /// Silence unused-variable lint for helpers used only under cfg(unix).
    #[allow(dead_code)]
    fn unused(_p: PathBuf) {}
}
