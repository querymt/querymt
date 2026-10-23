//! Native commit-hook runner for an explicit result acceptance (step 6 of
//! design decision 9).
//!
//! By the time hooks run, the touched files are already on disk
//! ([`crate::worktree::apply`]), so hooks observe the result exactly as it
//! would be committed. Hooks never see the checkout's real index: a private
//! index built from the merged tree is written into the per-worktree Git
//! directory and exported as `GIT_INDEX_FILE`, so a hook that re-stages
//! files changes only that private copy, which finalization (task 22.7)
//! re-reads before it writes the commit.
//!
//! Hooks run in Git's order, from the resolved `core.hooksPath` (default
//! `<common_dir>/hooks`), and there is no bypass:
//!
//! 1. `pre-commit`
//! 2. `prepare-commit-msg <message-file> message`
//! 3. `commit-msg <message-file>`
//!
//! Each runs with the checkout root as working directory, no stdin,
//! stdout/stderr captured, `GIT_INDEX_FILE` set to the private index,
//! `GIT_EDITOR=:`, and every other inherited `GIT_*` variable removed so an
//! ambient environment cannot redirect a hook to another repository. A hook
//! file that is not executable is skipped, as Git does. Each hook is limited
//! to [`HOOK_TIMEOUT`]; on expiry (or when the run is cancelled) the hook's
//! whole process tree is killed. On Windows hooks run through `sh`, and a
//! present hook without a `sh` on `PATH` refuses the acceptance.
//!
//! This phase never touches the working tree, the branch or the real index.
//! It leaves behind only files prefixed `qmt-integration-` in the Git
//! directory: the private index when every hook passed (finalization
//! consumes it), nothing otherwise.

use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::integrate::{AppliedIntegration, Refusal, Stop};
use crate::worktree::repo_ops::{hooks_dir, native_err, open_acceptance_checkout, parse_oid};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Upper bound for a single hook.
pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(600);

/// Longest tail of a hook's combined output that is kept for reporting.
const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

const PRE_COMMIT: &str = "pre-commit";
const PREPARE_COMMIT_MSG: &str = "prepare-commit-msg";
const COMMIT_MSG: &str = "commit-msg";
const POST_COMMIT: &str = "post-commit";

/// Everything the hook phase needs, as plain data (the held index lock and
/// repository handles stay with the caller).
#[derive(Debug, Clone)]
pub struct HookRequest {
    /// Working-tree root of the target checkout.
    pub checkout: PathBuf,
    /// Operation id from the persisted intent; names the scratch files.
    pub operation_id: String,
    /// The tree the private index is built from.
    pub merged_tree: String,
    /// Initial commit message, as handed to `prepare-commit-msg`.
    pub message: String,
}

impl HookRequest {
    pub fn for_applied(applied: &AppliedIntegration, message: impl Into<String>) -> Self {
        Self {
            checkout: applied.prepared.checkout.clone(),
            operation_id: applied.operation_id.clone(),
            merged_tree: applied.prepared.merged_tree.clone(),
            message: message.into(),
        }
    }
}

/// How a hook ended unsuccessfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStatus {
    Exit(i32),
    /// Terminated without an exit code (a signal).
    Killed,
    TimedOut,
    /// The hook file could not be started.
    SpawnFailed(String),
}

impl std::fmt::Display for HookStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit(code) => write!(f, "exited with status {code}"),
            Self::Killed => f.write_str("was terminated by a signal"),
            Self::TimedOut => write!(f, "timed out after {} s", HOOK_TIMEOUT.as_secs()),
            Self::SpawnFailed(err) => write!(f, "could not be started: {err}"),
        }
    }
}

/// A hook that rejected the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRejection {
    pub hook: &'static str,
    pub status: HookStatus,
    /// Tail of the hook's combined stdout and stderr.
    pub output: String,
}

