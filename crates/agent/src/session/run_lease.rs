//! Cross-process, per-session run leases.
//!
//! A local run holds an exclusive OS-level file lock on
//! `<root>/<session_id>.lock` for its whole lifetime. The operating system
//! releases the lock when the owning process exits for any reason, so a lease
//! that can be acquired means no live process is running that session.
//!
//! Notes:
//! - Acquisition never blocks: `None` means "owned by someone alive".
//!   [`RunLeaseRoot::acquire_with_grace`] retries for a short bounded time so a
//!   momentary holder (a reconciler or delete in this or another process) is
//!   not mistaken for a live run.
//! - Lease files are never deleted by the lease itself (deleting a lock file
//!   lets two holders lock different files). Leftover files are harmless.
//! - Files are opened through `std` only, so handles are close-on-exec on Unix
//!   and non-inheritable on Windows; spawned tools cannot extend a lease.
//! - Exclusivity also holds within one process: `flock` (Unix) and
//!   `LockFileEx` (Windows) conflict between separate opens of the same file.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Longest accepted session id; real ids are 36-character UUIDs.
const MAX_SESSION_ID_LEN: usize = 64;

/// Directory of run leases for one sessions database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLeaseRoot {
    root: PathBuf,
}

/// An exclusive lease on one session. Dropping it releases the lock.
#[derive(Debug)]
pub struct RunLease {
    // Held only for its lock; closing the handle releases it.
    _file: File,
}

/// Lease root for a sessions database: `<db_dir>/<db_stem>.run-leases`.
///
/// Returns `None` for databases without a stable on-disk location
/// (in-memory databases).
pub fn run_lease_root_for_db(db_path: &Path) -> Option<PathBuf> {
    if db_path.as_os_str().is_empty() || db_path == Path::new(":memory:") {
        return None;
    }
    let stem = db_path.file_stem()?;
    let mut dir_name = stem.to_os_string();
    dir_name.push(".run-leases");
    Some(match db_path.parent() {
        Some(parent) => parent.join(dir_name),
        None => PathBuf::from(dir_name),
    })
}

