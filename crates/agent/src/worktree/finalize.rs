//! Finalization of an explicit result acceptance (steps 7-10 of design
//! decision 9): turn the hooked private index into a commit, advance the
//! branch together with the integration record, and bring the real index in
//! line, all under the `index.lock` taken during preparation.
//!
//! 1. Re-read the private index the hooks saw. Paths a hook re-staged inside
//!    the baseline-relative delta are accepted; any other difference from the
//!    merged tree (or an unmerged entry) refuses with
//!    `hook_changed_paths_outside_delta`.
//! 2. Clean the message like `git commit -m` (trailing whitespace stripped,
//!    blank runs collapsed, leading and trailing blank lines dropped) and
//!    refuse an empty one.
//! 3. Write the tree from the private index and a commit with the configured
//!    author and committer whose only parent is the recorded `HEAD`.
//! 4. Record the commit in the persisted intent, then run **one** reference
//!    transaction: the branch must still match `HEAD` and the integration
//!    record `refs/qmt/integrations/<workspace>/<result>` must not exist. A
//!    failed compare-and-swap is `branch_moved`; an existing record reports
//!    the original commit. The record can therefore never be missing for an
//!    integrated commit.
//! 5. Write the changed entries into the real index with fresh stat data,
//!    commit the lock, remove the intent. (`post-commit` runs afterwards,
//!    see [`crate::worktree::hooks::run_post_commit`].)
//!
//! Any failure before the reference transaction restores the touched paths
//! to `HEAD`, removes the intent and the private index, and releases the
//! lock; nothing else was changed. After the transaction the branch has
//! moved, so a failure to update the index keeps the intent (which now names
//! the commit) for the next acceptance to complete.

use crate::worktree::apply::write_intent;
use crate::worktree::apply::{IntegrationIntent, read_intent, remove_intent, rollback_touched};
use crate::worktree::domain::ChangedPath;
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::integrate::{
    AppliedIntegration, FinalizeFault, FinalizeOutcome, FinalizeRequest, FinalizeStage,
    IntegratedCommit, PreparedIntegration, Refusal,
};
use crate::worktree::repo_ops::{
    FlatTree, diff_flattened, flatten_tree, native_err, open_acceptance_checkout, parse_oid,
    write_tree_from_index,
};
use gix::bstr::{BString, ByteSlice};
use gix::hash::ObjectId;
use gix::index::entry::{Flags, Stat};
use gix::refs::Target;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// Steps 7-10 for `request`; see the module docs.
pub(crate) fn finalize_blocking(request: FinalizeRequest) -> WorktreeResult<FinalizeOutcome> {
    let FinalizeRequest {
        applied,
        hooked,
        integration_ref,
        mut fault,
    } = request;
    let AppliedIntegration {
        prepared, written, ..
    } = applied;
    let PreparedIntegration {
        checkout,
        head,
        branch,
        head_tree,
        merged_tree,
        delta: _,
        touched,
        lock,
    } = prepared;

    let repo = open_acceptance_checkout(&checkout)?;
    let head_id = parse_oid(&head)?;
    let txn = Transaction {
        repo: &repo,
        git_dir: repo.git_dir(),
        head_tree: parse_oid(&head_tree)?,
        touched: &touched,
        private_index: &hooked.private_index,
    };

    // Simulated process death: no rollback, the lock file stays behind.
    if let Some(error) = take_fault(&mut fault, FinalizeStage::Commit) {
        lock.leak();
        return Err(error);
    }
    // Re-staging is accepted only on touched paths: those were checked for
    // local work; other delta paths (already integrated by a sibling) were
    // not and may carry the user's own edits.
    let staged = match stage(
        &txn,
        &hooked.message,
        parse_oid(&merged_tree)?,
        head_id,
        &touched,
        &integration_ref,
    ) {
        Ok(Ok(staged)) => staged,
        Ok(Err(refusal)) => return txn.abandon(FinalizeOutcome::Refused(refusal)),
        Err(err) => return txn.fail(err),
    };
    if let Some(error) = take_fault(&mut fault, FinalizeStage::RefTransaction) {
        lock.leak();
        return Err(error);
    }

    // Step 9: one reference transaction.
    let branch_ref = ref_name(&format!("refs/heads/{branch}"))?;
    let record_ref = ref_name(&integration_ref)?;
    let edits = [
        RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: format!("commit: {}", staged.subject).into(),
                },
                new: Target::Object(staged.commit),
                expected: PreviousValue::MustExistAndMatch(Target::Object(head_id)),
            },
            name: branch_ref.clone(),
            deref: false,
        },
        RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "QueryMT result integration".into(),
                },
                new: Target::Object(staged.commit),
                expected: PreviousValue::MustNotExist,
            },
            name: record_ref.clone(),
            deref: false,
        },
    ];
    if let Err(err) = repo.edit_references(edits) {
        return match classify_ref_failure(
            &repo,
            &record_ref,
            &branch_ref,
            head_id,
            staged.commit,
            err,
        ) {
            Ok(RefFailure::Untouched(outcome)) => txn.abandon(outcome),
            Ok(RefFailure::Torn(err)) => {
                // One of the two refs already names the new commit: rolling
                // the files back here could leave the branch at a commit
                // the checkout no longer matches, or a record for a commit
                // that is on no branch. The intent names the commit and the
                // record, so recovery can finish or undo it consistently.
                drop(lock);
                Err(err)
            }
            Err(err) => txn.fail(err),
        };
    }
    if let Some(error) = take_fault(&mut fault, FinalizeStage::IndexUpdate) {
        lock.leak();
        return Err(error);
    }
    let _ = std::fs::remove_file(&hooked.private_index);

    // Step 10: the real index, then the lock, then the intent. The branch has
    // moved; a failure from here on is completed by the next acceptance.
    let index_failure = |err: String| {
        WorktreeError::InvalidOperation(format!(
            "the branch {branch} advanced to {} but updating the index failed ({err}); the next \
             acceptance on this checkout completes the index update",
            staged.commit
        ))
    };
    let index = updated_index(
        &repo,
        &checkout,
        &staged.index_paths,
        &staged.flat,
        StatSource::Written {
            written: &written,
            restaged: &staged.restaged,
        },
    )
    .map_err(|e| index_failure(e.to_string()))?;
    lock.commit_index(&index)
        .map_err(|e| index_failure(e.to_string()))?;
    if let Err(err) = remove_intent(txn.git_dir) {
        tracing::warn!(error = %err, "could not remove the integration intent after committing");
    }

    let head_flat = flatten(&repo, txn.head_tree)?;
    Ok(FinalizeOutcome::Integrated(IntegratedCommit {
        commit: staged.commit.to_string(),
        parent: head,
        branch,
        changed_paths: diff_flattened(&head_flat, &staged.flat),
    }))
}