/// All hooks passed. The caller owns the private index from here on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookedIntegration {
    /// Private index the hooks saw (and possibly re-staged into).
    pub private_index: PathBuf,
    /// Commit message after `prepare-commit-msg` and `commit-msg`, exactly as
    /// the message file holds it (cleanup and the empty check are finalization's).
    pub message: Vec<u8>,
    /// Hooks that actually ran, in order.
    pub hooks_run: Vec<&'static str>,
}

#[derive(Debug)]
pub enum HookOutcome {
    Passed(HookedIntegration),
    /// A hook exited non-zero (or failed to run); its scratch files are gone.
    Rejected(HookRejection),
    /// A precondition failed before any hook ran.
    Refused(Refusal),
}

/// Path of the private index of operation `operation_id`.
pub(crate) fn private_index_path(git_dir: &Path, operation_id: &str) -> PathBuf {
    git_dir.join(format!("qmt-integration-{operation_id}.index"))
}

fn message_file_path(git_dir: &Path, operation_id: &str) -> PathBuf {
    git_dir.join(format!("qmt-integration-{operation_id}.msg"))
}

fn output_file_path(git_dir: &Path, operation_id: &str) -> PathBuf {
    git_dir.join(format!("qmt-integration-{operation_id}.hook-output"))
}

struct PlannedHook {
    name: &'static str,
    path: PathBuf,
}

struct HookPlan {
    checkout: PathBuf,
    hooks: Vec<PlannedHook>,
    private_index: PathBuf,
    /// Whether hooks see `private_index` as `GIT_INDEX_FILE`; `post-commit`
    /// runs against the real index and gets none.
    export_index: bool,
    message_file: PathBuf,
    output_file: PathBuf,
    /// Interpreter used to run hooks; only set on Windows.
    sh: Option<PathBuf>,
}

