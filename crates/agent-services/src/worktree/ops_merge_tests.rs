//! Scenario tests of `WorktreeOps::merge` over a real repository: an
//! unmanaged source checkout (session `sess-src`) and a top-level worktree
//! created from it and owned by `sess-wt`.

use super::*;
use crate::worktree::GitRunner as _;
use crate::worktree::git::CliGit;
use crate::worktree::test_support::{commit_files, init_repo, materialize};
use crate::worktree::{FailurePoint, FaultInjectingRepoOps, GixRepoOps, SessionCreationOptions};
use parking_lot::Mutex as StdMutex;
use querymt_agent_sqlite::repo_workspace::SqliteWorkspaceRepository;
use querymt_agent_sqlite::session::schema;
use rusqlite::Connection;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// One `announce_archived` call: owner, merge, removal.
type Announced = (String, Option<ArchiveMerge>, Option<ArchiveRemoval>);

/// The session layer, reduced to what merge asks of it.
#[derive(Default)]
struct TestHost {
    busy: std::sync::Mutex<HashSet<String>>,
    announced: std::sync::Mutex<Vec<Announced>>,
    source_cwd: std::sync::Mutex<Option<PathBuf>>,
}

impl TestHost {
    fn set_busy(&self, session_id: &str) {
        self.busy.lock().unwrap().insert(session_id.to_owned());
    }

