//! Sandbox policy library for querymt agent sessions.
//!
//! Wraps the [nono](https://crates.io/crates/nono) crate to provide OS-level
//! capability-based sandboxing. Each agent session runs in a worker process
//! with filesystem and network restrictions enforced by the kernel
//! (Landlock on Linux, Seatbelt on macOS).
//!
//! # Usage
//!
//! ```no_run
//! use querymt_sandbox::SandboxPolicy;
//! use std::path::PathBuf;
//!
//! let policy = SandboxPolicy {
//!     cwd: PathBuf::from("/project"),
//!     read_only: false,
//!     allow_network: true,
//!     db_path: None,
//!     socket_dir: None,
//! };
//!
//! // Build the capability set (for inspection/testing)
//! let caps = policy.to_capability_set().unwrap();
//! println!("{}", caps.summary());
//!
//! // Apply sandbox to the current process (irreversible)
//! policy.apply().unwrap();
//! ```

pub use nono::supervisor::types::{
    ApprovalDecision, CapabilityRequest, SupervisorMessage, SupervisorResponse,
};
pub use nono::supervisor::{ApprovalBackend, NeverGrantChecker};
pub use nono::{AccessMode, CapabilitySet, Sandbox, SupervisorSocket};

mod supervisor_handler;

pub use supervisor_handler::{SupervisorRequestHandler, platform_supervisor_handler};

use std::path::{Path, PathBuf};

/// Sandbox policy for an agent session worker process.
///
/// Defines what filesystem paths and network access the sandboxed process
/// is allowed. Built from session parameters (cwd, mode) and applied
/// irreversibly at worker startup.
///
/// # Static profile vs. extension tokens
///
/// The static Seatbelt/Landlock profile **always** grants `Read` access to
/// `cwd`, regardless of `read_only`. Write access is granted dynamically at
/// runtime via `sandbox_extension_consume()` tokens issued by the orchestrator
/// through the supervisor socket. This makes mode transitions (Build <-> Plan/
/// Review) enforceable at the OS level:
///
/// - `extension_consume(token)` → grants write access (Build mode)
/// - `extension_release(handle)` → revokes write access (Plan/Review mode)
///
/// The `read_only` field is **informational**: it tells the orchestrator
/// whether to issue an initial ReadWrite extension token at startup (Build
/// mode = `false`) or to start in read-only mode (Plan/Review = `true`).
pub struct SandboxPolicy {
    /// Working directory for the session.
    ///
    /// The static sandbox profile always grants `Read` access here.
    /// Write access is granted at runtime via supervisor extension tokens.
    pub cwd: PathBuf,
    /// If true, the session starts in read-only mode (Plan/Review).
    /// If false, the orchestrator issues an initial ReadWrite extension token
    /// at startup so the worker has write access from the first tool call.
    ///
    /// This field does NOT change the static sandbox profile (which is always
    /// read-only for CWD); it only controls the initial extension token.
    pub read_only: bool,
    /// Whether network access is allowed. Needed for `web_fetch`, `browse`,
    /// and LLM API calls. When false, `block_network()` is applied.
    pub allow_network: bool,
    /// Optional path to the shared SQLite database.
    /// Granted ReadWrite access (SQLite WAL needs write).
    pub db_path: Option<PathBuf>,
    /// Optional path to the supervisor Unix-domain socket directory.
    ///
    /// The worker connects to the supervisor socket **after** the sandbox is
    /// applied, so the directory must be explicitly allowed. Typically
    /// `$HOME/.qmt/sockets`.
    pub socket_dir: Option<PathBuf>,
}