impl HookPlan {
    /// Remove every scratch file this phase created, including the lock a
    /// hook's own Git commands may have left next to the private index.
    fn discard(&self, keep_index: bool) {
        let mut lock = self.private_index.as_os_str().to_owned();
        lock.push(".lock");
        let mut doomed = vec![
            self.message_file.clone(),
            self.output_file.clone(),
            PathBuf::from(lock),
        ];
        if !keep_index {
            doomed.push(self.private_index.clone());
        }
        for path in doomed {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Run the commit hooks for `request`; see the module docs.
pub(crate) async fn run_hooks(
    request: HookRequest,
    timeout: Duration,
) -> WorktreeResult<HookOutcome> {
    let planning = request.clone();
    let planned = tokio::task::spawn_blocking(move || plan_hooks(&planning))
        .await
        .map_err(native_err)?;
    let plan = match planned {
        Ok(plan) => plan,
        Err(Stop::Refused(refusal)) => return Ok(HookOutcome::Refused(refusal)),
        Err(Stop::Failed(err)) => return Err(err),
    };

    let mut hooks_run = Vec::new();
    match run_planned(&plan, timeout, &mut hooks_run).await {
        Ok(None) => {}
        Ok(Some(rejection)) => {
            plan.discard(false);
            return Ok(HookOutcome::Rejected(rejection));
        }
        Err(err) => {
            plan.discard(false);
            return Err(err);
        }
    }
    let message = match tokio::fs::read(&plan.message_file).await {
        Ok(message) => message,
        Err(err) => {
            plan.discard(false);
            return Err(err.into());
        }
    };
    plan.discard(true);
    Ok(HookOutcome::Passed(HookedIntegration {
        private_index: plan.private_index,
        message,
        hooks_run,
    }))
}

/// Run `post-commit` once the commit is final (step 10). Its outcome never
/// changes the acceptance: a problem is returned as text for the caller to
/// log, `None` means it passed or there is no such hook. The hook sees the
/// real index and the finished checkout.
pub(crate) async fn run_post_commit(
    checkout: PathBuf,
    operation_id: String,
    timeout: Duration,
) -> Option<String> {
    let planned =
        tokio::task::spawn_blocking(move || plan_post_commit(&checkout, &operation_id)).await;
    let plan = match planned {
        Ok(Ok(Some(plan))) => plan,
        Ok(Ok(None)) => return None,
        Ok(Err(Stop::Refused(refusal))) => return Some(refusal.message),
        Ok(Err(Stop::Failed(err))) => return Some(err.to_string()),
        Err(err) => return Some(err.to_string()),
    };
    let result = run_hook(&plan, &plan.hooks[0], &[], timeout).await;
    let _ = std::fs::remove_file(&plan.output_file);
    match result {
        Ok(None) => None,
        Ok(Some(rejection)) => Some(format!(
            "the {POST_COMMIT} hook {}: {}",
            rejection.status,
            rejection.output.trim()
        )),
        Err(err) => Some(err.to_string()),
    }
}

fn plan_post_commit(checkout: &Path, operation_id: &str) -> Result<Option<HookPlan>, Stop> {
    let repo = open_acceptance_checkout(checkout)?;
    let path = hooks_dir(&repo)?.join(POST_COMMIT);
    if !is_runnable(&path) {
        return Ok(None);
    }
    let sh = resolve_sh(true)?;
    Ok(Some(HookPlan {
        checkout: checkout.to_path_buf(),
        hooks: vec![PlannedHook {
            name: POST_COMMIT,
            path,
        }],
        private_index: PathBuf::new(),
        export_index: false,
        message_file: PathBuf::new(),
        output_file: output_file_path(repo.git_dir(), operation_id),
        sh,
    }))
}

/// Blocking half: resolve the hooks, write the private index and the
/// initial message file.
fn plan_hooks(request: &HookRequest) -> Result<HookPlan, Stop> {
    let repo = open_acceptance_checkout(&request.checkout)?;
    let git_dir = repo.git_dir().to_path_buf();
    let hooks_dir = hooks_dir(&repo)?;

    let mut hooks = Vec::new();
    for name in [PRE_COMMIT, PREPARE_COMMIT_MSG, COMMIT_MSG] {
        let path = hooks_dir.join(name);
        if is_runnable(&path) {
            hooks.push(PlannedHook { name, path });
        }
    }
    let sh = resolve_sh(!hooks.is_empty())?;

    let merged_tree = parse_oid(&request.merged_tree)?;
    let state = gix::index::State::from_tree(&merged_tree, &repo.objects, Default::default())
        .map_err(native_err)?;
    let private_index = private_index_path(&git_dir, &request.operation_id);
    gix::index::File::from_state(state, private_index.clone())
        .write(Default::default())
        .map_err(native_err)?;

    let message_file = message_file_path(&git_dir, &request.operation_id);
    if let Err(err) = std::fs::write(&message_file, request.message.as_bytes()) {
        let _ = std::fs::remove_file(&private_index);
        return Err(WorktreeError::from(err).into());
    }
    Ok(HookPlan {
        checkout: request.checkout.clone(),
        hooks,
        private_index,
        export_index: true,
        message_file,
        output_file: output_file_path(&git_dir, &request.operation_id),
        sh,
    })
}

/// Whether `path` names a hook Git would run: a regular file (symlinks are
/// followed) with an execute bit. Non-executable hooks are skipped, as Git
/// does, and noted for diagnosis.
fn is_runnable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            tracing::debug!(hook = %path.display(), "ignoring a hook that is not executable");
            return false;
        }
    }
    true
}

/// Interpreter for hooks. Hooks are executed directly on Unix; on Windows
/// they need a `sh` from `PATH`, and a present hook without one refuses.
#[cfg(not(windows))]
fn resolve_sh(_hooks_present: bool) -> Result<Option<PathBuf>, Stop> {
    Ok(None)
}

#[cfg(windows)]
fn resolve_sh(hooks_present: bool) -> Result<Option<PathBuf>, Stop> {
    if !hooks_present {
        return Ok(None);
    }
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("sh.exe"))
            .find(|candidate| candidate.is_file())
    });
    match found {
        Some(sh) => Ok(Some(sh)),
        None => Err(crate::worktree::integrate::refuse(
            "hook_requires_sh",
            "a commit hook is present but no `sh` was found on PATH to run it",
        )),
    }
}