/// Consume the simulated crash when it is armed for `stage`.
fn take_fault(fault: &mut Option<FinalizeFault>, stage: FinalizeStage) -> Option<WorktreeError> {
    if fault.as_ref().is_some_and(|armed| armed.stage == stage) {
        return fault.take().map(|armed| armed.error);
    }
    None
}

/// What a failure before the reference transaction has to undo.
struct Transaction<'a> {
    repo: &'a gix::Repository,
    git_dir: &'a Path,
    head_tree: ObjectId,
    touched: &'a [ChangedPath],
    private_index: &'a Path,
}

impl Transaction<'_> {
    /// Restore the touched paths to `HEAD`, drop the intent and the private
    /// index, then return `outcome`. When the restore itself fails the
    /// intent is kept so the next acceptance can recover.
    fn abandon<T>(&self, outcome: T) -> WorktreeResult<T> {
        rollback_touched(self.repo, self.head_tree, self.touched).map_err(|err| {
            WorktreeError::InvalidOperation(format!(
                "restoring the touched paths failed ({err}); the integration intent is kept so \
                 the next acceptance can recover"
            ))
        })?;
        if let Err(err) = remove_intent(self.git_dir) {
            tracing::warn!(error = %err, "could not remove the integration intent after rollback");
        }
        let _ = std::fs::remove_file(self.private_index);
        Ok(outcome)
    }

    /// [`abandon`](Self::abandon), then fail with `err`.
    fn fail<T>(&self, err: WorktreeError) -> WorktreeResult<T> {
        self.abandon(())?;
        Err(err)
    }
}

/// The commit that was written and everything the later steps need.
struct Staged {
    commit: ObjectId,
    subject: String,
    /// `path -> (mode, id)` of the committed tree.
    flat: FlatTree,
    /// Paths whose real-index entries the commit replaces.
    index_paths: BTreeSet<String>,
    /// Paths whose committed entry differs from the merged tree (a hook
    /// re-staged them); the file on disk is not known to match.
    restaged: BTreeSet<String>,
}

