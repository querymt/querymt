//! Checkout-scoped cooperative mutation gates.
//!
//! A gate serializes *cooperating QueryMT writers* on one checkout: built-in
//! mutating tools, undo/redo restoration, and baseline capture. Sessions
//! hold a gate only around the mutating operation itself — never while
//! waiting on a model, a user, or a delegate — so a capture can observe a
//! quiescent checkout without stalling execution.
//!
//! Coordination is a real OS-level file lock (std `File::try_lock`) under
//! the durable
//! worktree storage root, keyed by the *canonical* checkout root, so it
//! excludes both concurrent tasks and other QueryMT processes operating on
//! the same checkout. Sessions without a gate configured run unchanged.
//!
//! Opaque operations (shell commands, MCP tools) are deliberately **not**
//! gated: their filesystem writes are invisible to QueryMT and cannot be
//! fenced. Baseline capture validates observed content (HEAD/index/manifest)
//! around the gated window instead, retrying or failing when external
//! writers race the capture.

use crate::worktree::allocate::repo_slug;
use crate::worktree::error::WorktreeResult;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Cooperative mutation gate bound to one canonical checkout root.
///
/// Clone-free sharing via `Arc`; every [`run`](Self::run) invocation
/// acquires the checkout's lock for the duration of the given future and
/// releases it afterwards. The gate is never held across anything the
/// caller awaits outside `run`.
#[derive(Debug, Clone)]
pub struct CheckoutMutationGate {
    storage_root: PathBuf,
    /// Canonical checkout root this gate serializes writes for.
    checkout_root: PathBuf,
}

impl CheckoutMutationGate {
    /// Build a gate for `checkout_root`, canonicalizing the path so linked
    /// worktrees, subdirectory cwds, and symlinked paths map to one key.
    pub fn new(
        storage_root: impl Into<PathBuf>,
        checkout_root: impl AsRef<Path>,
    ) -> WorktreeResult<Self> {
        Ok(Self {
            storage_root: storage_root.into(),
            checkout_root: std::fs::canonicalize(checkout_root.as_ref())?,
        })
    }

    /// Canonical checkout root the gate serializes.
    pub fn checkout_root(&self) -> &Path {
        &self.checkout_root
    }

    /// Lock file path for this checkout under the durable storage root.
    pub fn gate_path(&self) -> PathBuf {
        self.storage_root
            .join("gates")
            .join(format!("{}.lock", repo_slug(&self.checkout_root)))
    }

    /// Run `fut` while holding this checkout's mutation gate.
    ///
    /// Acquisition waits (bounded, like repository administration locking)
    /// for other cooperating writers; the lock is released as soon as `fut`
    /// completes — the gate is never held across an await outside `fut`.
    pub async fn run<F>(&self, fut: F) -> WorktreeResult<F::Output>
    where
        F: Future,
    {
        let path = self.gate_path();
        crate::worktree::lock::with_lock_file(&path, async { Ok(fut.await) }).await
    }
}

/// Activity leases last for the actor runtime, unlike short mutation gates.
/// Shared leases permit explicit shared delegates; archival/removal requires
/// an exclusive lease, including against actors in other processes.
#[derive(Debug)]
pub(crate) struct CheckoutActivityLease {
    _file: std::fs::File,
}