/// Run the planned hooks in order; the first failure is returned.
async fn run_planned(
    plan: &HookPlan,
    timeout: Duration,
    hooks_run: &mut Vec<&'static str>,
) -> WorktreeResult<Option<HookRejection>> {
    for hook in &plan.hooks {
        let args: Vec<OsString> = match hook.name {
            PREPARE_COMMIT_MSG => vec![plan.message_file.clone().into(), "message".into()],
            COMMIT_MSG => vec![plan.message_file.clone().into()],
            _ => Vec::new(),
        };
        hooks_run.push(hook.name);
        if let Some(rejection) = run_hook(plan, hook, &args, timeout).await? {
            return Ok(Some(rejection));
        }
    }
    Ok(None)
}

async fn run_hook(
    plan: &HookPlan,
    hook: &PlannedHook,
    args: &[OsString],
    timeout: Duration,
) -> WorktreeResult<Option<HookRejection>> {
    // stdout and stderr share one file instead of pipes: a background process
    // that outlives the hook and keeps a pipe open could otherwise stall the
    // read until the timeout.
    let output = std::fs::File::create(&plan.output_file)?;
    let mut command = hook_command(plan, hook, args, std::env::vars_os().map(|(key, _)| key));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true);

    let status = match command.spawn() {
        Err(err) => HookStatus::SpawnFailed(err.to_string()),
        Ok(mut child) => {
            let mut tree = ProcessTree::new(child.id());
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) => {
                    tree.disarm();
                    match (status.success(), status.code()) {
                        (true, _) => {
                            return Ok(None);
                        }
                        (false, Some(code)) => HookStatus::Exit(code),
                        (false, None) => HookStatus::Killed,
                    }
                }
                Ok(Err(err)) => {
                    return Err(WorktreeError::InvalidOperation(format!(
                        "waiting for the {} hook failed: {err}",
                        hook.name
                    )));
                }
                Err(_) => {
                    tree.kill();
                    let _ = child.wait().await;
                    HookStatus::TimedOut
                }
            }
        }
    };
    Ok(Some(HookRejection {
        hook: hook.name,
        status,
        output: read_tail(&plan.output_file),
    }))
}

