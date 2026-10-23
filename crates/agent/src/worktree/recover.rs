//! Crash recovery of an explicit result acceptance (task 22.8).
//!
//! An acceptance persists an intent in the checkout's per-worktree Git
//! directory before it writes a file (see [`crate::worktree::apply`]) and
//! records the integration commit in it before it moves the branch (see
//! [`crate::worktree::finalize`]). A surviving intent therefore says exactly
//! how far the transaction got, and the branch tells which side of the
//! reference transaction it died on:
//!
//! - the branch is still at the recorded `HEAD`: nothing was committed. The
//!   touched paths are restored to `HEAD`, a record reference left behind by
//!   a torn reference transaction is deleted (only while it still names the
//!   recorded commit), scratch files and the intent are removed;
//! - the branch is at the recorded commit: the commit stands. The record is
//!   created if it is missing and the real index is brought in line with the
//!   commit, then the intent is removed;
//! - anything else (the branch moved to neither, or `HEAD` left the branch)
//!   is refused and the intent is kept: replaying either direction could
//!   overwrite someone else's work.
//!
//! Recovery takes the checkout's real `index.lock` itself. A lock that is
//! already there is removed **only** when its identity (see
//! [`lock_identity`]) equals the one recorded in the intent, which proves the
//! interrupted transaction created it. Any other lock belongs to someone else
//! and is never touched: the recovery is refused with `index_locked`.
//!
//! Callers run recovery under the checkout's mutation gate, which is what
//! keeps a live acceptance of another process from being mistaken for a
//! dead one.

use crate::worktree::apply::{
    IntegrationIntent, read_intent, remove_intent, rollback_touched, workdir,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::finalize::{StatSource, flatten, updated_index};
use crate::worktree::integrate::{IntegratedCommit, RecoveryOutcome, Refusal, Stop, refuse};
use crate::worktree::protect::{IndexLock, lock_identity};
use crate::worktree::repo_ops::{
    FlatTree, diff_flattened, native_err, open_acceptance_checkout, parse_oid,
};
use gix::bstr::BString;
use gix::hash::ObjectId;
use gix::refs::Target;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Resolve the intent left in `checkout`'s Git directory, if any.
pub(crate) fn recover_blocking(checkout: &Path) -> WorktreeResult<RecoveryOutcome> {
    let repo = open_acceptance_checkout(checkout)?;
    let git_dir = repo.git_dir().to_path_buf();
    let Some(intent) = read_intent(&git_dir)? else {
        return Ok(RecoveryOutcome::NothingPending);
    };
    match recover(&repo, &git_dir, &intent) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Refused(refusal)) => Ok(RecoveryOutcome::Refused(refusal)),
        Err(Stop::Failed(err)) => Err(err),
    }
}

fn recover(
    repo: &gix::Repository,
    git_dir: &Path,
    intent: &IntegrationIntent,
) -> Result<RecoveryOutcome, Stop> {
    let branch_ref = format!("refs/heads/{}", intent.branch);
    let on_branch = repo
        .head_name()
        .map_err(native_err)?
        .is_some_and(|name| name.as_bstr() == branch_ref.as_str());
    if !on_branch {
        return Err(refuse(
            "head_changed",
            format!(
                "an unfinished integration (operation {}) was recorded on branch {}, but HEAD is \
                 no longer on it; nothing was changed and the intent at {} is kept",
                intent.operation_id,
                intent.branch,
                git_dir.join(crate::worktree::apply::INTENT_FILE).display()
            ),
        ));
    }

    let lock = take_lock(&repo.index_path(), intent)?;

    let head = parse_oid(&intent.head)?;
    let commit = intent.commit.as_deref().map(parse_oid).transpose()?;
    let tip = resolve(repo, &branch_ref)?;

    if tip == Some(head) {
        // The touched paths were clean when the transaction started, but the
        // crash may be old: anything edited since is someone's work and is
        // never overwritten.
        let edited = edited_touched_paths(repo, intent)?;
        if !edited.is_empty() {
            return Err(Refusal::new(
                "touched_paths_edited",
                format!(
                    "an unfinished integration (operation {}) would be rolled back, but these \
                     paths changed since it was applied; nothing was changed and the intent at \
                     {} is kept. Restore or move those files, then retry",
                    intent.operation_id,
                    git_dir.join(crate::worktree::apply::INTENT_FILE).display()
                ),
            )
            .with_paths(edited)
            .into());
        }
        roll_back(repo, git_dir, intent, commit)?;
        drop(lock);
        return Ok(RecoveryOutcome::RolledBack {
            operation_id: intent.operation_id.clone(),
            paths: intent
                .touched
                .iter()
                .map(|change| change.path.clone())
                .collect(),
        });
    }
    if let Some(commit) = commit
        && tip == Some(commit)
    {
        let integrated = complete(repo, git_dir, intent, commit, lock)?;
        return Ok(RecoveryOutcome::Completed {
            operation_id: intent.operation_id.clone(),
            integrated,
        });
    }
    let found = tip.map_or_else(|| "missing".to_string(), |id| id.to_string());
    Err(refuse(
        "branch_moved",
        format!(
            "an unfinished integration (operation {}) expected branch {} at {}{}, but it is at \
             {found}; nothing was changed and the intent at {} is kept for manual inspection",
            intent.operation_id,
            intent.branch,
            intent.head,
            intent
                .commit
                .as_deref()
                .map(|commit| format!(" or {commit}"))
                .unwrap_or_default(),
            git_dir.join(crate::worktree::apply::INTENT_FILE).display()
        ),
    ))
}