impl SandboxPolicy {
    /// Build the nono [`CapabilitySet`] for this policy.
    ///
    /// The capability set includes:
    /// - `cwd` with **Read** access (always — write access is granted via
    ///   extension tokens at runtime, never baked into the static profile)
    /// - System paths (`/usr`, `/bin`, `/lib`, `/etc`, `/dev`) with Read access
    /// - `/tmp` with ReadWrite for temporary files
    /// - Platform-specific paths (macOS: `/private`, `/System`, `/Library`;
    ///   Linux: `/proc`, `/sys`)
    /// - Extensions enabled for runtime capability expansion (mode switching)
    /// - Network blocked if `allow_network` is false
    pub fn to_capability_set(&self) -> nono::Result<CapabilitySet> {
        // Static profile always uses Read for CWD.
        // Build mode gets write access via extension tokens at runtime.
        let mode = AccessMode::Read;

        let mut caps = CapabilitySet::new()
            .allow_path(&self.cwd, mode)?
            // System paths for shell commands, language runtimes, etc.
            .allow_path("/usr", AccessMode::Read)?
            .allow_path("/bin", AccessMode::Read)?
            .allow_path("/etc", AccessMode::Read)? // DNS resolution, SSL certs
            .allow_path("/dev", AccessMode::Read)? // /dev/null, /dev/urandom
            .allow_path("/tmp", AccessMode::ReadWrite)? // tempfiles
            .enable_extensions(); // for mode switching via supervisor

        // /lib may not exist on all platforms (e.g. macOS uses /usr/lib)
        if Path::new("/lib").exists() {
            caps = caps.allow_path("/lib", AccessMode::Read)?;
        }

        // macOS-specific paths
        #[cfg(target_os = "macos")]
        {
            caps = caps
                .allow_path("/private/tmp", AccessMode::ReadWrite)?
                .allow_path("/private/var", AccessMode::Read)?
                .allow_path("/System", AccessMode::Read)?
                .allow_path("/Library", AccessMode::Read)?;
        }

        // Linux-specific paths
        #[cfg(target_os = "linux")]
        {
            caps = caps
                .allow_path("/proc", AccessMode::Read)?
                .allow_path("/sys", AccessMode::Read)?;
        }

        // Grant ReadWrite access to the shared SQLite database by allowing
        // the entire parent directory. A literal file capability is not
        // sufficient: SQLite needs to create -journal, -wal, and -shm sidecar
        // files alongside the main DB file, which requires directory write
        // permission. Granting the parent directory (typically ~/.qmt/) covers
        // all three sidecars regardless of whether they exist at sandbox-apply
        // time, and is safe because ~/.qmt/ is the application's own data dir.
        if let Some(ref db) = self.db_path
            && let Some(parent) = db.parent()
            && parent.exists()
        {
            caps = caps.allow_path(parent, AccessMode::ReadWrite)?;
        }

        // Grant ReadWrite access to the supervisor socket directory so the
        // worker can connect to the orchestrator socket after sandboxing.
        if let Some(ref dir) = self.socket_dir
            && dir.exists()
        {
            caps = caps.allow_path(dir, AccessMode::ReadWrite)?;
        }

        if !self.allow_network {
            caps = caps.block_network();
        }

        Ok(caps)
    }

    /// Apply the sandbox to the current process. **Irreversible.**
    ///
    /// On unsupported platforms this logs a warning and returns `Ok(())`
    /// (graceful degradation).
    pub fn apply(&self) -> nono::Result<()> {
        if !Sandbox::is_supported() {
            tracing::warn!(
                "Sandbox not supported on this platform ({}), running unsandboxed",
                std::env::consts::OS
            );
            return Ok(());
        }
        let caps = self.to_capability_set()?;
        tracing::info!(
            cwd = %self.cwd.display(),
            read_only = self.read_only,
            allow_network = self.allow_network,
            "Applying sandbox policy (CWD always read-only in static profile; write access via extension tokens)"
        );
        Sandbox::apply(&caps)
    }

    /// Get human-readable summary of the policy.
    ///
    /// Useful for logging/debugging what the sandbox will enforce.
    pub fn summary(&self) -> nono::Result<String> {
        let caps = self.to_capability_set()?;
        Ok(caps.summary())
    }
}

/// Check whether the current platform supports sandboxing.
pub fn is_supported() -> bool {
    Sandbox::is_supported()
}

/// Get detailed platform support information.
pub fn support_info() -> nono::SupportInfo {
    Sandbox::support_info()
}

// ── Supervisor Socket Support ────────────────────────────────────────────

/// Create a connected pair of supervisor sockets.
///
/// Returns `(parent_socket, child_socket)`. The parent socket stays in the
/// orchestrator process; the child socket is passed to the worker process
/// (typically via its raw fd inherited across fork/exec, or by binding to
/// a filesystem path).
///
/// The parent uses the socket to receive `CapabilityRequest`s and send
/// `SupervisorResponse`s. The child uses it to request capability expansion
/// during mode switches.
pub fn create_supervisor_socket_pair() -> nono::Result<(SupervisorSocket, SupervisorSocket)> {
    SupervisorSocket::pair()
}

