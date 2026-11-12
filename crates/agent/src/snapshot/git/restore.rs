//! Restore side of the git snapshot backend: diff and checkout.
//!
//! Every file is written to a temporary file in its target directory and
//! renamed into place, so a reader sees either the old or the restored
//! content. Every write, create and delete error is propagated.

use super::GitSnapshotBackend;
use super::confined::Root;
use crate::snapshot::backend::{SnapshotError, SnapshotResult};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// What a restore did besides writing small files from memory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RestoreStats {
    /// Blobs streamed from disk to disk without being held in memory.
    pub(crate) streamed_files: usize,
}

/// Where the bytes of a blob come from.
enum BlobSource<'a> {
    /// Already in memory (small or packed objects).
    Bytes(&'a [u8]),
    /// A loose, zlib-wrapped object file that is streamed.
    Loose {
        path: PathBuf,
        id: gix::ObjectId,
        size: u64,
    },
}

fn parse_commit_id(sha: &str) -> SnapshotResult<gix::ObjectId> {
    gix::ObjectId::from_hex(sha.as_bytes())
        .map_err(|_| SnapshotError::InvalidSnapshotId(sha.to_string()))
}

pub(super) fn commit_tree<'r>(
    repo: &'r gix::Repository,
    sha: &str,
) -> SnapshotResult<gix::Tree<'r>> {
    let commit = repo
        .find_commit(parse_commit_id(sha)?)
        .map_err(|e| SnapshotError::NotFound(format!("commit {sha}: {e}")))?;
    commit
        .tree()
        .map_err(|e| SnapshotError::Repository(format!("Failed to get tree: {e}")))
}

fn is_executable(mode: gix::objs::tree::EntryMode) -> bool {
    mode.kind() == gix::objs::tree::EntryKind::BlobExecutable
}

/// Permission bits for a restored file (git's rule): keep the current mode of
/// an existing regular file and let the executable bits follow the snapshot;
/// a new file gets 0644 or 0755.
#[cfg(unix)]
pub(super) fn restored_mode(existing: Option<u32>, executable: bool) -> u32 {
    match existing {
        Some(mode) => {
            // Never carry setuid/setgid/sticky over: git does not either.
            let base = mode & 0o666;
            if executable {
                // Execute wherever read is allowed.
                base | ((mode & 0o444) >> 2)
            } else {
                base
            }
        }
        None if executable => 0o755,
        None => 0o644,
    }
}

impl GitSnapshotBackend {
    /// Paths that differ between two snapshots. A move is reported as both its
    /// old and its new path (no rename detection), sorted and de-duplicated.
    pub(super) fn diff_commits(
        repo: &gix::Repository,
        from: &str,
        to: &str,
    ) -> SnapshotResult<Vec<PathBuf>> {
        let from_id = parse_commit_id(from)?;
        let to_id = parse_commit_id(to)?;

        let from_commit = repo
            .find_commit(from_id)
            .map_err(|e| SnapshotError::NotFound(format!("'from' commit {from}: {e}")))?;
        let to_commit = repo
            .find_commit(to_id)
            .map_err(|e| SnapshotError::NotFound(format!("'to' commit {to}: {e}")))?;

        let from_tree = from_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'from' tree: {e}")))?;
        let to_tree = to_commit
            .tree()
            .map_err(|e| SnapshotError::Repository(format!("Failed to get 'to' tree: {e}")))?;

        // Explicit default options: `None` would take rewrite tracking from the
        // git configuration, which hides one side of a move and reads blobs.
        let changes = repo
            .diff_tree_to_tree(
                Some(&from_tree),
                Some(&to_tree),
                Some(gix::diff::Options::default()),
            )
            .map_err(|e| SnapshotError::Repository(format!("Failed to compute tree diff: {e}")))?;

        let mut changed_files = BTreeSet::new();
        for change in &changes {
            // Skip tree (directory) entries: gix emits a tree modification for
            // every ancestor directory whose hash changed, and restoring such a
            // path would try to write a tree object over a directory.
            if change.entry_mode().is_tree() {
                continue;
            }
            match std::str::from_utf8(change.location().as_ref()) {
                Ok(path) => {
                    changed_files.insert(PathBuf::from(path));
                }
                Err(_) => tracing::debug!(
                    path = %change.location(),
                    "snapshot diff skips a path that is not valid UTF-8"
                ),
            }
        }
        Ok(changed_files.into_iter().collect())
    }

