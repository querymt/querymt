//! Canonical repository and checkout identity resolution.
//!
//! - Repository identity derives from the canonical Git **common directory**,
//!   so every linked worktree of one project shares a single project
//!   identity while keeping distinct checkout identities.
//! - Checkout identity is the actual working-tree root of the checkout being
//!   resolved (main checkout or linked worktree) plus whether it is linked.
//! - Relative-cwd resolution maps a session cwd onto checkout-relative
//!   coordinates and validates that the mapped directory exists in a target
//!   checkout (subdirectory sessions fail explicitly when the baseline lacks
//!   their directory).
//!
//! Git operations here go through `gix` only: `gix::discover` for discovery
//! and `gix` repository accessors for `git_dir` / `common_dir`. No Git CLI.

use crate::worktree::domain::RepositoryIdentity;
use crate::worktree::error::{WorktreeError, WorktreeResult};
use std::path::{Path, PathBuf};

/// Identity of the checkout that a session actually executes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// Canonical absolute path of the working-tree root (main checkout or
    /// linked worktree).
    pub root: PathBuf,
    /// Canonical absolute path of this checkout's Git administrative
    /// directory (for linked worktrees this lives under
    /// `<common_dir>/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// Whether this checkout is a linked worktree rather than the main one.
    pub is_linked_worktree: bool,
}

/// A resolved Git checkout: project identity plus checkout identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepository {
    /// Project identity shared by all checkouts of the repository.
    pub identity: RepositoryIdentity,
    /// Identity of the resolved checkout.
    pub checkout: CheckoutIdentity,
}

/// Resolve the repository and checkout containing `cwd`.
///
/// Discovery walks upward from `cwd`; resolution fails for non-Git
/// directories and bare repositories (explicitly unsupported in this
/// version). All returned paths are canonicalized.
pub fn resolve_repository(cwd: &Path) -> WorktreeResult<ResolvedRepository> {
    let repo = gix::discover(cwd).map_err(|err| match err {
        gix::discover::Error::Discover(e) if is_no_repository(&e) => {
            WorktreeError::NotGitRepository {
                path: cwd.to_path_buf(),
            }
        }
        other => WorktreeError::GitDiscovery(other.to_string()),
    })?;

    let work_dir = repo
        .workdir()
        .ok_or_else(|| WorktreeError::BareRepositoryUnsupported {
            path: cwd.to_path_buf(),
        })?;

    let root = std::fs::canonicalize(work_dir)?;
    let git_dir = std::fs::canonicalize(repo.git_dir())?;
    let common_dir = std::fs::canonicalize(repo.common_dir())?;
    let is_linked_worktree = git_dir != common_dir;

    Ok(ResolvedRepository {
        identity: RepositoryIdentity {
            common_dir,
            origin_path: root.clone(),
            project_name: RepositoryIdentity::project_name_from_origin(&root),
        },
        checkout: CheckoutIdentity {
            root,
            git_dir,
            is_linked_worktree,
        },
    })
}

/// Classify gix-discover 0.48 (paired with gix 0.80) upwards-discovery
/// errors that mean "no usable repository here".
///
/// gix 0.80 predates the `gix::discover::is_not_found` helper (added by
/// 0.85-era releases), so the variants are matched explicitly; revisit when
/// the crate upgrades past 0.84.
fn is_no_repository(err: &gix::discover::upwards::Error) -> bool {
    use gix::discover::upwards::Error as E;
    matches!(
        err,
        E::NoGitRepository { .. }
            | E::NoGitRepositoryWithinCeiling { .. }
            | E::NoGitRepositoryWithinFs { .. }
            | E::NoTrustedGitRepository { .. }
    )
}

/// Resolve `cwd` relative to `checkout_root` (both canonicalized).
///
/// `cwd` being exactly the checkout root yields an empty relative path;
/// anything outside the checkout errors.
pub fn relative_cwd(checkout_root: &Path, cwd: &Path) -> WorktreeResult<PathBuf> {
    let root = std::fs::canonicalize(checkout_root)?;
    let cwd = std::fs::canonicalize(cwd)?;
    cwd.strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| WorktreeError::RelativeCwdOutsideCheckout { cwd, root })
}