/// Hold the real `index.lock` for the recovery. An existing lock is removed
/// only when the intent proves the interrupted transaction created it.
fn take_lock(index_path: &Path, intent: &IntegrationIntent) -> Result<IndexLock, Stop> {
    let mut lock_name = index_path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    if std::fs::symlink_metadata(&lock_path).is_ok() {
        let ours = intent.index_lock_identity.is_some()
            && lock_identity(&lock_path) == intent.index_lock_identity;
        if !ours {
            return Err(refuse(
                "index_locked",
                format!(
                    "{} exists and was not created by the interrupted integration; QueryMT never \
                     removes a lock it does not own",
                    lock_path.display()
                ),
            ));
        }
        std::fs::remove_file(&lock_path).map_err(WorktreeError::from)?;
    }
    IndexLock::acquire(index_path)
}

/// The branch never moved: undo the file writes and anything the torn
/// reference transaction may have left.
fn roll_back(
    repo: &gix::Repository,
    git_dir: &Path,
    intent: &IntegrationIntent,
    commit: Option<ObjectId>,
) -> WorktreeResult<()> {
    // A reference transaction updates the branch and the record together; if
    // it was cut short only the record may exist. Delete it while it still
    // names the commit that is being abandoned.
    if let (Some(commit), Some(record)) = (commit, intent.integration_ref.as_deref())
        && resolve(repo, record)? == Some(commit)
    {
        let name = gix::refs::FullName::try_from(record)
            .map_err(|_| WorktreeError::InvalidOperation(format!("invalid ref name: {record}")))?;
        repo.edit_references([RefEdit {
            change: Change::Delete {
                expected: PreviousValue::MustExistAndMatch(Target::Object(commit)),
                log: RefLog::AndReference,
            },
            name,
            deref: false,
        }])
        .map_err(native_err)?;
    }
    rollback_touched(repo, parse_oid(&intent.head_tree)?, &intent.touched)?;
    finish(git_dir, intent)
}

/// The branch is at the recorded commit: make the record and the real index
/// agree with it.
fn complete(
    repo: &gix::Repository,
    git_dir: &Path,
    intent: &IntegrationIntent,
    commit: ObjectId,
    lock: IndexLock,
) -> WorktreeResult<IntegratedCommit> {
    if let Some(record) = intent.integration_ref.as_deref()
        && resolve(repo, record)?.is_none()
    {
        repo.reference(
            record,
            commit,
            PreviousValue::MustNotExist,
            "QueryMT result integration",
        )
        .map_err(native_err)?;
    }

    let tree = repo
        .find_commit(commit)
        .map_err(native_err)?
        .tree_id()
        .map_err(native_err)?
        .detach();
    let flat = flatten(repo, tree)?;
    let head_flat = flatten(repo, parse_oid(&intent.head_tree)?)?;

    // Only entries that still differ from the commit are rewritten, so an
    // index that was already updated (the crash came after it) is not
    // disturbed.
    let stale = stale_index_paths(repo, intent, &flat)?;
    if stale.is_empty() {
        drop(lock);
    } else {
        let index = updated_index(repo, workdir(repo)?, &stale, &flat, StatSource::Unknown)?;
        lock.commit_index(&index)?;
    }
    finish(git_dir, intent)?;

    Ok(IntegratedCommit {
        commit: commit.to_string(),
        parent: intent.head.clone(),
        branch: intent.branch.clone(),
        changed_paths: diff_flattened(&head_flat, &flat),
    })
}