    /// Restore `paths` from a commit: paths the snapshot has are rewritten,
    /// paths it lacks are deleted.
    pub(super) fn checkout_paths(
        &self,
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
        paths: &[PathBuf],
    ) -> SnapshotResult<RestoreStats> {
        let tree = commit_tree(repo, commit_sha)?;
        let root = Root::open(worktree)?;
        let mut stats = RestoreStats::default();

        for path in paths {
            // Rejects `..`, absolute and empty paths before any lookup.
            super::confined::validate(path)?;
            let entry = tree.lookup_entry_by_path(path).map_err(|e| {
                SnapshotError::Repository(format!("Failed to look up tree entry: {e}"))
            })?;

            match entry {
                Some(entry) => {
                    // Defense-in-depth: `diff_commits` filters directories out,
                    // but callers may pass one directly.
                    if entry.mode().is_tree() {
                        continue;
                    }
                    self.restore_blob(
                        repo,
                        &root,
                        path,
                        entry.object_id(),
                        is_executable(entry.mode()),
                        &mut stats,
                    )?;
                }
                None => root.remove_file(path)?,
            }
        }
        Ok(stats)
    }

    /// Restore the entire snapshot tree into the worktree. Files created after
    /// the snapshot are left alone.
    pub(super) fn checkout_all(
        &self,
        repo: &gix::Repository,
        worktree: &Path,
        commit_sha: &str,
    ) -> SnapshotResult<RestoreStats> {
        let tree = commit_tree(repo, commit_sha)?;
        let root = Root::open(worktree)?;
        let mut stats = RestoreStats::default();
        self.restore_tree_recursive(repo, &root, &tree, Path::new(""), &mut stats)?;
        Ok(stats)
    }

    fn restore_tree_recursive(
        &self,
        repo: &gix::Repository,
        root: &Root,
        tree: &gix::Tree<'_>,
        prefix: &Path,
        stats: &mut RestoreStats,
    ) -> SnapshotResult<()> {
        for entry in tree.iter() {
            let entry = entry.map_err(|e| {
                SnapshotError::Repository(format!("Failed to read tree entry: {e}"))
            })?;
            let name = std::str::from_utf8(entry.filename().as_ref()).map_err(|e| {
                SnapshotError::Repository(format!("Invalid UTF-8 in filename: {e}"))
            })?;
            let rel = prefix.join(name);

            if entry.mode().is_tree() {
                let sub_tree = entry
                    .object()
                    .map_err(|e| {
                        SnapshotError::Repository(format!("Failed to find subtree object: {e}"))
                    })?
                    .try_into_tree()
                    .map_err(|_| SnapshotError::Repository("Expected tree object".to_string()))?;
                self.restore_tree_recursive(repo, root, &sub_tree, &rel, stats)?;
            } else if entry.mode().is_blob() || entry.mode().is_blob_or_symlink() {
                self.restore_blob(
                    repo,
                    root,
                    &rel,
                    entry.object_id(),
                    is_executable(entry.mode()),
                    stats,
                )?;
            }
        }
        Ok(())
    }

