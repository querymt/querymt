//! Git CLI invocation adapter for linked-worktree administration.
//!
//! Design decision 3 reserves the Git CLI for the operations gix does not
//! provide (notably `git worktree add/remove/prune` and isolated-index
//! capture) rather than reimplementing Git checkout/index semantics. This
//! module is the narrow, testable boundary for those invocations:
//!
//! - argv construction is a pure function (`base_command`) so the argv
//!   contract is unit-testable without spawning processes;
//! - output capture is typed (`GitOutput`) so domain behavior stays
//!   independently testable;
//! - exit mapping is explicit: `run` returns output for any exit status,
//!   and `GitOutput::into_checked` maps non-zero exits to
//!   [`WorktreeError::GitCommand`];
//! - config isolation: ambient `GIT_*` location overrides (`GIT_DIR`,
//!   `GIT_WORK_TREE`, `GIT_INDEX_FILE`, …) are removed so commands operate
//!   on the checkout they are pointed at; index churn is disabled with
//!   `GIT_OPTIONAL_LOCKS=0`;
//! - every invocation has a timeout; hung children are killed.
//!
//! Commands always run with `repo_root` as the working directory.

use crate::worktree::error::{WorktreeError, WorktreeResult};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Default timeout for Git CLI invocations.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Typed result of a Git CLI invocation.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Numeric exit code, when the process exited normally (None if
    /// terminated by a signal).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    /// Lossy UTF-8 view of captured stdout.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Lossy UTF-8 view of captured stderr.
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Map a non-zero exit to [`WorktreeError::GitCommand`], passing
    /// successful output through unchanged.
    pub fn into_checked(self, argv: &[&str]) -> WorktreeResult<GitOutput> {
        if self.success {
            Ok(self)
        } else {
            Err(WorktreeError::GitCommand {
                argv: display_argv(argv),
                status: match self.code {
                    Some(code) => format!("exit {code}"),
                    None => "terminated by signal".to_string(),
                },
                stderr: truncate_for_display(&self.stderr_str()),
            })
        }
    }
}

/// The injectable boundary for Git CLI operations. Production uses
/// [`CliGit`]; tests substitute deterministic fakes.
#[async_trait]
pub trait GitRunner: Send + Sync {
    /// Run `git <args…>` with `repo_root` as the working directory.
    ///
    /// Returns the captured output for any exit status; callers decide how
    /// to interpret failures (see [`GitOutput::into_checked`]).
    async fn run(&self, repo_root: &Path, args: &[&str]) -> WorktreeResult<GitOutput> {
        self.run_with_env(repo_root, args, &[]).await
    }

    /// Like [`run`](Self::run), but with additional *explicitly provided*
    /// Git environment variables (e.g. `GIT_INDEX_FILE` for private-index
    /// capture, author identity for private commits).
    ///
    /// Keys must be on the adapter's controlled allowlist — inherited
    /// environment is still stripped first, so an explicit setting can
    /// never be overridden by ambient env and a rejected key fails before
    /// any process is spawned.
    async fn run_with_env(
        &self,
        repo_root: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> WorktreeResult<GitOutput>;
}

/// Production [`GitRunner`] backed by the `git` binary.
#[derive(Debug, Clone)]
pub struct CliGit {
    git_bin: PathBuf,
    timeout: Duration,
}

impl Default for CliGit {
    fn default() -> Self {
        Self {
            git_bin: PathBuf::from("git"),
            timeout: DEFAULT_GIT_TIMEOUT,
        }
    }
}

impl CliGit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the git binary path (primarily for testing).
    pub fn with_git_bin(mut self, git_bin: impl Into<PathBuf>) -> Self {
        self.git_bin = git_bin.into();
        self
    }