/// Paths of the intent whose real-index entry differs from the commit's.
fn stale_index_paths(
    repo: &gix::Repository,
    intent: &IntegrationIntent,
    flat: &FlatTree,
) -> WorktreeResult<BTreeSet<String>> {
    let index = gix::index::File::at(
        repo.index_path(),
        repo.object_hash(),
        false,
        Default::default(),
    )
    .map_err(native_err)?;
    Ok(intent
        .index_paths
        .iter()
        .filter(|path| {
            let current = index.entry_by_path(path.as_str().into());
            let wanted = flat.get(&BString::from(path.as_str()));
            match (current, wanted) {
                (None, None) => false,
                (Some(entry), Some((mode, id))) => entry.mode != *mode || entry.id != *id,
                _ => true,
            }
        })
        .cloned()
        .collect())
}

/// Touched paths whose working-tree content is neither what the interrupted
/// transaction wrote (the merged tree) nor what `HEAD` has. A missing file
/// is never "edited": restoring it loses nothing. Sorted by path.
fn edited_touched_paths(
    repo: &gix::Repository,
    intent: &IntegrationIntent,
) -> WorktreeResult<Vec<String>> {
    let root = workdir(repo)?.to_path_buf();
    let head_flat = flatten(repo, parse_oid(&intent.head_tree)?)?;
    let merged_flat = flatten(repo, parse_oid(&intent.merged_tree)?)?;
    let (mut pipeline, index) = repo.filter_pipeline(None).map_err(native_err)?;
    let mut edited = Vec::new();
    for change in &intent.touched {
        let key = BString::from(change.path.as_str());
        let on_disk = match worktree_entry(repo, &mut pipeline, &index, &root, &change.path)? {
            OnDisk::Missing => continue,
            OnDisk::Other => {
                edited.push(change.path.clone());
                continue;
            }
            OnDisk::Entry(mode, id) => (mode, id),
        };
        let matches = |wanted: Option<&(gix::index::entry::Mode, ObjectId)>| {
            wanted.is_some_and(|wanted| same_entry(*wanted, on_disk))
        };
        if !matches(merged_flat.get(&key)) && !matches(head_flat.get(&key)) {
            edited.push(change.path.clone());
        }
    }
    edited.sort();
    Ok(edited)
}

enum OnDisk {
    Missing,
    /// A directory or another non-blob: something this transaction did not
    /// put there.
    Other,
    Entry(gix::index::entry::Mode, ObjectId),
}

/// The blob id and mode the file at `path` would be staged with (through
/// the repository's clean filters, so `eol` conversion compares equal).
fn worktree_entry(
    repo: &gix::Repository,
    pipeline: &mut gix::filter::Pipeline<'_>,
    index: &gix::index::State,
    root: &Path,
    path: &str,
) -> WorktreeResult<OnDisk> {
    use gix::filter::plumbing::pipeline::convert::ToGitOutcome;
    use gix::index::entry::Mode;
    use std::io::Read;

    let absolute = root.join(path);
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(OnDisk::Missing),
        Err(err) => return Err(err.into()),
    };
    let hash = |data: &[u8]| {
        gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, data).map_err(native_err)
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&absolute)?;
        let target = gix::path::into_bstr(target).map_err(native_err)?;
        let target = gix::path::to_unix_separators_on_windows(target);
        return Ok(OnDisk::Entry(Mode::SYMLINK, hash(&target)?));
    }
    if !metadata.is_file() {
        return Ok(OnDisk::Other);
    }
    let file = std::fs::File::open(&absolute)?;
    let mut data = Vec::new();
    match pipeline
        .convert_to_git(file, Path::new(path), index)
        .map_err(native_err)?
    {
        ToGitOutcome::Unchanged(mut file) => {
            file.read_to_end(&mut data)?;
        }
        ToGitOutcome::Buffer(buffer) => data.extend_from_slice(buffer),
        ToGitOutcome::Process(mut stream) => {
            stream.read_to_end(&mut data)?;
        }
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            Mode::FILE_EXECUTABLE
        } else {
            Mode::FILE
        }
    };
    #[cfg(not(unix))]
    let mode = Mode::FILE;
    Ok(OnDisk::Entry(mode, hash(&data)?))
}