/// A session id is usable as a file name on every platform when it is a short
/// token of ASCII alphanumerics and `-` (UUIDs satisfy this).
fn is_valid_session_token(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl RunLeaseRoot {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Lease file path for `session_id`.
    pub fn lease_path(&self, session_id: &str) -> io::Result<PathBuf> {
        if !is_valid_session_token(session_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid session id for run lease: {session_id:?}"),
            ));
        }
        Ok(self.root.join(format!("{session_id}.lock")))
    }

    /// Try to take the session's lease without blocking.
    ///
    /// `Ok(None)` means another holder (any process, including this one) owns
    /// it. Other I/O failures are returned as errors.
    pub fn try_acquire(&self, session_id: &str) -> io::Result<Option<RunLease>> {
        let path = self.lease_path(session_id)?;
        let open = || {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
        };
        // The directory is created only on first use, not on every acquire.
        let file = match open() {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.root)?;
                open()?
            }
            other => other?,
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(RunLease { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    /// [`Self::try_acquire`] on the blocking pool, so file I/O never stalls
    /// an async worker.
    pub async fn acquire(&self, session_id: &str) -> io::Result<Option<RunLease>> {
        let root = self.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || root.try_acquire(&session_id))
            .await
            .map_err(io::Error::other)?
    }

    /// Acquire, retrying for up to `grace` while the lease is busy.
    ///
    /// Reconciliation and session delete hold a lease for a few milliseconds.
    /// A run start that collides with one of them must wait, not report the
    /// session as running elsewhere. `Ok(None)` after the grace period means
    /// a real run owns the session.
    pub async fn acquire_with_grace(
        &self,
        session_id: &str,
        grace: Duration,
    ) -> io::Result<Option<RunLease>> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(lease) = self.acquire(session_id).await? {
                return Ok(Some(lease));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Best-effort removal of a deleted session's lease file, only when the
    /// lease is free. Failures are ignored; the file is just an empty marker.
    ///
    /// Deleting a lock file can let two acquirers lock different files (one
    /// holding the unlinked inode). That is acceptable only because the
    /// session is already deleted and will not run again.
    pub fn remove_if_free(&self, session_id: &str) {
        let Ok(path) = self.lease_path(session_id) else {
            return;
        };
        if let Ok(Some(lease)) = self.try_acquire(session_id) {
            // Unix: unlink while still holding the lock, narrowing the window.
            // Windows: release first; deleting a file that is open leaves it
            // delete-pending and makes later opens fail.
            #[cfg(unix)]
            {
                let _ = std::fs::remove_file(&path);
                drop(lease);
            }
            #[cfg(not(unix))]
            {
                drop(lease);
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const SID: &str = "01a17c4b-3b1f-72d2-99e1-16024ff942d5";

    fn root(dir: &TempDir) -> RunLeaseRoot {
        RunLeaseRoot::new(dir.path().join("sessions.run-leases"))
    }

    fn acquire_within(root: &RunLeaseRoot, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if root.try_acquire(SID).unwrap().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_marker(marker: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "helper never signalled readiness"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Re-execute this test binary running one of the `#[ignore]` helpers.
    fn spawn_helper(name: &str, envs: &[(&str, &Path)]) -> Child {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            &format!("session::run_lease::tests::{name}"),
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.spawn().unwrap()
    }

    /// Helper process: take the lease, signal readiness, then hold it.
    #[test]
    #[ignore]
    fn hold_lease_helper() {
        let root = RunLeaseRoot::new(std::env::var_os("QMT_LEASE_ROOT").unwrap().into());
        let marker = PathBuf::from(std::env::var_os("QMT_LEASE_MARKER").unwrap());
        let _lease = root.try_acquire(SID).unwrap().expect("lease free");
        std::fs::write(marker, b"held").unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Helper process: a long-lived child that holds no lease.
    #[test]
    #[ignore]
    fn sleep_helper() {
        let marker = PathBuf::from(std::env::var_os("QMT_LEASE_MARKER").unwrap());
        std::fs::write(marker, b"up").unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn acquire_busy_and_release_on_drop() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);

        let lease = root.try_acquire(SID).unwrap().expect("first acquire");
        // Same-process exclusivity (design D4).
        assert!(root.try_acquire(SID).unwrap().is_none());
        // Other sessions are independent.
        assert!(
            root.try_acquire("11111111-1111-1111-1111-111111111111")
                .unwrap()
                .is_some()
        );

        drop(lease);
        assert!(root.try_acquire(SID).unwrap().is_some());
    }

    #[test]
    fn invalid_session_ids_are_rejected() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        for bad in ["", "..", "a/b", "a\\b", "C:evil", "a.lock", &"x".repeat(65)] {
            let err = root.try_acquire(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn grace_acquire_waits_out_a_momentary_holder() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        let lease = root.try_acquire(SID).unwrap().unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            drop(lease);
        });
        let acquired = root
            .acquire_with_grace(SID, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(acquired.is_some(), "lease freed during grace was not taken");
        release.await.unwrap();

        // A holder that outlasts the grace period means busy.
        let _held = acquired;
        assert!(
            root.acquire_with_grace(SID, Duration::from_millis(50))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn leftover_lease_file_does_not_block() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        std::fs::create_dir_all(root.path()).unwrap();
        std::fs::write(root.lease_path(SID).unwrap(), b"").unwrap();

        assert!(root.try_acquire(SID).unwrap().is_some());
        // The file survives the lease.
        assert!(root.lease_path(SID).unwrap().exists());
    }

    #[test]
    fn remove_if_free_respects_holder() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        let lease = root.try_acquire(SID).unwrap().unwrap();

        root.remove_if_free(SID);
        assert!(root.lease_path(SID).unwrap().exists());

        drop(lease);
        root.remove_if_free(SID);
        assert!(!root.lease_path(SID).unwrap().exists());
    }

    #[test]
    fn lease_root_follows_database_path() {
        assert_eq!(
            run_lease_root_for_db(Path::new("/home/u/.qmt/sessions.db")),
            Some(PathBuf::from("/home/u/.qmt/sessions.run-leases"))
        );
        assert_eq!(run_lease_root_for_db(Path::new(":memory:")), None);
    }

    #[cfg(windows)]
    #[test]
    fn lease_root_follows_windows_database_path() {
        assert_eq!(
            run_lease_root_for_db(Path::new(r"C:\Users\x\.qmt\sessions.db")),
            Some(PathBuf::from(r"C:\Users\x\.qmt\sessions.run-leases"))
        );
    }

    #[test]
    fn lease_is_released_when_holder_process_is_killed() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        let marker = dir.path().join("held.marker");

        let mut child = spawn_helper(
            "hold_lease_helper",
            &[
                ("QMT_LEASE_ROOT", root.path()),
                ("QMT_LEASE_MARKER", &marker),
            ],
        );
        wait_for_marker(&marker);
        assert!(
            root.try_acquire(SID).unwrap().is_none(),
            "child holds lease"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        // Windows may release a terminated process's locks asynchronously.
        assert!(
            acquire_within(&root, Duration::from_secs(5)),
            "lease not released after holder was killed"
        );
    }

    #[test]
    fn lease_handle_is_not_inherited_by_child_processes() {
        let dir = TempDir::new().unwrap();
        let root = root(&dir);
        let marker = dir.path().join("sleep.marker");

        let lease = root.try_acquire(SID).unwrap().unwrap();
        let mut child = spawn_helper("sleep_helper", &[("QMT_LEASE_MARKER", &marker)]);
        wait_for_marker(&marker);

        drop(lease);
        let acquired = root.try_acquire(SID).unwrap().is_some();
        let still_running = child.try_wait().unwrap().is_none();

        child.kill().unwrap();
        child.wait().unwrap();

        assert!(still_running, "child exited early");
        assert!(acquired, "child process kept the lease alive");
    }
}
