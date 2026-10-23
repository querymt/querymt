//! Shared gix-based repository fixtures for worktree unit tests.
//!
//! Only compiled for tests. All fixtures build repositories through `gix`
//! object APIs and materialize working-tree files directly; no Git CLI is
//! used for fixture setup (the CLI adapter tests exercise the adapter
//! itself).

#![cfg(test)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::worktree::repo_workspace::WorkspaceRepository;

/// Initialize a non-bare repository with a commit identity in its config
/// (mirroring the snapshot backend's config handling).
pub(crate) fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    gix::init(tmp.path()).unwrap();
    let config_path = tmp.path().join(".git/config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
    fs::write(&config_path, config).unwrap();
    tmp
}

/// Recursively write nested trees for `(relative_path, blob)` entries and
/// return the root tree id.
pub(crate) fn write_tree_recursive(
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
pub(crate) fn commit_files(root: &Path, message: &str, files: &[(&str, &str)]) -> gix::ObjectId {
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
pub(crate) fn materialize(root: &Path, files: &[(&str, &str)]) {
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
pub(crate) fn init_repo_with_subdir() -> (tempfile::TempDir, gix::ObjectId, gix::ObjectId) {
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

/// Registry persistence boundaries that provisioning depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RegistryFaultPoint {
    /// Persisting the initial provisioning intent.
    InsertWorkspace,
    /// Binding the captured baseline identity.
    BindBaseline,
    /// The final provisioning → ready lifecycle transition.
    TransitionLifecycle,
}

/// Wraps a [`WorkspaceRepository`], injecting failures at named persistence
/// boundaries so tests can interrupt provisioning exactly between durable
/// writes without fabricating SQLite errors or parsing Git commands.
pub(crate) struct FaultInjectingWorkspaceRepository {
    inner: std::sync::Arc<dyn WorkspaceRepository>,
    failures: std::sync::Mutex<std::collections::HashMap<RegistryFaultPoint, (usize, String)>>,
}

impl FaultInjectingWorkspaceRepository {
    pub(crate) fn new(inner: std::sync::Arc<dyn WorkspaceRepository>) -> Self {
        Self {
            inner,
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Fail the next `times` calls at `point` with `message`.
    pub(crate) fn fail_next(&self, point: RegistryFaultPoint, times: usize, message: &str) {
        self.failures
            .lock()
            .unwrap()
            .insert(point, (times, message.to_string()));
    }

    fn take_failure(
        &self,
        point: RegistryFaultPoint,
    ) -> Option<crate::session::error::SessionError> {
        let mut failures = self.failures.lock().unwrap();
        let entry = failures.get_mut(&point)?;
        let (times, message) = entry;
        if *times == 0 {
            return None;
        }
        *times -= 1;
        Some(crate::session::error::SessionError::Other(format!(
            "injected {point:?} failure: {message}"
        )))
    }

    fn inner(&self) -> &std::sync::Arc<dyn WorkspaceRepository> {
        &self.inner
    }
}

#[async_trait::async_trait]
impl WorkspaceRepository for FaultInjectingWorkspaceRepository {
    async fn insert_workspace(
        &self,
        record: crate::worktree::domain::WorkspaceRecord,
    ) -> crate::session::error::SessionResult<crate::worktree::domain::WorkspaceRecord> {
        if let Some(err) = self.take_failure(RegistryFaultPoint::InsertWorkspace) {
            return Err(err);
        }
        self.inner().insert_workspace(record).await
    }

    async fn get_workspace(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
    ) -> crate::session::error::SessionResult<Option<crate::worktree::domain::WorkspaceRecord>>
    {
        self.inner().get_workspace(id).await
    }

    async fn get_workspace_by_checkout(
        &self,
        checkout_path: &std::path::Path,
    ) -> crate::session::error::SessionResult<Option<crate::worktree::domain::WorkspaceRecord>>
    {
        self.inner().get_workspace_by_checkout(checkout_path).await
    }

    async fn get_workspace_by_session(
        &self,
        session_id: &str,
    ) -> crate::session::error::SessionResult<Option<crate::worktree::domain::WorkspaceRecord>>
    {
        self.inner().get_workspace_by_session(session_id).await
    }

    async fn list_workspaces(
        &self,
    ) -> crate::session::error::SessionResult<Vec<crate::worktree::domain::WorkspaceRecord>> {
        self.inner().list_workspaces().await
    }

    async fn list_workspaces_by_repository(
        &self,
        common_dir: &std::path::Path,
    ) -> crate::session::error::SessionResult<Vec<crate::worktree::domain::WorkspaceRecord>> {
        self.inner().list_workspaces_by_repository(common_dir).await
    }

    async fn list_workspaces_by_batch(
        &self,
        batch_id: &str,
    ) -> crate::session::error::SessionResult<Vec<crate::worktree::domain::WorkspaceRecord>> {
        self.inner().list_workspaces_by_batch(batch_id).await
    }

    async fn update_workspace(
        &self,
        record: crate::worktree::domain::WorkspaceRecord,
    ) -> crate::session::error::SessionResult<()> {
        self.inner().update_workspace(record).await
    }

    async fn transition_lifecycle(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
        from: crate::worktree::domain::WorkspaceLifecycle,
        to: crate::worktree::domain::WorkspaceLifecycle,
    ) -> crate::session::error::SessionResult<bool> {
        if let Some(err) = self.take_failure(RegistryFaultPoint::TransitionLifecycle) {
            return Err(err);
        }
        self.inner().transition_lifecycle(id, from, to).await
    }

    async fn attach_owner(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
        owner_session_id: &str,
    ) -> crate::session::error::SessionResult<crate::worktree::domain::WorkspaceRecord> {
        self.inner().attach_owner(id, owner_session_id).await
    }

    async fn detach_owner(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
    ) -> crate::session::error::SessionResult<crate::worktree::domain::WorkspaceRecord> {
        self.inner().detach_owner(id).await
    }

    async fn bind_baseline(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
        baseline: &crate::worktree::domain::Baseline,
    ) -> crate::session::error::SessionResult<()> {
        if let Some(err) = self.take_failure(RegistryFaultPoint::BindBaseline) {
            return Err(err);
        }
        self.inner().bind_baseline(id, baseline).await
    }

    async fn record_provisioning_phase(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
        phase: crate::worktree::domain::ProvisioningPhase,
    ) -> crate::session::error::SessionResult<bool> {
        self.inner().record_provisioning_phase(id, phase).await
    }

    async fn record_result(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
        provenance: &crate::worktree::domain::ResultProvenance,
    ) -> crate::session::error::SessionResult<()> {
        self.inner().record_result(id, provenance).await
    }

    async fn list_results(
        &self,
        id: &crate::worktree::domain::WorkspaceId,
    ) -> crate::session::error::SessionResult<Vec<crate::worktree::domain::ResultProvenance>> {
        self.inner().list_results(id).await
    }
}

const CHILD_ENV: &str = "QMT_TEST_CHILD";

/// True inside a process started by [`run_in_child`].
pub(crate) fn in_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Re-run the test at `test_path` (module path below the crate root, e.g.
/// `worktree::repo_ops::tests::probe`) in a child process with `env`
/// applied. gix reads the process environment while opening a repository,
/// so checks that depend on it must not mutate the shared test process.
/// The child half returns early unless [`in_child`] is true.
pub(crate) fn run_in_child(test_path: &str, env: &[(&str, &std::ffi::OsStr)]) {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", test_path, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{test_path} failed in the child:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "the child probe must actually run: {stdout}"
    );
}

/// Write the real index from the current HEAD tree, as a checkout after
/// `git commit` would have it. Fixtures that build commits through object
/// APIs never write one, and a missing index reads as "every tracked file is
/// staged for deletion".
pub(crate) fn write_index_from_head(root: &Path) {
    let repo = gix::open(root).unwrap();
    let tree = repo.head_commit().unwrap().tree_id().unwrap();
    let mut index = repo.index_from_tree(&tree).unwrap();
    index.write(Default::default()).unwrap();
}

/// Run the Git CLI in `root` (fixture setup only, never production code) and
/// return trimmed stdout. Inherited repository redirection is cleared.
pub(crate) fn git(root: &Path, args: &[&str]) -> String {
    git_with_stdin(root, args, "")
}

pub(crate) fn git_with_stdin(root: &Path, args: &[&str], stdin: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