/// Whether a tree entry and an on-disk entry agree. Platforms without an
/// executable bit compare regular files by content only.
fn same_entry(
    wanted: (gix::index::entry::Mode, ObjectId),
    found: (gix::index::entry::Mode, ObjectId),
) -> bool {
    use gix::index::entry::Mode;
    if wanted.1 != found.1 {
        return false;
    }
    let regular = |mode: Mode| mode == Mode::FILE || mode == Mode::FILE_EXECUTABLE;
    if cfg!(unix) {
        wanted.0 == found.0
    } else {
        wanted.0 == found.0 || (regular(wanted.0) && regular(found.0))
    }
}

/// Remove the scratch files of the operation and then the intent itself.
fn finish(git_dir: &Path, intent: &IntegrationIntent) -> WorktreeResult<()> {
    let prefix = format!("qmt-integration-{}.", intent.operation_id);
    if let Ok(entries) = std::fs::read_dir(git_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    remove_intent(git_dir)?;
    Ok(())
}

fn resolve(repo: &gix::Repository, name: &str) -> WorktreeResult<Option<ObjectId>> {
    match repo.try_find_reference(name).map_err(native_err)? {
        Some(mut reference) => Ok(Some(reference.peel_to_id().map_err(native_err)?.detach())),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::apply::tests::{applied, fixture, prepared, read};
    use crate::worktree::apply::{INTENT_FILE, apply_blocking};
    use crate::worktree::finalize::finalize_blocking;
    use crate::worktree::hooks::{HOOK_TIMEOUT, HookOutcome, HookRequest, run_hooks};
    use crate::worktree::integrate::{
        AppliedIntegration, FinalizeFault, FinalizeOutcome, FinalizeRequest, FinalizeStage,
    };
    use crate::worktree::repo_ops::{
        FailurePoint, FaultInjectingRepoOps, GixRepoOps, RepositoryOps,
    };
    use std::sync::Arc;

    const RECORD: &str = "refs/qmt/integrations/ws/result";

    fn lock_path(root: &Path) -> PathBuf {
        root.join(".git/index.lock")
    }

    fn ref_target(root: &Path, name: &str) -> Option<ObjectId> {
        let repo = gix::open(root).unwrap();
        repo.try_find_reference(name)
            .unwrap()
            .map(|mut reference| reference.peel_to_id().unwrap().detach())
    }

    fn scratch(root: &Path) -> Vec<String> {
        std::fs::read_dir(root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("qmt-integration-"))
            .collect()
    }

    fn crash(stage: FinalizeStage) -> FinalizeFault {
        FinalizeFault {
            stage,
            error: WorktreeError::InvalidOperation("simulated crash".into()),
        }
    }

    async fn hooked(applied: &AppliedIntegration) -> crate::worktree::hooks::HookedIntegration {
        match run_hooks(HookRequest::for_applied(applied, "msg\n"), HOOK_TIMEOUT)
            .await
            .unwrap()
        {
            HookOutcome::Passed(hooked) => hooked,
            other => panic!("expected the hooks to pass, got {other:?}"),
        }
    }

    /// A checkout whose acceptance died at `stage` of finalization: applied
    /// files, the intent, and the `index.lock` all stay behind.
    async fn crashed_at(stage: FinalizeStage) -> (tempfile::TempDir, ObjectId) {
        let (dir, base, result) = fixture();
        let applied = applied(apply_blocking(prepared(dir.path(), base, result)).unwrap());
        let hooked = hooked(&applied).await;
        let err = finalize_blocking(FinalizeRequest {
            applied,
            hooked,
            integration_ref: RECORD.to_string(),
            fault: Some(crash(stage)),
        })
        .unwrap_err();
        assert!(err.to_string().contains("simulated crash"), "{err}");
        let root = dir.path();
        assert!(lock_path(root).exists(), "the crash leaves the lock behind");
        assert!(root.join(".git").join(INTENT_FILE).exists());
        (dir, base)
    }

    /// Everything a rollback must restore.
    fn assert_rolled_back(root: &Path, base: ObjectId) {
        assert_eq!(ref_target(root, "HEAD"), Some(base));
        assert_eq!(ref_target(root, RECORD), None);
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert_eq!(read(root, "mod.crlf"), b"old\r\n");
        assert!(!root.join("run.sh").exists());
        assert!(!root.join("new").exists());
        assert!(!root.join("link").exists());
        assert!(!root.join(".git").join(INTENT_FILE).exists());
        assert!(!lock_path(root).exists());
        assert!(scratch(root).is_empty(), "left behind: {:?}", scratch(root));
    }

    #[test]
    fn nothing_is_pending_without_an_intent() {
        let (dir, _base, _result) = fixture();
        assert_eq!(
            recover_blocking(dir.path()).unwrap(),
            RecoveryOutcome::NothingPending
        );
    }

    #[tokio::test]
    async fn a_crash_after_apply_is_rolled_back_and_its_own_lock_removed() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        std::fs::write(root.join("scratch.local"), "mine\n").unwrap();
        let applied = applied(apply_blocking(prepared(root, base, result)).unwrap());
        applied.prepared.lock.leak();

        let outcome = recover_blocking(root).unwrap();

        let RecoveryOutcome::RolledBack { paths, .. } = outcome else {
            panic!("expected a rollback, got {outcome:?}");
        };
        assert!(paths.contains(&"run.sh".to_string()));
        assert_rolled_back(root, base);
        assert_eq!(read(root, "scratch.local"), b"mine\n");
    }

    #[tokio::test]
    async fn a_touched_path_edited_after_the_crash_is_never_overwritten() {
        let (dir, base, result) = fixture();
        let root = dir.path();
        let applied = applied(apply_blocking(prepared(root, base, result)).unwrap());
        applied.prepared.lock.leak();
        // The user kept working on a file the crashed acceptance wrote.
        std::fs::write(root.join("run.sh"), "#!/bin/sh\necho mine\n").unwrap();

        let RecoveryOutcome::Refused(refusal) = recover_blocking(root).unwrap() else {
            panic!("an edited touched path must refuse the rollback");
        };

        assert_eq!(refusal.code, "touched_paths_edited");
        assert_eq!(refusal.paths, ["run.sh"]);
        assert_eq!(read(root, "run.sh"), b"#!/bin/sh\necho mine\n");
        // Nothing else was rolled back either, and the intent is kept.
        assert!(root.join("new/dir/file.txt").exists());
        assert!(root.join(".git").join(INTENT_FILE).exists());
        assert_eq!(ref_target(root, "HEAD"), Some(base));

        // Once the user restores the file, recovery proceeds. Paths restored
        // to HEAD content or deleted by hand count as unedited too.
        std::fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::remove_file(root.join("new/dir/file.txt")).unwrap();
        let outcome = recover_blocking(root).unwrap();
        assert!(
            matches!(outcome, RecoveryOutcome::RolledBack { .. }),
            "{outcome:?}"
        );
        assert_rolled_back(root, base);
    }

    #[tokio::test]
    async fn a_crash_after_the_hooks_is_rolled_back() {
        let (dir, base) = crashed_at(FinalizeStage::Commit).await;
        // The private index of the hook phase is still there.
        assert!(!scratch(dir.path()).is_empty());

        let outcome = recover_blocking(dir.path()).unwrap();

        assert!(matches!(outcome, RecoveryOutcome::RolledBack { .. }));
        assert_rolled_back(dir.path(), base);
    }

    #[tokio::test]
    async fn a_crash_before_the_reference_transaction_is_rolled_back() {
        let (dir, base) = crashed_at(FinalizeStage::RefTransaction).await;
        let intent = read_intent(&dir.path().join(".git")).unwrap().unwrap();
        assert!(intent.commit.is_some(), "the commit is recorded first");
        assert_eq!(intent.integration_ref.as_deref(), Some(RECORD));

        let outcome = recover_blocking(dir.path()).unwrap();

        assert!(matches!(outcome, RecoveryOutcome::RolledBack { .. }));
        assert_rolled_back(dir.path(), base);
    }

    #[tokio::test]
    async fn a_torn_reference_transaction_loses_its_record() {
        let (dir, base) = crashed_at(FinalizeStage::RefTransaction).await;
        let root = dir.path();
        // Only the record was written before the process died.
        let intent = read_intent(&root.join(".git")).unwrap().unwrap();
        let commit: ObjectId = intent.commit.as_deref().unwrap().parse().unwrap();
        gix::open(root)
            .unwrap()
            .reference(RECORD, commit, PreviousValue::MustNotExist, "torn")
            .unwrap();

        let outcome = recover_blocking(root).unwrap();

        assert!(matches!(outcome, RecoveryOutcome::RolledBack { .. }));
        assert_rolled_back(root, base);
    }

    #[tokio::test]
    async fn a_crash_after_the_branch_moved_completes_the_index_update() {
        let (dir, base) = crashed_at(FinalizeStage::IndexUpdate).await;
        let root = dir.path();
        let commit = ref_target(root, RECORD).expect("the record was written with the branch");
        assert_eq!(ref_target(root, "HEAD"), Some(commit));
        let index_before = std::fs::read(root.join(".git/index")).unwrap();

        let outcome = recover_blocking(root).unwrap();

        let RecoveryOutcome::Completed { integrated, .. } = outcome else {
            panic!("expected a completion, got {outcome:?}");
        };
        assert_eq!(integrated.commit, commit.to_string());
        assert_eq!(integrated.parent, base.to_string());
        assert!(integrated.changed_paths.iter().any(|c| c.path == "run.sh"));
        // The commit stands and the real index now matches it.
        assert_eq!(ref_target(root, "HEAD"), Some(commit));
        assert_ne!(
            std::fs::read(root.join(".git/index")).unwrap(),
            index_before
        );
        let repo = gix::open(root).unwrap();
        let index = gix::index::File::at(
            repo.index_path(),
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        assert!(index.entry_by_path("run.sh".into()).is_some());
        assert!(index.entry_by_path("gone.txt".into()).is_none());
        assert!(index.entry_by_path("new/dir/file.txt".into()).is_some());
        assert!(!lock_path(root).exists());
        assert!(!root.join(".git").join(INTENT_FILE).exists());
        assert!(scratch(root).is_empty());
        // Applied files are the committed ones.
        assert_eq!(read(root, "run.sh"), b"#!/bin/sh\necho hi\n");

        // Idempotent: nothing is pending any more.
        assert_eq!(
            recover_blocking(root).unwrap(),
            RecoveryOutcome::NothingPending
        );
    }

    #[tokio::test]
    async fn a_missing_record_is_recreated_when_the_commit_stands() {
        let (dir, _base) = crashed_at(FinalizeStage::IndexUpdate).await;
        let root = dir.path();
        let commit = ref_target(root, RECORD).unwrap();
        gix::open(root)
            .unwrap()
            .find_reference(RECORD)
            .unwrap()
            .delete()
            .unwrap();
        assert_eq!(ref_target(root, RECORD), None);

        let outcome = recover_blocking(root).unwrap();

        assert!(matches!(outcome, RecoveryOutcome::Completed { .. }));
        assert_eq!(ref_target(root, RECORD), Some(commit));
    }

    #[tokio::test]
    async fn a_foreign_index_lock_is_never_removed() {
        let (dir, base) = crashed_at(FinalizeStage::Commit).await;
        let root = dir.path();
        // The lock now looks like somebody else's: it does not carry the
        // identity the interrupted transaction recorded.
        let mut intent = read_intent(&root.join(".git")).unwrap().unwrap();
        let recorded = intent.index_lock_identity.clone();
        assert!(recorded.is_some(), "the lock identity is recorded");
        intent.index_lock_identity = Some("someone-else".into());
        crate::worktree::apply::write_intent(&root.join(".git"), &intent).unwrap();
        std::fs::write(lock_path(root), "foreign").unwrap();

        let outcome = recover_blocking(root).unwrap();

        let RecoveryOutcome::Refused(refusal) = outcome else {
            panic!("expected a refusal, got {outcome:?}");
        };
        assert_eq!(refusal.code, "index_locked");
        assert_eq!(std::fs::read(lock_path(root)).unwrap(), b"foreign");
        // Nothing was rolled back: the intent and the applied files remain.
        assert!(root.join(".git").join(INTENT_FILE).exists());
        assert!(root.join("run.sh").exists());

        // A lock without any recorded identity is foreign as well.
        intent.index_lock_identity = None;
        crate::worktree::apply::write_intent(&root.join(".git"), &intent).unwrap();
        let RecoveryOutcome::Refused(refusal) = recover_blocking(root).unwrap() else {
            panic!("a lock without identity must be refused");
        };
        assert_eq!(refusal.code, "index_locked");
        assert_eq!(std::fs::read(lock_path(root)).unwrap(), b"foreign");

        // Once the interrupted transaction's own lock is back, recovery goes on.
        intent.index_lock_identity = recorded;
        crate::worktree::apply::write_intent(&root.join(".git"), &intent).unwrap();
        std::fs::remove_file(lock_path(root)).unwrap();
        assert!(matches!(
            recover_blocking(root).unwrap(),
            RecoveryOutcome::RolledBack { .. }
        ));
        assert_rolled_back(root, base);
    }

    #[tokio::test]
    async fn a_branch_that_moved_elsewhere_is_refused_and_the_intent_kept() {
        let (dir, base) = crashed_at(FinalizeStage::Commit).await;
        let root = dir.path();
        let repo = gix::open(root).unwrap();
        let tree = repo.head_commit().unwrap().tree_id().unwrap().detach();
        let external = repo
            .commit("HEAD", "external", tree, vec![base])
            .unwrap()
            .detach();

        let outcome = recover_blocking(root).unwrap();

        let RecoveryOutcome::Refused(refusal) = outcome else {
            panic!("expected a refusal, got {outcome:?}");
        };
        assert_eq!(refusal.code, "branch_moved");
        assert_eq!(ref_target(root, "HEAD"), Some(external));
        assert!(root.join(".git").join(INTENT_FILE).exists());
        assert!(root.join("run.sh").exists(), "no file was touched");
        assert!(
            !lock_path(root).exists(),
            "the recovery's own lock is released"
        );
    }

    #[tokio::test]
    async fn the_boundary_injects_each_crash_and_the_next_recovery_repairs_it() {
        let ops = FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new()));
        for (point, completes) in [
            (FailurePoint::FinalizeCommit, false),
            (FailurePoint::FinalizeRefTransaction, false),
            (FailurePoint::FinalizeIndexUpdate, true),
        ] {
            let (dir, base, result) = fixture();
            let root = dir.path();
            let applied = applied(ops_apply(root, base, result));
            let hooked = hooked(&applied).await;
            ops.fail_next(point, 1, "boom");

            let err = ops
                .finalize_integration(FinalizeRequest {
                    applied,
                    hooked,
                    integration_ref: RECORD.to_string(),
                    fault: None,
                })
                .await
                .unwrap_err();
            assert!(err.to_string().contains("boom"), "{point:?}: {err}");
            assert!(root.join(".git").join(INTENT_FILE).exists(), "{point:?}");

            match ops.recover_integration(root).await.unwrap() {
                RecoveryOutcome::RolledBack { .. } => {
                    assert!(!completes, "{point:?}");
                    assert_rolled_back(root, base);
                }
                RecoveryOutcome::Completed { integrated, .. } => {
                    assert!(completes, "{point:?}");
                    assert_eq!(
                        ref_target(root, "HEAD").unwrap().to_string(),
                        integrated.commit
                    );
                    assert!(!lock_path(root).exists());
                }
                other => panic!("{point:?}: unexpected {other:?}"),
            }
            assert_eq!(
                ops.recover_integration(root).await.unwrap(),
                RecoveryOutcome::NothingPending,
                "{point:?}"
            );
        }

        // Recovery itself can be made to fail.
        let (dir, _base, _result) = fixture();
        ops.fail_next(FailurePoint::RecoverIntegration, 1, "boom");
        assert!(ops.recover_integration(dir.path()).await.is_err());
        assert_eq!(
            ops.recover_integration(dir.path()).await.unwrap(),
            RecoveryOutcome::NothingPending
        );
    }

    fn ops_apply(
        root: &Path,
        base: ObjectId,
        result: ObjectId,
    ) -> crate::worktree::integrate::ApplyOutcome {
        apply_blocking(prepared(root, base, result)).unwrap()
    }

    #[tokio::test]
    async fn a_finalize_that_fails_normally_is_still_recoverable() {
        // Not a crash: finalization completes and nothing is pending.
        let (dir, base, result) = fixture();
        let applied = applied(apply_blocking(prepared(dir.path(), base, result)).unwrap());
        let hooked = hooked(&applied).await;
        let outcome = finalize_blocking(FinalizeRequest {
            applied,
            hooked,
            integration_ref: RECORD.to_string(),
            fault: None,
        })
        .unwrap();
        assert!(matches!(outcome, FinalizeOutcome::Integrated(_)));
        assert_eq!(
            recover_blocking(dir.path()).unwrap(),
            RecoveryOutcome::NothingPending
        );
    }
}
