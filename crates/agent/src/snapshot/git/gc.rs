//! Opt-in garbage collection and store-size reporting for the git snapshot
//! backend.
//!
//! With neither retention limit set nothing is ever deleted. With a limit
//! set, snapshots beyond the window are pruned and, once enough of them are
//! pruned, the content only they referenced is deleted from disk.

use super::object_id;
use super::{GitSnapshotBackend, Limits, SnapshotLock};
use crate::snapshot::backend::{
    GcConfig, GcResult, SnapshotError, SnapshotId, SnapshotResult, io_err,
};
use gix::hashtable::HashSet;
use std::collections::HashMap;
use std::fs;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Content is only deleted once at least this many snapshots were pruned, so
/// the full object walk is amortised.
const SWEEP_THRESHOLD: usize = 10;

const SECS_PER_DAY: u64 = 86_400;

impl GitSnapshotBackend {
    /// List all commits with timestamps by walking first parents from HEAD.
    /// The walk ends at the first missing commit, i.e. at the last sweep.
    pub(super) fn list_commits(repo: &gix::Repository) -> SnapshotResult<Vec<(SnapshotId, i64)>> {
        let Ok(head) = repo.head_commit() else {
            return Ok(Vec::new()); // Empty repo
        };

        let mut commits = Vec::new();
        let mut current = Some(head.id().detach());

        while let Some(oid) = current {
            match repo.find_commit(oid) {
                Ok(commit) => {
                    let timestamp = commit.time().map_or(0, |t| t.seconds);
                    commits.push((SnapshotId::from(oid), timestamp));
                    current = commit.parent_ids().next().map(gix::Id::detach);
                }
                Err(e) => {
                    // The first pruned commit ends the chain after a sweep;
                    // anything else is worth a trace.
                    tracing::debug!(commit = %oid, error = %e, "snapshot history ends here");
                    break;
                }
            }
        }

        Ok(commits)
    }
}

/// Prune snapshots outside the retention window and, when enough were pruned
/// and no snapshot or restore is running, delete the content only they used.
///
/// `now` is injected so tests can age snapshots without sleeping.
pub(super) fn collect_garbage(
    repo: &gix::Repository,
    cfg: &GcConfig,
    now: SystemTime,
) -> SnapshotResult<GcResult> {
    collect_garbage_with(repo, cfg, now, || {})
}

/// [`collect_garbage`] with a hook that runs after the unlocked estimate and
/// before the exclusive lock is requested, so tests can land a snapshot in
/// exactly that window.
pub(super) fn collect_garbage_with(
    repo: &gix::Repository,
    cfg: &GcConfig,
    now: SystemTime,
    before_lock: impl FnOnce(),
) -> SnapshotResult<GcResult> {
    let commits = GitSnapshotBackend::list_commits(repo)?;
    if !cfg.is_enabled() {
        return Ok(GcResult {
            removed_count: 0,
            remaining_count: commits.len(),
            deleted_objects: 0,
        });
    }

    // Unlocked estimate: decides whether a sweep is worth the lock at all.
    let estimate = estimate_result(&commits, cfg, now);
    if estimate.removed_count < SWEEP_THRESHOLD {
        return Ok(estimate);
    }
    before_lock();
    let Some(_exclusive) = SnapshotLock::try_exclusive(repo.git_dir())? else {
        tracing::debug!("snapshot sweep skipped: a snapshot or restore is running");
        return Ok(estimate);
    };

    // A snapshot may have been committed between the estimate and the lock.
    // Decide what to keep from a listing taken under the exclusive lock, so
    // the sweep can never treat a freshly committed snapshot as garbage.
    let commits = GitSnapshotBackend::list_commits(repo)?;
    let keep = retained_prefix(&commits, cfg, now);
    let (kept, pruned) = commits.split_at(keep);
    let mut result = GcResult {
        removed_count: pruned.len(),
        remaining_count: kept.len(),
        deleted_objects: 0,
    };
    if pruned.len() >= SWEEP_THRESHOLD {
        result.deleted_objects = sweep(repo, kept, pruned)?;
    }
    Ok(result)
}

fn estimate_result(commits: &[(SnapshotId, i64)], cfg: &GcConfig, now: SystemTime) -> GcResult {
    let keep = retained_prefix(commits, cfg, now);
    GcResult {
        removed_count: commits.len() - keep,
        remaining_count: keep,
        deleted_objects: 0,
    }
}

/// How many of the newest `commits` are kept: HEAD always, then every commit
/// newer than the age cutoff, up to the count limit.
fn retained_prefix(commits: &[(SnapshotId, i64)], cfg: &GcConfig, now: SystemTime) -> usize {
    if commits.is_empty() {
        return 0;
    }
    let cutoff = cfg.max_age_days.map(|days| cutoff_secs(now, days));
    let within_age = commits
        .iter()
        .skip(1)
        .take_while(|(_, ts)| cutoff.is_none_or(|c| *ts >= c))
        .count();
    let limit = cfg.max_snapshots.map_or(usize::MAX, NonZeroUsize::get);
    (1 + within_age).min(limit)
}

fn cutoff_secs(now: SystemTime, max_age_days: NonZeroU64) -> i64 {
    let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let window = max_age_days.get().saturating_mul(SECS_PER_DAY);
    i64::try_from(now_secs.saturating_sub(window)).unwrap_or(i64::MAX)
}