/// The command for one hook. `inherited` is the set of environment variable
/// names to consider for removal (the process environment in production).
fn hook_command(
    plan: &HookPlan,
    hook: &PlannedHook,
    args: &[OsString],
    inherited: impl Iterator<Item = OsString>,
) -> tokio::process::Command {
    let mut command = match &plan.sh {
        Some(sh) => {
            let mut command = tokio::process::Command::new(sh);
            // `sh` on Windows wants forward slashes.
            command.arg(hook.path.to_string_lossy().replace('\\', "/"));
            command
        }
        None => tokio::process::Command::new(&hook.path),
    };
    command.args(args).current_dir(&plan.checkout);
    for key in inherited {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    if plan.export_index {
        command.env("GIT_INDEX_FILE", &plan.private_index);
    }
    command.env("GIT_EDITOR", ":");
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Last [`OUTPUT_TAIL_BYTES`] of the hook's output file, lossily decoded.
fn read_tail(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Kills a hook's whole process tree when dropped while armed, so a timeout
/// or a cancelled acceptance never leaves hook descendants running.
struct ProcessTree {
    pid: Option<u32>,
}

impl ProcessTree {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }

    fn kill(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_tree(pid);
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    // SAFETY: `process_group(0)` made the hook the leader of its own process
    // group, so the negated pid addresses that group and nothing else. The
    // guard is disarmed as soon as the hook is reaped, before its pid can
    // be reused.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_tree(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::apply::apply_blocking;
    use crate::worktree::apply::tests::{applied, fixture, prepared, read};
    use crate::worktree::repo_ops::{
        FailurePoint, FaultInjectingRepoOps, GixRepoOps, RepositoryOps,
    };
    use std::ffi::OsStr;
    use std::sync::Arc;

    /// A checkout at `B0` with the fixture result applied, as the hook phase
    /// finds it.
    fn applied_fixture() -> (tempfile::TempDir, AppliedIntegration) {
        let (dir, base, result) = fixture();
        let prepared = prepared(dir.path(), base, result);
        let applied = applied(apply_blocking(prepared).unwrap());
        (dir, applied)
    }

    fn request(applied: &AppliedIntegration, message: &str) -> HookRequest {
        HookRequest::for_applied(applied, message)
    }

    fn install_hook(root: &Path, name: &str, script: &str) {
        install_hook_in(&root.join(".git/hooks"), name, script, 0o755);
    }

    fn install_hook_in(dir: &Path, name: &str, script: &str, mode: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    fn passed(outcome: HookOutcome) -> HookedIntegration {
        match outcome {
            HookOutcome::Passed(passed) => passed,
            other => panic!("expected the hooks to pass, got {other:?}"),
        }
    }

    fn rejected(outcome: HookOutcome) -> HookRejection {
        match outcome {
            HookOutcome::Rejected(rejection) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    fn scratch_files(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("qmt-integration-") && !name.ends_with("intent.json"))
            .collect();
        names.sort();
        names
    }

    async fn run(applied: &AppliedIntegration, message: &str) -> HookOutcome {
        run_hooks(request(applied, message), HOOK_TIMEOUT)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn without_hooks_the_private_index_holds_the_merged_tree() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        let real_index = std::fs::read(root.join(".git/index")).unwrap();

        let passed = passed(run(&applied, "accept result\n").await);

        assert!(passed.hooks_run.is_empty());
        assert_eq!(passed.message, b"accept result\n");
        assert_eq!(
            passed.private_index,
            private_index_path(&root.join(".git"), &applied.operation_id)
        );
        let index = gix::index::File::at(
            &passed.private_index,
            gix::hash::Kind::Sha1,
            false,
            Default::default(),
        )
        .unwrap();
        let mut paths: Vec<String> = index
            .entries()
            .iter()
            .map(|entry| entry.path(&index).to_string())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                ".gitattributes",
                "keep.txt",
                "link",
                "mod.crlf",
                "new/dir/file.txt",
                "plain.txt",
                "run.sh",
                "sub/stay.txt"
            ]
        );
        let run_sh = index
            .entry_by_path("run.sh".into())
            .expect("run.sh is staged");
        assert_eq!(run_sh.mode, gix::index::entry::Mode::FILE_EXECUTABLE);
        // Only the private index and nothing else is left; the real index is
        // untouched.
        assert_eq!(
            scratch_files(root),
            [format!("qmt-integration-{}.index", applied.operation_id)]
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), real_index);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_observe_applied_files_and_the_documented_environment() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook(
            root,
            PRE_COMMIT,
            "#!/bin/sh\n\
             cat new/dir/file.txt > .git/seen-content\n\
             printf '%s' \"$PWD\" > .git/seen-cwd\n\
             printf '%s' \"$GIT_INDEX_FILE\" > .git/seen-index\n\
             printf '%s' \"$GIT_EDITOR\" > .git/seen-editor\n\
             test ! -e gone.txt && echo deleted > .git/seen-deleted\n",
        );

        let passed = passed(run(&applied, "msg\n").await);

        assert_eq!(passed.hooks_run, [PRE_COMMIT]);
        assert_eq!(read(root, ".git/seen-content"), b"added\n");
        assert_eq!(read(root, ".git/seen-deleted"), b"deleted\n");
        assert_eq!(read(root, ".git/seen-editor"), b":");
        assert_eq!(
            read(root, ".git/seen-index"),
            passed.private_index.to_string_lossy().as_bytes()
        );
        let seen_cwd = PathBuf::from(String::from_utf8(read(root, ".git/seen-cwd")).unwrap());
        assert_eq!(
            seen_cwd.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn message_hooks_edit_the_commit_message_in_git_order() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook(
            root,
            PRE_COMMIT,
            "#!/bin/sh\necho pre-commit >> .git/order\n",
        );
        install_hook(
            root,
            PREPARE_COMMIT_MSG,
            "#!/bin/sh\n\
             echo prepare-commit-msg >> .git/order\n\
             printf '%s' \"$2\" > .git/seen-source\n\
             { printf 'prefix: '; cat \"$1\"; } > \"$1.new\" && mv \"$1.new\" \"$1\"\n",
        );
        install_hook(
            root,
            COMMIT_MSG,
            "#!/bin/sh\n\
             echo commit-msg >> .git/order\n\
             printf '\\nChecked-by: hook\\n' >> \"$1\"\n",
        );

        let passed = passed(run(&applied, "subject\n").await);

        assert_eq!(
            passed.hooks_run,
            [PRE_COMMIT, PREPARE_COMMIT_MSG, COMMIT_MSG]
        );
        assert_eq!(
            read(root, ".git/order"),
            b"pre-commit\nprepare-commit-msg\ncommit-msg\n"
        );
        assert_eq!(read(root, ".git/seen-source"), b"message");
        assert_eq!(passed.message, b"prefix: subject\n\nChecked-by: hook\n");
        // The message file is consumed; only the private index remains.
        assert_eq!(
            scratch_files(root),
            [format!("qmt-integration-{}.index", applied.operation_id)]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_hook_rejects_with_its_output_and_stops_the_run() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook(
            root,
            PRE_COMMIT,
            "#!/bin/sh\necho 'lint failed' >&2\necho 'details' \nexit 3\n",
        );
        install_hook(root, COMMIT_MSG, "#!/bin/sh\ntouch .git/commit-msg-ran\n");

        let rejection = rejected(run(&applied, "msg\n").await);

        assert_eq!(rejection.hook, PRE_COMMIT);
        assert_eq!(rejection.status, HookStatus::Exit(3));
        assert!(rejection.output.contains("lint failed"), "{rejection:?}");
        assert!(rejection.output.contains("details"), "{rejection:?}");
        assert!(!root.join(".git/commit-msg-ran").exists());
        // Scratch files are gone; undoing the applied files is the caller's.
        assert!(scratch_files(root).is_empty());
        assert_eq!(read(root, "new/dir/file.txt"), b"added\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hook_that_cannot_start_rejects() {
        let (dir, applied) = applied_fixture();
        // Executable, but the interpreter does not exist.
        install_hook(
            dir.path(),
            PRE_COMMIT,
            "#!/nonexistent/interpreter\nexit 0\n",
        );

        let rejection = rejected(run(&applied, "msg\n").await);

        assert_eq!(rejection.hook, PRE_COMMIT);
        assert!(
            matches!(rejection.status, HookStatus::SpawnFailed(_)),
            "{rejection:?}"
        );
        assert!(scratch_files(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_executable_hooks_are_skipped() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook_in(
            &root.join(".git/hooks"),
            PRE_COMMIT,
            "#!/bin/sh\ntouch .git/ran\nexit 1\n",
            0o644,
        );
        // A directory with a hook's name is not a hook either.
        std::fs::create_dir_all(root.join(".git/hooks").join(COMMIT_MSG)).unwrap();

        let passed = passed(run(&applied, "msg\n").await);

        assert!(passed.hooks_run.is_empty());
        assert!(!root.join(".git/ran").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn core_hooks_path_selects_the_hook_directory() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        let config = root.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("\n[core]\n\thooksPath = custom-hooks\n");
        std::fs::write(&config, text).unwrap();
        install_hook(root, PRE_COMMIT, "#!/bin/sh\ntouch .git/default-ran\n");
        install_hook_in(
            &root.join("custom-hooks"),
            PRE_COMMIT,
            "#!/bin/sh\ntouch .git/custom-ran\n",
            0o755,
        );

        let passed = passed(run(&applied, "msg\n").await);

        assert_eq!(passed.hooks_run, [PRE_COMMIT]);
        assert!(root.join(".git/custom-ran").exists());
        assert!(!root.join(".git/default-ran").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_hook_is_killed_with_its_descendants() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook(
            root,
            PRE_COMMIT,
            "#!/bin/sh\nsleep 300 &\necho $! > .git/sleeper.pid\nwait\n",
        );

        let started = std::time::Instant::now();
        let outcome = run_hooks(request(&applied, "msg\n"), Duration::from_millis(500))
            .await
            .unwrap();
        let rejection = rejected(outcome);

        assert_eq!(rejection.hook, PRE_COMMIT);
        assert_eq!(rejection.status, HookStatus::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(30));
        let pid: i32 = String::from_utf8(read(root, ".git/sleeper.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for the process.
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "the hook's descendant survived the timeout");
        assert!(scratch_files(root).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_run_kills_the_running_hook() {
        let (dir, applied) = applied_fixture();
        let root = dir.path();
        install_hook(
            root,
            PRE_COMMIT,
            "#!/bin/sh\nsleep 300 &\necho $! > .git/sleeper.pid\nwait\n",
        );

        let cancelled = tokio::time::timeout(
            Duration::from_millis(800),
            run_hooks(request(&applied, "msg\n"), HOOK_TIMEOUT),
        )
        .await;
        assert!(cancelled.is_err(), "the hook should still be running");

        let pid: i32 = String::from_utf8(read(root, ".git/sleeper.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for the process.
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "the hook's descendant survived cancellation");
    }

    #[test]
    fn inherited_git_variables_are_removed_and_the_hook_variables_set() {
        let plan = HookPlan {
            checkout: PathBuf::from("/checkout"),
            hooks: Vec::new(),
            private_index: PathBuf::from("/checkout/.git/private.index"),
            export_index: true,
            message_file: PathBuf::from("/checkout/.git/msg"),
            output_file: PathBuf::from("/checkout/.git/out"),
            sh: None,
        };
        let hook = PlannedHook {
            name: PRE_COMMIT,
            path: PathBuf::from("/checkout/.git/hooks/pre-commit"),
        };
        let inherited = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "git_common_dir",
            "GIT_AUTHOR_NAME",
            "PATH",
            "HOME",
        ]
        .map(OsString::from);

        let command = hook_command(&plan, &hook, &[], inherited.into_iter());
        let command = command.as_std();
        let envs: std::collections::BTreeMap<String, Option<String>> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        for removed in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "git_common_dir",
            "GIT_AUTHOR_NAME",
        ] {
            assert_eq!(envs.get(removed), Some(&None), "{removed} must be removed");
        }
        assert_eq!(
            envs.get("GIT_INDEX_FILE"),
            Some(&Some("/checkout/.git/private.index".to_string()))
        );
        assert_eq!(envs.get("GIT_EDITOR"), Some(&Some(":".to_string())));
        assert!(!envs.contains_key("PATH") && !envs.contains_key("HOME"));
        assert_eq!(command.get_current_dir(), Some(Path::new("/checkout")));
        assert_eq!(
            command.get_program(),
            OsStr::new("/checkout/.git/hooks/pre-commit")
        );
    }

    #[tokio::test]
    async fn the_repository_ops_boundary_runs_hooks_and_injects_faults() {
        let (dir, applied) = applied_fixture();
        let ops = FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new()));
        ops.fail_next(FailurePoint::RunHooks, 1, "boom");

        let err = ops
            .run_integration_hooks(request(&applied, "msg\n"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert!(scratch_files(dir.path()).is_empty());

        let outcome = ops
            .run_integration_hooks(request(&applied, "msg\n"))
            .await
            .unwrap();
        let passed = passed(outcome);
        assert_eq!(passed.message, b"msg\n");
        // The intent written by the apply phase is untouched by this phase.
        assert!(matches!(
            crate::worktree::apply::read_intent(&dir.path().join(".git")),
            Ok(Some(_))
        ));
    }
}