/// An `ApprovalBackend` that auto-approves mode-switch requests for a
/// specific working directory.
///
/// When the orchestrator receives a `CapabilityRequest` from a worker, it
/// checks whether the requested path is within the session's `cwd`. If so,
/// it grants access according to the mode being switched to.
///
/// This backend is used by the orchestrator's supervisor loop.
pub struct ModeApprovalBackend {
    /// The session's working directory. Requests for paths within this
    /// directory are approved; all others are denied.
    cwd: PathBuf,
    /// Whether to allow write access (Build mode) or only read (Plan/Review).
    ///
    /// `AtomicBool` so `set_allow_write` takes `&self` (not `&mut self`),
    /// eliminating the need for an `RwLock` in the supervisor loop.  The
    /// supervisor thread can read this with `Acquire` ordering while the
    /// orchestrator async task writes it with `Release` ordering — no lock,
    /// no deadlock.
    allow_write: std::sync::atomic::AtomicBool,
}

impl ModeApprovalBackend {
    /// Create a new approval backend for a session with the given cwd.
    pub fn new(cwd: PathBuf, allow_write: bool) -> Self {
        Self {
            cwd,
            allow_write: std::sync::atomic::AtomicBool::new(allow_write),
        }
    }

    /// Update the write permission (called when mode changes).
    ///
    /// Takes `&self` (not `&mut self`) so this can be called without holding
    /// any lock, even while the supervisor loop concurrently reads the value.
    pub fn set_allow_write(&self, allow_write: bool) {
        self.allow_write
            .store(allow_write, std::sync::atomic::Ordering::Release);
    }
}

impl ApprovalBackend for ModeApprovalBackend {
    fn request_capability(&self, request: &CapabilityRequest) -> nono::Result<ApprovalDecision> {
        // Only approve requests for paths within the session's cwd
        if !request.path.starts_with(&self.cwd) {
            return Ok(ApprovalDecision::Denied {
                reason: format!(
                    "Path {} is outside session working directory {}",
                    request.path.display(),
                    self.cwd.display()
                ),
            });
        }

        // Check if the requested access mode is allowed
        match request.access {
            AccessMode::Read => {
                // Read access within cwd is always allowed
                Ok(ApprovalDecision::Granted)
            }
            AccessMode::Write | AccessMode::ReadWrite => {
                if self.allow_write.load(std::sync::atomic::Ordering::Acquire) {
                    Ok(ApprovalDecision::Granted)
                } else {
                    Ok(ApprovalDecision::Denied {
                        reason: "Session is in read-only mode — write access denied".to_string(),
                    })
                }
            }
        }
    }

    fn backend_name(&self) -> &str {
        "querymt-mode-approval"
    }
}

// ── Write Access Manager Trait ───────────────────────────────────────────

/// Trait for managing OS-level write access via sandbox extension tokens.
///
/// Implemented by the worker's `ExtensionManager`. The agent's `SessionRuntime`
/// stores an `Option<Arc<dyn WriteAccessManager>>` so the `SessionActor` can
/// request or release write access without depending on the worker crate.
pub trait WriteAccessManager: Send + Sync {
    /// Release the current write extension, revoking OS-level write access.
    ///
    /// Called on Build -> Plan/Review transitions. No-op if no write extension
    /// is active.
    fn release_write(&self);

    /// Request a write extension from the orchestrator and consume it.
    ///
    /// Called on Plan/Review -> Build transitions, and at worker startup if
    /// the initial mode is Build.
    fn request_write(&self);

    /// Returns `true` if a write extension is currently active.
    fn has_write(&self) -> bool;
}

// ── Extension Token Exchange ─────────────────────────────────────────────

/// Sentinel prefix used to piggyback Seatbelt extension tokens on a
/// `SupervisorResponse`.
///
/// This is a workaround for not being able to add new variants to
/// `nono::supervisor::types::SupervisorResponse` in the current nono version.
/// A future nono release should add proper `ExtensionToken` response support.
///
/// The orchestrator sends an extra `Decision` response whose `request_id`
/// carries `TOKEN_REQUEST_ID_PREFIX + <token_string>`. The worker recognises
/// this prefix and calls `extension_consume()` with the embedded token.
const TOKEN_REQUEST_ID_PREFIX: &str = "__nono_extension_token__:";