    fn check_idle(&self, session_id: &str) -> Result<(), WorktreeOpError> {
        if self.busy.lock().unwrap().contains(session_id) {
            return Err(WorktreeOpError::rejected(
                "owner_active",
                format!("session {session_id} has a running turn or queued input"),
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl OwnerHost for TestHost {
    async fn is_local(&self, _session_id: &str) -> Result<bool, WorktreeOpError> {
        Ok(true)
    }

    async fn quiesce_owner(&self, session_id: &str) -> Result<(), WorktreeOpError> {
        self.check_idle(session_id)
    }

    async fn ensure_idle(&self, session_id: &str) -> Result<(), WorktreeOpError> {
        self.check_idle(session_id)
    }

    async fn session_cwd(&self, session_id: &str) -> Result<Option<PathBuf>, WorktreeOpError> {
        Ok(if session_id == "sess-src" {
            self.source_cwd.lock().unwrap().clone()
        } else {
            None
        })
    }

    async fn announce_archived(
        &self,
        owner: &str,
        _workspace_id: &str,
        _by_session_id: Option<&str>,
        removal: Option<ArchiveRemoval>,
        merged: Option<ArchiveMerge>,
    ) -> Option<i64> {
        let mut announced = self.announced.lock().unwrap();
        announced.push((owner.to_owned(), merged, removal));
        Some(announced.len() as i64)
    }
}

struct World {
    repo: TempDir,
    _storage: TempDir,
    faults: Arc<FaultInjectingRepoOps>,
    ops: WorktreeOps,
    record: WorkspaceRecord,
    host: TestHost,
}

async fn git(path: &Path, args: &[&str]) -> String {
    CliGit::new()
        .run(path, args)
        .await
        .unwrap()
        .stdout_str()
        .trim()
        .to_string()
}

/// `dirty_source`: the source has an uncommitted edit to `file.txt` when the
/// worktree is created from its current working tree.
async fn world(dirty_source: bool) -> World {
    let repo = init_repo();
    let files = [("file.txt", "one"), (".gitignore", "*.log\n")];
    commit_files(repo.path(), "init", &files);
    materialize(repo.path(), &files);
    // gix fixtures leave no index: build it so the checkout is clean.
    git(repo.path(), &["reset", "-q"]).await;
    let storage = TempDir::new().unwrap();
    let faults = Arc::new(FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new())));
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
    schema::init_schema(&mut conn).unwrap();
    for id in ["sess-src", "sess-wt"] {
        conn.execute(
            "INSERT INTO sessions (public_id, name, created_at, updated_at) \
             VALUES (?1, ?1, '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
            [id],
        )
        .unwrap();
    }
    let service = Arc::new(WorkspaceService::new(
        faults.clone(),
        Arc::new(SqliteWorkspaceRepository::new(Arc::new(StdMutex::new(
            conn,
        )))),
        storage.path(),
    ));
    let baseline = if dirty_source {
        fs::write(repo.path().join("file.txt"), "uncommitted").unwrap();
        BaselineMode::CurrentWorkingTree
    } else {
        BaselineMode::Commit
    };
    let binding = service
        .allocate_for_new_session(
            repo.path(),
            &SessionCreationOptions::isolated_worktree(baseline).with_source_session("sess-src"),
        )
        .await
        .unwrap()
        .expect("isolation allocates");
    let record = service
        .bind_owner(&binding.record.id, "sess-wt")
        .await
        .unwrap();
    let host = TestHost::default();
    *host.source_cwd.lock().unwrap() = Some(repo.path().to_path_buf());
    World {
        repo,
        _storage: storage,
        faults,
        ops: WorktreeOps::new(service),
        record,
        host,
    }
}

impl World {
    fn src(&self) -> &Path {
        self.repo.path()
    }

    fn wt(&self) -> &Path {
        &self.record.checkout_path
    }

    fn target(&self) -> WorkspaceTarget {
        WorkspaceTarget::Workspace(self.record.id.to_string())
    }

    fn write_wt(&self, path: &str, content: &str) {
        fs::write(self.wt().join(path), content).unwrap();
    }

    async fn commit_wt(&self, path: &str, content: &str) -> String {
        self.write_wt(path, content);
        git(self.wt(), &["add", path]).await;
        git(self.wt(), &["commit", "-q", "-m", path]).await;
        git(self.wt(), &["rev-parse", "HEAD"]).await
    }

    async fn commit_src(&self, path: &str, content: &str) {
        fs::write(self.src().join(path), content).unwrap();
        git(self.src(), &["add", path]).await;
        git(self.src(), &["commit", "-q", "-m", path]).await;
    }

    async fn src_head(&self) -> String {
        git(self.src(), &["rev-parse", "HEAD"]).await
    }

    async fn merge_with(
        &self,
        commit: Option<&str>,
        follow_up: Option<FollowUp>,
    ) -> Result<WorktreeMergeResponse, WorktreeOpError> {
        self.ops
            .merge(
                &self.host,
                &self.target(),
                MergeOptions {
                    commit: commit.map(str::to_owned),
                    follow_up,
                },
            )
            .await
    }

    async fn merge(&self) -> Result<WorktreeMergeResponse, WorktreeOpError> {
        self.merge_with(None, None).await
    }

    async fn results(&self) -> Vec<ResultProvenance> {
        self.ops
            .service()
            .registry()
            .list_results(&self.record.id)
            .await
            .unwrap()
    }

    /// Publish the working state without merging.
    async fn publish(&self) -> String {
        self.ops
            .publish(&self.target())
            .await
            .unwrap()
            .result
            .result_commit
            .unwrap()
    }
}

fn paths(response: &WorktreeMergeResponse) -> Vec<&str> {
    response
        .changed_paths
        .iter()
        .map(|c| c.path.as_str())
        .collect()
}

fn refused(result: Result<WorktreeMergeResponse, WorktreeOpError>) -> WorktreeOpError {
    match result {
        Ok(done) => panic!("expected a refusal, merged: {done:?}"),
        Err(err) => err,
    }
}

fn error_paths(err: &WorktreeOpError) -> Vec<String> {
    match err {
        WorktreeOpError::Refused { paths, .. } => paths.clone(),
        other => panic!("expected a refusal with paths, got {other:?}"),
    }
}

// ── Version selection ───────────────────────────────────────────────────────

/// A new commit, staged and unstaged edits, a non-ignored untracked file and
/// an ignored one: the snapshot's delta (without the ignored file) is merged
/// and the worktree itself is untouched.
#[tokio::test]
async fn default_merge_snapshots_the_working_state() {
    let w = world(false).await;
    w.commit_wt("committed.txt", "c").await;
    w.write_wt("staged.txt", "s");
    git(w.wt(), &["add", "staged.txt"]).await;
    w.write_wt("file.txt", "unstaged edit");
    w.write_wt("untracked.txt", "u");
    w.write_wt("junk.log", "ignored");
    let head_before = git(w.wt(), &["rev-parse", "HEAD"]).await;
    let status_before = git(w.wt(), &["status", "--porcelain", "--ignored"]).await;

    let done = w.merge().await.unwrap();

    assert_eq!(done.outcome, "merged");
    assert!(done.published);
    assert_eq!(
        paths(&done),
        ["committed.txt", "file.txt", "staged.txt", "untracked.txt"]
    );
    assert_eq!(done.ignored_files, ["junk.log"]);
    assert_eq!(w.results().await.len(), 1);
    assert_eq!(
        done.result.result_commit,
        w.results().await[0].result_commit
    );
    assert_eq!(done.commit.as_deref(), Some(w.src_head().await.as_str()));
    assert!(!w.src().join("junk.log").exists());
    assert_eq!(
        fs::read_to_string(w.src().join("untracked.txt")).unwrap(),
        "u"
    );
    // The worktree's branch, index and files are unchanged.
    assert_eq!(git(w.wt(), &["rev-parse", "HEAD"]).await, head_before);
    assert_eq!(
        git(w.wt(), &["status", "--porcelain", "--ignored"]).await,
        status_before
    );
}

/// An earlier retained result is merged as it was, whatever the worktree
/// holds now, and nothing new is published.
#[tokio::test]
async fn merge_a_previously_published_result() {
    let w = world(false).await;
    w.write_wt("early.txt", "early");
    let early = w.publish().await;
    w.write_wt("late.txt", "late");

    let done = w.merge_with(Some(&early[..8]), None).await.unwrap();

    assert_eq!(done.outcome, "merged");
    assert!(!done.published);
    assert_eq!(paths(&done), ["early.txt"]);
    assert_eq!(w.results().await.len(), 1);
    assert!(w.src().join("early.txt").exists());
    assert!(!w.src().join("late.txt").exists());
}

/// A commit on the worktree branch is retained as a result and its delta
/// merged: later commits are not part of it.
#[tokio::test]
async fn merge_a_commit_on_the_worktree_branch() {
    let w = world(false).await;
    let first = w.commit_wt("first.txt", "1").await;
    w.commit_wt("second.txt", "2").await;

    let done = w.merge_with(Some(&first), None).await.unwrap();

    assert_eq!(done.outcome, "merged");
    assert!(!done.published);
    assert_eq!(paths(&done), ["first.txt"]);
    let results = w.results().await;
    assert_eq!(results.len(), 1, "the branch commit is retained");
    assert_eq!(results[0].result_commit.as_deref(), Some(first.as_str()));
    assert!(
        results[0]
            .result_ref
            .as_deref()
            .is_some_and(|r| r.starts_with("refs/qmt/results/"))
    );
    assert!(!w.src().join("second.txt").exists());
}

/// A commit outside the worktree's own history, the baseline itself, and an
/// ambiguous result prefix are refused without changes.
#[tokio::test]
async fn merge_refuses_an_unrelated_commit() {
    let w = world(false).await;
    w.write_wt("b.txt", "b");
    let baseline = w.record.baseline.as_ref().unwrap().baseline_commit.clone();
    w.commit_src("elsewhere.txt", "e").await;
    let unrelated = w.src_head().await;
    let head = w.src_head().await;

    for rev in [unrelated.as_str(), baseline.as_str(), "feedfacefeedface"] {
        let err = refused(w.merge_with(Some(rev), None).await);
        assert_eq!(err.code(), "commit_not_on_workspace_branch", "{rev}");
    }

    // Two retained results sharing a prefix cannot be told apart.
    let registry = w.ops.service().registry();
    for tail in ["1", "2"] {
        registry
            .record_result(
                &w.record.id,
                &ResultProvenance {
                    baseline_commit: baseline.clone(),
                    result_commit: Some(format!("abcdef1{tail}{}", "0".repeat(32))),
                    result_ref: None,
                    verification: None,
                    captured_at: time::OffsetDateTime::now_utc(),
                    captured_head: None,
                    delegation_id: None,
                },
            )
            .await
            .unwrap();
    }
    // Shorter than seven digits is not a result prefix at all.
    let err = refused(w.merge_with(Some("abcdef"), None).await);
    assert_eq!(err.code(), "commit_not_on_workspace_branch");
    // Seven digits match both rows.
    let err = refused(w.merge_with(Some("abcdef1"), None).await);
    assert_eq!(err.code(), "ambiguous_result_commit");
    assert_eq!(error_paths(&err).len(), 2);
    assert_eq!(w.src_head().await, head);
    assert!(!w.src().join("b.txt").exists());
}

/// An archived workspace merges only an explicit version; the other
/// lifecycles are refused with their own code.
#[tokio::test]
async fn merge_refused_for_an_unmergeable_workspace() {
    let w = world(false).await;
    w.write_wt("b.txt", "b");
    let result = w.publish().await;
    w.ops.archive(&w.host, &w.target()).await.unwrap();
    let head = w.src_head().await;

    let err = refused(w.merge().await);
    assert_eq!(err.code(), "workspace_archived");
    assert_eq!(w.src_head().await, head);
    assert_eq!(w.results().await.len(), 1, "no snapshot of retired state");

    // An explicit version is still mergeable.
    let done = w.merge_with(Some(&result), None).await.unwrap();
    assert_eq!(done.outcome, "merged");

    // A workspace that is not ready is refused with its lifecycle.
    let w = world(false).await;
    let registry = w.ops.service().registry().clone();
    let mut failed = registry.get_workspace(&w.record.id).await.unwrap().unwrap();
    failed.lifecycle = WorkspaceLifecycle::Failed;
    registry.update_workspace(failed).await.unwrap();
    let err = refused(w.merge().await);
    assert_eq!(err.code(), "workspace_not_ready");
    assert!(err.to_string().contains("failed"), "{err}");
}

// ── Busy sessions ───────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_while_the_owner_is_busy() {
    let w = world(false).await;
    w.write_wt("b.txt", "b");
    w.host.set_busy("sess-wt");

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "owner_active");
    assert!(w.results().await.is_empty(), "no result is published");
}

#[tokio::test]
async fn merge_refused_while_the_source_session_is_busy() {
    let w = world(false).await;
    w.write_wt("b.txt", "b");
    let head = w.src_head().await;
    w.host.set_busy("sess-src");

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "source_active");
    assert!(w.results().await.is_empty(), "nothing is published either");
    assert_eq!(w.src_head().await, head);
}

