//! Worker process manager for sandboxed agent sessions.
//!
//! Behind `#[cfg(feature = "sandbox")]`. The orchestrator uses `WorkerManager`
//! to spawn, track, and destroy sandboxed worker processes. Each worker hosts
//! exactly one `SessionActor` inside a nono sandbox.
//!
//! # Architecture
//!
//! ```text
//! Orchestrator (unsandboxed)
//!   └── WorkerManager
//!         ├── WorkerHandle { child, session_ref, cwd }  // session-1
//!         └── WorkerHandle { child, session_ref, cwd }  // session-2
//! ```
//!
//! Communication between orchestrator and worker happens through the kameo
//! actor mesh. The `SessionActorRef::Remote` returned by `spawn_worker()`
//! is stored in `SessionRegistry` — all downstream code works unchanged.

use crate::agent::core::AgentMode;
use crate::agent::remote::SessionActorRef;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "sandbox")]
use querymt_sandbox::{ModeApprovalBackend, SupervisorSocket};
use std::sync::Arc;

#[cfg(feature = "remote")]
use crate::agent::remote::MeshHandle;

/// Manages sandboxed worker processes for agent sessions.
///
/// Each session gets its own worker process with a nono sandbox applied
/// at startup. The worker manager tracks these processes and handles
/// lifecycle events (spawn, mode switch, destroy).
pub struct WorkerManager {
    workers: HashMap<String, WorkerHandle>,
    /// Handle to the kameo mesh for DHT lookups and passing the orchestrator's
    /// listen address to spawned workers.
    #[cfg(feature = "remote")]
    mesh: Option<MeshHandle>,
    /// Path to the shared SQLite database, passed to each worker via `--db-path`.
    db_path: Option<PathBuf>,
    /// Path to the `querymt-worker` binary.
    ///
    /// Defaults to `worker_binary_default()` — the binary next to the current
    /// executable, falling back to a bare `querymt-worker` name (PATH lookup).
    worker_binary: PathBuf,
}

/// Handle to a running worker process.
struct WorkerHandle {
    /// The child process handle.
    child: tokio::process::Child,
    /// Remote actor reference to the session hosted in the worker.
    #[allow(dead_code)]
    session_ref: SessionActorRef,
    /// Working directory the worker was launched with.
    cwd: PathBuf,
    /// Current agent mode of the worker.
    current_mode: AgentMode,
    /// Approval backend that decides whether to grant/deny capability requests.
    ///
    /// Shared via `Arc` so both the supervisor loop (`spawn_blocking` thread)
    /// and `switch_mode` (async task) can access it concurrently.  The
    /// `allow_write` flag inside uses an `AtomicBool`, so no lock is needed:
    /// `switch_mode` stores with `Release` ordering and the supervisor loop
    /// loads with `Acquire` ordering.
    #[cfg(feature = "sandbox")]
    approval_backend: Option<Arc<ModeApprovalBackend>>,
    /// Handle to the background task that runs the supervisor loop.
    ///
    /// The supervisor loop holds exclusive ownership of the `SupervisorSocket`
    /// (needed for `recv_message` / `send_response`). `WorkerHandle` does NOT
    /// store the socket directly.
    #[cfg(feature = "sandbox")]
    supervisor_task: Option<tokio::task::JoinHandle<()>>,
}

/// Error type for worker manager operations.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("Failed to spawn worker process: {0}")]
    SpawnFailed(String),
    #[error("Worker not found for session: {0}")]
    NotFound(String),
    #[error("Worker registration timeout for session: {0}")]
    RegistrationTimeout(String),
    #[error("Mode switch failed: {0}")]
    ModeSwitchFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve the default path to the `querymt-worker` binary.
///
/// Looks for the binary next to the current executable first (covers the
/// `cargo run --example coder_agent` case where both binaries live in
/// `target/debug/`). Falls back to a bare name so the OS PATH is searched.
fn worker_binary_default() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("querymt-worker");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("querymt-worker")
}

impl WorkerManager {
    /// Create a new empty worker manager.
    pub fn new() -> Self {
        Self {
            workers: HashMap::new(),
            #[cfg(feature = "remote")]
            mesh: None,
            db_path: None,
            worker_binary: "/Users/wiking/qmt/querymt/target/debug/querymt-worker".into(), // worker_binary: worker_binary_default(),
        }
    }