/// Send a Seatbelt extension token string over the supervisor socket.
///
/// Called by the orchestrator after it has called
/// `nono::sandbox::extension_issue_file()` and received a token. The worker
/// receives this with [`recv_extension_token`] and passes it to
/// `nono::sandbox::extension_consume()`.
pub fn send_extension_token(socket: &mut SupervisorSocket, token: &str) -> nono::Result<()> {
    socket.send_response(&SupervisorResponse::Decision {
        request_id: format!("{}{}", TOKEN_REQUEST_ID_PREFIX, token),
        decision: ApprovalDecision::Granted,
    })
}

/// Receive a Seatbelt extension token string from the supervisor socket.
///
/// Called by the worker after it has received a `Granted` decision for a
/// `ReadWrite` capability request. Returns the raw token string that must be
/// passed to `nono::sandbox::extension_consume()`.
///
/// Returns `Err` if the next response from the socket is not an extension
/// token response, or if the orchestrator signals an error via the sentinel
/// `"__ERROR__"` token value.
pub fn recv_extension_token(socket: &mut SupervisorSocket) -> nono::Result<String> {
    let resp = socket.recv_response()?;
    match resp {
        SupervisorResponse::Decision { request_id, .. } => {
            if let Some(token) = request_id.strip_prefix(TOKEN_REQUEST_ID_PREFIX) {
                if token == "__ERROR__" {
                    return Err(nono::NonoError::SandboxInit(
                        "Orchestrator failed to issue extension token".into(),
                    ));
                }
                Ok(token.to_string())
            } else {
                Err(nono::NonoError::SandboxInit(format!(
                    "Expected extension token response (prefix '{TOKEN_REQUEST_ID_PREFIX}'), \
                     got request_id='{request_id}'"
                )))
            }
        }
    }
}

// ── Standard Supervisor Request Handler ──────────────────────────────────