/// Validate that `relative_cwd` exists in `checkout_root`.
///
/// Used before launching an isolated session: a subdirectory session whose
/// directory is absent from the selected baseline fails explicitly instead
/// of silently executing at the checkout root.
pub fn ensure_relative_cwd_exists(checkout_root: &Path, relative_cwd: &Path) -> WorktreeResult<()> {
    if relative_cwd.as_os_str().is_empty() {
        return Ok(());
    }
    let target = checkout_root.join(relative_cwd);
    if target.is_dir() {
        Ok(())
    } else {
        Err(WorktreeError::RelativeCwdAbsent {
            relative: relative_cwd.to_path_buf(),
            root: checkout_root.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    // -- gix-only fixtures ---------------------------------------------------
    //
    // All repository fixtures are built with `gix` object APIs and plain
    // filesystem writes; no Git CLI is invoked. gix 0.80 has no
    // linked-worktree creation API (that arrives with the task 2.1 Git
    // administration adapter), so the linked-worktree fixture writes the
    // standard worktree layout gix itself reads back during discovery.

    /// Initialize a non-bare repository with a commit identity in its config
    /// (mirroring the snapshot backend's config handling).
    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        gix::init(tmp.path()).unwrap();
        let config_path = tmp.path().join(".git/config");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
        fs::write(&config_path, config).unwrap();
        tmp
    }

    /// Recursively write nested trees for `(relative_path, blob)` entries and
    /// return the root tree id.
    fn write_tree_recursive(
        repo: &gix::Repository,
        entries: Vec<(String, gix::ObjectId)>,
    ) -> gix::ObjectId {
        let mut blobs: Vec<(String, gix::ObjectId)> = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<(String, gix::ObjectId)>> = BTreeMap::new();

        for (path, oid) in entries {
            if let Some((dir, rest)) = path.split_once('/') {
                subdirs
                    .entry(dir.to_string())
                    .or_default()
                    .push((rest.to_string(), oid));
            } else {
                blobs.push((path, oid));
            }
        }

        let mut tree_entries: Vec<gix::objs::tree::Entry> = Vec::new();

        for (dir_name, sub_entries) in subdirs {
            let sub_tree_id = write_tree_recursive(repo, sub_entries);
            tree_entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: dir_name.as_str().into(),
                oid: sub_tree_id,
            });
        }

        for (name, oid) in blobs {
            tree_entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: name.as_str().into(),
                oid,
            });
        }

        tree_entries.sort();
        repo.write_object(&gix::objs::Tree {
            entries: tree_entries,
        })
        .unwrap()
        .detach()
    }

    /// Write `files` as blobs/trees and commit them on the current HEAD,
    /// returning the new commit id.
    fn commit_files(root: &Path, message: &str, files: &[(&str, &str)]) -> gix::ObjectId {
        let repo = gix::open(root).unwrap();
        let entries: Vec<(String, gix::ObjectId)> = files
            .iter()
            .map(|(path, content)| {
                let oid = repo.write_blob(content.as_bytes()).unwrap().detach();
                (path.to_string(), oid)
            })
            .collect();

        let tree_id = write_tree_recursive(&repo, entries);

        let parent_ids: Vec<gix::ObjectId> = match repo.head_commit() {
            Ok(commit) => vec![commit.id().detach()],
            Err(_) => Vec::new(),
        };

        repo.commit("HEAD", message, tree_id, parent_ids)
            .unwrap()
            .detach()
    }

    /// Write `files` into the working tree at `root` (gix fixtures create
    /// objects in the ODB only; the checkout must be materialized too).
    fn materialize(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(target, content).unwrap();
        }
    }

    /// Standard content history: commit 1 has `file.txt`; commit 2 adds
    /// `sub/file.txt`. Both checkouts are materialized on disk. Returns the
    /// temp dir and both commit ids.
    fn init_repo_with_subdir() -> (TempDir, gix::ObjectId, gix::ObjectId) {
        let tmp = init_repo();
        let first = commit_files(tmp.path(), "first", &[("file.txt", "one")]);
        materialize(tmp.path(), &[("file.txt", "one")]);
        let second = commit_files(
            tmp.path(),
            "add sub",
            &[("file.txt", "one"), ("sub/file.txt", "sub")],
        );
        materialize(tmp.path(), &[("file.txt", "one"), ("sub/file.txt", "sub")]);
        (tmp, first, second)
    }

    /// Assemble a linked-worktree checkout at `wt_root` attached to
    /// `main_root` with the given branch and commit, materializing `files`.
    fn create_linked_worktree(
        main_root: &Path,
        wt_root: &Path,
        branch: &str,
        commit: gix::ObjectId,
        files: &[(&str, &str)],
    ) {
        let admin = main_root.join(".git/worktrees").join(branch);
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt_root.join(".git").display()),
        )
        .unwrap();

        let refs_dir = main_root.join(".git/refs/heads");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join(branch), format!("{}\n", commit)).unwrap();

        fs::create_dir_all(wt_root).unwrap();
        fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();

        for (path, content) in files {
            let target = wt_root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(target, content).unwrap();
        }
    }

    // -- assertions ----------------------------------------------------------

    #[test]
    fn resolves_from_repository_root() {
        let (tmp, _, _) = init_repo_with_subdir();
        let root = tmp.path();
        let canonical_root = fs::canonicalize(root).unwrap();

        let resolved = resolve_repository(root).unwrap();
        assert_eq!(resolved.checkout.root, canonical_root);
        assert!(!resolved.checkout.is_linked_worktree);
        assert_eq!(
            resolved.checkout.git_dir,
            fs::canonicalize(root.join(".git")).unwrap()
        );
        assert_eq!(resolved.identity.common_dir, resolved.checkout.git_dir);
        assert_eq!(resolved.identity.origin_path, canonical_root);
        assert_eq!(
            resolved.identity.project_name,
            root.file_name().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn resolves_from_subdirectory_with_same_identity() {
        let (tmp, _, _) = init_repo_with_subdir();
        let root = tmp.path();
        let sub = root.join("sub");

        let from_root = resolve_repository(root).unwrap();
        let from_sub = resolve_repository(&sub).unwrap();

        // Project identity is shared; checkout root is still the repo root.
        assert_eq!(from_sub.identity, from_root.identity);
        assert_eq!(from_sub.checkout.root, from_root.checkout.root);

        let rel = relative_cwd(&from_sub.checkout.root, &sub).unwrap();
        assert_eq!(rel, PathBuf::from("sub"));
        assert!(ensure_relative_cwd_exists(&from_sub.checkout.root, &rel).is_ok());
    }

    #[test]
    fn resolves_existing_linked_worktree_with_shared_project_identity() {
        let (tmp, _, head) = init_repo_with_subdir();
        let root = tmp.path();
        let wt = root.join("wt");
        create_linked_worktree(
            root,
            &wt,
            "wt-branch",
            head,
            &[("file.txt", "one"), ("sub/file.txt", "sub")],
        );

        let main = resolve_repository(root).unwrap();
        let linked = resolve_repository(&wt).unwrap();

        // Same project (canonical common dir), distinct checkout.
        assert_eq!(linked.identity.common_dir, main.identity.common_dir);
        assert_ne!(linked.checkout.root, main.checkout.root);
        assert!(linked.checkout.is_linked_worktree);
        assert!(!main.checkout.is_linked_worktree);
        assert!(
            linked
                .checkout
                .git_dir
                .starts_with(main.identity.common_dir.join("worktrees"))
        );

        // Relative paths resolve inside the linked checkout too.
        let sub = linked.checkout.root.join("sub");
        let rel = relative_cwd(&linked.checkout.root, &sub).unwrap();
        assert_eq!(rel, PathBuf::from("sub"));
        assert!(ensure_relative_cwd_exists(&linked.checkout.root, &rel).is_ok());
    }

    #[test]
    fn absent_baseline_subdirectory_fails_explicitly() {
        let (tmp, first, _) = init_repo_with_subdir();
        let root = tmp.path();

        // Baseline predates `sub/`.
        let wt = root.join("old-wt");
        create_linked_worktree(root, &wt, "old-wt-branch", first, &[("file.txt", "one")]);

        let linked = resolve_repository(&wt).unwrap();
        let err = ensure_relative_cwd_exists(&linked.checkout.root, Path::new("sub")).unwrap_err();
        assert!(
            matches!(err, WorktreeError::RelativeCwdAbsent { ref relative, .. } if relative == &PathBuf::from("sub")),
            "unexpected error: {err:?}"
        );

        // The checkout root itself is always valid.
        assert!(ensure_relative_cwd_exists(&linked.checkout.root, Path::new("")).is_ok());
    }

    #[test]
    fn non_git_directories_are_rejected() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("plain")).unwrap();

        let err = resolve_repository(tmp.path()).unwrap_err();
        assert!(
            matches!(err, WorktreeError::NotGitRepository { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn bare_repositories_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("bare.git");
        gix::init_bare(&bare).unwrap();

        let err = resolve_repository(&bare).unwrap_err();
        assert!(
            matches!(err, WorktreeError::BareRepositoryUnsupported { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn cwd_outside_checkout_root_is_rejected() {
        let (tmp, _, _) = init_repo_with_subdir();
        let other = TempDir::new().unwrap();

        let resolved = resolve_repository(tmp.path()).unwrap();
        let err = relative_cwd(&resolved.checkout.root, other.path()).unwrap_err();
        assert!(
            matches!(err, WorktreeError::RelativeCwdOutsideCheckout { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn checkout_root_resolves_to_empty_relative_cwd() {
        let (tmp, _, _) = init_repo_with_subdir();
        let resolved = resolve_repository(tmp.path()).unwrap();
        let rel = relative_cwd(&resolved.checkout.root, resolved.checkout.root.as_path()).unwrap();
        assert!(rel.as_os_str().is_empty());
    }
}