// ── Protection of local work, hooks, conflicts ──────────────────────────────

#[tokio::test]
async fn merge_refuses_touched_paths_with_local_edits() {
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    fs::write(w.src().join("b.txt"), "mine").unwrap();
    let head = w.src_head().await;

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "touches_local_changes");
    assert_eq!(error_paths(&err), ["b.txt"]);
    assert_eq!(err.error_data()["paths"], serde_json::json!(["b.txt"]));
    assert_eq!(fs::read_to_string(w.src().join("b.txt")).unwrap(), "mine");
    assert_eq!(w.src_head().await, head);
}

/// `file.txt` was uncommitted in the source when the worktree was created
/// from its working tree; the worktree then edited it further, and the source
/// has since committed that work in progress itself, so the merge itself is
/// clean. The worktree's result still cannot be separated from the inherited
/// edit, so it is refused.
#[tokio::test]
async fn merge_refuses_inherited_uncommitted_edits() {
    let w = world(true).await;
    w.write_wt("file.txt", "uncommitted\nworktree edit\n");
    w.commit_src("file.txt", "uncommitted").await;
    let head = w.src_head().await;

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "baseline_local_edits");
    assert_eq!(error_paths(&err), ["file.txt"]);
    assert_eq!(w.src_head().await, head);
    assert_eq!(
        fs::read_to_string(w.src().join("file.txt")).unwrap(),
        "uncommitted"
    );
    assert_eq!(git(w.src(), &["status", "--porcelain"]).await, "");
}