impl Drop for CheckoutActivityLease {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl CheckoutActivityLease {
    pub(crate) fn acquire(
        storage: &Path,
        checkout: &Path,
        exclusive: bool,
    ) -> WorktreeResult<Self> {
        let checkout = std::fs::canonicalize(checkout).unwrap_or_else(|_| {
            match (checkout.parent(), checkout.file_name()) {
                (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                    .map(|parent| parent.join(name))
                    .unwrap_or_else(|_| checkout.to_path_buf()),
                _ => checkout.to_path_buf(),
            }
        });
        let path = storage
            .join("activity")
            .join(format!("{}.lock", repo_slug(&checkout)));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        result.map_err(|err| crate::worktree::WorktreeError::InvalidOperation(format!("checkout {} has active execution or administration: {err}; close its sessions first", checkout.display())))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn activity_lease_subprocess_helper() {
        let Ok(storage) = std::env::var("QMT_ACTIVITY_TEST_STORAGE") else {
            return;
        };
        let checkout = std::env::var("QMT_ACTIVITY_TEST_CHECKOUT").unwrap();
        let lease =
            CheckoutActivityLease::acquire(Path::new(&storage), Path::new(&checkout), false)
                .unwrap();
        std::fs::write(Path::new(&storage).join("held"), b"held").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !Path::new(&storage).join("release").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(lease);
    }

    #[tokio::test]
    async fn checkout_activity_blocks_archive_across_processes() {
        let storage = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worktree::gate::tests::activity_lease_subprocess_helper",
            ])
            .env("QMT_ACTIVITY_TEST_STORAGE", storage.path())
            .env("QMT_ACTIVITY_TEST_CHECKOUT", checkout.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !storage.path().join("held").exists() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(CheckoutActivityLease::acquire(storage.path(), checkout.path(), true).is_err());
        std::fs::write(storage.path().join("release"), b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(CheckoutActivityLease::acquire(storage.path(), checkout.path(), true).is_ok());
    }

    fn gate_for(storage: &Path, checkout: &Path) -> CheckoutMutationGate {
        CheckoutMutationGate::new(storage, checkout).unwrap()
    }

    #[tokio::test]
    async fn gate_serializes_concurrent_cooperating_writers() {
        let storage = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        let gate = Arc::new(gate_for(storage.path(), checkout.path()));

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            handles.push(tokio::spawn(async move {
                gate.run(async {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
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
            "cooperating writers must never overlap"
        );
    }

    #[tokio::test]
    async fn concurrent_capture_and_mutation_cannot_deadlock() {
        let storage = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        let gate = Arc::new(gate_for(storage.path(), checkout.path()));

        // "Capture" holds the gate briefly; a concurrent "mutation" waits.
        // Both must finish well within the test timeout.
        let capture = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.run(async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    "captured"
                })
                .await
                .unwrap()
            })
        };
        let mutation = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.run(async { "mutated" }).await.unwrap() })
        };

        let (capture, mutation) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(capture, mutation)
        })
        .await
        .expect("capture/mutation must not deadlock");
        assert_eq!(capture.unwrap(), "captured");
        assert_eq!(mutation.unwrap(), "mutated");
    }

    #[tokio::test]
    async fn gate_is_not_held_for_waiters_outside_the_gate() {
        let storage = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        let gate = Arc::new(gate_for(storage.path(), checkout.path()));

        // A "delegate" simulated wait runs outside any gate scope; it must
        // finish while the gate is still held by the "capture".
        let capture = {
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.run(async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                })
                .await
                .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::time::timeout(Duration::from_millis(50), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        })
        .await
        .expect("delegate waiting must not be blocked by a held gate");
        tokio::time::timeout(Duration::from_secs(2), capture)
            .await
            .expect("capture releases the gate")
            .unwrap();

        // After release, another writer proceeds immediately.
        tokio::time::timeout(Duration::from_secs(2), async {
            gate.run(async {}).await.unwrap();
        })
        .await
        .expect("gate is free after release");
    }

    #[test]
    fn gate_keys_are_canonical_and_namespaced_per_checkout() {
        let storage = TempDir::new().unwrap();
        let checkout = TempDir::new().unwrap();
        let a = gate_for(storage.path(), checkout.path());
        // The same checkout through a redundant nested spelling maps to the
        // same gate (`<checkout>/sub/..` canonicalizes to `<checkout>`).
        let nested = checkout.path().join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let weird = nested.join("..");
        let b = CheckoutMutationGate::new(storage.path(), &weird).unwrap();
        assert_eq!(a.checkout_root(), b.checkout_root());
        assert_eq!(a.gate_path(), b.gate_path());
        assert!(a.gate_path().starts_with(storage.path()));
        assert!(
            a.gate_path().to_string_lossy().ends_with(".lock"),
            "gate path: {}",
            a.gate_path().display()
        );
    }

    #[test]
    fn missing_checkout_root_is_rejected() {
        let storage = TempDir::new().unwrap();
        let err = CheckoutMutationGate::new(storage.path(), "/definitely/missing/path");
        assert!(err.is_err(), "canonicalization must fail for absent roots");
    }
}