    /// Write blob `id` to `worktree/rel`. Blobs of at least `stream_bytes`
    /// that are stored loose are streamed; everything else is borrowed from
    /// the object database.
    fn restore_blob(
        &self,
        repo: &gix::Repository,
        root: &Root,
        rel: &Path,
        id: gix::ObjectId,
        executable: bool,
        stats: &mut RestoreStats,
    ) -> SnapshotResult<()> {
        let target = root.path().join(rel);

        let size = repo
            .find_header(id)
            .map_err(|e| SnapshotError::Repository(format!("Failed to read blob header: {e}")))?
            .size();
        if size >= self.limits.stream_bytes {
            let loose = loose_object_path(repo, &id);
            if loose.is_file() {
                write_entry(
                    root,
                    rel,
                    &target,
                    BlobSource::Loose {
                        path: loose,
                        id,
                        size,
                    },
                    executable,
                )?;
                stats.streamed_files += 1;
                return Ok(());
            }
        }

        let object = repo
            .find_object(id)
            .map_err(|e| SnapshotError::Repository(format!("Failed to get blob object: {e}")))?;
        write_entry(
            root,
            rel,
            &target,
            BlobSource::Bytes(&object.data),
            executable,
        )
    }
}

fn loose_object_path(repo: &gix::Repository, id: &gix::oid) -> PathBuf {
    let hex = id.to_hex().to_string();
    repo.git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

/// Atomically replace `rel` below the worktree root with the blob. The write
/// goes through [`Root`], which never follows a symlink out of the worktree and
/// renames a temporary file into place, removing it on every failure.
fn write_entry(
    root: &Root,
    rel: &Path,
    target: &Path,
    source: BlobSource<'_>,
    executable: bool,
) -> SnapshotResult<()> {
    root.replace_file(rel, executable, |file| match source {
        BlobSource::Bytes(bytes) => file.write_all(bytes).map_err(|e| SnapshotError::Io {
            op: "write",
            path: target.to_path_buf(),
            source: e,
        }),
        BlobSource::Loose { path, id, size } => {
            stream_loose(&path, id, size, file).map_err(|e| match e {
                StreamError::Io(source) => SnapshotError::Io {
                    op: "write",
                    path: target.to_path_buf(),
                    source,
                },
                StreamError::Corrupt(why) => SnapshotError::Repository(format!(
                    "object {id} for {} is corrupt: {why}",
                    target.display()
                )),
            })
        }
    })
}

enum StreamError {
    Io(std::io::Error),
    Corrupt(String),
}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Inflate a loose blob object into `out` without holding it in memory,
/// checking the header and the content hash. The caller only persists `out`
/// after this returns `Ok`.
fn stream_loose(
    path: &Path,
    id: gix::ObjectId,
    expected_size: u64,
    out: &mut fs::File,
) -> Result<(), StreamError> {
    let file = fs::File::open(path)?;
    let mut inflate = flate2::bufread::ZlibDecoder::new(BufReader::new(file));

    // `blob <size>\0` is at most a few dozen bytes.
    let mut header = Vec::with_capacity(32);
    let mut byte = [0u8; 1];
    loop {
        inflate.read_exact(&mut byte)?;
        header.push(byte[0]);
        if byte[0] == 0 {
            break;
        }
        if header.len() > 64 {
            return Err(StreamError::Corrupt("object header too long".into()));
        }
    }
    let (kind, size, _) = gix::objs::decode::loose_header(&header)
        .map_err(|e| StreamError::Corrupt(format!("bad object header: {e}")))?;
    if kind != gix::objs::Kind::Blob || size != expected_size {
        return Err(StreamError::Corrupt(format!(
            "expected a blob of {expected_size} bytes, found {kind} of {size}"
        )));
    }

    let mut hasher = gix::hash::hasher(id.kind());
    hasher.update(&header);
    let mut buf = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    loop {
        let n = inflate.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        written += n as u64;
    }
    if written != size {
        return Err(StreamError::Corrupt(format!(
            "expected {size} bytes, read {written}"
        )));
    }
    let actual = hasher
        .try_finalize()
        .map_err(|e| StreamError::Corrupt(format!("hash check failed: {e}")))?;
    if actual != id {
        return Err(StreamError::Corrupt(format!("content hashes to {actual}")));
    }
    Ok(())
}