#[tokio::test]
async fn merge_reports_overlapping_edits_as_a_conflict() {
    let w = world(false).await;
    w.write_wt("file.txt", "worktree");
    w.commit_src("file.txt", "source").await;
    let head = w.src_head().await;

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "merge_conflict");
    assert_eq!(error_paths(&err), ["file.txt"]);
    assert_eq!(w.src_head().await, head);
    assert_eq!(
        fs::read_to_string(w.src().join("file.txt")).unwrap(),
        "source"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn merge_rejected_by_a_commit_hook() {
    use std::os::unix::fs::PermissionsExt;

    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    let hooks = w.src().join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\necho no-way >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let head = w.src_head().await;

    let err = refused(w.merge().await);

    assert_eq!(err.code(), "commit_rejected");
    assert!(err.to_string().contains("no-way"), "{err}");
    assert_eq!(w.src_head().await, head);
    assert!(
        !w.src().join("b.txt").exists(),
        "touched paths are restored"
    );
    assert_eq!(git(w.src(), &["status", "--porcelain"]).await, "");
}

#[tokio::test]
async fn merge_refused_for_unsupported_source_state() {
    // Detached HEAD.
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    git(w.src(), &["checkout", "-q", "--detach"]).await;
    let head = w.src_head().await;
    let err = refused(w.merge().await);
    assert_eq!(err.code(), "detached_head");
    assert_eq!(w.src_head().await, head);

    // Commit signing.
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    git(w.src(), &["config", "commit.gpgSign", "true"]).await;
    let head = w.src_head().await;
    let err = refused(w.merge().await);
    assert_eq!(err.code(), "signing_unsupported");
    assert_eq!(w.src_head().await, head);
    assert!(!w.src().join("b.txt").exists());
}

// ── Repeated and interrupted merges ─────────────────────────────────────────

#[tokio::test]
async fn merging_the_same_version_again() {
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    let first = w.merge().await.unwrap();
    let result = first.result.result_commit.clone().unwrap();
    let head = w.src_head().await;

    let again = w.merge_with(Some(&result), None).await.unwrap();

    assert_eq!(again.outcome, "already_merged");
    assert_eq!(again.commit, first.commit);
    assert_eq!(w.src_head().await, head, "no second commit");

    // The record no longer proves anything once the branch was reset.
    git(w.src(), &["reset", "-q", "--hard", "HEAD~1"]).await;
    let err = refused(w.merge_with(Some(&result), None).await);
    assert_eq!(err.code(), "integration_record_unreachable");
}

#[tokio::test]
async fn source_already_contains_the_changes() {
    let w = world(false).await;
    w.write_wt("file.txt", "two");
    w.commit_src("file.txt", "two").await;
    let head = w.src_head().await;

    let done = w.merge().await.unwrap();

    assert_eq!(done.outcome, "no_changes");
    assert_eq!(done.commit, None);
    assert_eq!(w.src_head().await, head);
}

/// A crash inside finalization is recovered by the next merge (or by the
/// failing one when the branch had already moved).
#[tokio::test]
async fn merge_interrupted_by_a_crash() {
    // Before the branch moved: the failing merge reports the failure and
    // leaves the source unchanged; a second merge of the retained result
    // succeeds.
    for point in [
        FailurePoint::FinalizeCommit,
        FailurePoint::FinalizeRefTransaction,
    ] {
        let w = world(false).await;
        w.write_wt("b.txt", "worktree");
        let head = w.src_head().await;
        w.faults.fail_next(point, 1, "boom");

        let err = refused(w.merge().await);

        assert!(err.to_string().contains("boom"), "{point:?}: {err}");
        assert_eq!(w.src_head().await, head, "{point:?}");
        assert!(!w.src().join("b.txt").exists(), "{point:?}");
        let result = w.results().await[0].result_commit.clone().unwrap();
        let done = w.merge_with(Some(&result), None).await.unwrap();
        assert_eq!(done.outcome, "merged", "{point:?}");
        assert_eq!(done.commit.as_deref(), Some(w.src_head().await.as_str()));
    }

    // After the branch moved: recovery completes the index update and the
    // merge reports its commit; a repeat is a replay.
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");
    w.faults
        .fail_next(FailurePoint::FinalizeIndexUpdate, 1, "boom");
    let done = w.merge().await.unwrap();
    assert_eq!(done.outcome, "merged");
    assert_eq!(done.commit.as_deref(), Some(w.src_head().await.as_str()));
    assert_eq!(
        fs::read_to_string(w.src().join("b.txt")).unwrap(),
        "worktree"
    );
    assert_eq!(git(w.src(), &["status", "--porcelain"]).await, "");
    let result = done.result.result_commit.unwrap();
    let again = w.merge_with(Some(&result), None).await.unwrap();
    assert_eq!(again.outcome, "already_merged");
    assert_eq!(again.commit, done.commit);
}

// ── Follow-up archive / removal ─────────────────────────────────────────────

#[tokio::test]
async fn merge_then_remove() {
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");

    let done = w.merge_with(None, Some(FollowUp::Remove)).await.unwrap();

    assert_eq!(done.outcome, "merged");
    let follow_up = done.follow_up.expect("follow-up reported");
    assert!(follow_up.archived && follow_up.removed, "{follow_up:?}");
    assert!(follow_up.error.is_none());
    let removal = follow_up.removal.expect("removal report");
    assert!(removal.checkout_removed);
    assert!(
        removal
            .retained_refs
            .iter()
            .any(|r| r.starts_with("refs/qmt/results/")),
        "{removal:?}"
    );
    assert!(!w.wt().exists());
    // The archived update of the owner states the merge.
    let announced = w.host.announced.lock().unwrap();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0].0, "sess-wt");
    let merged = announced[0].1.as_ref().expect("merge in the announcement");
    assert_eq!(merged.outcome, "merged");
    assert_eq!(merged.commit, done.commit);
    assert_eq!(announced[0].2, Some(ArchiveRemoval::Removed));
}