/// Steps 7 and 8, plus recording the commit in the intent. Writes only
/// objects; the outer `Result` is a failure, the inner one a refusal.
fn stage(
    txn: &Transaction<'_>,
    raw_message: &[u8],
    merged_tree: ObjectId,
    head: ObjectId,
    restageable: &[ChangedPath],
    integration_ref: &str,
) -> WorktreeResult<Result<Staged, Refusal>> {
    let repo = txn.repo;
    let private = gix::index::File::at(
        txn.private_index,
        repo.object_hash(),
        false,
        Default::default(),
    )
    .map_err(native_err)?;

    let mut flat = FlatTree::new();
    let mut unmerged = Vec::new();
    for entry in private.entries() {
        let path = entry.path(&private);
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            unmerged.push(path.to_str_lossy().into_owned());
            continue;
        }
        flat.insert(path.to_owned(), (entry.mode, entry.id));
    }
    let merged_flat = flatten(repo, merged_tree)?;
    let hook_changes = diff_flattened(&merged_flat, &flat);
    let in_delta: HashSet<&str> = restageable
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    let mut outside: Vec<String> = hook_changes
        .iter()
        .map(|change| change.path.clone())
        .filter(|path| !in_delta.contains(path.as_str()))
        .chain(unmerged)
        .collect();
    if !outside.is_empty() {
        outside.sort();
        outside.dedup();
        return Ok(Err(Refusal::new(
            "hook_changed_paths_outside_delta",
            "a commit hook changed the staged content of paths this acceptance does not write; \
             those changes are left in place, nothing was committed",
        )
        .with_paths(outside)));
    }

    let message = clean_message(raw_message);
    if message.is_empty() {
        return Ok(Err(Refusal::new(
            "empty_commit_message",
            "the commit message is empty after cleanup",
        )));
    }
    let subject = String::from_utf8_lossy(message.lines().next().unwrap_or_default()).into_owned();

    let kept: HashSet<BString> = flat.keys().cloned().collect();
    let tree = write_tree_from_index(repo, &private, &kept)?;
    let identity = |role: &str, found: Option<Result<gix::actor::SignatureRef<'_>, _>>| {
        let signature = found
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!("no {role} identity is configured"))
            })?
            .map_err(native_err)?;
        signature.to_owned().map_err(native_err)
    };
    let commit = repo
        .write_object(&gix::objs::Commit {
            tree,
            parents: vec![head].into(),
            author: identity("author", repo.author())?,
            committer: identity("committer", repo.committer())?,
            encoding: None,
            message: message.into(),
            extra_headers: Default::default(),
        })
        .map_err(native_err)?
        .detach();

    let restaged: BTreeSet<String> = hook_changes.into_iter().map(|change| change.path).collect();
    let index_paths: BTreeSet<String> = txn
        .touched
        .iter()
        .map(|change| change.path.clone())
        .chain(restaged.iter().cloned())
        .collect();
    let mut intent: IntegrationIntent = read_intent(txn.git_dir)?.ok_or_else(|| {
        WorktreeError::InvalidOperation("the integration intent disappeared".into())
    })?;
    intent.commit = Some(commit.to_string());
    intent.integration_ref = Some(integration_ref.to_string());
    intent.index_paths = index_paths.iter().cloned().collect();
    write_intent(txn.git_dir, &intent)?;

    Ok(Ok(Staged {
        commit,
        subject,
        flat,
        index_paths,
        restaged,
    }))
}

