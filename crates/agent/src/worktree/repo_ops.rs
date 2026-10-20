//! Typed repository operations for worktree administration and capture.
//!
//! This is the native replacement for the former argv/stdout-shaped
//! [`GitRunner`](crate::worktree::git) boundary: every built-in repository
//! operation is a named, typed method implemented directly against `gix`
//! primitives (references, worktrees, index, objects). No production path
//! in this module spawns a Git executable, and no caller parses command
//! output.
//!
//! All methods are async but every blocking gix call runs on the blocking
//! thread pool via [`tokio::task::spawn_blocking`], never on an async
//! executor thread.
//!
//! Tests never fabricate command output: fault injection happens at the
//! operation level through [`FaultInjectingRepoOps`].

use crate::worktree::domain::{ChangeKind, ChangedPath, ManagedBranch};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};

/// A linked-worktree registration observed in the shared repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorktree {
    /// Canonical checkout root (working-tree path).
    pub path: PathBuf,
    /// Attached branch short name (`foo` for `refs/heads/foo`), or `None`
    /// when the checkout is detached.
    pub branch: Option<String>,
    /// Whether this registration is the repository's main checkout.
    pub is_main: bool,
}

/// Live working-tree state of one checkout.
///
/// Typed replacement for `status --porcelain` parsing: the lists hold
/// checkout-relative paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckoutStatus {
    /// Tracked files whose contents or staging differ from HEAD.
    pub uncommitted: Vec<String>,
    /// Untracked, non-ignored files.
    pub untracked: Vec<String>,
    /// Ignored (untracked) files.
    pub ignored: Vec<String>,
}

/// Result of a working-state capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedWorkingState {
    /// Full object id of the source HEAD at capture time.
    pub source_head: String,
    /// Tree object id of the captured working state.
    pub tree: String,
    /// Baseline commit id (the source HEAD when the tree matched it).
    pub commit: String,
    /// Whether a private commit object was created (`false` means HEAD was
    /// reused because the working tree matched it).
    pub created_commit: bool,
    /// Ignored untracked paths observed during capture (disclosure only).
    pub ignored_files: Vec<String>,
}

/// Typed repository operations (native gix implementation; no Git executable).
#[async_trait]
pub trait RepositoryOps: Send + Sync {
    /// Resolve a revision expression (e.g. `HEAD`, `HEAD^{commit}`, a ref
    /// name) to a full commit object id; `None` when it does not resolve.
    ///
    /// `common_dir` is the repository administration handle: the canonical
    /// Git common directory shared by every checkout of the project. It is
    /// never a working directory guessed from a checkout (see
    /// [`crate::worktree::identity`]).
    async fn resolve_commit(&self, common_dir: &Path, expr: &str)
    -> WorktreeResult<Option<String>>;

