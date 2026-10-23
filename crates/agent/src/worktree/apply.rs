//! Applying a prepared integration to the working tree (step 5 of design
//! decision 9).
//!
//! The transaction first persists its *intent* in the checkout's per-worktree
//! Git directory (`<git-dir>/qmt-integration-intent.json`: operation id, the
//! `HEAD` and branch it started from, the merged tree, the touched paths and
//! the `index.lock` it holds). Only then are files written, so a crash at any
//! later point leaves enough information for the next acceptance to roll the
//! touched paths back or to finish the index update (task 22.8).
//!
//! Files are written through gix's worktree checkout, restricted to exactly
//! the touched entries: the merged tree is turned into an in-memory index,
//! every other entry is marked `SKIP_WORKTREE` for the checkout (the entries
//! stay in the index so `.gitattributes` files of the merged tree still
//! resolve), and the repository's own checkout options supply the filter
//! pipeline (`text`/`eol`/`autocrlf`), executable-bit and symlink handling.
//! Deletions remove the file and any directory they leave empty.
//!
//! Touched paths were proven free of local work by
//! [`crate::worktree::protect`] while the real `index.lock` was held, so they
//! are cleared before the checkout and re-created exclusively
//! (`destination_is_initially_empty`): a file that appears in between is a
//! collision and fails the operation instead of being overwritten. Any
//! failure restores the touched paths to `HEAD` ([`rollback_touched`]) and
//! removes the intent; the branch and the real index contents are never
//! changed by this phase.

use crate::worktree::domain::{ChangeKind, ChangedPath};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::integrate::{AppliedIntegration, ApplyOutcome, PreparedIntegration, Refusal};
use crate::worktree::repo_ops::{AttributeSource, native_err, open_acceptance_checkout, parse_oid};
use gix::bstr::BString;
use gix::hash::ObjectId;
use gix::index::entry::{Flags, Mode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Name of the intent file inside the per-worktree Git directory.
pub(crate) const INTENT_FILE: &str = "qmt-integration-intent.json";

/// What an in-flight acceptance is about to do to a checkout. Persisted
/// before the first file is written and removed when the transaction ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IntegrationIntent {
    pub version: u32,
    pub operation_id: String,
    /// Working-tree root (informational; recovery locates the intent from
    /// the checkout it opens).
    pub checkout: String,
    /// `HEAD` commit the transaction started on.
    pub head: String,
    /// Short name of the branch that would advance.
    pub branch: String,
    pub head_tree: String,
    pub merged_tree: String,
    /// `index.lock` the transaction holds; proves QueryMT owns that lock.
    pub index_lock: String,
    /// Identity of that lock file when it was taken (see
    /// [`crate::worktree::protect::lock_identity`]). Recovery removes a
    /// surviving lock only when it still has this identity.
    #[serde(default)]
    pub index_lock_identity: Option<String>,
    /// Paths written by the transaction, `HEAD -> merged tree`.
    pub touched: Vec<ChangedPath>,
    /// The integration commit, recorded before the branch is updated so a
    /// crash can tell "branch still at `head`" (roll back) from "branch at
    /// the commit" (finish the index update).
    #[serde(default)]
    pub commit: Option<String>,
    /// Paths whose real-index entries the commit replaces: the touched paths
    /// plus delta paths a hook re-staged. Set together with `commit`.
    #[serde(default)]
    pub index_paths: Vec<String>,
    /// Create-only record ref written by the same reference transaction as
    /// the branch update. Set together with `commit`; recovery uses it to
    /// repair or remove a record left by a half-finished transaction.
    #[serde(default)]
    pub integration_ref: Option<String>,
}

pub(crate) fn intent_path(git_dir: &Path) -> PathBuf {
    git_dir.join(INTENT_FILE)
}

