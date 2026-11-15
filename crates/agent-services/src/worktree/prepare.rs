//! Preparation of an explicit result acceptance (steps 1-4 of design
//! decision 9): take the checkout's real `index.lock`, preflight the target,
//! compute the typed baseline-to-result delta, three-way merge it with the
//! checkout's current HEAD, and refuse when the paths it would write carry
//! the user's own work ([`crate::worktree::protect`]).
//!
//! Nothing here changes the checkout's files, index contents, or any ref.
//! The side effects are the merged tree object written to the object
//! database (unreachable until a later phase commits it) and the lock file,
//! which is released on every refusal and handed to the caller inside
//! [`PreparedIntegration`] on success. Everything runs on the
//! calling thread; [`RepositoryOps::prepare_integration`] moves it onto the
//! blocking pool.
//!
//! [`RepositoryOps::prepare_integration`]: crate::worktree::RepositoryOps::prepare_integration

use crate::worktree::domain::ChangeKind;
use crate::worktree::error::WorktreeResult;
use crate::worktree::integrate::{
    PrepareOutcome, PrepareRequest, PreparedIntegration, Refusal, Stop, refuse,
};
use crate::worktree::protect::{IndexLock, inherited_baseline_edits, protect_local_work};
use crate::worktree::repo_ops::{
    AttributeSource, FlatTree, diff_flattened, find_external_filter, flatten_tree, native_err,
    open_acceptance_checkout, parse_oid, repo_is_sparse,
};
use gix::bstr::{BStr, BString, ByteSlice};
use gix::hash::ObjectId;
use gix::index::entry::Mode;

/// Preflight, delta and merge for `request`; see the module docs.
pub(crate) fn prepare_blocking(request: &PrepareRequest) -> WorktreeResult<PrepareOutcome> {
    match prepare(request) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Refused(refusal)) => Ok(PrepareOutcome::Refused(refusal)),
        Err(Stop::Failed(err)) => Err(err),
    }
}

/// The checkout's current branch tip, resolved by [`preflight`].
struct Target {
    head: ObjectId,
    head_tree: ObjectId,
    branch: String,
}

fn prepare(request: &PrepareRequest) -> Result<PrepareOutcome, Stop> {
    let repo = open_acceptance_checkout(&request.checkout)?;
    // Held until this function returns a refusal (dropped) or `Ready`
    // (handed to the caller). A lock that already exists is never removed.
    let lock = IndexLock::acquire(&repo.index_path())?;
    let target = preflight(&repo)?;
    let base_tree = commit_tree(&repo, "baseline", &request.baseline_commit)?;
    let result_tree = commit_tree(&repo, "result", &request.result_commit)?;

    // Delta: B0 -> R, flattened and compared without rename detection so the
    // merge below reports exactly the same paths.
    let base = flatten(&repo, base_tree)?;
    let result = flatten(&repo, result_tree)?;
    let delta = diff_flattened(&base, &result);
    if delta.is_empty() {
        return Ok(PrepareOutcome::NoChanges);
    }
    let gitlinks: Vec<String> = delta
        .iter()
        .filter(|change| {
            let path = change.path.as_bytes().as_bstr();
            [&base, &result].iter().any(|tree| {
                tree.get(path)
                    .is_some_and(|(mode, _)| *mode == Mode::COMMIT)
            })
        })
        .map(|change| change.path.clone())
        .collect();
    if !gitlinks.is_empty() {
        return Err(Refusal::new(
            "unsupported_gitlink",
            "the result changes a submodule (gitlink); submodules are unsupported in this version",
        )
        .with_paths(gitlinks)
        .into());
    }

    // Merge: layering on a moved branch works because HEAD already holds any
    // earlier accepted sibling; overlapping edits stay unresolved.
    let options = repo
        .tree_merge_options()
        .map_err(native_err)?
        .with_rewrites(None);
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some(BStr::new("baseline")),
        current: Some(BStr::new("HEAD")),
        other: Some(BStr::new("result")),
    };
    let mut merge = repo
        .merge_trees(base_tree, target.head_tree, result_tree, labels, options)
        .map_err(native_err)?;
    let mut conflicted: Vec<String> = merge
        .conflicts
        .iter()
        .filter(|conflict| conflict.is_unresolved(gix::merge::tree::TreatAsUnresolved::git()))
        .map(|conflict| conflict.ours.location().to_str_lossy().into_owned())
        .collect();
    let merged_tree = merge.tree.write().map_err(native_err)?.detach();

    let head_flat = flatten(&repo, target.head_tree)?;
    let merged_flat = flatten(&repo, merged_tree)?;
    let touched = diff_flattened(&head_flat, &merged_flat);

    // Paths that would be written must not need an external filter process
    // (Git LFS or custom clean/smudge); attributes come from the merged tree
    // itself because that is what the checkout will contain.
    let written: Vec<BString> = touched
        .iter()
        .filter(|change| change.kind != ChangeKind::Deleted)
        .map(|change| BString::from(change.path.as_str()))
        .collect();
    if !written.is_empty() {
        let index = gix::index::State::from_tree(&merged_tree, &repo.objects, Default::default())
            .map_err(native_err)?;
        if let Some(found) = find_external_filter(
            &repo,
            &index,
            written.iter().map(|path| path.as_bstr()),
            AttributeSource::IdMapping,
        )? {
            let path = found.path.to_str_lossy().into_owned();
            let message = if found.filter.as_bytes() == b"lfs" {
                format!(
                    "Git LFS content is unsupported in this version (path `{path}` resolves to filter=lfs)"
                )
            } else {
                format!(
                    "external filter processes are unsupported in this version \
                     (path `{path}` resolves to filter={})",
                    found.filter.to_str_lossy()
                )
            };
            return Err(Refusal::new("unsupported_filter", message)
                .with_paths(vec![path])
                .into());
        }
    }

    if !conflicted.is_empty() {
        conflicted.sort();
        conflicted.dedup();
        return Ok(PrepareOutcome::Conflicted { paths: conflicted });
    }
    if merged_tree == target.head_tree {
        return Ok(PrepareOutcome::NoChanges);
    }

    // Local-work protection, under the lock: unmerged entries, and any
    // staged/unstaged/untracked/ignored state on a path we would write.
    protect_local_work(&repo, target.head_tree, &touched)?;
    if let Some(source_head) = &request.source_head {
        // The baseline was the parent's working state. Paths where it already
        // differed from the HEAD it was captured on carry uncommitted parent
        // edits that the result commit also contains.
        let source_tree = commit_tree(&repo, "source HEAD", source_head)?;
        let inherited = inherited_baseline_edits(&flatten(&repo, source_tree)?, &base, &touched);
        if !inherited.is_empty() {
            return Err(Refusal::new(
                "baseline_local_edits",
                "the delegate's baseline already contained uncommitted edits to these paths, \
                 so the result cannot be separated from them",
            )
            .with_paths(inherited)
            .into());
        }
    }

    let checkout = repo
        .workdir()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| refuse("bare_checkout", "the target has no working tree"))?;
    Ok(PrepareOutcome::Ready(PreparedIntegration {
        checkout,
        head: target.head.to_string(),
        branch: target.branch,
        head_tree: target.head_tree.to_string(),
        merged_tree: merged_tree.to_string(),
        delta,
        touched,
        lock,
    }))
}