    /// Full object id at the branch tip, or `None` when the branch is absent.
    async fn branch_tip(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<Option<String>>;

    /// Create a local branch at `target` (commit id). Fails when the branch
    /// already exists.
    async fn create_branch(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        target: &str,
    ) -> WorktreeResult<()>;

    /// Delete a local branch unconditionally (safety policy lives in the
    /// lifecycle layer; conditional deletion is a later remediation).
    async fn delete_branch(&self, common_dir: &Path, branch: &ManagedBranch) -> WorktreeResult<()>;

    /// Delete a local branch only while it still points at
    /// `expected_target` (an expected-target reference transaction).
    ///
    /// Returns `Ok(true)` when the branch was deleted, including when it had
    /// already been deleted concurrently. Returns `Ok(false)` when an
    /// external writer moved the branch between observation and deletion —
    /// the branch and its new history are retained and the caller must
    /// disclose the race. The branch is never deleted unconditionally.
    async fn delete_branch_if_unchanged(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        expected_target: &str,
    ) -> WorktreeResult<bool>;

    /// Create or move a ref to `commit` (plain `update-ref` semantics).
    async fn set_ref(&self, common_dir: &Path, ref_name: &str, commit: &str) -> WorktreeResult<()>;

    /// Create a ref only when it does not already exist.
    async fn create_ref_if_absent(
        &self,
        common_dir: &Path,
        ref_name: &str,
        commit: &str,
    ) -> WorktreeResult<()>;

    /// Linked worktrees registered in the repository, including a main
    /// checkout that has a working tree. The main registration of a bare
    /// common repository has no working tree and is not a usable checkout;
    /// it is skipped rather than reported.
    async fn list_worktrees(&self, common_dir: &Path) -> WorktreeResult<Vec<RegisteredWorktree>>;

    /// Create a linked worktree at `path` checking out `branch`.
    async fn add_worktree(
        &self,
        common_dir: &Path,
        path: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<()>;

    /// Remove a linked worktree (checkout directory plus registration
    /// metadata). Refuses dirty checkouts; the lifecycle layer has already
    /// verified content safety.
    async fn remove_worktree(&self, common_dir: &Path, path: &Path) -> WorktreeResult<()>;

    /// Prune registration metadata of linked worktrees whose checkout
    /// directory no longer exists.
    async fn prune_worktrees(&self, common_dir: &Path) -> WorktreeResult<()>;

    /// Working-tree state (uncommitted/untracked/ignored) for a checkout.
    async fn checkout_status(&self, checkout: &Path) -> WorktreeResult<CheckoutStatus>;

    /// HEAD commit of a checkout; `None` when HEAD is unborn.
    async fn head_commit(&self, checkout: &Path) -> WorktreeResult<Option<String>>;

    /// Attached branch (short name) of a checkout; `None` when detached.
    async fn current_branch(&self, checkout: &Path) -> WorktreeResult<Option<String>>;

    /// Count of commits reachable from `tip` but not `base`.
    async fn commits_between_count(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<usize>;

    /// Commits reachable from `tip` but not `base`, newest first.
    async fn commits_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<String>>;

    /// Whether `ancestor` is reachable from `descendant`.
    async fn is_ancestor(
        &self,
        common_dir: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> WorktreeResult<bool>;

    /// Paths changed between two immutable commits (or trees), sorted by
    /// path, with their change kinds. Operates purely on repository objects
    /// through the canonical common directory — live working-tree state is
    /// never consulted, so results stay stable across later edits.
    async fn changed_paths_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<ChangedPath>>;

    // -- working-state capture primitives (private index/tree/commit) ----

    /// Captured source HEAD and tree; reuses HEAD when the working state is
    /// identical to it. `ignored_files` lists ignored untracked paths for
    /// disclosure.
    async fn capture_working_state(&self, checkout: &Path) -> WorktreeResult<CapturedWorkingState>;

    /// Deterministic manifest of the real index entries (path, stage, mode,
    /// object id) for before/after equality checks.
    async fn index_manifest(&self, checkout: &Path) -> WorktreeResult<Vec<u8>>;

    /// Whether the real index contains unmerged (stage > 0) entries.
    async fn has_unmerged_entries(&self, checkout: &Path) -> WorktreeResult<bool>;

    /// Whether `core.sparseCheckout` is enabled for the checkout.
    async fn sparse_checkout_enabled(&self, checkout: &Path) -> WorktreeResult<bool>;
}

/// Native repository operations backed entirely by `gix`.
///
/// Every method runs its blocking work via [`tokio::task::spawn_blocking`].
#[derive(Debug, Clone, Default)]
pub struct GixRepoOps;

impl GixRepoOps {
    pub fn new() -> Self {
        Self
    }
}

/// Map a `gix` error to the worktree error taxonomy.
fn native_err(err: impl std::fmt::Display) -> WorktreeError {
    WorktreeError::GitDiscovery(err.to_string())
}

/// True when the erased error means "not found" (unknown revision/ref or
/// unborn HEAD). gix reports missing revisions as validation-class spec
/// parse failures ("couldn't parse revision"), so both classes count.
fn is_not_found(err: &gix::Error) -> bool {
    matches!(
        err.classify().dominant_class(),
        Some(gix_error::Class::NotFound | gix_error::Class::Validation)
    )
}

/// Open a repository for administration at `path`.
///
/// Callers pass the canonical common directory (the administration handle
/// resolved by [`crate::worktree::identity`]) — a `.git` directory, an
/// external Git directory, or a bare repository — never a working directory
/// reconstructed from a checkout.
fn open_repo(path: &Path) -> Result<gix::Repository, WorktreeError> {
    // `isolated()` ignores inherited GIT_DIR / GIT_WORK_TREE / GIT_INDEX_FILE
    // overrides so an ambient environment cannot redirect operations to a
    // different repository, worktree, or index (repository-local config is
    // still honored).
    gix::open_opts(path, gix::open::Options::isolated()).map_err(native_err)
}

/// Open a checkout for status enumeration.
///
/// `status.showUntrackedFiles=no` in repository config disables gix's
/// directory walk entirely, which would silently hide untracked and ignored
/// content from removal-safety checks. The override guarantees the walk
/// stays available; the explicit builder in [`RepositoryOps::checkout_status`]
/// then selects per-file emission.
fn open_repo_for_status(path: &Path) -> Result<gix::Repository, WorktreeError> {
    gix::open_opts(
        path,
        gix::open::Options::isolated().config_overrides(["status.showUntrackedFiles=all"]),
    )
    .map_err(native_err)
}

fn full_branch_name(branch: &ManagedBranch) -> gix::refs::FullName {
    gix::refs::FullName::try_from(format!("refs/heads/{}", branch.as_str()).as_str())
        .expect("validated managed branch name")
}

fn full_name(name: &str) -> WorktreeResult<gix::refs::FullName> {
    gix::refs::FullName::try_from(name)
        .map_err(|_| WorktreeError::InvalidOperation(format!("invalid ref name: {name}")))
}

fn parse_oid(expr: &str) -> WorktreeResult<gix::hash::ObjectId> {
    gix::hash::ObjectId::from_hex(expr.as_bytes())
        .map_err(|_| WorktreeError::InvalidOperation(format!("not a valid object id: {expr}")))
}

fn ref_edit_update(
    name: gix::refs::FullName,
    target: gix::hash::ObjectId,
    message: &str,
) -> RefEdit {
    RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: message.into(),
            },
            new: gix::refs::Target::Object(target),
            expected: PreviousValue::Any,
        },
        name,
        deref: false,
    }
}

#[async_trait]
impl RepositoryOps for GixRepoOps {
    async fn resolve_commit(
        &self,
        common_dir: &Path,
        expr: &str,
    ) -> WorktreeResult<Option<String>> {
        let common_dir = common_dir.to_path_buf();
        let expr = expr.to_string();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            match repo.rev_parse_single(expr.as_bytes()) {
                Ok(id) => Ok(Some(id.detach().to_string())),
                Err(err) if is_not_found(&err) => Ok(None),
                Err(err) => Err(native_err(err)),
            }
        })
        .await
        .map_err(native_err)?
    }

    async fn branch_tip(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<Option<String>> {
        let common_dir = common_dir.to_path_buf();
        let full = full_branch_name(branch);
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            match repo.try_find_reference(full.as_bstr()) {
                Ok(Some(mut reference)) => {
                    let id = reference.peel_to_id().map_err(native_err)?;
                    Ok(Some(id.detach().to_string()))
                }
                Ok(None) => Ok(None),
                Err(err) => Err(native_err(err)),
            }
        })
        .await
        .map_err(native_err)?
    }

    async fn create_branch(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        target: &str,
    ) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let full = full_branch_name(branch);
        let target = parse_oid(target)?;
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let edit = RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: "QueryMT managed branch".into(),
                    },
                    new: gix::refs::Target::Object(target),
                    expected: PreviousValue::MustNotExist,
                },
                name: full,
                deref: false,
            };
            repo.edit_reference(edit).map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn delete_branch(&self, common_dir: &Path, branch: &ManagedBranch) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let full = full_branch_name(branch);
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let edit = RefEdit {
                change: Change::Delete {
                    log: RefLog::AndReference,
                    expected: PreviousValue::Any,
                },
                name: full,
                deref: false,
            };
            repo.edit_reference(edit).map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn delete_branch_if_unchanged(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        expected_target: &str,
    ) -> WorktreeResult<bool> {
        let common_dir = common_dir.to_path_buf();
        let full = full_branch_name(branch);
        let expected = parse_oid(expected_target)?;
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let edit = RefEdit {
                change: Change::Delete {
                    log: RefLog::AndReference,
                    // Expected-target transaction: the deletion applies only
                    // while the branch still points at the observed commit.
                    expected: PreviousValue::MustExistAndMatch(Target::Object(expected)),
                },
                name: full.clone(),
                deref: false,
            };
            match repo.edit_reference(edit) {
                Ok(_) => Ok(true),
                Err(_) => {
                    // Classify by current state instead of matching gix
                    // error variants: an absent reference means the goal was
                    // achieved concurrently; a moved target means an
                    // external writer won the race and the branch (with its
                    // new history) must be retained.
                    match repo.find_reference(&full) {
                        Err(_) => Ok(true),
                        Ok(mut reference) => {
                            let current = reference.peel_to_id().map_err(native_err)?;
                            if current == expected {
                                Err(native_err(format!(
                                    "conditional deletion of {full} failed unexpectedly"
                                )))
                            } else {
                                Ok(false)
                            }
                        }
                    }
                }
            }
        })
        .await
        .map_err(native_err)?
    }

    async fn set_ref(&self, common_dir: &Path, ref_name: &str, commit: &str) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let full = full_name(ref_name)?;
        let target = parse_oid(commit)?;
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            repo.edit_reference(ref_edit_update(full, target, "QueryMT retained ref"))
                .map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn create_ref_if_absent(
        &self,
        common_dir: &Path,
        ref_name: &str,
        commit: &str,
    ) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let full = full_name(ref_name)?;
        let target = parse_oid(commit)?;
        let exists_msg = format!("ref {ref_name} already exists");
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            if repo
                .try_find_reference(full.as_bstr())
                .map_err(native_err)?
                .is_some()
            {
                return Err(WorktreeError::InvalidOperation(exists_msg));
            }
            let edit = RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: "QueryMT retained result".into(),
                    },
                    new: gix::refs::Target::Object(target),
                    expected: PreviousValue::MustNotExist,
                },
                name: full,
                deref: false,
            };
            repo.edit_reference(edit).map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn list_worktrees(&self, common_dir: &Path) -> WorktreeResult<Vec<RegisteredWorktree>> {
        let common_dir_param = common_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            // Opened at the canonical common directory itself: this is the
            // shared administration handle that works for ordinary `.git`
            // directories, separate Git directories, and bare common
            // repositories alike. No working directory is guessed.
            let repo = open_repo(&common_dir_param)?;
            let repo_common_dir = repo.common_dir().to_path_buf();
            let mut out = Vec::new();
            for checkout in repo.worktrees_including_main().map_err(native_err)? {
                let checkout = checkout.map_err(native_err)?;
                // The main registration of a bare common repository has no
                // working tree: it is repository administration, not a
                // usable checkout, so it is skipped instead of failing the
                // enumeration for bare-backed linked checkouts.
                let Some(path) = checkout.workdir() else {
                    continue;
                };
                let path = path.to_path_buf();
                let is_main = checkout.git_dir() == repo_common_dir;
                let branch = repo_branch_of(&checkout);
                out.push(RegisteredWorktree {
                    path: std::fs::canonicalize(&path).unwrap_or(path),
                    branch,
                    is_main,
                });
            }
            Ok(out)
        })
        .await
        .map_err(native_err)?
    }

    async fn add_worktree(
        &self,
        common_dir: &Path,
        path: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let path = path.to_path_buf();
        let full = full_branch_name(branch);
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let head = gix::worktree::add::Head::Attached(full);
            let interrupt = std::sync::atomic::AtomicBool::new(false);
            repo.add_worktree(&path, head, gix::progress::Discard, &interrupt)
                .map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn remove_worktree(&self, common_dir: &Path, path: &Path) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            repo.remove_worktree(
                &path,
                gix::worktree::remove::Force::Never,
                gix::progress::Discard,
            )
            .map_err(native_err)?;
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn prune_worktrees(&self, common_dir: &Path) -> WorktreeResult<()> {
        let common_dir = common_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let common_dir = repo.common_dir().to_path_buf();
            for proxy in repo.worktrees().map_err(native_err)? {
                if proxy.base().map_err(native_err)?.exists() {
                    continue;
                }
                // Registration metadata of a vanished checkout: remove the
                // administrative directory under the common Git directory.
                if let Ok(id) = proxy.id() {
                    let admin = common_dir
                        .join("worktrees")
                        .join(id.to_str_lossy().into_owned());
                    if admin.is_dir() {
                        std::fs::remove_dir_all(&admin).map_err(native_err)?;
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(native_err)?
    }

    async fn checkout_status(&self, checkout: &Path) -> WorktreeResult<CheckoutStatus> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo_for_status(&checkout)?;
            let mut status = CheckoutStatus::default();

            // Index-vs-worktree changes plus untracked/ignored discovery.
            // `into_index_worktree_iter` deliberately deactivates the
            // HEAD-tree comparison thread so no object access happens off
            // this thread; staged changes are computed below, synchronously.
            let platform = repo
                .status(gix::progress::Discard)
                .map_err(native_err)?
                .untracked_files(gix::status::UntrackedFiles::Files)
                .dirwalk_options(|mut opts| {
                    opts.set_emit_ignored(Some(gix::dir::walk::EmissionMode::Matching));
                    opts
                });
            for item in platform
                .into_index_worktree_iter(Vec::new())
                .map_err(native_err)?
            {
                let item = item.map_err(native_err)?;
                match item {
                    gix::status::index_worktree::Item::Modification { rela_path, .. } => {
                        status.uncommitted.push(rela_path.to_string());
                    }
                    gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
                        match classify_dirwalk_entry(entry) {
                            Some(DirwalkDisposition::Ignored(path)) => {
                                status.ignored.push(path);
                            }
                            Some(DirwalkDisposition::Untracked(path)) => {
                                status.untracked.push(path);
                            }
                            None => {}
                        }
                    }
                    gix::status::index_worktree::Item::Rewrite { dirwalk_entry, .. } => {
                        match classify_dirwalk_entry(dirwalk_entry) {
                            Some(DirwalkDisposition::Ignored(path)) => {
                                status.ignored.push(path);
                            }
                            Some(DirwalkDisposition::Untracked(path)) => {
                                status.untracked.push(path);
                            }
                            None => {}
                        }
                    }
                }
            }

            // Staged changes: HEAD-tree vs real index, compared on this
            // thread (matches `status --porcelain` staged reporting, which
            // feeds the removal-safety dirty check).
            if let Some(head_tree) = head_tree_id(&repo)? {
                let mut head_entries = std::collections::HashMap::new();
                flatten_tree(&repo, head_tree, &mut head_entries, String::new())?;
                if repo.index_path().exists() {
                    let index = repo.open_index().map_err(native_err)?;
                    for entry in index.entries() {
                        if entry.stage() != gix::index::entry::Stage::Unconflicted {
                            continue;
                        }
                        let path: gix::bstr::BString = entry.path(&index).to_vec().into();
                        match head_entries.remove(&path) {
                            Some((mode, oid)) if mode == entry.mode && oid == entry.id => {}
                            _ => status.uncommitted.push(path.to_string()),
                        }
                    }
                }
                for path in head_entries.keys() {
                    status.uncommitted.push(path.to_string());
                }
            }
            Ok(status)
        })
        .await
        .map_err(native_err)?
    }

    async fn head_commit(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&checkout)?;
            match repo.head_id() {
                Ok(id) => Ok(Some(id.detach().to_string())),
                Err(err) if is_not_found(&err) => Ok(None),
                Err(err) => Err(native_err(err)),
            }
        })
        .await
        .map_err(native_err)?
    }

    async fn current_branch(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&checkout)?;
            let branch = repo.head_name().map_err(native_err)?.and_then(|full| {
                String::from_utf8(full.as_bstr().to_vec())
                    .ok()
                    .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_string))
            });
            Ok(branch)
        })
        .await
        .map_err(native_err)?
    }

    async fn commits_between_count(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<usize> {
        Ok(self.commits_between(common_dir, base, tip).await?.len())
    }

    async fn commits_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<String>> {
        let common_dir = common_dir.to_path_buf();
        let base = base.to_string();
        let tip = tip.to_string();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let base = repo
                .rev_parse_single(base.as_bytes())
                .map_err(native_err)?
                .detach();
            let tip = repo
                .rev_parse_single(tip.as_bytes())
                .map_err(native_err)?
                .detach();
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![tip];
            let mut commits: Vec<(gix::date::Time, String)> = Vec::new();
            while let Some(id) = stack.pop() {
                if id == base || !seen.insert(id) {
                    continue;
                }
                let commit = repo.find_commit(id).map_err(native_err)?;
                let time = commit.time().map_err(native_err)?;
                stack.extend(commit.parent_ids().map(|p| p.detach()));
                commits.push((time, id.to_string()));
            }
            // Newest first.
            commits.sort_by(|a, b| b.0.seconds.cmp(&a.0.seconds).then_with(|| b.1.cmp(&a.1)));
            Ok(commits.into_iter().map(|(_, oid)| oid).collect())
        })
        .await
        .map_err(native_err)?
    }

    async fn is_ancestor(
        &self,
        common_dir: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> WorktreeResult<bool> {
        let common_dir = common_dir.to_path_buf();
        let ancestor = ancestor.to_string();
        let descendant = descendant.to_string();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let ancestor = repo
                .rev_parse_single(ancestor.as_bytes())
                .map_err(native_err)?
                .detach();
            let descendant = repo
                .rev_parse_single(descendant.as_bytes())
                .map_err(native_err)?
                .detach();
            if ancestor == descendant {
                return Ok(true);
            }
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![descendant];
            while let Some(id) = stack.pop() {
                if id == ancestor {
                    return Ok(true);
                }
                if !seen.insert(id) {
                    continue;
                }
                let commit = repo.find_commit(id).map_err(native_err)?;
                stack.extend(commit.parent_ids().map(|p| p.detach()));
            }
            Ok(false)
        })
        .await
        .map_err(native_err)?
    }

    async fn changed_paths_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<ChangedPath>> {
        let common_dir = common_dir.to_path_buf();
        let base = base.to_string();
        let tip = tip.to_string();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&common_dir)?;
            let base = repo
                .rev_parse_single(base.as_bytes())
                .map_err(native_err)?
                .detach();
            let tip = repo
                .rev_parse_single(tip.as_bytes())
                .map_err(native_err)?
                .detach();
            let base_tree = peel_to_tree(&repo, base)?;
            let tip_tree = peel_to_tree(&repo, tip)?;
            changed_paths_between_trees(&repo, base_tree, tip_tree)
        })
        .await
        .map_err(native_err)?
    }

    async fn capture_working_state(&self, checkout: &Path) -> WorktreeResult<CapturedWorkingState> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            // The status pass below must see untracked files even when the
            // repository config disables them (`status.showUntrackedFiles`),
            // otherwise the baseline would silently miss content.
            let repo = open_repo_for_status(&checkout)?;
            let head = repo.head_id().map_err(|err| {
                if is_not_found(&err) {
                    WorktreeError::InvalidOperation(
                        "cannot capture baseline: checkout has no commits (unborn HEAD)".into(),
                    )
                } else {
                    native_err(err)
                }
            })?;
            let source_head = head.detach();
            let head_commit = repo.find_commit(head).map_err(native_err)?;
            let head_tree = head_commit.tree_id().map_err(native_err)?.detach();

            // Seed a private index from HEAD and reconcile it with actual
            // working contents (deletions, modifications, untracked
            // non-ignored files), never touching the real index.
            let mut private_index =
                gix::index::State::from_tree(&head_tree, &repo.objects, Default::default())
                    .map_err(native_err)?;

            // Reject active submodules before touching the working tree: a
            // gitlink checkout is a directory and cannot be read as blob
            // content.
            reject_gitlinks(&repo, head_tree)?;

            let (ignored_files, kept) =
                classify_and_reconcile(&repo, &mut private_index, source_head, head_tree)?;

            // Reject candidate paths whose effective attributes select an
            // external filter (Git LFS or any custom clean/smudge process);
            // resolution covers nested `.gitattributes`, Git-directory
            // attributes, and macros.
            reject_external_filter_paths(&repo, &private_index, &kept)?;

            let tree = write_tree_from_index(&repo, &private_index, &kept)?;

            let (commit, created_commit) = if tree == head_tree {
                (source_head, false)
            } else {
                let signature = internal_signature();
                let commit = gix::objs::Commit {
                    tree,
                    parents: vec![source_head].into(),
                    author: signature.clone(),
                    committer: signature,
                    encoding: None,
                    message: INTERNAL_COMMIT_MESSAGE.as_bytes().into(),
                    extra_headers: Default::default(),
                };
                let written = repo.write_object(&commit).map_err(native_err)?;
                (written.detach(), true)
            };

            Ok(CapturedWorkingState {
                source_head: source_head.to_string(),
                tree: tree.to_string(),
                commit: commit.to_string(),
                created_commit,
                ignored_files,
            })
        })
        .await
        .map_err(native_err)?
    }

    async fn index_manifest(&self, checkout: &Path) -> WorktreeResult<Vec<u8>> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&checkout)?;
            if !repo.index_path().exists() {
                return Ok(Vec::new());
            }
            let index = repo.open_index().map_err(native_err)?;
            let mut manifest = Vec::new();
            for entry in index.entries() {
                manifest.extend_from_slice(entry.path(&index));
                manifest.push(b' ');
                manifest.extend_from_slice((entry.stage() as u8).to_string().as_bytes());
                manifest.push(b' ');
                manifest.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
                manifest.push(b' ');
                manifest.extend_from_slice(entry.id.to_string().as_bytes());
                manifest.push(b'\n');
            }
            Ok(manifest)
        })
        .await
        .map_err(native_err)?
    }

    async fn has_unmerged_entries(&self, checkout: &Path) -> WorktreeResult<bool> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&checkout)?;
            if !repo.index_path().exists() {
                return Ok(false);
            }
            let index = repo.open_index().map_err(native_err)?;
            Ok(index
                .entries()
                .iter()
                .any(|entry| entry.stage() != gix::index::entry::Stage::Unconflicted))
        })
        .await
        .map_err(native_err)?
    }

    async fn sparse_checkout_enabled(&self, checkout: &Path) -> WorktreeResult<bool> {
        let checkout = checkout.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let repo = open_repo(&checkout)?;
            let value = repo.config_snapshot().string("core.sparseCheckout");
            Ok(value.is_some_and(|v| {
                v.as_bstr() == b"true".as_bstr() || v.as_bstr() == b"True".as_bstr()
            }))
        })
        .await
        .map_err(native_err)?
    }
}