    /// Override the path to the `querymt-worker` binary.
    pub fn set_worker_binary(&mut self, path: PathBuf) {
        self.worker_binary = path;
    }

    /// Create a worker manager with mesh and database path.
    ///
    /// The mesh handle is used to:
    /// - Derive the orchestrator's listen address for `--mesh-peer`
    /// - Perform DHT lookups in `wait_for_registration()`
    ///
    /// The `db_path` is passed to each worker via `--db-path`.
    #[cfg(feature = "remote")]
    pub fn with_mesh_and_db(mesh: MeshHandle, db_path: PathBuf) -> Self {
        Self {
            workers: HashMap::new(),
            mesh: Some(mesh),
            db_path: Some(db_path),
            worker_binary: worker_binary_default(),
        }
    }

    /// Set or update the mesh handle.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&mut self, mesh: MeshHandle) {
        self.mesh = Some(mesh);
    }

    /// Return a clone of the mesh handle, if set.
    #[cfg(feature = "remote")]
    pub fn mesh_handle(&self) -> Option<MeshHandle> {
        self.mesh.clone()
    }

    /// Set or update the database path.
    pub fn set_db_path(&mut self, db_path: PathBuf) {
        self.db_path = Some(db_path);
    }

    /// Spawn a sandboxed worker for a new session.
    ///
    /// The worker process:
    /// 1. Applies a nono sandbox with the given `cwd` and `mode`
    /// 2. Registers in the kameo mesh
    /// 3. Hosts a `SessionActor`
    ///
    /// Returns a `SessionActorRef::Remote` pointing to the worker's actor.
    pub async fn spawn_worker(
        &mut self,
        session_id: &str,
        cwd: PathBuf,
        mode: AgentMode,
    ) -> Result<SessionActorRef, WorkerError> {
        tracing::info!(
            session_id = session_id,
            cwd = %cwd.display(),
            mode = %mode,
            "Spawning sandboxed worker"
        );

        // Bind the supervisor socket BEFORE spawning the worker.
        //
        // Protocol:
        //   1. Orchestrator calls UnixListener::bind(path)  → socket file now exists
        //   2. Worker process starts and calls SupervisorSocket::connect(path) → succeeds
        //   3. Orchestrator calls listener.accept() → returns the connected stream
        //
        // We split bind() and accept() so the socket file exists by the time the
        // worker process starts, avoiding the "No such file or directory" error that
        // occurred when both sides called connect() and nobody called bind().
        #[cfg(feature = "sandbox")]
        let (supervisor_socket_path, supervisor_listener) = {
            let path = querymt_utils::providers::socket_dir()
                .map_err(|e| WorkerError::SpawnFailed(e.to_string()))?
                .join(format!("sup-{}.sock", session_id));
            // Clean up any stale socket from a previous run
            let _ = std::fs::remove_file(&path);
            // Bind now so the socket file exists before the worker spawns.
            let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|e| {
                WorkerError::SpawnFailed(format!(
                    "Failed to bind supervisor socket at {}: {e}",
                    path.display()
                ))
            })?;
            // Restrict to owner-only access (0700).
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                let _ = std::fs::set_permissions(&path, perms);
            }
            (path, listener)
        };

        // Resolve the orchestrator's actual listen address for the worker CLI.
        // The address is populated by SwarmEvent::NewListenAddr shortly after
        // bootstrap, so we retry briefly if it isn't set yet.
        #[cfg(feature = "remote")]
        let orchestrator_addr = {
            let mesh = self.mesh.as_ref().ok_or_else(|| {
                WorkerError::SpawnFailed(
                    "mesh handle not set on WorkerManager; cannot pass --mesh-peer to worker"
                        .to_string(),
                )
            })?;
            // Poll up to 20 × 10 ms = 200 ms for the listen address to appear.
            let mut addr_opt = None;
            for _ in 0..20 {
                addr_opt = mesh.worker_bootstrap_addr();
                if addr_opt.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            addr_opt.ok_or_else(|| {
                WorkerError::SpawnFailed(
                    "mesh listen address not available after 200ms; \
                     is bootstrap_mesh() fully initialised?"
                        .to_string(),
                )
            })?
        };

        let db_path_arg = self.db_path.as_ref().ok_or_else(|| {
            WorkerError::SpawnFailed(
                "db_path not set on WorkerManager; cannot pass --db-path to worker".to_string(),
            )
        })?;

        // Spawn worker binary as child process
        let mut cmd = tokio::process::Command::new(&self.worker_binary);
        cmd.arg("--cwd")
            .arg(&cwd)
            .arg("--mode")
            .arg(mode.as_str())
            .arg("--session-id")
            .arg(session_id)
            .arg("--db-path")
            .arg(db_path_arg)
            .kill_on_drop(true); // Clean up if orchestrator drops the handle

        #[cfg(feature = "remote")]
        cmd.arg("--mesh-peer").arg(&orchestrator_addr);

        // If remote feature is not enabled, pass a placeholder (the worker
        // requires --mesh-peer but won't actually use it without remote).
        #[cfg(not(feature = "remote"))]
        cmd.arg("--mesh-peer").arg("placeholder");

        #[cfg(feature = "sandbox")]
        cmd.arg("--supervisor-socket").arg(&supervisor_socket_path);

        let child = cmd
            .spawn()
            .map_err(|e| WorkerError::SpawnFailed(e.to_string()))?;

        let child_pid = child.id();
        tracing::info!(
            session_id = session_id,
            pid = ?child_pid,
            "Worker process spawned"
        );

        // Accept the worker's supervisor connection, start the supervisor loop,
        // and THEN wait for DHT registration.
        //
        // ORDER MATTERS — this is why we don't use tokio::join! here:
        //
        // The worker's startup sequence is:
        //   1. Apply sandbox
        //   2. Connect supervisor socket
        //   3. Call request_write() — BLOCKING: sends CapabilityRequest and waits
        //      for TWO responses (Decision + extension token) before returning
        //   4. Bootstrap mesh
        //   5. Register SessionActor in DHT
        //
        // If we ran accept() and wait_for_registration() concurrently (as a
        // join!), the supervisor loop would only be spawned AFTER both completed.
        // But wait_for_registration() can never succeed while the worker is stuck
        // in step 3, and step 3 is stuck because nobody is reading from the
        // supervisor socket yet — a deadlock.
        //
        // The fix: accept() first, then immediately spawn the supervisor loop so
        // it can respond to the worker's request_write(), unblocking the worker
        // so it can proceed to steps 4-5 (mesh + DHT), after which
        // wait_for_registration() succeeds.
        #[cfg(feature = "sandbox")]
        let (session_ref, approval_backend, supervisor_task) = {
            // Step A: accept the worker's connection (blocking, but fast — the
            // worker connects immediately after applying the sandbox).
            let accept_result =
                tokio::task::spawn_blocking(move || supervisor_listener.accept()).await;

            let (backend_arc, task) = match accept_result {
                Ok(Ok((stream, _addr))) => {
                    let parent_socket = SupervisorSocket::from_stream(stream);
                    let backend = ModeApprovalBackend::new(cwd.clone(), !mode.is_read_only());
                    tracing::info!(session_id = session_id, "Supervisor socket accepted");

                    // Step B: spawn the supervisor loop NOW, before waiting for
                    // DHT registration. The loop must be running so it can respond
                    // to the worker's initial request_write() call (which blocks
                    // the worker until it receives two responses). Only after the
                    // supervisor loop handles that exchange will the worker proceed
                    // to bootstrap the mesh and register in the DHT.
                    let backend_arc = Arc::new(backend);
                    let backend_for_loop = backend_arc.clone();
                    let sid = session_id.to_string();
                    let mut socket = parent_socket;

                    let task = tokio::task::spawn_blocking(move || {
                        let mut handler = querymt_sandbox::platform_supervisor_handler();
                        loop {
                            match handler.handle_request(&mut socket, &*backend_for_loop, None) {
                                Ok(true) => {
                                    continue;
                                }
                                Ok(false) => {
                                    tracing::info!(
                                        session_id = %sid,
                                        "Supervisor: worker disconnected"
                                    );
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        session_id = %sid,
                                        error = %e,
                                        "Supervisor loop error"
                                    );
                                    break;
                                }
                            }
                        }
                    });

                    tracing::info!(session_id = session_id, "Supervisor loop started");
                    (Some(backend_arc), Some(task))
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        session_id = session_id,
                        error = %e,
                        "Supervisor socket accept failed, capability expansion disabled"
                    );
                    (None, None)
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = session_id,
                        error = %e,
                        "Supervisor accept task panicked, capability expansion disabled"
                    );
                    (None, None)
                }
            };

            // Step C: NOW wait for DHT registration. The supervisor loop is
            // already running, so the worker's request_write() will be answered,
            // unblocking it to proceed with mesh bootstrap and DHT registration.
            let session_ref_inner = self
                .wait_for_registration(session_id)
                .await
                .map_err(|e| WorkerError::RegistrationTimeout(e.to_string()))?;

            (session_ref_inner, backend_arc, task)
        };

        // When the sandbox feature is disabled, wait for DHT registration normally.
        #[cfg(not(feature = "sandbox"))]
        let session_ref = self
            .wait_for_registration(session_id)
            .await
            .map_err(|e| WorkerError::RegistrationTimeout(e.to_string()))?;

        // Store handle
        self.workers.insert(
            session_id.to_string(),
            WorkerHandle {
                child,
                session_ref: session_ref.clone(),
                cwd,
                current_mode: mode,
                #[cfg(feature = "sandbox")]
                approval_backend,
                #[cfg(feature = "sandbox")]
                supervisor_task,
            },
        );

        tracing::info!(session_id = session_id, "Worker registered and ready");

        Ok(session_ref)
    }

    /// Wait for a worker to register in the mesh and return its actor ref.
    ///
    /// Polls the kameo mesh DHT with exponential backoff until the
    /// session actor is discoverable, or times out after ~30 seconds.
    async fn wait_for_registration(
        &self,
        session_id: &str,
    ) -> Result<SessionActorRef, WorkerError> {
        #[cfg(feature = "remote")]
        {
            use crate::agent::remote::dht_name;
            use crate::agent::session_actor::SessionActor;

            let mesh = self.mesh.as_ref().ok_or_else(|| {
                WorkerError::RegistrationTimeout(
                    "no mesh handle available for DHT lookup".to_string(),
                )
            })?;

            let dht_name = dht_name::session(session_id);

            // Poll with exponential backoff: 250ms, 500ms, 1s, 2s, 4s, 8s, 8s, 8s
            // Total worst-case wait: ~31.75s
            let backoffs = [250, 500, 1000, 2000, 4000, 8000, 8000, 8000];
            for (attempt, &delay_ms) in backoffs.iter().enumerate() {
                match mesh.lookup_actor::<SessionActor>(dht_name.clone()).await {
                    Ok(Some(remote_ref)) => {
                        tracing::info!(
                            session_id,
                            attempt,
                            "Worker session actor discovered in DHT",
                        );
                        return Ok(SessionActorRef::remote(remote_ref, "worker".to_string()));
                    }
                    Ok(None) => {
                        tracing::debug!(session_id, attempt, delay_ms, "DHT lookup miss, retrying");
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id,
                            attempt,
                            error = %e,
                            "DHT lookup error"
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
            }

            Err(WorkerError::RegistrationTimeout(format!(
                "session {} not found in DHT after 30s",
                session_id,
            )))
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = session_id;
            Err(WorkerError::RegistrationTimeout(
                "mesh registration requires the 'remote' feature".to_string(),
            ))
        }
    }

    /// Handle a mode switch by issuing/revoking extension tokens.
    ///
    /// On macOS, this uses `sandbox_extension_consume()`/`sandbox_extension_release()`
    /// through the supervisor socket. On Linux, this uses seccomp-notify.
    ///
    /// The mode change is also forwarded to the worker's `SessionActor`.
    pub async fn switch_mode(
        &mut self,
        session_id: &str,
        new_mode: AgentMode,
    ) -> Result<(), WorkerError> {
        let worker = self
            .workers
            .get_mut(session_id)
            .ok_or_else(|| WorkerError::NotFound(session_id.to_string()))?;

        let old_mode = worker.current_mode;
        if old_mode == new_mode {
            return Ok(());
        }

        tracing::info!(
            session_id = session_id,
            old_mode = %old_mode,
            new_mode = %new_mode,
            "Switching worker mode"
        );

        // Update the approval backend so the supervisor loop grants/denies
        // capability requests according to the new mode.
        //
        // AtomicBool store with Release ordering — the supervisor loop loads
        // with Acquire ordering on its next iteration, so it always sees the
        // updated value.  No lock needed, no deadlock possible.
        #[cfg(feature = "sandbox")]
        if let Some(ref backend_arc) = worker.approval_backend {
            let allow_write = !new_mode.is_read_only();
            backend_arc.set_allow_write(allow_write);
            tracing::debug!(
                session_id = session_id,
                allow_write = allow_write,
                "Updated supervisor approval backend for mode switch"
            );
        }

        // Forward mode change to the SessionActor
        worker
            .session_ref
            .set_mode(new_mode)
            .await
            .map_err(|e| WorkerError::ModeSwitchFailed(e.to_string()))?;

        worker.current_mode = new_mode;

        tracing::info!(
            session_id = session_id,
            mode = %new_mode,
            "Worker mode switched"
        );

        Ok(())
    }

    /// Kill a worker process when a session ends.
    ///
    /// Sends SIGKILL to the child process and removes the worker from tracking.
    pub async fn destroy_worker(&mut self, session_id: &str) -> Result<(), WorkerError> {
        if let Some(mut worker) = self.workers.remove(session_id) {
            tracing::info!(
                session_id = session_id,
                pid = ?worker.child.id(),
                "Destroying worker"
            );
            // Abort the supervisor loop task if running
            #[cfg(feature = "sandbox")]
            if let Some(task) = worker.supervisor_task.take() {
                task.abort();
            }
            let _ = worker.child.kill().await;
            // Clean up the supervisor socket file
            let socket_path = querymt_utils::providers::socket_dir()
                .ok()
                .map(|d| d.join(format!("sup-{}.sock", session_id)));
            if let Some(p) = socket_path {
                let _ = std::fs::remove_file(p);
            }
            tracing::info!(session_id = session_id, "Worker destroyed");
        } else {
            tracing::debug!(
                session_id = session_id,
                "No worker to destroy (already gone)"
            );
        }
        Ok(())
    }

    /// Number of active workers.
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// Whether there are no active workers.
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// Check if a worker exists for a given session.
    pub fn has_worker(&self, session_id: &str) -> bool {
        self.workers.contains_key(session_id)
    }

    /// List all active session IDs with workers.
    pub fn session_ids(&self) -> Vec<String> {
        self.workers.keys().cloned().collect()
    }

    /// Destroy all workers. Called during orchestrator shutdown.
    pub async fn destroy_all(&mut self) {
        let ids: Vec<String> = self.workers.keys().cloned().collect();
        for id in ids {
            let _ = self.destroy_worker(&id).await;
        }
    }
}

impl Default for WorkerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WorkerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerManager")
            .field("workers_count", &self.workers.len())
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

/// Test-only helpers for injecting fake workers into WorkerManager without
/// spawning real child processes. Used by UI handler tests.
#[cfg(test)]
impl WorkerManager {
    /// Inject a local `SessionActorRef` as a fake worker entry.
    ///
    /// Uses a dummy `sleep infinity` child process (or `/usr/bin/true`) so
    /// `WorkerHandle` has a valid `Child`. The `session_ref` is a local actor.
    /// Only for use in unit tests.
    pub async fn inject_local_worker(
        &mut self,
        session_id: &str,
        session_ref: SessionActorRef,
        mode: AgentMode,
        cwd: PathBuf,
    ) {
        use tokio::process::Command;
        // Spawn a trivial long-lived process just to have a Child handle.
        let child = Command::new("sleep")
            .arg("3600")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn sleep for test worker");
        self.workers.insert(
            session_id.to_string(),
            WorkerHandle {
                child,
                session_ref,
                cwd,
                current_mode: mode,
                #[cfg(feature = "sandbox")]
                approval_backend: None,
                #[cfg(feature = "sandbox")]
                supervisor_task: None,
            },
        );
    }

    /// Return the tracked mode for a worker. Panics if session not found.
    pub fn worker_current_mode(&self, session_id: &str) -> AgentMode {
        self.workers
            .get(session_id)
            .expect("no worker for session")
            .current_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_manager_is_empty() {
        let mgr = WorkerManager::new();
        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);
        assert!(mgr.session_ids().is_empty());
    }

    #[test]
    fn test_has_worker_returns_false_for_unknown() {
        let mgr = WorkerManager::new();
        assert!(!mgr.has_worker("nonexistent"));
    }

    #[tokio::test]
    async fn test_destroy_nonexistent_worker_is_ok() {
        let mut mgr = WorkerManager::new();
        // Should not error
        mgr.destroy_worker("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_destroy_all_empty() {
        let mut mgr = WorkerManager::new();
        mgr.destroy_all().await;
        assert!(mgr.is_empty());
    }
}