#[tokio::test]
async fn merge_then_archive_announces_the_merge() {
    let w = world(false).await;
    w.write_wt("b.txt", "worktree");

    let done = w.merge_with(None, Some(FollowUp::Archive)).await.unwrap();

    let follow_up = done.follow_up.unwrap();
    assert!(follow_up.archived && !follow_up.removed);
    assert!(w.wt().exists(), "archive keeps the checkout");
    let record = w.ops.lookup(w.record.id.as_str()).await.unwrap();
    assert_eq!(record.lifecycle, WorkspaceLifecycle::Archived);
    let announced = w.host.announced.lock().unwrap();
    assert_eq!(announced[0].1.as_ref().unwrap().commit, done.commit);
    assert_eq!(announced[0].2, None);
}

/// An older result is merged while the worktree holds later unpublished
/// edits: the merge stands, the archive happens, the removal is refused.
#[tokio::test]
async fn merge_succeeds_but_removal_is_refused() {
    let w = world(false).await;
    w.write_wt("older.txt", "older");
    let older = w.publish().await;
    w.write_wt("later.txt", "later, never published");

    let done = w
        .merge_with(Some(&older), Some(FollowUp::Remove))
        .await
        .unwrap();

    assert_eq!(done.outcome, "merged");
    assert_eq!(paths(&done), ["older.txt"]);
    let follow_up = done.follow_up.unwrap();
    assert!(follow_up.archived);
    assert!(!follow_up.removed);
    assert_eq!(follow_up.error.unwrap().code, "unpreserved_content");
    assert!(w.wt().join("later.txt").exists());
    let record = w.ops.lookup(w.record.id.as_str()).await.unwrap();
    assert_eq!(record.lifecycle, WorkspaceLifecycle::Archived);
    assert!(w.src().join("older.txt").exists());
}