/// Attached branch (short name) of a checkout repository, if any.
fn repo_branch_of(checkout: &gix::Repository) -> Option<String> {
    checkout.head_name().ok()?.and_then(|full| {
        String::from_utf8(full.as_bstr().to_vec())
            .ok()
            .and_then(|name| name.strip_prefix("refs/heads/").map(str::to_string))
    })
}

fn internal_signature() -> gix::actor::Signature {
    gix::actor::Signature {
        name: INTERNAL_COMMIT_NAME.into(),
        email: INTERNAL_COMMIT_EMAIL.into(),
        time: gix::date::Time::now_local_or_utc(),
    }
}

/// How a directory-walk entry should be counted for status and capture.
enum DirwalkDisposition {
    Ignored(String),
    Untracked(String),
}

fn classify_dirwalk_entry(entry: gix::dir::Entry) -> Option<DirwalkDisposition> {
    if !matches!(
        entry.disk_kind,
        Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink)
    ) {
        return None;
    }
    let path = entry.rela_path.to_string();
    match entry.status {
        gix::dir::entry::Status::Ignored(_) => Some(DirwalkDisposition::Ignored(path)),
        gix::dir::entry::Status::Untracked => Some(DirwalkDisposition::Untracked(path)),
        _ => None,
    }
}