/// The persisted intent of `git_dir`, if any.
pub(crate) fn read_intent(git_dir: &Path) -> WorktreeResult<Option<IntegrationIntent>> {
    match std::fs::read(intent_path(git_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|err| {
            WorktreeError::InvalidOperation(format!(
                "the integration intent at {} is unreadable: {err}",
                intent_path(git_dir).display()
            ))
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Durably write the intent: temp file, `fsync`, rename, `fsync` of the
/// directory (best effort), so a crash never leaves a torn intent.
pub(crate) fn write_intent(git_dir: &Path, intent: &IntegrationIntent) -> WorktreeResult<()> {
    let bytes = serde_json::to_vec_pretty(intent).map_err(native_err)?;
    let tmp = git_dir.join(format!("{INTENT_FILE}.tmp"));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, intent_path(git_dir))?;
        if let Ok(dir) = std::fs::File::open(git_dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(err) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

pub(crate) fn remove_intent(git_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(intent_path(git_dir)) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Persist the intent and write the touched entries; see the module docs.
pub(crate) fn apply_blocking(prepared: PreparedIntegration) -> WorktreeResult<ApplyOutcome> {
    let repo = open_acceptance_checkout(&prepared.checkout)?;
    let git_dir = repo.git_dir().to_path_buf();

    // A leftover intent belongs to an earlier, unfinished transaction; it
    // must be recovered first, never overwritten.
    if intent_path(&git_dir).exists() {
        return Ok(ApplyOutcome::Refused(Refusal::new(
            "integration_pending",
            format!(
                "{} exists: an earlier acceptance on this checkout did not finish",
                intent_path(&git_dir).display()
            ),
        )));
    }
    // The branch must still be where preparation saw it.
    let head = repo.head_id().map_err(native_err)?.detach();
    if head != parse_oid(&prepared.head)? {
        return Ok(ApplyOutcome::Refused(Refusal::new(
            "branch_moved",
            format!(
                "the branch moved from {} to {head} while the integration was prepared",
                prepared.head
            ),
        )));
    }

    let head_tree = parse_oid(&prepared.head_tree)?;
    let merged_tree = parse_oid(&prepared.merged_tree)?;
    let operation_id = uuid::Uuid::new_v4().to_string();
    write_intent(
        &git_dir,
        &IntegrationIntent {
            version: 1,
            operation_id: operation_id.clone(),
            checkout: prepared.checkout.to_string_lossy().into_owned(),
            head: prepared.head.clone(),
            branch: prepared.branch.clone(),
            head_tree: prepared.head_tree.clone(),
            merged_tree: prepared.merged_tree.clone(),
            index_lock: prepared.lock.path().to_string_lossy().into_owned(),
            index_lock_identity: crate::worktree::protect::lock_identity(prepared.lock.path()),
            touched: prepared.touched.clone(),
            commit: None,
            index_paths: Vec::new(),
            integration_ref: None,
        },
    )?;

    let wanted: BTreeSet<BString> = prepared
        .touched
        .iter()
        .filter(|change| change.kind != ChangeKind::Deleted)
        .map(|change| BString::from(change.path.as_str()))
        .collect();
    let applied = clear_paths(&prepared.checkout, &prepared.touched)
        .and_then(|()| checkout_entries(&repo, merged_tree, &wanted));
    if let Err(err) = applied {
        // Nothing but the touched paths changed, and they were clean.
        return match rollback_touched(&repo, head_tree, &prepared.touched) {
            Ok(()) => {
                if let Err(remove_err) = remove_intent(&git_dir) {
                    tracing::warn!(error = %remove_err, "could not remove the integration intent after rollback");
                }
                Err(err)
            }
            Err(rollback_err) => Err(WorktreeError::InvalidOperation(format!(
                "{err}; restoring the touched paths also failed ({rollback_err}); the intent at \
                 {} is kept so the next acceptance can recover",
                intent_path(&git_dir).display()
            ))),
        };
    }

    let written = wanted
        .iter()
        .filter_map(|path| {
            let path = path.to_string();
            crate::worktree::finalize::current_stat(&prepared.checkout.join(&path))
                .map(|stat| (path, stat))
        })
        .collect();
    Ok(ApplyOutcome::Applied(Box::new(AppliedIntegration {
        intent_path: intent_path(&git_dir),
        operation_id,
        prepared,
        written,
    })))
}

/// Undo an applied integration in the same process, *while still holding*
/// its `index.lock` (so nothing can slip in between the failure and the
/// rollback): restore the touched paths to `HEAD`, remove the scratch files
/// and the intent, then release the lock. The touched files were written by
/// this very transaction (and possibly changed by its own hooks), so they
/// are restored without the content check crash recovery applies. When the
/// restore fails the intent is kept for the next acceptance.
pub(crate) fn abandon_applied(applied: AppliedIntegration) -> WorktreeResult<()> {
    let AppliedIntegration {
        prepared,
        operation_id,
        ..
    } = applied;
    let repo = open_acceptance_checkout(&prepared.checkout)?;
    let git_dir = repo.git_dir().to_path_buf();
    rollback_touched(&repo, parse_oid(&prepared.head_tree)?, &prepared.touched).map_err(|err| {
        WorktreeError::InvalidOperation(format!(
            "restoring the touched paths failed ({err}); the intent at {} is kept so the next \
             acceptance can recover",
            intent_path(&git_dir).display()
        ))
    })?;
    let prefix = format!("qmt-integration-{operation_id}.");
    if let Ok(entries) = std::fs::read_dir(&git_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    remove_intent(&git_dir)?;
    drop(prepared.lock);
    Ok(())
}

/// Restore every touched path to its state in `head_tree`: clear what is on
/// disk, then check the `HEAD` entries of those paths out again. Idempotent,
/// so recovery can repeat it after a crash. The touched paths carried no
/// user work when the transaction started.
pub(crate) fn rollback_touched(
    repo: &gix::Repository,
    head_tree: ObjectId,
    touched: &[ChangedPath],
) -> WorktreeResult<()> {
    clear_paths(workdir(repo)?, touched)?;
    let wanted: BTreeSet<BString> = touched
        .iter()
        .map(|change| BString::from(change.path.as_str()))
        .collect();
    checkout_entries(repo, head_tree, &wanted)
}

pub(crate) fn workdir(repo: &gix::Repository) -> WorktreeResult<&Path> {
    repo.workdir()
        .ok_or_else(|| WorktreeError::InvalidOperation("the target has no working tree".into()))
}

/// Remove the files and symlinks at the touched paths and then every
/// directory that became empty. Directories at touched paths are left alone
/// (they hold something this transaction did not put there), and nothing is
/// removed below a symlinked directory.
fn clear_paths(root: &Path, touched: &[ChangedPath]) -> WorktreeResult<()> {
    let mut parents = BTreeSet::new();
    for change in touched {
        let relative = PathBuf::from(&change.path);
        if has_symlinked_ancestor(root, &relative) {
            continue;
        }
        let absolute = root.join(&relative);
        let ancestors = || {
            relative
                .ancestors()
                .skip(1)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .collect::<Vec<_>>()
        };
        match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) if !metadata.is_dir() => {
                std::fs::remove_file(&absolute)?;
                parents.extend(ancestors());
            }
            Ok(_) => {}
            // Already gone (removed by hand after a crash): directories it
            // leaves empty are still cleaned up.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                parents.extend(ancestors());
            }
            Err(err) => return Err(err.into()),
        }
    }
    // Children sort after their parents, so reverse order empties bottom-up.
    for dir in parents.iter().rev() {
        let _ = std::fs::remove_dir(root.join(dir));
    }
    Ok(())
}

fn has_symlinked_ancestor(root: &Path, relative: &Path) -> bool {
    relative
        .ancestors()
        .skip(1)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| {
            std::fs::symlink_metadata(root.join(dir))
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
        })
}

/// Check the entries of `tree` named in `wanted` out into the working tree
/// using the repository's checkout options, leaving all other paths alone.
fn checkout_entries(
    repo: &gix::Repository,
    tree: ObjectId,
    wanted: &BTreeSet<BString>,
) -> WorktreeResult<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let root = workdir(repo)?.to_path_buf();
    let mut index = gix::index::State::from_tree(&tree, &repo.objects, Default::default())
        .map_err(native_err)?;
    // Every other entry stays in the index (its `.gitattributes` blobs feed
    // the attribute stack) but is skipped by the checkout. Submodule
    // entries are never written.
    let mut selected = 0usize;
    for (entry, path) in index.entries_mut_with_paths() {
        if entry.mode != Mode::COMMIT && wanted.contains(path) {
            selected += 1;
        } else {
            entry.flags.insert(Flags::SKIP_WORKTREE | Flags::EXTENDED);
        }
    }
    if selected == 0 {
        return Ok(());
    }

    let mut options = repo
        .checkout_options(AttributeSource::IdMapping)
        .map_err(native_err)?;
    options.destination_is_initially_empty = true;
    options.overwrite_existing = false;
    options.keep_going = false;
    let objects = repo.objects.clone().into_arc().map_err(native_err)?;
    let interrupt = AtomicBool::new(false);
    let outcome = gix::worktree::state::checkout(
        &mut index,
        root,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &interrupt,
        options,
    )
    .map_err(native_err)?;

    if !outcome.collisions.is_empty() || !outcome.errors.is_empty() {
        let mut problems: Vec<String> = outcome
            .collisions
            .iter()
            .map(|collision| format!("{} ({:?})", collision.path, collision.error_kind))
            .collect();
        problems.extend(
            outcome
                .errors
                .iter()
                .map(|record| format!("{}: {}", record.path, record.error)),
        );
        return Err(WorktreeError::InvalidOperation(format!(
            "could not write {} of the touched paths: {}",
            problems.len(),
            problems.join("; ")
        )));
    }
    if !outcome.delayed_paths_unknown.is_empty() || !outcome.delayed_paths_unprocessed.is_empty() {
        return Err(WorktreeError::InvalidOperation(
            "a filter process left touched paths unwritten".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::worktree::integrate::{PrepareOutcome, PrepareRequest};
    use crate::worktree::prepare::prepare_blocking;
    use crate::worktree::repo_ops::{
        FailurePoint, FaultInjectingRepoOps, GixRepoOps, RepositoryOps, internal_signature,
    };
    use crate::worktree::test_support::{init_repo, materialize, write_index_from_head};
    use gix::objs::tree::EntryKind;
    use std::sync::Arc;

    const BASE: &[(&str, &str)] = &[
        (".gitattributes", "*.crlf text eol=crlf\n"),
        ("keep.txt", "keep\n"),
        ("gone.txt", "bye\n"),
        ("mod.crlf", "old\n"),
        ("sub/stay.txt", "stay\n"),
        ("plain.txt", "plain\n"),
    ];

    fn tree_of(repo: &gix::Repository, entries: &[(&str, EntryKind, &[u8])]) -> ObjectId {
        let mut editor = repo
            .edit_tree(ObjectId::empty_tree(repo.object_hash()))
            .unwrap();
        for (path, kind, content) in entries {
            let id = repo.write_blob(*content).unwrap().detach();
            editor.upsert(*path, *kind, id).unwrap();
        }
        editor.write().unwrap().detach()
    }

    fn base_entries() -> Vec<(&'static str, EntryKind, &'static [u8])> {
        BASE.iter()
            .map(|(path, content)| (*path, EntryKind::Blob, content.as_bytes()))
            .collect()
    }

    fn detached_commit(repo: &gix::Repository, tree: ObjectId, parent: ObjectId) -> ObjectId {
        repo.write_object(&gix::objs::Commit {
            tree,
            parents: vec![parent].into(),
            author: internal_signature(),
            committer: internal_signature(),
            encoding: None,
            message: "result".into(),
            extra_headers: Default::default(),
        })
        .unwrap()
        .detach()
    }

    /// A checkout at `B0`, and a result `R` that edits an eol-filtered file,
    /// deletes a file, adds an executable, a symlink, and a file in a new
    /// directory.
    pub(crate) fn fixture() -> (tempfile::TempDir, ObjectId, ObjectId) {
        let dir = init_repo();
        let repo = gix::open(dir.path()).unwrap();
        let base_tree = tree_of(&repo, &base_entries());
        let base = repo
            .commit("HEAD", "base", base_tree, Vec::<ObjectId>::new())
            .unwrap()
            .detach();
        materialize(dir.path(), BASE);
        write_index_from_head(dir.path());

        let mut result: Vec<(&str, EntryKind, &[u8])> = base_entries()
            .into_iter()
            .filter(|(path, _, _)| *path != "gone.txt" && *path != "mod.crlf")
            .collect();
        result.push(("mod.crlf", EntryKind::Blob, b"new\nline\n"));
        result.push(("run.sh", EntryKind::BlobExecutable, b"#!/bin/sh\necho hi\n"));
        result.push(("link", EntryKind::Link, b"keep.txt"));
        result.push(("new/dir/file.txt", EntryKind::Blob, b"added\n"));
        let result_tree = tree_of(&repo, &result);
        let result = detached_commit(&repo, result_tree, base);
        (dir, base, result)
    }

    pub(crate) fn prepared(root: &Path, base: ObjectId, result: ObjectId) -> PreparedIntegration {
        let outcome = prepare_blocking(&PrepareRequest {
            checkout: root.to_path_buf(),
            baseline_commit: base.to_string(),
            result_commit: result.to_string(),
            source_head: None,
        })
        .unwrap();
        match outcome {
            PrepareOutcome::Ready(ready) => ready,
            other => panic!("expected a prepared integration, got {other:?}"),
        }
    }

    pub(crate) fn applied(outcome: ApplyOutcome) -> AppliedIntegration {
        match outcome {
            ApplyOutcome::Applied(applied) => *applied,
            other => panic!("expected an applied integration, got {other:?}"),
        }
    }

    pub(crate) fn read(root: &Path, path: &str) -> Vec<u8> {
        std::fs::read(root.join(path)).unwrap()
    }

    fn lock_path(root: &Path) -> PathBuf {
        root.join(".git/index.lock")
    }

    #[test]
    fn applies_exactly_the_touched_entries() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        // Unrelated local work must stay as it is.
        std::fs::write(root.join("scratch.local"), "mine\n").unwrap();
        std::fs::write(root.join("plain.txt"), "edited by the user\n").unwrap();
        let index_before = std::fs::read(root.join(".git/index")).unwrap();
        let head_before = std::fs::read(root.join(".git/HEAD")).unwrap();

        let prepared = prepared(root, base, result);
        let touched = prepared.touched.clone();
        let merged_tree = prepared.merged_tree.clone();
        let applied = applied(apply_blocking(prepared).unwrap());

        // Attribute-filtered file: the eol=crlf attribute applies on write.
        assert_eq!(read(root, "mod.crlf"), b"new\r\nline\r\n");
        // Addition into new nested directories, deletion, plain content.
        assert_eq!(read(root, "new/dir/file.txt"), b"added\n");
        assert!(!root.join("gone.txt").exists());
        assert_eq!(read(root, "run.sh"), b"#!/bin/sh\necho hi\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "executable bit lost: {mode:o}");
            let link = root.join("link");
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("keep.txt"));
        }
        // Untouched paths, including local work, are left alone.
        assert_eq!(read(root, "keep.txt"), b"keep\n");
        assert_eq!(read(root, "sub/stay.txt"), b"stay\n");
        assert_eq!(read(root, "plain.txt"), b"edited by the user\n");
        assert_eq!(read(root, "scratch.local"), b"mine\n");
        // Branch and real index contents are untouched; the lock is held.
        assert_eq!(std::fs::read(root.join(".git/HEAD")).unwrap(), head_before);
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            index_before
        );
        assert!(lock_path(root).exists());
        assert_eq!(applied.prepared.lock.path(), lock_path(root));

        // The intent is durable and describes the transaction.
        let intent = read_intent(&root.join(".git")).unwrap().unwrap();
        assert_eq!(intent.operation_id, applied.operation_id);
        assert_eq!(intent.head, base.to_string());
        assert_eq!(intent.merged_tree, merged_tree);
        assert_eq!(intent.touched, touched);
        assert_eq!(intent.index_lock, lock_path(root).to_string_lossy());
        assert_eq!(applied.intent_path, root.join(".git").join(INTENT_FILE));
        assert!(
            !root
                .join(".git")
                .join(format!("{INTENT_FILE}.tmp"))
                .exists()
        );
    }

    #[test]
    fn a_failed_write_restores_the_touched_paths_and_removes_the_intent() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        let prepared = prepared(root, base, result);
        // Something appears at a touched path after the local-work check.
        std::fs::create_dir(root.join("run.sh")).unwrap();
        let index_before = std::fs::read(root.join(".git/index")).unwrap();

        let err = apply_blocking(prepared).unwrap_err();
        assert!(err.to_string().contains("run.sh"), "{err}");

        // Everything the transaction touched is back at HEAD...
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert_eq!(read(root, "mod.crlf"), b"old\r\n");
        assert!(!root.join("new").exists());
        assert!(!root.join("link").exists());
        // ...the interloper is untouched, and nothing is left behind.
        assert!(root.join("run.sh").is_dir());
        assert_eq!(read(root, "keep.txt"), b"keep\n");
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            index_before
        );
        assert!(read_intent(&root.join(".git")).unwrap().is_none());
        assert!(!lock_path(root).exists());
    }

    #[test]
    fn a_pending_intent_is_never_overwritten() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        let intent = root.join(".git").join(INTENT_FILE);
        std::fs::write(&intent, "from an earlier run").unwrap();
        let prepared = prepared(root, base, result);

        match apply_blocking(prepared).unwrap() {
            ApplyOutcome::Refused(refusal) => assert_eq!(refusal.code, "integration_pending"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(&intent).unwrap(), b"from an earlier run");
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert!(!root.join("run.sh").exists());
        assert!(!lock_path(root).exists());
    }

    #[test]
    fn a_moved_branch_is_refused_before_anything_is_written() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        let prepared = prepared(root, base, result);
        let repo = gix::open(root).unwrap();
        let moved_tree = tree_of(&repo, &base_entries());
        repo.commit("HEAD", "external", moved_tree, vec![base])
            .unwrap();

        match apply_blocking(prepared).unwrap() {
            ApplyOutcome::Refused(refusal) => assert_eq!(refusal.code, "branch_moved"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert!(!root.join("run.sh").exists());
        assert!(read_intent(&root.join(".git")).unwrap().is_none());
        assert!(!lock_path(root).exists());
    }

    #[tokio::test]
    async fn the_repository_ops_boundary_applies_and_injects_faults() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        let ops = FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new()));
        ops.fail_next(FailurePoint::ApplyIntegration, 1, "boom");

        let err = ops
            .apply_integration(prepared(root, base, result))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        // The injected failure happened before anything was written, and
        // dropping the prepared value released the lock.
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert!(!lock_path(root).exists());

        let outcome = ops
            .apply_integration(prepared(root, base, result))
            .await
            .unwrap();
        applied(outcome);
        assert!(!root.join("gone.txt").exists());
        assert_eq!(read(root, "new/dir/file.txt"), b"added\n");
    }
}