/// Refuse states in which an acceptance commit cannot or must not be made.
/// Order follows the design: unborn/detached HEAD, operation in progress,
/// sparse checkout, then identity and signing.
fn preflight(repo: &gix::Repository) -> Result<Target, Stop> {
    let head = repo.head().map_err(native_err)?;
    if head.is_unborn() {
        return Err(refuse(
            "unborn_head",
            "the target checkout has no commits yet (unborn HEAD)",
        ));
    }
    if head.is_detached() {
        return Err(refuse(
            "detached_head",
            "the target checkout has a detached HEAD; check out a branch to accept a result",
        ));
    }
    let branch = head
        .referent_name()
        .map(|name| name.shorten().to_string())
        .ok_or_else(|| refuse("detached_head", "HEAD does not name a branch"))?;
    let head_id = head
        .id()
        .ok_or_else(|| refuse("unborn_head", "HEAD does not point at a commit"))?
        .detach();
    drop(head);

    // `state()` inspects the per-worktree Git directory, so a merge left in
    // another checkout of the same repository does not block this one.
    if let Some(state) = repo.state() {
        return Err(refuse(
            "operation_in_progress",
            format!("a {state:?} operation is in progress in the target checkout"),
        ));
    }
    if repo_is_sparse(repo) {
        return Err(refuse(
            "sparse_checkout",
            "the target checkout uses sparse checkout, which is unsupported in this version",
        ));
    }
    for (role, identity) in [("committer", repo.committer()), ("author", repo.author())] {
        match identity {
            None => {
                return Err(refuse(
                    "missing_identity",
                    format!("no {role} identity is configured (set user.name and user.email)"),
                ));
            }
            Some(parsed) => {
                parsed.map_err(native_err)?;
            }
        }
    }
    if repo
        .config_snapshot()
        .boolean("commit.gpgSign")
        .unwrap_or(false)
    {
        return Err(refuse(
            "signing_unsupported",
            "commit.gpgSign is enabled; QueryMT cannot sign commits and will not create an \
             unsigned one instead",
        ));
    }

    let head_tree = repo
        .find_commit(head_id)
        .map_err(native_err)?
        .tree_id()
        .map_err(native_err)?
        .detach();
    Ok(Target {
        head: head_id,
        head_tree,
        branch,
    })
}