/// `git commit -m` message cleanup: trailing whitespace is stripped from
/// every line, runs of blank lines collapse to one, leading and trailing
/// blank lines go, and a non-empty message ends in a newline.
pub(crate) fn clean_message(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut pending_blank = false;
    for line in raw.split(|byte| *byte == b'\n') {
        let line = line.trim_ascii_end();
        if line.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }
        if pending_blank {
            out.push(b'\n');
            pending_blank = false;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// How a failed reference transaction left the refs.
#[derive(Debug)]
enum RefFailure {
    /// Neither ref names the new commit: nothing of the transaction took
    /// effect and the checkout can be rolled back.
    Untouched(FinalizeOutcome),
    /// The transaction was partially applied (gix does not roll a failed
    /// commit back): one ref already names the new commit. Recovery must
    /// resolve it from the intent.
    Torn(WorktreeError),
}

/// What a failed reference transaction means, judged by current state. A
/// ref that already names `commit` means the transaction was torn; else an
/// existing integration record is a repeated acceptance and a branch that is
/// no longer at `head` lost the compare-and-swap.
fn classify_ref_failure(
    repo: &gix::Repository,
    record_ref: &gix::refs::FullName,
    branch_ref: &gix::refs::FullName,
    head: ObjectId,
    commit: ObjectId,
    err: impl std::fmt::Display,
) -> WorktreeResult<RefFailure> {
    let resolve = |name: &gix::refs::FullName| -> WorktreeResult<Option<ObjectId>> {
        match repo
            .try_find_reference(name.as_bstr())
            .map_err(native_err)?
        {
            Some(mut reference) => Ok(Some(reference.peel_to_id().map_err(native_err)?.detach())),
            None => Ok(None),
        }
    };
    let record = resolve(record_ref)?;
    let branch = resolve(branch_ref)?;
    if record == Some(commit) || branch == Some(commit) {
        return Ok(RefFailure::Torn(WorktreeError::InvalidOperation(format!(
            "the reference transaction for {commit} was only partially applied ({err}); the \
             integration intent is kept and recovery resolves it"
        ))));
    }
    if let Some(existing) = record {
        return Ok(RefFailure::Untouched(FinalizeOutcome::AlreadyIntegrated {
            commit: existing.to_string(),
        }));
    }
    match branch {
        Some(current) if current == head => Err(native_err(err)),
        Some(current) => Ok(RefFailure::Untouched(FinalizeOutcome::Refused(
            Refusal::new(
                "branch_moved",
                format!(
                    "the branch moved from {head} to {current} while the result was being \
                     committed"
                ),
            ),
        ))),
        None => Ok(RefFailure::Untouched(FinalizeOutcome::Refused(
            Refusal::new(
                "branch_moved",
                "the branch was deleted while the result was being committed",
            ),
        ))),
    }
}

/// Where the stat data of a rewritten index entry comes from.
pub(crate) enum StatSource<'a> {
    /// Right after this process wrote the files: an entry gets the stat of
    /// its file only when the file is still exactly what apply wrote (same
    /// stat as recorded then) and no hook re-staged a different blob.
    /// Anything else gets the default stat, which Git always re-examines, so
    /// an edit made by a hook (or anyone) after apply stays visible.
    Written {
        written: &'a std::collections::BTreeMap<String, Stat>,
        restaged: &'a BTreeSet<String>,
    },
    /// Crash recovery: files may have changed since; always the default.
    Unknown,
}

/// The real index with the entries of `index_paths` replaced by those of
/// the commit (`flat`), with stat data chosen by `stats` (see
/// [`StatSource`]): never a stat that could hide an edit.
pub(crate) fn updated_index(
    repo: &gix::Repository,
    checkout: &Path,
    index_paths: &BTreeSet<String>,
    flat: &FlatTree,
    stats: StatSource<'_>,
) -> WorktreeResult<gix::index::File> {
    let mut index = gix::index::File::at(
        repo.index_path(),
        repo.object_hash(),
        false,
        Default::default(),
    )
    .map_err(native_err)?;
    let replaced: HashSet<BString> = index_paths
        .iter()
        .map(|path| BString::from(path.as_str()))
        .collect();
    index.remove_entries(|_, path, _| replaced.contains(path));
    for path in index_paths {
        let key = BString::from(path.as_str());
        let Some((mode, id)) = flat.get(&key) else {
            continue;
        };
        let stat = match &stats {
            StatSource::Written { written, restaged } if !restaged.contains(path) => {
                match (written.get(path), current_stat(&checkout.join(path))) {
                    (Some(recorded), Some(now)) if *recorded == now => now,
                    _ => Stat::default(),
                }
            }
            _ => Stat::default(),
        };
        index.dangerously_push_entry(stat, *id, Flags::empty(), *mode, key.as_bstr());
    }
    index.sort_entries();
    // The cached tree is stale for every directory above a replaced path.
    index.remove_tree();
    Ok(index)
}

/// Stat data of the file at `path`, when it can be read.
pub(crate) fn current_stat(path: &Path) -> Option<Stat> {
    gix::index::fs::Metadata::from_path_no_follow(path)
        .ok()
        .and_then(|metadata| Stat::from_fs(&metadata).ok())
}

pub(crate) fn flatten(repo: &gix::Repository, tree: ObjectId) -> WorktreeResult<FlatTree> {
    let mut flat = FlatTree::new();
    flatten_tree(repo, tree, &mut flat, String::new())?;
    Ok(flat)
}

fn ref_name(name: &str) -> WorktreeResult<gix::refs::FullName> {
    gix::refs::FullName::try_from(name)
        .map_err(|_| WorktreeError::InvalidOperation(format!("invalid ref name: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::apply::tests::{applied, fixture, prepared, read};
    use crate::worktree::apply::{INTENT_FILE, apply_blocking};
    use crate::worktree::hooks::{
        HOOK_TIMEOUT, HookOutcome, HookRequest, HookedIntegration, run_hooks,
    };
    use crate::worktree::protect::scan_local_state;
    use crate::worktree::repo_ops::{
        FailurePoint, FaultInjectingRepoOps, GixRepoOps, RepositoryOps,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    const RECORD: &str = "refs/qmt/integrations/ws/result";

    /// The fixture result applied and hooked, ready to finalize.
    async fn ready(message: &str) -> (tempfile::TempDir, ObjectId, FinalizeRequest) {
        let (dir, base, result) = fixture();
        let prepared = prepared(dir.path(), base, result);
        let applied = applied(apply_blocking(prepared).unwrap());
        let hooked = match run_hooks(HookRequest::for_applied(&applied, message), HOOK_TIMEOUT)
            .await
            .unwrap()
        {
            HookOutcome::Passed(hooked) => hooked,
            other => panic!("expected the hooks to pass, got {other:?}"),
        };
        let request = FinalizeRequest {
            applied,
            hooked,
            integration_ref: RECORD.to_string(),
            fault: None,
        };
        (dir, base, request)
    }

    fn lock_path(root: &Path) -> PathBuf {
        root.join(".git/index.lock")
    }

    fn integrated(outcome: FinalizeOutcome) -> IntegratedCommit {
        match outcome {
            FinalizeOutcome::Integrated(commit) => commit,
            other => panic!("expected an integrated commit, got {other:?}"),
        }
    }

    fn refused(outcome: FinalizeOutcome) -> Refusal {
        match outcome {
            FinalizeOutcome::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn ref_target(root: &Path, name: &str) -> Option<ObjectId> {
        let repo = gix::open(root).unwrap();
        repo.try_find_reference(name)
            .unwrap()
            .map(|mut reference| reference.peel_to_id().unwrap().detach())
    }

    /// Branch, record, intent, private index and lock as they must look
    /// after any outcome other than `Integrated`.
    fn assert_untouched(root: &Path, base: ObjectId) {
        assert_eq!(ref_target(root, "HEAD"), Some(base));
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert_eq!(read(root, "mod.crlf"), b"old\r\n");
        assert!(!root.join("run.sh").exists());
        assert!(!root.join("new").exists());
        assert!(!root.join("link").exists());
        assert!(!root.join(".git").join(INTENT_FILE).exists());
        assert!(!lock_path(root).exists());
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("qmt-integration-"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    /// Replace the staged blob of `path` in the private index, like a hook
    /// running `git add`.
    fn restage(request: &FinalizeRequest, path: &str, content: &[u8]) -> ObjectId {
        let repo = gix::open(request.applied.prepared.checkout.as_path()).unwrap();
        let id = repo.write_blob(content).unwrap().detach();
        let mut index = gix::index::File::at(
            &request.hooked.private_index,
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        let position = index.entry_index_by_path(path.into()).unwrap();
        index.entries_mut()[position].id = id;
        index.remove_tree();
        index.write(Default::default()).unwrap();
        id
    }

    #[tokio::test]
    async fn commits_the_delta_and_updates_the_real_index() {
        let (dir, base, request) = ready("  accept result  \n\n\n\nsecond   \n\n").await;
        let root = dir.path();
        std::fs::write(root.join("scratch.local"), "mine\n").unwrap();
        std::fs::write(root.join("plain.txt"), "edited by the user\n").unwrap();
        let touched: Vec<_> = request
            .applied
            .prepared
            .touched
            .iter()
            .map(|change| change.path.clone())
            .collect();

        let commit = integrated(finalize_blocking(request).unwrap());

        // The commit: parent is the old HEAD, message cleaned, identity from
        // the repository configuration.
        let repo = gix::open(root).unwrap();
        let commit_id: ObjectId = commit.commit.parse().unwrap();
        assert_eq!(commit.parent, base.to_string());
        assert_eq!(
            commit.branch,
            repo.head_name().unwrap().unwrap().shorten().to_string()
        );
        let object = repo.find_commit(commit_id).unwrap();
        let decoded = object.decode().unwrap();
        // Trailing whitespace and blank runs go; leading spaces stay, as
        // with `git commit -m`.
        assert_eq!(decoded.message, b"  accept result\n\nsecond\n".as_bstr());
        assert_eq!(decoded.author().unwrap().name, "test");
        assert_eq!(decoded.committer().unwrap().email, "test@example.com");
        assert_eq!(decoded.parents().collect::<Vec<_>>(), vec![base]);
        let changed: Vec<_> = commit
            .changed_paths
            .iter()
            .map(|c| c.path.clone())
            .collect();
        assert_eq!(changed, touched);

        // The branch and the create-only record point at it.
        assert_eq!(ref_target(root, "HEAD"), Some(commit_id));
        assert_eq!(ref_target(root, RECORD), Some(commit_id));

        // The real index matches the commit for every touched path and is
        // untouched everywhere else, local staging included.
        let tree = object.tree_id().unwrap().detach();
        let mut committed = FlatTree::new();
        flatten_tree(&repo, tree, &mut committed, String::new()).unwrap();
        let index = gix::index::File::at(
            repo.index_path(),
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        for path in &touched {
            let entry = index.entry_by_path(path.as_str().into());
            match committed.get(path.as_bytes().as_bstr()) {
                Some((mode, id)) => {
                    let entry = entry.unwrap_or_else(|| panic!("{path} missing from the index"));
                    assert_eq!((entry.mode, entry.id), (*mode, *id), "{path}");
                }
                None => assert!(entry.is_none(), "{path} must be gone from the index"),
            }
        }
        let plain = index.entry_by_path("plain.txt".into()).unwrap();
        assert_eq!(plain.id, committed[b"plain.txt".as_bstr()].1);

        // Fresh stat data was taken from the files just written.
        let run_sh = index.entry_by_path("run.sh".into()).unwrap();
        let metadata = gix::index::fs::Metadata::from_path_no_follow(&root.join("run.sh")).unwrap();
        assert_eq!(run_sh.stat, Stat::from_fs(&metadata).unwrap());

        // Status against the new HEAD sees only the user's own work.
        let handle = open_acceptance_checkout(root).unwrap();
        let state = scan_local_state(&handle, tree).unwrap();
        let occupied: Vec<_> = state.occupied.iter().map(String::as_str).collect();
        assert_eq!(occupied, ["plain.txt", "scratch.local"]);
        assert!(state.unmerged.is_empty());
        assert_eq!(read(root, "plain.txt"), b"edited by the user\n");

        // The transaction left nothing behind.
        assert!(!lock_path(root).exists());
        assert!(!root.join(".git").join(INTENT_FILE).exists());
        let leftovers: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("qmt-integration-"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn a_hook_restaging_a_delta_path_is_committed() {
        let (dir, _base, request) = ready("msg\n").await;
        let root = dir.path();
        let id = restage(&request, "mod.crlf", b"formatted by a hook\n");

        let commit = integrated(finalize_blocking(request).unwrap());

        let repo = gix::open(root).unwrap();
        let tree = repo
            .find_commit(commit.commit.parse::<ObjectId>().unwrap())
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let mut committed = FlatTree::new();
        flatten_tree(&repo, tree, &mut committed, String::new()).unwrap();
        assert_eq!(committed[b"mod.crlf".as_bstr()].1, id);
        let index = gix::index::File::at(
            repo.index_path(),
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        assert_eq!(index.entry_by_path("mod.crlf".into()).unwrap().id, id);
    }

    #[tokio::test]
    async fn a_hook_staging_outside_the_delta_is_refused() {
        let (dir, base, request) = ready("msg\n").await;
        let root = dir.path();
        restage(&request, "keep.txt", b"sneaked in\n");

        let refusal = refused(finalize_blocking(request).unwrap());

        assert_eq!(refusal.code, "hook_changed_paths_outside_delta");
        assert_eq!(refusal.paths, ["keep.txt"]);
        assert_untouched(root, base);
        assert_eq!(ref_target(root, RECORD), None);
    }

    #[tokio::test]
    async fn restaging_a_delta_path_this_acceptance_does_not_write_is_refused() {
        // A delta path a sibling already integrated is not touched, so it
        // was never checked for local work: a hook must not stage it.
        let (dir, base, mut request) = ready("msg\n").await;
        let root = dir.path();
        request.applied.prepared.delta.push(ChangedPath {
            path: "keep.txt".into(),
            kind: crate::worktree::domain::ChangeKind::Modified,
        });
        restage(&request, "keep.txt", b"user work swept in\n");

        let refusal = refused(finalize_blocking(request).unwrap());

        assert_eq!(refusal.code, "hook_changed_paths_outside_delta");
        assert_eq!(refusal.paths, ["keep.txt"]);
        assert_untouched(root, base);
    }

    #[tokio::test]
    async fn a_hook_edit_without_restaging_stays_visible_to_status() {
        // A formatter in pre-commit rewrote a touched file but did not
        // `git add` it: the commit holds the applied blob, and the index must
        // not get the new file's stat (that would hide the edit).
        let (dir, _base, request) = ready("msg\n").await;
        let root = dir.path();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("new/dir/file.txt"), "reformatted by a hook\n").unwrap();

        let commit = integrated(finalize_blocking(request).unwrap());

        let repo = gix::open(root).unwrap();
        let index = gix::index::File::at(
            repo.index_path(),
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        let entry = index.entry_by_path("new/dir/file.txt".into()).unwrap();
        assert_eq!(entry.stat, Stat::default());
        // An untouched-by-hooks path still gets fresh stat data.
        assert_ne!(
            index.entry_by_path("run.sh".into()).unwrap().stat,
            Stat::default()
        );
        let tree = repo
            .find_commit(commit.commit.parse::<ObjectId>().unwrap())
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let handle = open_acceptance_checkout(root).unwrap();
        let state = scan_local_state(&handle, tree).unwrap();
        assert!(
            state.occupied.contains("new/dir/file.txt"),
            "{:?}",
            state.occupied
        );
    }

    #[test]
    fn a_torn_reference_transaction_is_left_for_recovery() {
        let (dir, base, result) = fixture();
        let repo = gix::open(dir.path()).unwrap();
        let commit: ObjectId = result;
        let branch = ref_name(
            repo.head_name()
                .unwrap()
                .unwrap()
                .as_bstr()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let record = ref_name(RECORD).unwrap();

        // Only the record was written.
        repo.reference(RECORD, commit, PreviousValue::MustNotExist, "torn")
            .unwrap();
        let outcome =
            classify_ref_failure(&repo, &record, &branch, base, commit, "io error").unwrap();
        assert!(matches!(outcome, RefFailure::Torn(_)), "{outcome:?}");

        // Only the branch was moved.
        repo.edit_references([RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: record.clone(),
            deref: false,
        }])
        .unwrap();
        repo.reference(branch.as_bstr(), commit, PreviousValue::Any, "torn branch")
            .unwrap();
        let outcome =
            classify_ref_failure(&repo, &record, &branch, base, commit, "io error").unwrap();
        assert!(matches!(outcome, RefFailure::Torn(_)), "{outcome:?}");

        // A branch moved to some third commit lost the compare-and-swap.
        let other = ObjectId::null(gix::hash::Kind::Sha1);
        let outcome =
            classify_ref_failure(&repo, &record, &branch, base, other, "io error").unwrap();
        assert!(
            matches!(outcome, RefFailure::Untouched(FinalizeOutcome::Refused(_))),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_refused() {
        let (dir, base, request) = ready("  \n\n \t\n").await;

        let refusal = refused(finalize_blocking(request).unwrap());

        assert_eq!(refusal.code, "empty_commit_message");
        assert_untouched(dir.path(), base);
        assert_eq!(ref_target(dir.path(), RECORD), None);
    }

    #[tokio::test]
    async fn a_branch_moved_after_the_hooks_loses_the_compare_and_swap() {
        let (dir, base, request) = ready("msg\n").await;
        let root = dir.path();
        let repo = gix::open(root).unwrap();
        let tree = repo.head_commit().unwrap().tree_id().unwrap().detach();
        let external = repo
            .commit("HEAD", "external", tree, vec![base])
            .unwrap()
            .detach();

        let refusal = refused(finalize_blocking(request).unwrap());

        assert_eq!(refusal.code, "branch_moved");
        // The external commit stays; the touched paths are restored; there is
        // no record and no half-written state.
        assert_eq!(ref_target(root, "HEAD"), Some(external));
        assert_eq!(ref_target(root, RECORD), None);
        assert_eq!(read(root, "gone.txt"), b"bye\n");
        assert!(!root.join("run.sh").exists());
        assert!(!root.join(".git").join(INTENT_FILE).exists());
        assert!(!lock_path(root).exists());
    }

    #[tokio::test]
    async fn an_existing_record_reports_the_original_commit() {
        let (dir, base, request) = ready("msg\n").await;
        let root = dir.path();
        gix::open(root)
            .unwrap()
            .reference(
                RECORD,
                base,
                PreviousValue::MustNotExist,
                "earlier integration",
            )
            .unwrap();

        let outcome = finalize_blocking(request).unwrap();

        assert_eq!(
            outcome,
            FinalizeOutcome::AlreadyIntegrated {
                commit: base.to_string()
            }
        );
        assert_untouched(root, base);
        assert_eq!(ref_target(root, RECORD), Some(base));
    }

    #[test]
    fn message_cleanup_follows_git_commit_dash_m() {
        for (raw, expected) in [
            ("subject", "subject\n"),
            ("  \n\nsubject  \n", "subject\n"),
            ("a\n\n\n\nb\n", "a\n\nb\n"),
            ("a \t\r\nb\r\n", "a\nb\n"),
            ("a\n\n\n", "a\n"),
            ("  indented\n", "  indented\n"),
            ("# not a comment\n", "# not a comment\n"),
            ("", ""),
            (" \n\t\n", ""),
        ] {
            assert_eq!(
                clean_message(raw.as_bytes()),
                expected.as_bytes(),
                "{raw:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_boundary_finalizes_runs_post_commit_and_injects_faults() {
        let (dir, base, request) = ready("msg\n").await;
        let root = dir.path();
        let ops = FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new()));
        ops.fail_next(FailurePoint::FinalizeIntegration, 1, "boom");
        let err = ops.finalize_integration(request).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        // Nothing was finalized; the failed request still holds its state,
        // which the caller must roll back (dropping the lock released it).
        assert_eq!(ref_target(root, "HEAD"), Some(base));
        assert!(!lock_path(root).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_commit_runs_after_the_lock_is_released_and_cannot_fail_the_acceptance() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _base, request) = ready("msg\n").await;
        let root = dir.path();
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("post-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\n\
             test -e .git/index.lock && echo locked > .git/post-commit-saw-lock\n\
             test -z \"$GIT_INDEX_FILE\" && echo clean > .git/post-commit-env\n\
             cat run.sh > .git/post-commit-saw-file\n\
             echo failing >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome = GixRepoOps::new()
            .finalize_integration(request)
            .await
            .unwrap();

        integrated(outcome);
        assert!(!root.join(".git/post-commit-saw-lock").exists());
        assert_eq!(read(root, ".git/post-commit-env"), b"clean\n");
        assert_eq!(
            read(root, ".git/post-commit-saw-file"),
            b"#!/bin/sh\necho hi\n"
        );
        assert!(ref_target(root, RECORD).is_some());
    }

    #[tokio::test]
    async fn hooks_data_is_independent_of_the_finalize_request_shape() {
        // `HookedIntegration` is plain data: a caller may rebuild it.
        let (dir, base, mut request) = ready("original\n").await;
        let rebuilt = HookedIntegration {
            message: b"rebuilt\n".to_vec(),
            ..request.hooked.clone()
        };
        request.hooked = rebuilt;

        let commit = integrated(finalize_blocking(request).unwrap());

        let repo = gix::open(dir.path()).unwrap();
        let object = repo
            .find_commit(commit.commit.parse::<ObjectId>().unwrap())
            .unwrap();
        assert_eq!(object.decode().unwrap().message, b"rebuilt\n".as_bstr());
        assert_eq!(commit.parent, base.to_string());
    }
}