/// The trackable kinds of working-tree files, with the executable bit
/// resolved from filesystem metadata (gix's own `Kind` does not carry it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Symlink,
    Executable,
    Regular,
}

fn file_kind_from_metadata(md: &std::fs::Metadata) -> FileKind {
    if md.file_type().is_symlink() {
        FileKind::Symlink
    } else if is_executable(md) {
        FileKind::Executable
    } else {
        FileKind::Regular
    }
}

/// Checkout-relative path of a HEAD-tree vs index change.
/// HEAD's root tree id, or `None` when HEAD is unborn.
fn head_tree_id(repo: &gix::Repository) -> WorktreeResult<Option<gix::hash::ObjectId>> {
    match repo.head_id() {
        Ok(id) => {
            let commit = repo.find_commit(id).map_err(native_err)?;
            Ok(Some(commit.tree_id().map_err(native_err)?.detach()))
        }
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(native_err(err)),
    }
}

/// Flatten a tree into `path -> (index mode, object id)`, recursing into
/// subtrees. Runs on the calling thread only.
fn flatten_tree(
    repo: &gix::Repository,
    tree_id: gix::hash::ObjectId,
    out: &mut std::collections::HashMap<
        gix::bstr::BString,
        (gix::index::entry::Mode, gix::hash::ObjectId),
    >,
    prefix: String,
) -> WorktreeResult<()> {
    let tree = repo.find_tree(tree_id).map_err(native_err)?;
    for entry in tree.iter() {
        let entry = entry.map_err(native_err)?;
        let path: gix::bstr::BString = if prefix.is_empty() {
            entry.filename().to_owned()
        } else {
            format!("{}/{}", prefix, entry.filename()).into()
        };
        if entry.mode().kind() == gix::objs::tree::EntryKind::Tree {
            flatten_tree(repo, entry.oid().into(), out, path.to_string())?;
        } else {
            let mode = match entry.mode().kind() {
                gix::objs::tree::EntryKind::Link => gix::index::entry::Mode::SYMLINK,
                gix::objs::tree::EntryKind::BlobExecutable => {
                    gix::index::entry::Mode::FILE_EXECUTABLE
                }
                gix::objs::tree::EntryKind::Commit => gix::index::entry::Mode::DIR,
                _ => gix::index::entry::Mode::FILE,
            };
            out.insert(path, (mode, entry.oid().into()));
        }
    }
    Ok(())
}