    /// Override the per-invocation timeout (primarily for testing).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Environment overrides that must never leak into adapter invocations:
/// they would redirect Git to a different repository, index, or object
/// store than the checkout the command is pointed at.
const REMOVED_ENV_KEYS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_SUPER_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_VERSION",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_LITERAL_PATHSPECS",
    "GIT_REF_NAMESPACE",
    "GIT_REF_LOG_ACTION",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Build the underlying command for `git <args…>` rooted at `repo_root`.
///
/// Pure and synchronous so the argv/env contract can be asserted directly.
/// `tokio::process::Command::From(std::process::Command)` preserves all of
/// it at spawn time.
fn base_command(git_bin: &Path, repo_root: &Path, args: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new(git_bin);
    cmd.current_dir(repo_root);
    cmd.args(args);
    for key in REMOVED_ENV_KEYS {
        cmd.env_remove(key);
    }
    // Avoid index.lock churn for read-mostly administration commands.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// Environment keys that may be *explicitly* set on individual adapter
/// invocations (see [`GitRunner::run_with_env`]). Everything else is
/// rejected before spawning so a caller cannot redirect Git to a foreign
/// repository, object store, or hook configuration.
const ALLOWED_EXTRA_ENV_KEYS: &[&str] = &[
    "GIT_INDEX_FILE",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

fn base_command_with_env(
    git_bin: &Path,
    repo_root: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> WorktreeResult<std::process::Command> {
    for (key, _) in extra_env {
        if !ALLOWED_EXTRA_ENV_KEYS.contains(key) {
            return Err(WorktreeError::InvalidOperation(format!(
                "git environment key {key:?} is not allowed on adapter invocations"
            )));
        }
    }
    let mut cmd = base_command(git_bin, repo_root, args);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    Ok(cmd)
}

fn display_argv(args: &[&str]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Validate a Git ref name before it is ever handed to a CLI invocation.
///
/// Implements the `git check-ref-format` rules relevant to managed
/// branches so hostile or malformed names are rejected *before* reaching
/// Git. Most importantly, a leading `-` is rejected so a ref-like argument
/// can never be parsed as a git option (execution redirection).
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

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+".contains(&b))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn truncate_for_display(text: &str) -> String {
    const MAX: usize = 512;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &text[..end])
}

#[async_trait]
impl GitRunner for CliGit {
    async fn run_with_env(
        &self,
        repo_root: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> WorktreeResult<GitOutput> {
        let argv = display_argv(args);
        let mut cmd = tokio::process::Command::from(base_command_with_env(
            &self.git_bin,
            repo_root,
            args,
            extra_env,
        )?);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Ensures a timed-out child is killed when the wait future is
            // dropped below.
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(|source| WorktreeError::GitSpawn {
            argv: argv.clone(),
            source,
        })?;

        let waited = tokio::time::timeout(self.timeout, child.wait_with_output()).await;
        match waited {
            Err(_) => Err(WorktreeError::GitTimeout {
                argv,
                timeout_secs: self.timeout.as_secs(),
            }),
            Ok(Err(source)) => Err(WorktreeError::GitSpawn { argv, source }),
            Ok(Ok(output)) => Ok(GitOutput {
                success: output.status.success(),
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    // -- argv/env contract (pure) ------------------------------------------

    fn command_envs(cmd: &std::process::Command) -> BTreeMap<String, Option<String>> {
        use std::ffi::OsString;
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(OsString::from)
                        .map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn base_command_passes_argv_verbatim_and_sets_cwd() {
        let root = Path::new("/repo");
        let cmd = base_command(Path::new("git"), root, &["worktree", "list", "--porcelain"]);

        assert_eq!(cmd.get_current_dir(), Some(root));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["worktree", "list", "--porcelain"]);
    }

    #[test]
    fn base_command_isolates_git_environment() {
        let cmd = base_command(Path::new("git"), Path::new("/repo"), &["status"]);
        let envs = command_envs(&cmd);

        // Location overrides are removed (present with None value).
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert_eq!(
                envs.get(key),
                Some(&None),
                "{key} must be removed for config isolation"
            );
        }
        // Index churn disabled.
        assert_eq!(envs.get("GIT_OPTIONAL_LOCKS"), Some(&Some("0".to_string())));
    }

    #[test]
    fn base_command_with_env_rejects_non_allowlisted_keys() {
        // Location overrides must stay rejected even when "explicitly set".
        let err = base_command_with_env(
            Path::new("git"),
            Path::new("/repo"),
            &["status"],
            &[("GIT_DIR", "/evil")],
        );
        assert!(err.is_err(), "non-allowlisted env keys must be rejected");
        assert!(
            err.unwrap_err().to_string().contains("not allowed"),
            "error must name the rejected key"
        );

        // Allowlisted keys are applied on top of the stripped environment.
        let cmd = base_command_with_env(
            Path::new("git"),
            Path::new("/repo"),
            &["write-tree"],
            &[("GIT_INDEX_FILE", "/tmp/idx")],
        )
        .unwrap();
        let envs = command_envs(&cmd);
        assert_eq!(
            envs.get("GIT_INDEX_FILE"),
            Some(&Some("/tmp/idx".to_string()))
        );
    }

    #[test]
    fn display_argv_quotes_arguments_with_spaces() {
        assert_eq!(display_argv(&["add", "my file.txt"]), "add 'my file.txt'");
        assert_eq!(
            display_argv(&["worktree", "add", "a/b_c.d"]),
            "worktree add a/b_c.d"
        );
    }

    // -- real Git invocations ------------------------------------------------

    fn repo_with_commit() -> TempDir {
        let tmp = TempDir::new().unwrap();
        gix::init(tmp.path()).unwrap();
        let config_path = tmp.path().join(".git/config");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str("\n[user]\n\tname = test\n\temail = test@example.com\n");
        fs::write(&config_path, config).unwrap();

        let repo = gix::open(tmp.path()).unwrap();
        let blob = repo.write_blob(b"hello").unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "file.txt".into(),
                    oid: blob,
                }],
            })
            .unwrap()
            .detach();
        repo.commit("HEAD", "init", tree, Vec::<gix::ObjectId>::new())
            .unwrap();
        tmp
    }

    #[tokio::test]
    async fn run_captures_stdout_from_real_git() {
        let tmp = repo_with_commit();
        let runner = CliGit::new();

        let output = runner
            .run(tmp.path(), &["rev-parse", "--show-toplevel"])
            .await
            .unwrap();

        assert!(output.success);
        let expected = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            PathBuf::from(output.stdout_str().trim()),
            expected,
            "rev-parse --show-toplevel must report the checkout root"
        );
    }

    #[tokio::test]
    async fn into_checked_maps_nonzero_exit_to_typed_error() {
        let tmp = repo_with_commit();
        let runner = CliGit::new();

        let output = runner
            .run(
                tmp.path(),
                &["rev-parse", "--verify", "refs/heads/definitely-missing"],
            )
            .await
            .unwrap();
        assert!(!output.success, "missing ref must exit non-zero");

        let err = output
            .into_checked(&["rev-parse", "--verify", "refs/heads/definitely-missing"])
            .unwrap_err();
        match err {
            WorktreeError::GitCommand {
                argv,
                status,
                stderr,
            } => {
                assert!(argv.contains("rev-parse"), "argv recorded: {argv}");
                assert!(status.starts_with("exit "), "status: {status}");
                // Exact wording varies across git versions ("Needed a single
                // revision" etc.); the contract is that stderr is captured.
                assert!(!stderr.is_empty(), "stderr captured: {stderr:?}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn into_checked_passes_successful_output_through() {
        let tmp = repo_with_commit();
        let runner = CliGit::new();

        let output = runner
            .run(tmp.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let checked = output.into_checked(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(checked.stdout_str().trim().len(), 40, "full commit hash");
    }

    #[tokio::test]
    async fn missing_git_binary_maps_to_spawn_error() {
        let tmp = repo_with_commit();
        let runner = CliGit::new().with_git_bin("/nonexistent/path/to/git");

        let err = runner.run(tmp.path(), &["status"]).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::GitSpawn { ref argv, .. } if argv.contains("status")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn invalid_ref_names_are_rejected_before_execution() {
        for name in [
            "-rf",
            "--upload-pack=evil",
            "",
            "refs/heads//double",
            "refs/heads/..",
            "bad name",
            "tilde~1",
            "caret^2",
            "colon:bad",
            "question?mark",
            "star*glob",
            "bracket[0]",
            "back\\slash",
            "ends.lock",
            "ends.",
            "@{nested}",
            "trailing/",
            "ctl\x01char",
        ] {
            assert!(
                validate_ref_name(name).is_err(),
                "ref name {name:?} must be rejected"
            );
        }

        for name in [
            "refs/heads/main",
            "qmt/workspace/01933d2f-1111-7abc-8def-222222222222",
            "feature/one.two",
            "v1.0.0",
        ] {
            assert!(
                validate_ref_name(name).is_ok(),
                "ref name {name:?} must be accepted"
            );
        }
    }

    #[tokio::test]
    async fn shell_metacharacters_cannot_redirect_execution() {
        let tmp = repo_with_commit();
        let marker = tmp.path().join("pwned-marker");
        let runner = CliGit::new();

        // No shell is involved: the metacharacter payload is passed
        // literally as a ref name, git fails to resolve it, and nothing
        // executes. Ref validation also rejects it before invocation.
        let hostile = "refs/heads/x; touch pwned-marker";
        assert!(validate_ref_name(hostile).is_err());

        let output = runner
            .run(tmp.path(), &["rev-parse", "--verify", hostile])
            .await
            .unwrap();
        assert!(!output.success, "hostile ref must not resolve");
        assert!(
            !output.stdout_str().contains("pwned"),
            "shell metacharacters must not execute: {output:?}"
        );
        assert!(!marker.exists(), "no marker file may be created");
    }

    #[tokio::test]
    async fn inherited_git_environment_cannot_redirect_execution() {
        use tokio::sync::Mutex;

        static ENV_LOCK: Mutex<()> = Mutex::const_new(());
        let _guard = ENV_LOCK.lock().await;

        let tmp = repo_with_commit();
        let outside = TempDir::new().unwrap();

        // SAFETY: test-only environment mutation, serialized by ENV_LOCK and
        // restored on all paths below. Adapter-spawned git always strips
        // these variables, so no concurrent test observes the pollution.
        let saved_dir = std::env::var("GIT_DIR").ok();
        let saved_tree = std::env::var("GIT_WORK_TREE").ok();
        unsafe {
            std::env::set_var("GIT_DIR", outside.path().join(".git"));
            std::env::set_var("GIT_WORK_TREE", outside.path());
        }

        let runner = CliGit::new();
        let output = runner
            .run(tmp.path(), &["rev-parse", "--show-toplevel"])
            .await
            .unwrap();

        // Restore first so failures below don't leak state.
        // SAFETY: see above; same serialization guarantees apply.
        unsafe {
            match saved_dir {
                Some(v) => std::env::set_var("GIT_DIR", v),
                None => std::env::remove_var("GIT_DIR"),
            }
            match saved_tree {
                Some(v) => std::env::set_var("GIT_WORK_TREE", v),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
        }
        drop(_guard);

        assert!(output.success, "inherited GIT_DIR must not break execution");
        assert_eq!(
            PathBuf::from(output.stdout_str().trim()),
            fs::canonicalize(tmp.path()).unwrap(),
            "execution must stay on the requested checkout, not the injected one"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_hung_command() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = repo_with_commit();
        // A FIFO nobody writes to: git blocks opening it for reading.
        let fifo = tmp.path().join("blocked.fifo");
        nix_like_mkfifo(&fifo);
        fs::set_permissions(&fifo, fs::Permissions::from_mode(0o644)).unwrap();

        let runner = CliGit::new().with_timeout(Duration::from_millis(150));
        let err = runner
            .run(tmp.path(), &["hash-object", fifo.to_str().unwrap()])
            .await
            .unwrap_err();

        assert!(
            matches!(err, WorktreeError::GitTimeout { ref argv, timeout_secs: 0 } if argv.contains("hash-object")),
            "unexpected error: {err:?}"
        );
        // The hung child must not survive the timeout.
        std::thread::sleep(Duration::from_millis(300));
        let lingering = std::process::Command::new("pgrep")
            .arg("-f")
            .arg("hash-object")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(!lingering, "timed-out git process must be killed");
    }

    /// Create a FIFO without pulling in libc bindings.
    #[cfg(unix)]
    fn nix_like_mkfifo(path: &Path) {
        // `mkfifo` is exposed through the `nix`/`libc` crates; to stay
        // dependency-free, shell out to the system `mkfifo` tool, which is
        // part of coreutils and present wherever git is.
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo available on unix");
        assert!(status.success(), "failed to create fifo at {path:?}");
    }
}