#[tokio::test]
async fn follow_up_skipped_after_a_refused_merge() {
    for follow_up in [FollowUp::Archive, FollowUp::Remove] {
        let w = world(false).await;
        w.write_wt("b.txt", "worktree");
        fs::write(w.src().join("b.txt"), "mine").unwrap();

        let err = refused(w.merge_with(None, Some(follow_up)).await);

        assert_eq!(err.code(), "touches_local_changes");
        let record = w.ops.lookup(w.record.id.as_str()).await.unwrap();
        assert_eq!(record.lifecycle, WorkspaceLifecycle::Ready, "{follow_up:?}");
        assert!(w.wt().exists());
        assert!(w.host.announced.lock().unwrap().is_empty());
    }
}

// ── Pending handoff (delegation handoff core) ───────────────────────────────

/// A pending handoff holds the checkout's exclusive lease; dropping it
/// without a commit leaves the owner unchanged and releases the checkout.
#[tokio::test]
async fn pending_handoff_drop_releases_the_lease() {
    let w = world(false).await;

    let pending = w
        .ops
        .begin_handoff(&w.host, &w.target())
        .await
        .expect("idle owner begins a handoff");
    assert_eq!(pending.previous(), "sess-wt");
    assert_eq!(pending.record().id, w.record.id);

    // While pending, nothing else may take the checkout.
    let Err(held) = w.ops.acquire_exclusive_activity(&w.record).await else {
        panic!("a pending handoff must hold the checkout");
    };
    assert_eq!(held.code(), "owner_active");

    drop(pending);

    let after = w.ops.lookup(w.record.id.as_str()).await.unwrap();
    assert_eq!(after.ownership.owner_session_id.as_deref(), Some("sess-wt"));
    assert_eq!(after.lifecycle, WorkspaceLifecycle::Ready);
    w.ops
        .acquire_exclusive_activity(&w.record)
        .await
        .expect("the lease is released when the pending handoff is dropped");
}

/// Committing transfers ownership with one compare-and-swap and releases the
/// lease; a refused begin changes nothing.
#[tokio::test]
async fn pending_handoff_commit_transfers_ownership() {
    let w = world(false).await;

    let pending = w.ops.begin_handoff(&w.host, &w.target()).await.unwrap();
    let moved = pending.commit(&w.ops, "sess-src").await.unwrap();
    assert_eq!(
        moved.ownership.owner_session_id.as_deref(),
        Some("sess-src")
    );
    w.ops
        .acquire_exclusive_activity(&w.record)
        .await
        .expect("commit releases the lease");

    // The old owner no longer owns it; a busy new owner refuses a begin and
    // leaves ownership as it was.
    w.host.set_busy("sess-src");
    let Err(refused) = w.ops.begin_handoff(&w.host, &w.target()).await else {
        panic!("a busy owner must refuse the handoff");
    };
    assert_eq!(refused.code(), "owner_active");
    let after = w.ops.lookup(w.record.id.as_str()).await.unwrap();
    assert_eq!(
        after.ownership.owner_session_id.as_deref(),
        Some("sess-src")
    );
}
