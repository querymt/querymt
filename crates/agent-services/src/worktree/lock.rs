//! Cross-process repository-administration locking.
//!
//! All mutation of a project's managed worktrees (provisioning, recovery,
//! removal) serializes on a per-repository advisory file lock so concurrent
//! processes cannot interleave Git administration steps. The lock lives in
//! the durable worktree storage root — never inside the user's `.git` —
//! and is a real OS-level file lock (std `File::try_lock`), so exclusion
//! holds across processes, not just tasks.
//!
//! Acquisition polls `try_lock` with a bounded deadline; the held lock is
//! released when the file handle drops after `fut` completes.

use crate::worktree::allocate::repo_slug;
use crate::worktree::error::{WorktreeError, WorktreeResult};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::Instrument;

/// How long administration may wait for the per-repository lock.
const ADMIN_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll interval while waiting for the lock.
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Lock file path for a repository under `storage_root`.
pub fn admin_lock_path(storage_root: &Path, common_dir: &Path) -> PathBuf {
    storage_root.join(repo_slug(common_dir)).join("admin.lock")
}

/// Run `f` while holding the repository administration lock.
///
/// `storage_root` + `common_dir` select the lock file; the lock is held
/// across the entire future, so no other process (or task using this API)
/// can interleave worktree administration for the same project.
pub async fn with_admin_lock<T, Fut>(
    storage_root: &Path,
    common_dir: &Path,
    fut: Fut,
) -> WorktreeResult<T>
where
    Fut: Future<Output = WorktreeResult<T>>,
{
    let path = admin_lock_path(storage_root, common_dir);
    with_lock_file(&path, "admin", None, fut).await
}

/// Low-level form of [`with_admin_lock`] operating on an explicit lock
/// file path. Exposed for tests that must coordinate across processes.
///
/// Acquisition is traced as `worktree.lock.wait`: `lock` names the kind
/// (`batch`, `admin` or `gate`), `checkout` is the gate's canonical checkout
/// root (gates only) and `polls` counts the `try_lock` attempts that found
/// the lock held. The span ends when the lock is acquired, so it never
/// covers `fut`.
pub(crate) async fn with_lock_file<T, Fut>(
    path: &Path,
    lock: &'static str,
    checkout: Option<&Path>,
    fut: Fut,
) -> WorktreeResult<T>
where
    Fut: Future<Output = WorktreeResult<T>>,
{
    let span = tracing::info_span!(
        "worktree.lock.wait",
        lock,
        checkout = tracing::field::Empty,
        polls = tracing::field::Empty,
    );
    if let Some(checkout) = checkout {
        span.record("checkout", checkout.display().to_string());
    }
    let file = acquire_lock_file(path, span.clone())
        .instrument(span)
        .await?;
    // The lock is released when `file` drops at the end of this scope.
    let result = fut.await;
    drop(file);
    result
}

/// Open the lock file off the async workers, then poll the OS-level
/// exclusive lock with a bounded deadline. `try_lock` never blocks, so it
/// and the poll sleeps stay on the async task.
async fn acquire_lock_file(path: &Path, span: tracing::Span) -> WorktreeResult<std::fs::File> {
    let open_path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        if let Some(parent) = open_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&open_path)
    })
    .await
    .map_err(|err| {
        WorktreeError::InvalidOperation(format!(
            "failed to open lock file {}: {err}",
            path.display()
        ))
    })??;

    let deadline = Instant::now() + ADMIN_LOCK_TIMEOUT;
    let mut polls: i64 = 0;
    loop {
        match file.try_lock() {
            Ok(()) => {
                span.record("polls", polls);
                return Ok(file);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                polls += 1;
                if Instant::now() >= deadline {
                    span.record("polls", polls);
                    return Err(WorktreeError::InvalidOperation(format!(
                        "timed out waiting for repository administration lock at {}",
                        path.display()
                    )));
                }
                tokio::time::sleep(LOCK_POLL).await;
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(WorktreeError::InvalidOperation(format!(
                    "failed to lock {}: {err}",
                    path.display()
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[tokio::test]
    async fn admin_lock_serializes_concurrent_holders() {
        let storage = TempDir::new().unwrap();
        let common = Path::new("/some/project/.git");

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let storage_root = storage.path().to_path_buf();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let active = active.clone();
            let max_active = max_active.clone();
            let storage_root = storage_root.clone();
            handles.push(tokio::spawn(async move {
                with_admin_lock(&storage_root, common, async {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "lock holders must never overlap"
        );
        assert!(admin_lock_path(storage.path(), common).exists());
    }

    #[tokio::test]
    async fn admin_lock_path_namespaces_by_repository() {
        let storage = Path::new("/data/worktrees");
        let a = admin_lock_path(storage, Path::new("/repo-a/.git"));
        let b = admin_lock_path(storage, Path::new("/repo-b/.git"));
        assert_ne!(a, b);
        assert!(a.starts_with(storage));
        assert!(a.ends_with("admin.lock"));
    }

    /// Subprocess helper for the cross-process test below: holds the lock
    /// file named by `QMT_ADMIN_LOCK_PATH` briefly, marking a sentinel file
    /// once the lock is held.
    #[tokio::test]
    #[ignore]
    async fn hold_admin_lock_helper() {
        let path = PathBuf::from(std::env::var("QMT_ADMIN_LOCK_PATH").unwrap());
        let marker = PathBuf::from(std::env::var("QMT_ADMIN_LOCK_MARKER").unwrap());
        with_lock_file(&path, "admin", None, async {
            std::fs::write(&marker, b"held").unwrap();
            tokio::time::sleep(Duration::from_millis(900)).await;
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn admin_lock_blocks_across_processes() {
        let storage = TempDir::new().unwrap();
        let common_dir = TempDir::new().unwrap();
        let path = admin_lock_path(storage.path(), common_dir.path());
        let marker = storage.path().join("held.marker");
        let _ = std::fs::remove_file(&marker);

        // Spawn this test binary as a second process; it acquires the same
        // lock file through the OS and holds it for a while.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "worktree::lock::tests::hold_admin_lock_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("QMT_ADMIN_LOCK_PATH", &path)
            .env("QMT_ADMIN_LOCK_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Wait (bounded) until the helper reports holding the lock.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "helper never acquired the lock");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Our acquisition must block until the helper exits and releases.
        let start = Instant::now();
        with_lock_file(&path, "admin", None, async { Ok(()) })
            .await
            .unwrap();
        let elapsed = start.elapsed();

        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(
            elapsed >= Duration::from_millis(400),
            "lock acquisition must wait for the other process; took {elapsed:?}"
        );
    }
}