/// Tree of the commit named by `expr`, or an `unknown_object` refusal.
fn commit_tree(repo: &gix::Repository, role: &str, expr: &str) -> Result<ObjectId, Stop> {
    let id = parse_oid(expr)?;
    let object = repo
        .try_find_object(id)
        .map_err(native_err)?
        .ok_or_else(|| {
            refuse(
                "unknown_object",
                format!("{role} commit {expr} does not exist"),
            )
        })?;
    let commit = object.try_into_commit().map_err(|_| {
        refuse(
            "unknown_object",
            format!("{role} object {expr} is not a commit"),
        )
    })?;
    Ok(commit.tree_id().map_err(native_err)?.detach())
}

fn flatten(repo: &gix::Repository, tree: ObjectId) -> WorktreeResult<FlatTree> {
    let mut flat = FlatTree::new();
    flatten_tree(repo, tree, &mut flat, String::new())?;
    Ok(flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::domain::{ChangeKind, ChangedPath, ManagedBranch};
    use crate::worktree::repo_ops::{
        FailurePoint, FaultInjectingRepoOps, GixRepoOps, RepositoryOps, internal_signature,
    };
    use crate::worktree::test_support::{
        commit_files, git, git_with_stdin, in_child, init_repo, materialize, run_in_child,
        write_index_from_head, write_tree_recursive,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const BASE: &[(&str, &str)] = &[("a.txt", "1\n"), ("b.txt", "1\n"), ("c.txt", "c\n")];

    /// A repository whose HEAD, real index and working tree all hold `files`.
    fn committed_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, ObjectId) {
        let repo = init_repo();
        let head = advance(repo.path(), "base", files);
        (repo, head)
    }

    fn base_repo() -> (tempfile::TempDir, ObjectId) {
        committed_repo(BASE)
    }

    /// Commit `files` on HEAD and bring the working tree and real index in
    /// line with it, like a user committing their own work.
    fn advance(root: &Path, message: &str, files: &[(&str, &str)]) -> ObjectId {
        let id = commit_files(root, message, files);
        materialize(root, files);
        write_index_from_head(root);
        id
    }

    fn commit_with_tree(root: &Path, tree: ObjectId, parent: ObjectId) -> ObjectId {
        let repo = gix::open(root).unwrap();
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

    /// An unreferenced commit on top of `parent` holding exactly `files`.
    fn detached_commit(root: &Path, parent: ObjectId, files: &[(&str, &str)]) -> ObjectId {
        let repo = gix::open(root).unwrap();
        let entries = files
            .iter()
            .map(|(path, content)| {
                (
                    path.to_string(),
                    repo.write_blob(content.as_bytes()).unwrap().detach(),
                )
            })
            .collect();
        let tree = write_tree_recursive(&repo, entries);
        commit_with_tree(root, tree, parent)
    }

    fn with_file<'a>(
        files: &[(&'a str, &'a str)],
        path: &'a str,
        content: &'a str,
    ) -> Vec<(&'a str, &'a str)> {
        let mut out: Vec<_> = files.iter().filter(|(p, _)| *p != path).copied().collect();
        out.push((path, content));
        out
    }

    fn request(root: &Path, base: impl ToString, result: impl ToString) -> PrepareRequest {
        PrepareRequest {
            checkout: root.to_path_buf(),
            baseline_commit: base.to_string(),
            result_commit: result.to_string(),
            source_head: None,
        }
    }

    fn prepare_ok(root: &Path, base: impl ToString, result: impl ToString) -> PrepareOutcome {
        prepare_blocking(&request(root, base, result)).unwrap()
    }

    fn refusal(outcome: PrepareOutcome) -> Refusal {
        match outcome {
            PrepareOutcome::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn ready(outcome: PrepareOutcome) -> PreparedIntegration {
        match outcome {
            PrepareOutcome::Ready(ready) => ready,
            other => panic!("expected a prepared integration, got {other:?}"),
        }
    }

    fn changed(path: &str, kind: ChangeKind) -> ChangedPath {
        ChangedPath {
            path: path.into(),
            kind,
        }
    }

    fn blob_in(root: &Path, tree: &str, path: &str) -> ObjectId {
        let repo = gix::open(root).unwrap();
        let flat = flatten(&repo, tree.parse().unwrap()).unwrap();
        flat[path.as_bytes().as_bstr()].1
    }

    fn append_config(root: &Path, text: &str) {
        let path = root.join(".git/config");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(text);
        std::fs::write(path, config).unwrap();
    }

    /// HEAD still equals the baseline: the merge reduces to taking the result.
    #[test]
    fn head_at_the_baseline_takes_the_result() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));

        let ready = ready(prepare_ok(repo.path(), base, result));

        let g = gix::open(repo.path()).unwrap();
        let result_tree = g.find_commit(result).unwrap().tree_id().unwrap();
        assert_eq!(ready.merged_tree, result_tree.to_string());
        assert_eq!(ready.head, base.to_string());
        assert_eq!(ready.delta, vec![changed("b.txt", ChangeKind::Modified)]);
        assert_eq!(ready.touched, ready.delta);
        assert_eq!(
            ready.branch,
            g.head_name().unwrap().unwrap().shorten().to_string()
        );
    }

    /// A sibling accepted first moved HEAD; a non-overlapping result layers
    /// onto it and only its own paths are touched.
    #[test]
    fn non_overlapping_result_layers_on_a_moved_branch() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        let moved = advance(repo.path(), "sibling", &with_file(BASE, "a.txt", "x\n"));

        let ready = ready(prepare_ok(repo.path(), base, result));

        assert_eq!(ready.head, moved.to_string());
        assert_eq!(ready.delta, vec![changed("b.txt", ChangeKind::Modified)]);
        assert_eq!(ready.touched, vec![changed("b.txt", ChangeKind::Modified)]);
        let g = gix::open(repo.path()).unwrap();
        let sibling_a = g.write_blob(b"x\n").unwrap().detach();
        let result_b = g.write_blob(b"2\n").unwrap().detach();
        assert_eq!(blob_in(repo.path(), &ready.merged_tree, "a.txt"), sibling_a);
        assert_eq!(blob_in(repo.path(), &ready.merged_tree, "b.txt"), result_b);
    }

    #[test]
    fn overlapping_edits_are_reported_as_a_conflict() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        advance(repo.path(), "sibling", &with_file(BASE, "b.txt", "3\n"));

        assert_eq!(
            prepare_ok(repo.path(), base, result),
            PrepareOutcome::Conflicted {
                paths: vec!["b.txt".into()]
            }
        );
    }

    #[test]
    fn an_empty_delta_or_an_already_contained_delta_changes_nothing() {
        let (repo, base) = base_repo();
        let same = detached_commit(repo.path(), base, BASE);
        assert_eq!(
            prepare_ok(repo.path(), base, same),
            PrepareOutcome::NoChanges
        );

        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        advance(repo.path(), "sibling", &with_file(BASE, "b.txt", "2\n"));
        assert_eq!(
            prepare_ok(repo.path(), base, result),
            PrepareOutcome::NoChanges
        );
    }

    #[test]
    fn additions_and_deletions_are_typed_without_rename_detection() {
        let (repo, base) = base_repo();
        // `c.txt` moves to `moved.txt` unchanged: reported as delete + add.
        let files = [
            ("a.txt", "1\n"),
            ("b.txt", "1\n"),
            ("moved.txt", "c\n"),
            ("dir/new.txt", "n\n"),
        ];
        let result = detached_commit(repo.path(), base, &files);

        let ready = ready(prepare_ok(repo.path(), base, result));

        assert_eq!(
            ready.delta,
            vec![
                changed("c.txt", ChangeKind::Deleted),
                changed("dir/new.txt", ChangeKind::Added),
                changed("moved.txt", ChangeKind::Added),
            ]
        );
        assert_eq!(ready.touched, ready.delta);
    }

    #[test]
    fn unborn_head_is_refused() {
        let repo = init_repo();
        let r = refusal(prepare_ok(repo.path(), "1".repeat(40), "2".repeat(40)));
        assert_eq!(r.code, "unborn_head");
    }

    #[test]
    fn detached_head_is_refused() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        std::fs::write(repo.path().join(".git/HEAD"), format!("{base}\n")).unwrap();
        let r = refusal(prepare_ok(repo.path(), base, result));
        assert_eq!(r.code, "detached_head");
    }

    #[test]
    fn operations_in_progress_are_refused() {
        for marker in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            let (repo, base) = base_repo();
            let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
            std::fs::write(repo.path().join(".git").join(marker), format!("{base}\n")).unwrap();
            let r = refusal(prepare_ok(repo.path(), base, result));
            assert_eq!(r.code, "operation_in_progress", "{marker}");
        }
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        std::fs::create_dir(repo.path().join(".git/rebase-merge")).unwrap();
        let r = refusal(prepare_ok(repo.path(), base, result));
        assert_eq!(r.code, "operation_in_progress");
    }

    /// Markers are read from the per-worktree Git directory: one in the
    /// shared common directory does not block a linked checkout, one in the
    /// checkout's own Git directory does.
    #[tokio::test]
    async fn operation_markers_are_per_worktree() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        let common = repo.path().join(".git");
        let linked = tempfile::TempDir::new().unwrap();
        let checkout = linked.path().join("wt");
        let ops = GixRepoOps::new();
        let branch = ManagedBranch::from_raw("wt-branch");
        ops.create_branch(&common, &branch, &base.to_string())
            .await
            .unwrap();
        ops.add_worktree(&common, &checkout, &branch).await.unwrap();

        std::fs::write(common.join("MERGE_HEAD"), format!("{base}\n")).unwrap();
        ready(prepare_ok(&checkout, base, result));

        let git_dir = gix::open(&checkout).unwrap().git_dir().to_path_buf();
        assert_ne!(git_dir, common);
        std::fs::write(git_dir.join("MERGE_HEAD"), format!("{base}\n")).unwrap();
        let r = refusal(prepare_ok(&checkout, base, result));
        assert_eq!(r.code, "operation_in_progress");
    }

    #[test]
    fn sparse_checkout_is_refused() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        append_config(repo.path(), "\n[core]\n\tsparseCheckout = true\n");
        let r = refusal(prepare_ok(repo.path(), base, result));
        assert_eq!(r.code, "sparse_checkout");
    }

    #[test]
    fn unknown_objects_are_refused() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        let missing = "1".repeat(40);

        let r = refusal(prepare_ok(repo.path(), &missing, result));
        assert_eq!(r.code, "unknown_object");
        assert!(r.message.contains("baseline"), "{}", r.message);
        let r = refusal(prepare_ok(repo.path(), base, &missing));
        assert_eq!(r.code, "unknown_object");
        assert!(r.message.contains("result"), "{}", r.message);

        // A tree id is not a commit.
        let g = gix::open(repo.path()).unwrap();
        let tree = g.find_commit(base).unwrap().tree_id().unwrap();
        let r = refusal(prepare_ok(repo.path(), base, tree));
        assert_eq!(r.code, "unknown_object");
        assert!(r.message.contains("not a commit"), "{}", r.message);
    }

    #[test]
    fn commit_signing_is_refused_instead_of_creating_unsigned_commits() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        append_config(repo.path(), "\n[commit]\n\tgpgSign = true\n");
        let r = refusal(prepare_ok(repo.path(), base, result));
        assert_eq!(r.code, "signing_unsupported");
    }

    /// With no identity in repository, global, or environment scope the
    /// acceptance is refused; no fallback identity is invented.
    #[test]
    fn missing_identity_is_refused() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        let config_path = repo.path().join(".git/config");
        let config = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(&config_path, config.split("\n[user]").next().unwrap()).unwrap();
        let home = tempfile::TempDir::new().unwrap();
        run_in_child(
            "worktree::prepare::tests::missing_identity_child",
            &[
                ("HOME", home.path().as_os_str()),
                ("XDG_CONFIG_HOME", home.path().join("xdg").as_os_str()),
                ("QMT_TEST_REPO", repo.path().as_os_str()),
                ("QMT_TEST_BASE", base.to_string().as_ref()),
                ("QMT_TEST_RESULT", result.to_string().as_ref()),
            ],
        );
    }

    #[test]
    fn missing_identity_child() {
        if !in_child() {
            return;
        }
        let var = |name: &str| std::env::var(name).unwrap();
        let root = PathBuf::from(var("QMT_TEST_REPO"));
        let r = refusal(prepare_ok(
            &root,
            var("QMT_TEST_BASE"),
            var("QMT_TEST_RESULT"),
        ));
        assert_eq!(r.code, "missing_identity", "{}", r.message);
    }

    #[test]
    fn submodule_changes_are_refused_with_the_path() {
        let (repo, base) = base_repo();
        let g = gix::open(repo.path()).unwrap();
        let mut entries: Vec<gix::objs::tree::Entry> = BASE
            .iter()
            .map(|(path, content)| gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: (*path).into(),
                oid: g.write_blob(content.as_bytes()).unwrap().detach(),
            })
            .collect();
        entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Commit.into(),
            filename: "vendor".into(),
            oid: base,
        });
        entries.sort();
        let tree = g
            .write_object(&gix::objs::Tree { entries })
            .unwrap()
            .detach();
        let result = commit_with_tree(repo.path(), tree, base);

        let r = refusal(prepare_ok(repo.path(), base, result));

        assert_eq!(r.code, "unsupported_gitlink");
        assert_eq!(r.paths, vec!["vendor".to_string()]);
    }

    /// Attributes are resolved from the merged tree, not from a text scan or
    /// the working directory: the result brings its own `.gitattributes`.
    #[test]
    fn lfs_content_introduced_by_the_result_is_refused() {
        let (repo, base) = base_repo();
        let mut files = BASE.to_vec();
        files.push((
            ".gitattributes",
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        ));
        files.push(("assets/big.bin", "pointer\n"));
        let result = detached_commit(repo.path(), base, &files);

        let r = refusal(prepare_ok(repo.path(), base, result));

        assert_eq!(r.code, "unsupported_filter");
        assert_eq!(r.paths, vec!["assets/big.bin".to_string()]);
        assert!(r.message.contains("filter=lfs"), "{}", r.message);
    }

    /// The checkout's own attributes apply to the result's paths too.
    #[test]
    fn filters_declared_on_the_current_branch_apply_to_result_paths() {
        let repo = init_repo();
        let mut files = BASE.to_vec();
        files.push((".gitattributes", "*.dat filter=custom\n"));
        files.push(("x.dat", "1\n"));
        let base = advance(repo.path(), "base", &files);
        let result = detached_commit(repo.path(), base, &with_file(&files, "x.dat", "2\n"));

        let r = refusal(prepare_ok(repo.path(), base, result));

        assert_eq!(r.code, "unsupported_filter");
        assert_eq!(r.paths, vec!["x.dat".to_string()]);
        assert!(r.message.contains("filter=custom"), "{}", r.message);
    }

    /// Deleting a path never needs a filter, so it is not refused.
    #[test]
    fn deleting_filtered_paths_is_allowed() {
        let repo = init_repo();
        let mut files = BASE.to_vec();
        files.push((".gitattributes", "*.dat filter=custom\n"));
        files.push(("x.dat", "1\n"));
        let base = advance(repo.path(), "base", &files);
        let remaining: Vec<_> = files
            .iter()
            .filter(|(p, _)| *p != "x.dat")
            .copied()
            .collect();
        let result = detached_commit(repo.path(), base, &remaining);

        let ready = ready(prepare_ok(repo.path(), base, result));

        assert_eq!(ready.touched, vec![changed("x.dat", ChangeKind::Deleted)]);
    }

    /// Preparation is read-only: HEAD, the branch, the index and the working
    /// files are byte-for-byte unchanged.
    #[test]
    fn preparation_does_not_touch_the_checkout() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        std::fs::write(repo.path().join("a.txt"), "local edit\n").unwrap();
        let snapshot = |root: &Path| {
            let g = gix::open(root).unwrap();
            (
                g.head_id().unwrap().detach(),
                std::fs::read(root.join(".git/HEAD")).unwrap(),
                std::fs::read(root.join(".git/index")).ok(),
                std::fs::read(root.join("a.txt")).unwrap(),
                std::fs::read(root.join("b.txt")).unwrap(),
            )
        };
        let before = snapshot(repo.path());

        ready(prepare_ok(repo.path(), base, result));

        assert_eq!(snapshot(repo.path()), before);
    }

    #[tokio::test]
    async fn fault_injection_reaches_prepare_integration() {
        let (repo, base) = base_repo();
        let result = detached_commit(repo.path(), base, &with_file(BASE, "b.txt", "2\n"));
        let ops = FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new()));
        ops.fail_next(FailurePoint::PrepareIntegration, 1, "boom");

        let err = ops
            .prepare_integration(request(repo.path(), base, result))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");

        // The injected failure is consumed; the next call runs for real.
        let outcome = ops
            .prepare_integration(request(repo.path(), base, result))
            .await
            .unwrap();
        ready(outcome);
    }

    // ---- local-work protection and the real index.lock (task 22.4) --------

    /// Everything a refused preparation must leave alone: HEAD, refs, the
    /// real index bytes, and every working file. Objects legitimately grow
    /// (the merged tree), so `.git` is fingerprinted selectively.
    fn fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if rel == ".git" {
                    for name in ["HEAD", "index"] {
                        if let Ok(bytes) = std::fs::read(path.join(name)) {
                            out.push((format!(".git/{name}"), bytes));
                        }
                    }
                    walk(&path.join("refs"), root, out);
                } else if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push((rel, std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    /// Prepare and assert a refusal with `code` naming exactly `paths`,
    /// leaving the checkout byte-for-byte as it was and no lock behind.
    fn assert_refused_unchanged(
        root: &Path,
        base: ObjectId,
        result: ObjectId,
        code: &str,
        paths: &[&str],
    ) {
        let before = fingerprint(root);
        let r = refusal(prepare_ok(root, base, result));
        assert_eq!(r.code, code, "{}", r.message);
        let expected: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        assert_eq!(r.paths, expected);
        assert_eq!(fingerprint(root), before, "a refusal must change nothing");
        assert!(
            !root.join(".git/index.lock").exists(),
            "a refusal must release the index lock"
        );
    }

    fn result_editing_b(root: &Path, base: ObjectId) -> ObjectId {
        detached_commit(root, base, &with_file(BASE, "b.txt", "2\n"))
    }

    #[test]
    fn unstaged_edits_to_a_touched_path_are_refused() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        std::fs::write(repo.path().join("b.txt"), "my edit\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["b.txt"],
        );
    }

    #[test]
    fn staged_changes_to_a_touched_path_are_refused() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        std::fs::write(repo.path().join("b.txt"), "staged edit\n").unwrap();
        git(repo.path(), &["add", "b.txt"]);
        // The working file now matches the index: the state is purely staged.
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["b.txt"],
        );
    }

    #[test]
    fn staged_deletions_of_a_touched_path_are_refused() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        git(repo.path(), &["rm", "--quiet", "b.txt"]);
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["b.txt"],
        );
    }

    #[test]
    fn untracked_files_at_an_added_path_are_refused() {
        let (repo, base) = base_repo();
        let mut files = BASE.to_vec();
        files.push(("new.txt", "from the delegate\n"));
        let result = detached_commit(repo.path(), base, &files);
        std::fs::write(repo.path().join("new.txt"), "mine\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["new.txt"],
        );
    }

    #[test]
    fn ignored_files_at_an_added_path_are_refused() {
        let files = [("a.txt", "1\n"), (".gitignore", "*.log\nbuild/\n")];
        let (repo, base) = committed_repo(&files);
        let mut with_log = files.to_vec();
        with_log.push(("debug.log", "delegate\n"));
        let result = detached_commit(repo.path(), base, &with_log);
        std::fs::write(repo.path().join("debug.log"), "my ignored log\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["debug.log"],
        );
    }

    /// A result path below an ignored directory collides with that directory.
    #[test]
    fn paths_inside_ignored_directories_are_refused() {
        let files = [("a.txt", "1\n"), (".gitignore", "build/\n")];
        let (repo, base) = committed_repo(&files);
        let mut with_out = files.to_vec();
        with_out.push(("build/out.txt", "delegate\n"));
        let result = detached_commit(repo.path(), base, &with_out);
        std::fs::create_dir(repo.path().join("build")).unwrap();
        std::fs::write(repo.path().join("build/cache.bin"), "mine").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["build/out.txt"],
        );
    }

    /// An untracked file where the result needs a directory, and an
    /// untracked directory where the result needs a file, both collide.
    #[test]
    fn file_and_directory_containment_collisions_are_refused() {
        let (repo, base) = base_repo();
        let mut dir_needed = BASE.to_vec();
        dir_needed.push(("lib/mod.txt", "delegate\n"));
        let result = detached_commit(repo.path(), base, &dir_needed);
        std::fs::write(repo.path().join("lib"), "I am a file\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["lib/mod.txt"],
        );
        std::fs::remove_file(repo.path().join("lib")).unwrap();

        let mut file_needed = BASE.to_vec();
        file_needed.push(("docs", "delegate\n"));
        let result = detached_commit(repo.path(), base, &file_needed);
        std::fs::create_dir(repo.path().join("docs")).unwrap();
        std::fs::write(repo.path().join("docs/notes.md"), "mine\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["docs"],
        );
    }

    /// Local state on paths the result does not write is the user's business:
    /// preparation proceeds and stays delta-only.
    #[test]
    fn unrelated_local_work_does_not_block_acceptance() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        std::fs::write(repo.path().join("a.txt"), "unstaged\n").unwrap();
        std::fs::write(repo.path().join("c.txt"), "staged\n").unwrap();
        git(repo.path(), &["add", "c.txt"]);
        std::fs::write(repo.path().join("scratch.txt"), "untracked\n").unwrap();

        let ready = ready(prepare_ok(repo.path(), base, result));

        assert_eq!(ready.touched, vec![changed("b.txt", ChangeKind::Modified)]);
    }

    /// `status.showUntrackedFiles=no` must not hide untracked files from the
    /// protection scan.
    #[test]
    fn hidden_untracked_files_are_still_protected() {
        let (repo, base) = base_repo();
        let mut files = BASE.to_vec();
        files.push(("new.txt", "from the delegate\n"));
        let result = detached_commit(repo.path(), base, &files);
        append_config(repo.path(), "\n[status]\n\tshowUntrackedFiles = no\n");
        std::fs::write(repo.path().join("new.txt"), "mine\n").unwrap();
        assert_refused_unchanged(
            repo.path(),
            base,
            result,
            "touches_local_changes",
            &["new.txt"],
        );
    }

    /// Unresolved merge entries refuse acceptance outright, naming the paths,
    /// even when they are unrelated to the delta.
    #[test]
    fn unmerged_index_entries_are_refused() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        let blob =
            |content: &str| git_with_stdin(repo.path(), &["hash-object", "-w", "--stdin"], content);
        let (one, two, three) = (blob("base\n"), blob("ours\n"), blob("theirs\n"));
        git_with_stdin(
            repo.path(),
            &["update-index", "--index-info"],
            &format!(
                "0 {zero}\tc.txt\n100644 {one} 1\tc.txt\n100644 {two} 2\tc.txt\n\
                 100644 {three} 3\tc.txt\n",
                zero = "0".repeat(40)
            ),
        );
        assert_refused_unchanged(repo.path(), base, result, "unmerged_entries", &["c.txt"]);
    }

    #[test]
    fn baseline_local_edits_on_touched_paths_are_refused() {
        let (repo, head) = base_repo();
        // The parent had an uncommitted edit to b.txt when the delegate's
        // baseline was captured; the delegate then edited b.txt further.
        let baseline =
            detached_commit(repo.path(), head, &with_file(BASE, "b.txt", "parent wip\n"));
        let result = detached_commit(
            repo.path(),
            baseline,
            &with_file(BASE, "b.txt", "parent wip\ndelegate\n"),
        );
        // The parent then committed that work-in-progress itself, so HEAD
        // matches the baseline for b.txt and the merge is clean; the result
        // still cannot be separated from the inherited edit.
        advance(
            repo.path(),
            "parent commits its wip",
            &with_file(BASE, "b.txt", "parent wip\n"),
        );
        let mut req = request(repo.path(), baseline, result);
        req.source_head = Some(head.to_string());

        let before = fingerprint(repo.path());
        let r = refusal(prepare_blocking(&req).unwrap());

        assert_eq!(r.code, "baseline_local_edits", "{}", r.message);
        assert_eq!(r.paths, vec!["b.txt".to_string()]);
        assert_eq!(fingerprint(repo.path()), before);
        assert!(!repo.path().join(".git/index.lock").exists());
    }

    /// The spec's delta-only scenario: the baseline carried an edit to A, the
    /// delegate changed only B. B is accepted and A's inherited edit is not.
    #[test]
    fn inherited_edits_to_other_paths_are_not_swept_in() {
        let (repo, head) = base_repo();
        let baseline =
            detached_commit(repo.path(), head, &with_file(BASE, "a.txt", "parent wip\n"));
        let wip_with_b = with_file(&with_file(BASE, "a.txt", "parent wip\n"), "b.txt", "2\n");
        let result = detached_commit(repo.path(), baseline, &wip_with_b);
        let mut req = request(repo.path(), baseline, result);
        req.source_head = Some(head.to_string());

        let ready = ready(prepare_blocking(&req).unwrap());

        assert_eq!(ready.delta, vec![changed("b.txt", ChangeKind::Modified)]);
        assert_eq!(ready.touched, vec![changed("b.txt", ChangeKind::Modified)]);
        let g = gix::open(repo.path()).unwrap();
        let original_a = g.write_blob(b"1\n").unwrap().detach();
        assert_eq!(
            blob_in(repo.path(), &ready.merged_tree, "a.txt"),
            original_a
        );
    }

    /// An existing `index.lock` (Git, another process, a crash) is never
    /// taken over or removed.
    #[test]
    fn a_foreign_index_lock_is_respected_and_left_alone() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        let lock = repo.path().join(".git/index.lock");
        std::fs::write(&lock, "someone else\n").unwrap();

        let r = refusal(prepare_ok(repo.path(), base, result));

        assert_eq!(r.code, "index_locked", "{}", r.message);
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "someone else\n");
    }

    /// The lock is held exactly as long as the prepared integration lives and
    /// excludes a second preparation on the same checkout.
    #[test]
    fn the_index_lock_is_held_until_the_prepared_integration_is_dropped() {
        let (repo, base) = base_repo();
        let result = result_editing_b(repo.path(), base);
        let lock = repo.path().join(".git/index.lock");
        assert!(!lock.exists());

        let first = ready(prepare_ok(repo.path(), base, result));
        assert!(lock.exists(), "the lock must be held while Ready is alive");
        assert_eq!(first.lock.path(), lock);

        let second = refusal(prepare_ok(repo.path(), base, result));
        assert_eq!(second.code, "index_locked");
        assert!(lock.exists(), "the second attempt must not release ours");

        drop(first);
        assert!(!lock.exists(), "dropping releases the lock");
        ready(prepare_ok(repo.path(), base, result));
    }

    /// Every early refusal and non-ready outcome releases the lock too.
    #[test]
    fn non_ready_outcomes_release_the_lock() {
        let (repo, base) = base_repo();
        let lock = repo.path().join(".git/index.lock");

        let same = detached_commit(repo.path(), base, BASE);
        assert_eq!(
            prepare_ok(repo.path(), base, same),
            PrepareOutcome::NoChanges
        );
        assert!(!lock.exists());

        refusal(prepare_ok(repo.path(), base, "1".repeat(40)));
        assert!(!lock.exists());

        let result = result_editing_b(repo.path(), base);
        advance(repo.path(), "sibling", &with_file(BASE, "b.txt", "3\n"));
        assert!(matches!(
            prepare_ok(repo.path(), base, result),
            PrepareOutcome::Conflicted { .. }
        ));
        assert!(!lock.exists());
    }
}
