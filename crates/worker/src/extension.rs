//! Sandbox extension manager for dynamic Build/Plan mode switching.
//!
//! Manages Seatbelt extension tokens: consumes tokens to gain write access
//! (Build mode) and releases them to revoke write access (Plan/Review mode).
//!
//! # Design
//!
//! The static Seatbelt profile always grants only `Read` access to the session
//! CWD. Write access is obtained at runtime by:
//!
//! 1. Sending a `CapabilityRequest(CWD, ReadWrite)` to the orchestrator over
//!    the supervisor socket.
//! 2. Receiving a `Decision(Granted)` response followed by an extension token
//!    (via [`querymt_sandbox::recv_extension_token`]).
//! 3. Calling `nono::sandbox::extension_consume(token)` to activate the token;
//!    this returns a handle that can later be passed to
//!    `nono::sandbox::extension_release(handle)` to revoke the access.
//!
//! On Plan/Review -> Build transitions, `request_write()` is called to go
//! through steps 1–3. On Build -> Plan/Review transitions, `release_write()`
//! calls `extension_release()` on the stored handle, revoking OS-level write
//! access immediately.

use nono::supervisor::types::{CapabilityRequest, SupervisorMessage, SupervisorResponse};
use nono::{AccessMode, SupervisorSocket};
use querymt_sandbox::WriteAccessManager;
use std::path::PathBuf;
use std::sync::Mutex;

/// Manages Seatbelt extension tokens for a single worker session.
///
/// Thread-safe: both `release_write()` and `request_write()` can be called
/// from any thread. The internal `Mutex` serialises socket I/O and handle
/// management.
pub struct ExtensionManager {
    /// Supervisor socket for sending capability requests to the orchestrator.
    ///
    /// Wrapped in a `Mutex` so it can be shared across threads (the
    /// `SessionActor` calls extension methods from its async task, which may
    /// use `spawn_blocking` for blocking socket I/O in the future).
    socket: Mutex<SupervisorSocket>,
    /// Session working directory. Used as the path in `CapabilityRequest`.
    cwd: PathBuf,
    /// Unique session identifier. Used in `CapabilityRequest.session_id` and
    /// for generating unique `request_id` values.
    session_id: String,
    /// Handle returned by `nono::sandbox::extension_consume()`.
    ///
    /// `Some(h)` means the worker currently has OS-level write access to CWD.
    /// `None` means no write extension is active (read-only).
    current_handle: Mutex<Option<i64>>,
}

impl ExtensionManager {
    /// Create a new `ExtensionManager`.
    ///
    /// The manager takes ownership of the supervisor socket. The caller must
    /// not use the socket elsewhere after this call.
    pub fn new(socket: SupervisorSocket, cwd: PathBuf, session_id: String) -> Self {
        Self {
            socket: Mutex::new(socket),
            cwd,
            session_id,
            current_handle: Mutex::new(None),
        }
    }

    /// Release the current write extension, revoking OS-level write access.
    ///
    /// Called on Build -> Plan/Review transitions. If no write extension is
    /// active (e.g. the worker started in Plan mode), this is a no-op.
    ///
    /// On non-macOS platforms this is a no-op (Seatbelt extensions are
    /// macOS-specific; Linux Landlock tightening requires a separate approach).
    pub fn release_write(&self) {
        let mut handle = self.current_handle.lock().unwrap();
        if let Some(_h) = handle.take() {
            #[cfg(target_os = "macos")]
            match nono::sandbox::extension_release(_h) {
                Ok(()) => {
                    tracing::info!(
                        session_id = %self.session_id,
                        "Write access released (read-only mode)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to release write extension"
                    );
                }
            }
            #[cfg(not(target_os = "macos"))]
            tracing::info!(
                session_id = %self.session_id,
                "Write extension release requested (no-op on non-macOS)"
            );
        }
    }

    /// Request a write extension from the orchestrator and consume it.
    ///
    /// Called on Plan/Review -> Build transitions, and at startup if the
    /// initial mode is Build (since the static sandbox only grants `Read` to
    /// CWD).
    ///
    /// Sends a `CapabilityRequest(CWD, ReadWrite)` to the orchestrator. If
    /// the orchestrator grants the request, it also sends an extension token
    /// (via [`querymt_sandbox::recv_extension_token`]). The token is consumed
    /// with `nono::sandbox::extension_consume()` and the resulting handle is
    /// stored for later release.
    ///
    /// Logs a warning and returns without panicking if any step fails, so the
    /// worker degrades gracefully (the OS sandbox will simply deny writes).
    pub fn request_write(&self) {
        let mut socket = self.socket.lock().unwrap();

        let request = CapabilityRequest {
            request_id: format!("ext-rw-{}-{}", self.session_id, unique_suffix()),
            path: self.cwd.clone(),
            access: AccessMode::ReadWrite,
            reason: Some("Build mode write access".to_string()),
            child_pid: std::process::id(),
            session_id: self.session_id.clone(),
        };

        tracing::debug!(
            session_id = %self.session_id,
            path = %self.cwd.display(),
            "Requesting write extension from supervisor"
        );

        if let Err(e) = socket.send_message(&SupervisorMessage::Request(request)) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to send write extension request"
            );
            return;
        }

        let decision = match socket.recv_response() {
            Ok(SupervisorResponse::Decision { decision, .. }) => decision,
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to receive supervisor response for write extension"
                );
                return;
            }
        };

        if !decision.is_granted() {
            tracing::warn!(
                session_id = %self.session_id,
                "Write access denied by supervisor: {:?}",
                decision
            );
            return;
        }

        #[cfg(target_os = "macos")]
        {
            // Receive the extension token sent as a follow-up response.
            let token = match querymt_sandbox::recv_extension_token(&mut socket) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to receive extension token"
                    );
                    return;
                }
            };

            match nono::sandbox::extension_consume(&token) {
                Ok(h) => {
                    *self.current_handle.lock().unwrap() = Some(h);
                    tracing::info!(
                        session_id = %self.session_id,
                        "Write access granted (Build mode), extension handle={h}"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to consume extension token"
                    );
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            // Linux does not use token issue/consume. A Granted decision is the
            // authoritative write-enable signal for supervisor-mediated opens.
            *self.current_handle.lock().unwrap() = Some(0);
            tracing::info!(
                session_id = %self.session_id,
                "Write access granted (Build mode, non-macOS)"
            );
        }
    }

    /// Returns `true` if the manager currently holds an active write extension.
    pub fn has_write(&self) -> bool {
        self.current_handle.lock().unwrap().is_some()
    }
}

impl WriteAccessManager for ExtensionManager {
    fn release_write(&self) {
        self.release_write();
    }

    fn request_write(&self) {
        self.request_write();
    }

    fn has_write(&self) -> bool {
        self.has_write()
    }
}

/// Generate a unique suffix for `CapabilityRequest.request_id`.
///
/// Uses PID + nanoseconds since epoch for uniqueness without pulling in the
/// `uuid` crate (which is already available in the workspace but not declared
/// as a dependency of the worker crate).
fn unique_suffix() -> String {
    let pid = std::process::id();
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid}-{ns}")
}