/// Peel a resolved revision to its tree, accepting commits and trees.
fn peel_to_tree(
    repo: &gix::Repository,
    id: gix::hash::ObjectId,
) -> WorktreeResult<gix::hash::ObjectId> {
    Ok(repo
        .find_object(id)
        .map_err(native_err)?
        .peel_to_kind(gix::object::Kind::Tree)
        .map_err(native_err)?
        .id()
        .detach())
}

/// Diff two immutable trees into sorted path/kind pairs by flattening both
/// trees and comparing path → (mode, object) maps. A path present only in
/// `tip_tree` is added, only in `base_tree` is deleted, and otherwise
/// modified when either its object id or its mode differs. Runs on the
/// calling thread only.
fn changed_paths_between_trees(
    repo: &gix::Repository,
    base_tree: gix::hash::ObjectId,
    tip_tree: gix::hash::ObjectId,
) -> WorktreeResult<Vec<ChangedPath>> {
    let mut base = std::collections::HashMap::new();
    flatten_tree(repo, base_tree, &mut base, String::new())?;
    let mut tip = std::collections::HashMap::new();
    flatten_tree(repo, tip_tree, &mut tip, String::new())?;
    let mut changes: Vec<ChangedPath> = Vec::with_capacity(base.len().max(tip.len()));
    for (path, (tip_mode, tip_oid)) in &tip {
        match base.get(path) {
            None => changes.push(ChangedPath {
                path: path.to_string(),
                kind: ChangeKind::Added,
            }),
            Some((base_mode, base_oid)) => {
                if base_oid != tip_oid || base_mode != tip_mode {
                    changes.push(ChangedPath {
                        path: path.to_string(),
                        kind: ChangeKind::Modified,
                    });
                }
            }
        }
    }
    for path in base.keys() {
        if !tip.contains_key(path) {
            changes.push(ChangedPath {
                path: path.to_string(),
                kind: ChangeKind::Deleted,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// Classify the working tree and reconcile the private index with actual
/// working contents.
///
/// Equivalent to `git add -A` against the private index: tracked files use
/// their working contents (deletions applied), non-ignored untracked files
/// are added, ignored untracked files stay excluded and are reported.
fn classify_and_reconcile(
    repo: &gix::Repository,
    private_index: &mut gix::index::State,
    source_head: gix::hash::ObjectId,
    head_tree: gix::hash::ObjectId,
) -> WorktreeResult<(Vec<String>, std::collections::HashSet<gix::bstr::BString>)> {
    let worktree = repo
        .workdir()
        .ok_or_else(|| native_err("bare repository has no working tree"))?
        .to_path_buf();

    // One status pass classifies everything: modifications of tracked files,
    // untracked files, and ignored files.
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(native_err)?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .dirwalk_options(|mut opts| {
            opts.set_emit_ignored(Some(gix::dir::walk::EmissionMode::Matching));
            opts
        });

    let mut ignored: Vec<String> = Vec::new();
    let mut untracked: Vec<gix::bstr::BString> = Vec::new();
    for item in platform
        .into_index_worktree_iter(Vec::new())
        .map_err(native_err)?
    {
        let item = item.map_err(native_err)?;
        match item {
            gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
                match classify_dirwalk_entry(entry) {
                    Some(DirwalkDisposition::Ignored(path)) => {
                        ignored.push(path);
                    }
                    Some(DirwalkDisposition::Untracked(path)) => {
                        untracked.push(path.into_bytes().into());
                    }
                    None => {}
                }
            }
            gix::status::index_worktree::Item::Rewrite { dirwalk_entry, .. } => {
                match classify_dirwalk_entry(dirwalk_entry) {
                    Some(DirwalkDisposition::Ignored(path)) => {
                        ignored.push(path);
                    }
                    Some(DirwalkDisposition::Untracked(path)) => {
                        untracked.push(path.into_bytes().into());
                    }
                    None => {}
                }
            }
            gix::status::index_worktree::Item::Modification { .. } => {
                // Tracked modifications: the private index is
                // refreshed from disk for every tracked path below.
            }
        }
    }
    let _ = (source_head, head_tree);

    // Union tracked membership: HEAD-seeded private index plus the real
    // index's paths (staged files absent from HEAD stay capture candidates;
    // the real index itself is never modified). Unmerged entries are
    // rejected before capture, so only stage-0 entries are considered. A
    // missing index file (fresh checkout without a staging write) means an
    // empty union source.
    let real_index = if repo.index_path().exists() {
        Some(repo.open_index().map_err(native_err)?)
    } else {
        None
    };
    let mut kept: std::collections::HashSet<gix::bstr::BString> = Default::default();
    let mut additional: Vec<(gix::bstr::BString, gix::index::entry::Stat)> = Vec::new();
    if let Some(real_index) = &real_index {
        for entry in real_index.entries() {
            if entry.stage() != gix::index::entry::Stage::Unconflicted {
                continue;
            }
            let path = entry.path(real_index);
            if private_index.entry_index_by_path(path).is_err() {
                additional.push((path.to_vec().into(), entry.stat));
            }
        }
    }
    for (path, stat) in additional {
        private_index.dangerously_push_entry(
            stat,
            gix::hash::ObjectId::null(private_index.object_hash()),
            gix::index::entry::Flags::empty(),
            gix::index::entry::Mode::FILE,
            path.as_bstr(),
        );
        kept.insert(path);
    }
    private_index.sort_entries();

    // Refresh tracked entries against actual disk contents; drop deletions.
    let tracked_paths: Vec<gix::bstr::BString> = private_index
        .entries()
        .iter()
        .map(|entry| entry.path(private_index).to_vec().into())
        .collect();
    {
        let entries = private_index.entries_mut();
        for (entry, path) in entries.iter_mut().zip(&tracked_paths) {
            let abs = worktree.join(gix_pathbuf(path.as_bstr()));
            match std::fs::symlink_metadata(&abs) {
                Ok(md) if !md.is_dir() => {
                    let kind = file_kind_from_metadata(&md);
                    entry.id = write_blob_from_disk(repo, &abs, &md)?;
                    entry.mode = index_mode_for(kind);
                    kept.insert(path.clone());
                }
                // Deleted from the working tree: excluded from the
                // baseline. Directories (e.g. an uncaught gitlink
                // checkout) are never read as blob content.
                _ => {}
            }
        }
    }

    // Add non-ignored untracked files.
    for path in untracked {
        if private_index.entry_index_by_path(path.as_bstr()).is_ok() {
            continue;
        }
        let abs = worktree.join(gix_pathbuf(path.as_bstr()));
        let md = std::fs::symlink_metadata(&abs).map_err(native_err)?;
        let kind = file_kind_from_metadata(&md);
        let oid = write_blob_from_disk(repo, &abs, &md)?;
        let stat = gix::index::entry::Stat::default();
        kept.insert(path.clone());
        private_index.dangerously_push_entry(
            stat,
            oid,
            gix::index::entry::Flags::empty(),
            index_mode_for(kind),
            path.as_bstr(),
        );
    }
    private_index.sort_entries();

    ignored.sort();
    Ok((ignored, kept))
}

fn gix_pathbuf(path: &gix::bstr::BStr) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

/// Index mode spelling of a submodule gitlink (`160000`).
const GITLINK_MODE: &str = "160000";

/// Find the first submodule gitlink in `tree_id`, by tree entry kind —
/// active submodules are rejected whether or not `.gitmodules` exists.
fn find_gitlink_in_tree(
    repo: &gix::Repository,
    tree_id: gix::hash::ObjectId,
    prefix: &str,
) -> WorktreeResult<Option<String>> {
    let tree = repo.find_tree(tree_id).map_err(native_err)?;
    for entry in tree.iter() {
        let entry = entry.map_err(native_err)?;
        let path = if prefix.is_empty() {
            entry.filename().to_string()
        } else {
            format!("{prefix}/{}", entry.filename())
        };
        if entry.mode().kind() == gix::objs::tree::EntryKind::Commit {
            return Ok(Some(path));
        }
        if entry.mode().kind() == gix::objs::tree::EntryKind::Tree {
            if let Some(found) = find_gitlink_in_tree(repo, entry.oid().into(), &path)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// Reject active submodules: a gitlink in the HEAD tree or in the real
/// index (regardless of a `.gitmodules` file) cannot be materialized into a
/// private baseline in this version.
fn reject_gitlinks(repo: &gix::Repository, head_tree: gix::hash::ObjectId) -> WorktreeResult<()> {
    let submodule_error = |path: String| {
        WorktreeError::InvalidOperation(format!(
            "cannot capture baseline: submodules are unsupported in this version (gitlink at `{path}`)"
        ))
    };
    if let Some(path) = find_gitlink_in_tree(repo, head_tree, "")? {
        return Err(submodule_error(path));
    }
    if repo.index_path().exists() {
        let index = repo.open_index().map_err(native_err)?;
        for entry in index.entries() {
            if entry.stage() != gix::index::entry::Stage::Unconflicted {
                continue;
            }
            if format!("{:o}", entry.mode) == GITLINK_MODE {
                return Err(submodule_error(
                    entry.path(&index).to_str_lossy().into_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// Reject candidate paths whose effective attributes select an external
/// filter process, e.g. Git LFS (`filter=lfs`) or a custom clean/smudge
/// filter. Resolution uses gix's attribute stack — in-tree `.gitattributes`
/// files at any depth, `$GIT_DIR/info/attributes`, global attribute files,
/// and `[attr]` macros — never a root-file text scan, so support decisions
/// match Git's own attribute semantics.
fn reject_external_filter_paths(
    repo: &gix::Repository,
    private_index: &gix::index::State,
    kept: &std::collections::HashSet<gix::bstr::BString>,
) -> WorktreeResult<()> {
    if kept.is_empty() {
        return Ok(());
    }
    let mut stack = repo
        .attributes_only(
            private_index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
        )
        .map_err(native_err)?;
    let collection = stack.attributes_collection();
    let mut outcome = gix::attrs::search::Outcome::default();
    outcome.initialize_with_selection(collection, ["filter"]);
    for path in kept {
        let platform = stack
            .at_path(gix_pathbuf(path.as_bstr()), None)
            .map_err(native_err)?;
        if !platform.matching_attributes(&mut outcome) {
            continue;
        }
        for matched in outcome.iter_selected() {
            let assignment = &matched.assignment;
            if assignment.name.as_str() != "filter" {
                continue;
            }
            match &assignment.state {
                gix::attrs::StateRef::Value(value) if value.as_bstr() == b"lfs".as_bstr() => {
                    return Err(WorktreeError::InvalidOperation(format!(
                        "cannot capture baseline: Git LFS content is unsupported in this version \
                         (path `{}` resolves to filter=lfs)",
                        path.to_str_lossy()
                    )));
                }
                gix::attrs::StateRef::Value(value) => {
                    return Err(WorktreeError::InvalidOperation(format!(
                        "cannot capture baseline: external filter processes are unsupported in \
                         this version (path `{}` resolves to filter={})",
                        path.to_str_lossy(),
                        value.as_bstr().to_str_lossy()
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn is_executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

fn write_blob_from_disk(
    repo: &gix::Repository,
    abs: &Path,
    md: &std::fs::Metadata,
) -> WorktreeResult<gix::hash::ObjectId> {
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(abs).map_err(native_err)?;
        use std::os::unix::ffi::OsStrExt;
        return repo
            .write_blob(target.as_os_str().as_bytes())
            .map_err(native_err)
            .map(|id| id.detach());
    }
    let file = std::fs::File::open(abs).map_err(native_err)?;
    repo.write_blob_stream(std::io::BufReader::new(file))
        .map_err(native_err)
        .map(|id| id.detach())
}

fn index_mode_for(kind: FileKind) -> gix::index::entry::Mode {
    match kind {
        FileKind::Symlink => gix::index::entry::Mode::SYMLINK,
        FileKind::Executable => gix::index::entry::Mode::FILE_EXECUTABLE,
        FileKind::Regular => gix::index::entry::Mode::FILE,
    }
}

/// Write a tree object from the private index (staged snapshot semantics).
///
/// Only paths in `kept` are included: tracked files that still exist on
/// disk plus every unioned/untracked addition made during reconciliation.
fn write_tree_from_index(
    repo: &gix::Repository,
    index: &gix::index::State,
    kept: &std::collections::HashSet<gix::bstr::BString>,
) -> WorktreeResult<gix::hash::ObjectId> {
    #[derive(Default)]
    struct Dir {
        entries: std::collections::BTreeMap<gix::bstr::BString, TreeItem>,
    }
    enum TreeItem {
        File {
            oid: gix::hash::ObjectId,
            mode: gix::index::entry::Mode,
        },
        Dir(Dir),
    }

    let mut root = Dir::default();
    for entry in index.entries() {
        let path: gix::bstr::BString = entry.path(index).to_vec().into();
        if !kept.contains(&path) {
            continue;
        }
        let mut components: Vec<&gix::bstr::BStr> = path
            .as_bstr()
            .split(|byte| *byte == b'/')
            .map(gix::bstr::BStr::new)
            .collect();
        let file_name = components
            .pop()
            .ok_or_else(|| WorktreeError::InvalidOperation("empty index entry path".into()))?;
        let mut dir = &mut root;
        for component in components {
            let next = dir
                .entries
                .entry(component.to_owned())
                .or_insert_with(|| TreeItem::Dir(Dir::default()));
            match next {
                TreeItem::Dir(d) => dir = d,
                TreeItem::File { .. } => {
                    return Err(WorktreeError::InvalidOperation(
                        "index contains a file where a directory is required".into(),
                    ));
                }
            }
        }
        dir.entries.insert(
            file_name.to_owned(),
            TreeItem::File {
                oid: entry.id,
                mode: entry.mode,
            },
        );
    }

    fn write_dir(repo: &gix::Repository, dir: &Dir) -> WorktreeResult<gix::hash::ObjectId> {
        let mut entries: Vec<gix::objs::tree::Entry> = Vec::new();
        for (name, item) in &dir.entries {
            let (mode, oid) = match item {
                TreeItem::Dir(d) => (EntryKind::Tree, write_dir(repo, d)?),
                TreeItem::File { oid, mode } => {
                    let kind = if *mode == gix::index::entry::Mode::DIR {
                        EntryKind::Tree
                    } else if *mode == gix::index::entry::Mode::SYMLINK {
                        EntryKind::Link
                    } else if *mode == gix::index::entry::Mode::FILE_EXECUTABLE {
                        EntryKind::BlobExecutable
                    } else {
                        EntryKind::Blob
                    };
                    (kind, *oid)
                }
            };
            entries.push(gix::objs::tree::Entry {
                mode: mode.into(),
                filename: name.clone(),
                oid,
            });
        }
        let tree = gix::objs::Tree { entries };
        repo.write_object(&tree)
            .map_err(native_err)
            .map(|id| id.detach())
    }

    write_dir(repo, &root)
}

const INTERNAL_COMMIT_MESSAGE: &str = "QueryMT private baseline";
const INTERNAL_COMMIT_NAME: &str = "QueryMT";
const INTERNAL_COMMIT_EMAIL: &str = "querymt@localhost";

/// Validate a Git ref name without any command execution: no option-like
/// prefixes, no metacharacters, no ASCII control characters.
///
/// Managed refs never reach a shell; names are validated at the typed
/// boundary so malformed identifiers fail before any repository access.
/// The [`WorktreeError::GitCommand`] carrier is kept for message
/// compatibility: `argv` holds the rejected name, not a real command.
pub fn validate_ref_name(name: &str) -> WorktreeResult<()> {
    let invalid = |reason: &str| WorktreeError::GitCommand {
        argv: name.to_string(),
        status: "validation rejected".to_string(),
        stderr: format!("invalid ref name ({reason})"),
    };

    if name.is_empty() {
        return Err(invalid("empty"));
    }
    // Option injection: `-rf`, `--upload-pack=…`, …
    if name.starts_with('-') {
        return Err(invalid("must not start with '-'"));
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(invalid("must not start or end with '/'"));
    }
    if name.ends_with('.') {
        return Err(invalid("must not end with '.'"));
    }
    if name.ends_with(".lock") {
        return Err(invalid("must not end with '.lock'"));
    }
    if name.contains("..") {
        return Err(invalid("must not contain '..'"));
    }
    if name.contains("@{") {
        return Err(invalid("must not contain '@{'"));
    }
    if name.contains("//") {
        return Err(invalid("must not contain '//'"));
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(invalid("must not contain control characters"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(invalid(&format!("must not contain '{bad}'")));
    }
    Ok(())
}

/// Fault-injectable wrapper around any [`RepositoryOps`] implementation.
///
/// Tests arm deterministic failures at named operation boundaries so
/// allocation/capture/lifecycle error handling is exercised without
/// constructing argv, fabricating porcelain output, or depending on a Git
/// executable.
pub struct FaultInjectingRepoOps {
    inner: Arc<dyn RepositoryOps>,
    failures: std::sync::Mutex<std::collections::HashMap<FailurePoint, (usize, String)>>,
}

/// Named operation boundaries at which failures can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailurePoint {
    ResolveCommit,
    BranchTip,
    CreateBranch,
    DeleteBranch,
    DeleteBranchIfUnchanged,
    SetRef,
    CreateRefIfAbsent,
    ListWorktrees,
    AddWorktree,
    RemoveWorktree,
    PruneWorktrees,
    CheckoutStatus,
    HeadCommit,
    CurrentBranch,
    CommitsBetween,
    IsAncestor,
    ChangedPaths,
    CaptureWorkingState,
    IndexManifest,
    HasUnmergedEntries,
    SparseCheckoutEnabled,
}

impl FaultInjectingRepoOps {
    pub fn new(inner: Arc<dyn RepositoryOps>) -> Self {
        Self {
            inner,
            failures: std::sync::Mutex::new(Default::default()),
        }
    }

    /// Make the next `times` invocations of `point` fail with `message`.
    pub fn fail_next(&self, point: FailurePoint, times: usize, message: &str) {
        self.failures
            .lock()
            .expect("fault-injection mutex")
            .insert(point, (times.max(1), message.to_string()));
    }

    fn take_failure(&self, point: FailurePoint) -> Option<WorktreeError> {
        let mut failures = self.failures.lock().expect("fault-injection mutex");
        let (remaining, message) = failures.get(&point).cloned()?;
        if remaining <= 1 {
            failures.remove(&point);
        } else {
            failures.insert(point, (remaining - 1, message.clone()));
        }
        Some(WorktreeError::InvalidOperation(format!(
            "injected failure at {point:?}: {message}"
        )))
    }

    fn inner(&self) -> &Arc<dyn RepositoryOps> {
        &self.inner
    }
}

#[async_trait]
impl RepositoryOps for FaultInjectingRepoOps {
    async fn resolve_commit(
        &self,
        common_dir: &Path,
        expr: &str,
    ) -> WorktreeResult<Option<String>> {
        if let Some(err) = self.take_failure(FailurePoint::ResolveCommit) {
            return Err(err);
        }
        self.inner().resolve_commit(common_dir, expr).await
    }

    async fn branch_tip(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<Option<String>> {
        if let Some(err) = self.take_failure(FailurePoint::BranchTip) {
            return Err(err);
        }
        self.inner().branch_tip(common_dir, branch).await
    }

    async fn create_branch(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        target: &str,
    ) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::CreateBranch) {
            return Err(err);
        }
        self.inner().create_branch(common_dir, branch, target).await
    }

    async fn delete_branch(&self, common_dir: &Path, branch: &ManagedBranch) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::DeleteBranch) {
            return Err(err);
        }
        self.inner().delete_branch(common_dir, branch).await
    }

    async fn delete_branch_if_unchanged(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        expected_target: &str,
    ) -> WorktreeResult<bool> {
        if let Some(err) = self.take_failure(FailurePoint::DeleteBranchIfUnchanged) {
            return Err(err);
        }
        self.inner()
            .delete_branch_if_unchanged(common_dir, branch, expected_target)
            .await
    }

    async fn set_ref(&self, common_dir: &Path, ref_name: &str, commit: &str) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::SetRef) {
            return Err(err);
        }
        self.inner().set_ref(common_dir, ref_name, commit).await
    }

    async fn create_ref_if_absent(
        &self,
        common_dir: &Path,
        ref_name: &str,
        commit: &str,
    ) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::CreateRefIfAbsent) {
            return Err(err);
        }
        self.inner()
            .create_ref_if_absent(common_dir, ref_name, commit)
            .await
    }

    async fn list_worktrees(&self, common_dir: &Path) -> WorktreeResult<Vec<RegisteredWorktree>> {
        if let Some(err) = self.take_failure(FailurePoint::ListWorktrees) {
            return Err(err);
        }
        self.inner().list_worktrees(common_dir).await
    }

    async fn add_worktree(
        &self,
        common_dir: &Path,
        path: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::AddWorktree) {
            return Err(err);
        }
        self.inner().add_worktree(common_dir, path, branch).await
    }

    async fn remove_worktree(&self, common_dir: &Path, path: &Path) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::RemoveWorktree) {
            return Err(err);
        }
        self.inner().remove_worktree(common_dir, path).await
    }

    async fn prune_worktrees(&self, common_dir: &Path) -> WorktreeResult<()> {
        if let Some(err) = self.take_failure(FailurePoint::PruneWorktrees) {
            return Err(err);
        }
        self.inner().prune_worktrees(common_dir).await
    }

    async fn checkout_status(&self, checkout: &Path) -> WorktreeResult<CheckoutStatus> {
        if let Some(err) = self.take_failure(FailurePoint::CheckoutStatus) {
            return Err(err);
        }
        self.inner().checkout_status(checkout).await
    }

    async fn head_commit(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
        if let Some(err) = self.take_failure(FailurePoint::HeadCommit) {
            return Err(err);
        }
        self.inner().head_commit(checkout).await
    }

    async fn current_branch(&self, checkout: &Path) -> WorktreeResult<Option<String>> {
        if let Some(err) = self.take_failure(FailurePoint::CurrentBranch) {
            return Err(err);
        }
        self.inner().current_branch(checkout).await
    }

    async fn commits_between_count(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<usize> {
        if let Some(err) = self.take_failure(FailurePoint::CommitsBetween) {
            return Err(err);
        }
        self.inner()
            .commits_between_count(common_dir, base, tip)
            .await
    }

    async fn commits_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<String>> {
        if let Some(err) = self.take_failure(FailurePoint::CommitsBetween) {
            return Err(err);
        }
        self.inner().commits_between(common_dir, base, tip).await
    }

    async fn is_ancestor(
        &self,
        common_dir: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> WorktreeResult<bool> {
        if let Some(err) = self.take_failure(FailurePoint::IsAncestor) {
            return Err(err);
        }
        self.inner()
            .is_ancestor(common_dir, ancestor, descendant)
            .await
    }

    async fn changed_paths_between(
        &self,
        common_dir: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<ChangedPath>> {
        if let Some(err) = self.take_failure(FailurePoint::ChangedPaths) {
            return Err(err);
        }
        self.inner().changed_paths_between(common_dir, base, tip).await
    }

    async fn capture_working_state(&self, checkout: &Path) -> WorktreeResult<CapturedWorkingState> {
        if let Some(err) = self.take_failure(FailurePoint::CaptureWorkingState) {
            return Err(err);
        }
        self.inner().capture_working_state(checkout).await
    }

    async fn index_manifest(&self, checkout: &Path) -> WorktreeResult<Vec<u8>> {
        if let Some(err) = self.take_failure(FailurePoint::IndexManifest) {
            return Err(err);
        }
        self.inner().index_manifest(checkout).await
    }

    async fn has_unmerged_entries(&self, checkout: &Path) -> WorktreeResult<bool> {
        if let Some(err) = self.take_failure(FailurePoint::HasUnmergedEntries) {
            return Err(err);
        }
        self.inner().has_unmerged_entries(checkout).await
    }

    async fn sparse_checkout_enabled(&self, checkout: &Path) -> WorktreeResult<bool> {
        if let Some(err) = self.take_failure(FailurePoint::SparseCheckoutEnabled) {
            return Err(err);
        }
        self.inner().sparse_checkout_enabled(checkout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::test_support::{commit_files, init_repo};

    /// Inherited Git environment overrides must not redirect native
    /// operations: every op opens the explicit checkout path and derives
    /// the index from that repository rather than from GIT_DIR,
    /// GIT_WORK_TREE, or GIT_INDEX_FILE.
    #[tokio::test]
    async fn inherited_git_env_overrides_do_not_redirect_native_ops() {
        let repo = init_repo();
        commit_files(repo.path(), "initial", &[("README.md", "hello\n")]);
        std::fs::write(repo.path().join("untracked.txt"), "x").unwrap();

        let saved: Vec<(&str, Option<String>)> = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"]
            .map(|name| {
                // SAFETY: test process; no other thread reads these keys.
                (name, unsafe { std::env::var(name).ok() })
            })
            .into();
        // SAFETY: single-threaded mutation for this test's duration; native
        // operations ignore these variables (that is what is under test).
        unsafe {
            std::env::set_var("GIT_DIR", "/nonexistent/qmt-git-dir");
            std::env::set_var("GIT_WORK_TREE", "/nonexistent/qmt-worktree");
            std::env::set_var("GIT_INDEX_FILE", "/nonexistent/qmt-index");
        }

        let ops = GixRepoOps::new();
        let head = ops.head_commit(repo.path()).await.unwrap();
        let status = ops.checkout_status(repo.path()).await.unwrap();

        // SAFETY: restoring the process environment right after the probe so
        // concurrent tests never observe the bogus overrides, even if the
        // assertions below fail.
        unsafe {
            for (name, value) in &saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }

        assert!(head.is_some(), "HEAD resolves despite bogus GIT_DIR");
        assert!(
            status.untracked.iter().any(|p| p == "untracked.txt"),
            "untracked seen despite overrides: {status:?}"
        );
        // Note: the gix-native fixture writes no index, so the staged
        // comparison legitimately reports HEAD files as index deletions;
        // asserting an empty `uncommitted` would test fixture semantics,
        // not env isolation.
    }
}