/// Delete every loose object that no kept snapshot references. Parent links
/// are ignored, so pruned commits become unreachable without rewriting any
/// kept commit id. Pruned commits are deleted first, so an interrupted sweep
/// never leaves a reachable commit with missing blobs.
fn sweep(
    repo: &gix::Repository,
    kept: &[(SnapshotId, i64)],
    pruned: &[(SnapshotId, i64)],
) -> SnapshotResult<usize> {
    let mut reachable: HashSet<gix::ObjectId> = HashSet::default();
    for (id, _) in kept {
        let commit_id = object_id(id)?;
        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| SnapshotError::NotFound(format!("commit {id}: {e}")))?;
        let tree_id = commit
            .tree_id()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {e}")))?
            .detach();
        reachable.insert(commit_id);
        mark_tree(repo, tree_id, &mut reachable)?;
    }

    let objects = repo.git_dir().join("objects");
    let mut deleted = 0;
    for (id, _) in pruned {
        let oid = object_id(id)?;
        if !reachable.contains(&oid) {
            deleted += remove_loose(&objects, &oid)?;
        }
    }

    let store = gix::odb::loose::Store::at(&objects, repo.object_hash());
    let mut garbage = Vec::new();
    for oid in store.iter() {
        let oid = oid
            .map_err(|e| SnapshotError::Repository(format!("Failed to list loose objects: {e}")))?;
        if !reachable.contains(&oid) {
            garbage.push(oid);
        }
    }
    for oid in garbage {
        deleted += remove_loose(&objects, &oid)?;
    }
    Ok(deleted)
}

/// Add `tree_id` and everything below it to `reachable` without loading blobs.
fn mark_tree(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    reachable: &mut HashSet<gix::ObjectId>,
) -> SnapshotResult<()> {
    if !reachable.insert(tree_id) {
        return Ok(()); // already walked through another kept snapshot
    }
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| SnapshotError::NotFound(format!("tree {tree_id}: {e}")))?;
    for entry in tree.iter() {
        let entry = entry
            .map_err(|e| SnapshotError::Repository(format!("Failed to read tree entry: {e}")))?;
        let oid = entry.oid().to_owned();
        if entry.mode().is_tree() {
            mark_tree(repo, oid, reachable)?;
        } else {
            reachable.insert(oid);
        }
    }
    Ok(())
}

/// Delete one loose object. `Ok(0)` when it is not stored loose.
fn remove_loose(objects: &Path, oid: &gix::oid) -> SnapshotResult<usize> {
    let hex = oid.to_hex().to_string();
    let path = objects.join(&hex[..2]).join(&hex[2..]);
    match fs::remove_file(&path) {
        Ok(()) => Ok(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(io_err("delete", &path)(e)),
    }
}

// ==================== store-size gauge ====================

/// Per-store bookkeeping for the "store is large" warning.
#[derive(Debug, Default)]
struct StoreGauge {
    measured: bool,
    pending_bytes: u64,
    warned: bool,
}

/// Keyed by snapshot git dir. Only locked for short, non-blocking updates,
/// never across an `.await` (the callers run inside `spawn_blocking`).
static STORE_GAUGES: LazyLock<Mutex<HashMap<PathBuf, StoreGauge>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record `captured_bytes` of newly read content and, when it is time,
/// measure the store and warn once if it reached `limits.store_warn_bytes`.
///
/// Content that was read is never smaller than what it adds to disk, so the
/// warning comes at most `store_measure_every_bytes` of content late and never
/// early: only real measurements trigger it.
pub(super) fn note_captured(git_dir: &Path, captured_bytes: u64, limits: &Limits) {
    let should_measure = {
        let mut gauges = STORE_GAUGES.lock().unwrap_or_else(PoisonError::into_inner);
        let gauge = gauges.entry(git_dir.to_path_buf()).or_default();
        if gauge.warned {
            return;
        }
        gauge.pending_bytes = gauge.pending_bytes.saturating_add(captured_bytes);
        !gauge.measured || gauge.pending_bytes >= limits.store_measure_every_bytes
    };
    if !should_measure {
        return;
    }

    let size_bytes = dir_size(&git_dir.join("objects"));
    let warn = {
        let mut gauges = STORE_GAUGES.lock().unwrap_or_else(PoisonError::into_inner);
        let gauge = gauges.entry(git_dir.to_path_buf()).or_default();
        gauge.measured = true;
        gauge.pending_bytes = 0;
        let crossed = size_bytes >= limits.store_warn_bytes && !gauge.warned;
        gauge.warned |= crossed;
        crossed
    };
    if warn {
        tracing::warn!(
            store = %git_dir.display(),
            size_bytes,
            "snapshot store is large; add large paths to .ignore or set max_snapshots/max_age_days"
        );
    }
}

/// Total size of the files below `dir`. Unreadable entries are skipped: the
/// result only feeds an advisory warning.
pub(super) fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "cannot measure snapshot store");
                continue;
            }
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => pending.push(entry.path()),
                Ok(meta) => total = total.saturating_add(meta.len()),
                Err(_) => {}
            }
        }
    }
    total
}