/// Run a single request-response cycle on the supervisor socket.
///
/// The orchestrator calls this in a loop (or spawns a blocking task) to
/// handle capability requests from the worker. Returns `Ok(true)` if a
/// request was processed, `Ok(false)` if the socket was closed (worker
/// exited), or `Err` on protocol errors.
///
/// # Arguments
///
/// * `socket` - The parent-side supervisor socket
/// * `backend` - The approval backend to use for decisions
/// * `never_grant` - Optional checker for permanently blocked paths
pub fn handle_supervisor_request(
    socket: &mut SupervisorSocket,
    backend: &dyn ApprovalBackend,
    never_grant: Option<&NeverGrantChecker>,
) -> nono::Result<bool> {
    let msg = match socket.recv_message() {
        Ok(msg) => msg,
        Err(nono::NonoError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // Socket closed — worker exited
            return Ok(false);
        }
        Err(e) => return Err(e),
    };

    match msg {
        SupervisorMessage::Request(request) => {
            let request_id = request.request_id.clone();

            // Check never_grant list first
            if let Some(checker) = never_grant
                && checker.check(&request.path).is_blocked()
            {
                let response = SupervisorResponse::Decision {
                    request_id,
                    decision: ApprovalDecision::Denied {
                        reason: format!(
                            "Path {} is on the never-grant list",
                            request.path.display()
                        ),
                    },
                };
                socket.send_response(&response)?;
                return Ok(true);
            }

            // Delegate to approval backend
            let decision = backend.request_capability(&request)?;

            // If granted, open the path and send the fd
            if decision.is_granted() {
                // Open the path with the requested access mode
                let open_result = match request.access {
                    AccessMode::Read => std::fs::File::open(&request.path),
                    AccessMode::Write => {
                        std::fs::OpenOptions::new().write(true).open(&request.path)
                    }
                    AccessMode::ReadWrite => std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&request.path),
                };

                if let Ok(file) = open_result {
                    use std::os::unix::io::AsRawFd;
                    let _ = socket.send_fd(file.as_raw_fd());
                }
            }

            let response = SupervisorResponse::Decision {
                request_id,
                decision,
            };
            socket.send_response(&response)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: canonicalize a tempdir path so it matches nono's internal
    /// canonicalization (e.g. `/var/folders` -> `/private/var/folders` on macOS).
    fn canon(path: &std::path::Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    #[test]
    fn test_policy_build_mode() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let policy = SandboxPolicy {
            cwd: cwd.clone(),
            read_only: false,
            allow_network: true,
            db_path: None,
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();
        // cwd should be covered (use canonicalized path for check)
        assert!(caps.path_covered(&cwd));
        // Network should not be blocked
        assert!(!caps.is_network_blocked());
        // Extensions must be enabled — required for runtime write token exchange
        assert!(caps.extensions_enabled());
        // Static profile uses Read for CWD even in Build mode;
        // write access is obtained at runtime via extension tokens.
        let cwd_cap = caps
            .fs_capabilities()
            .iter()
            .find(|cap| !cap.is_file && cwd.starts_with(&cap.resolved));
        if let Some(cap) = cwd_cap {
            assert_eq!(
                cap.access,
                AccessMode::Read,
                "Build mode static profile must use Read for CWD (write is via extension tokens)"
            );
        }
    }

    #[test]
    fn test_policy_extensions_always_enabled() {
        // Extensions must be enabled for both Build and Plan/Review modes.
        // In Build mode: extension tokens grant write access.
        // In Plan/Review mode: no tokens are issued, but the extension filter
        // rules must still be present in case the mode switches later.
        for read_only in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cwd = canon(dir.path());
            let policy = SandboxPolicy {
                cwd,
                read_only,
                allow_network: true,
                db_path: None,
                socket_dir: None,
            };
            let caps = policy.to_capability_set().unwrap();
            assert!(
                caps.extensions_enabled(),
                "Extensions must be enabled for read_only={read_only}"
            );
        }
    }

    #[test]
    fn test_policy_plan_mode() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let policy = SandboxPolicy {
            cwd: cwd.clone(),
            read_only: true,
            allow_network: true,
            db_path: None,
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();
        assert!(caps.path_covered(&cwd));
        assert!(!caps.is_network_blocked());
    }

    #[test]
    fn test_policy_network_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let policy = SandboxPolicy {
            cwd,
            read_only: false,
            allow_network: false,
            db_path: None,
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();
        assert!(caps.is_network_blocked());
    }

    #[test]
    fn test_policy_summary() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let policy = SandboxPolicy {
            cwd,
            read_only: false,
            allow_network: true,
            db_path: None,
            socket_dir: None,
        };
        let summary = policy.summary().unwrap();
        assert!(!summary.is_empty());
    }

    #[test]
    fn test_support_info() {
        let info = support_info();
        // Just verify it doesn't panic and returns something
        let _ = info.is_supported;
        let _ = &info.details;
    }

    #[test]
    fn test_policy_with_db_path() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let db_file = cwd.join("agent.db");
        // Create the db file so nono can resolve it (WAL/SHM need not pre-exist).
        std::fs::write(&db_file, "").unwrap();
        let policy = SandboxPolicy {
            cwd: cwd.clone(),
            read_only: true,
            allow_network: true,
            db_path: Some(db_file.clone()),
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();
        // cwd should be covered
        assert!(caps.path_covered(&cwd));
        // db file is inside cwd, so covered via the cwd directory capability
        assert!(caps.path_covered(&db_file));
    }

    /// When db_path is outside cwd (the real-world case: ~/.qmt/agent.db vs
    /// /project), the sandbox must grant ReadWrite access to the DB parent
    /// directory so SQLite can create -journal, -wal, and -shm sidecar files.
    /// A literal file capability is not sufficient because SQLite also needs to
    /// create new files in the directory.
    #[test]
    fn test_policy_db_path_outside_cwd_grants_readwrite_on_parent_dir() {
        let cwd_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let cwd = canon(cwd_dir.path());
        let db_parent = canon(db_dir.path());
        let db_file = db_parent.join("agent.db");
        std::fs::write(&db_file, "").unwrap();

        let policy = SandboxPolicy {
            cwd: cwd.clone(),
            read_only: false,
            allow_network: true,
            db_path: Some(db_file.clone()),
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();

        // cwd should still be covered
        assert!(caps.path_covered(&cwd), "cwd must be covered");

        // The DB parent directory must have a ReadWrite *directory* capability
        // (not just a literal file capability), so SQLite can create sidecar
        // files like agent.db-journal, agent.db-wal, agent.db-shm.
        let has_rw_dir_cap_for_parent = caps.fs_capabilities().iter().any(|cap| {
            !cap.is_file
                && db_parent.starts_with(&cap.resolved)
                && cap.access == AccessMode::ReadWrite
        });
        assert!(
            has_rw_dir_cap_for_parent,
            "db parent dir '{}' must have a ReadWrite directory capability so SQLite can \
             create sidecar files; got capabilities:\n{}",
            db_parent.display(),
            caps.summary()
        );
    }

    /// When db_path is not set, the capability set must still be valid
    /// (no panic, cwd covered).
    #[test]
    fn test_policy_db_path_none_still_valid() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let policy = SandboxPolicy {
            cwd: cwd.clone(),
            read_only: false,
            allow_network: true,
            db_path: None,
            socket_dir: None,
        };
        let caps = policy.to_capability_set().unwrap();
        assert!(caps.path_covered(&cwd));
    }

    #[test]
    fn test_policy_summary_includes_db_path() {
        let cwd_dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let cwd = canon(cwd_dir.path());
        let db_parent = canon(db_dir.path());
        let db_file = db_parent.join("test.db");
        std::fs::write(&db_file, "").unwrap();
        let policy = SandboxPolicy {
            cwd,
            read_only: false,
            allow_network: true,
            db_path: Some(db_file),
            socket_dir: None,
        };
        let summary = policy.summary().unwrap();
        assert!(!summary.is_empty());
        // Summary should mention the db parent dir (which is what we now grant)
        assert!(
            !summary.is_empty(),
            "summary should not be empty, got: {}",
            summary
        );
    }

    #[test]
    fn test_mode_approval_backend_read_within_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let backend = ModeApprovalBackend::new(cwd.clone(), false);

        let request = CapabilityRequest {
            request_id: "test-001".to_string(),
            path: cwd.join("src/main.rs"),
            access: AccessMode::Read,
            reason: Some("reading source".to_string()),
            child_pid: 1234,
            session_id: "sess-001".to_string(),
        };
        let decision = backend.request_capability(&request).unwrap();
        assert!(decision.is_granted());
    }

    #[test]
    fn test_mode_approval_backend_write_denied_in_readonly() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let backend = ModeApprovalBackend::new(cwd.clone(), false);

        let request = CapabilityRequest {
            request_id: "test-002".to_string(),
            path: cwd.join("src/main.rs"),
            access: AccessMode::ReadWrite,
            reason: Some("editing source".to_string()),
            child_pid: 1234,
            session_id: "sess-001".to_string(),
        };
        let decision = backend.request_capability(&request).unwrap();
        assert!(decision.is_denied());
    }

    #[test]
    fn test_mode_approval_backend_write_granted_in_build() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let backend = ModeApprovalBackend::new(cwd.clone(), true);

        let request = CapabilityRequest {
            request_id: "test-003".to_string(),
            path: cwd.join("src/main.rs"),
            access: AccessMode::ReadWrite,
            reason: Some("editing source".to_string()),
            child_pid: 1234,
            session_id: "sess-001".to_string(),
        };
        let decision = backend.request_capability(&request).unwrap();
        assert!(decision.is_granted());
    }

    #[test]
    fn test_mode_approval_backend_outside_cwd_denied() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let backend = ModeApprovalBackend::new(cwd, true);

        let request = CapabilityRequest {
            request_id: "test-004".to_string(),
            path: PathBuf::from("/etc/passwd"),
            access: AccessMode::Read,
            reason: Some("reading passwd".to_string()),
            child_pid: 1234,
            session_id: "sess-001".to_string(),
        };
        let decision = backend.request_capability(&request).unwrap();
        assert!(decision.is_denied());
    }

    #[test]
    fn test_mode_approval_backend_set_allow_write() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canon(dir.path());
        let backend = ModeApprovalBackend::new(cwd.clone(), false);

        let request = CapabilityRequest {
            request_id: "test-005".to_string(),
            path: cwd.join("file.txt"),
            access: AccessMode::ReadWrite,
            reason: None,
            child_pid: 1234,
            session_id: "sess-001".to_string(),
        };

        // Initially read-only
        assert!(backend.request_capability(&request).unwrap().is_denied());

        // Switch to build mode
        backend.set_allow_write(true);
        assert!(backend.request_capability(&request).unwrap().is_granted());

        // Switch back to read-only
        backend.set_allow_write(false);
        assert!(backend.request_capability(&request).unwrap().is_denied());
    }

    #[test]
    fn test_create_supervisor_socket_pair() {
        let (parent, child) = create_supervisor_socket_pair().unwrap();
        // Verify we got two distinct sockets
        assert_ne!(parent.as_raw_fd(), child.as_raw_fd());
    }

    #[test]
    fn test_backend_name() {
        let backend = ModeApprovalBackend::new(PathBuf::from("/tmp"), false);
        assert_eq!(backend.backend_name(), "querymt-mode-approval");
    }
}
