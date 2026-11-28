//! Linking a snapshot store to its source repository's object directory.
//!
//! See [`super::sink`] for what a linked store trusts.

use super::sink::BASELINE_FILE;
use super::{GitSnapshotBackend, SnapshotLock};
use crate::snapshot::backend::{SnapshotError, SnapshotResult, io_err};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long linking waits for in-flight snapshots or restores of the store
/// to finish. Linking is best effort, so a store that stays busy is left
/// unlinked rather than delaying the session start.
const LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(2)
};
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Take the store's exclusive lock within [`LOCK_WAIT`], or `None`.
fn exclusive_within_wait(git_dir: &Path) -> SnapshotResult<Option<SnapshotLock>> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if let Some(lock) = SnapshotLock::try_exclusive(git_dir)? {
            return Ok(Some(lock));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// The source's object format, from `extensions.objectFormat` in its config
/// file (default `sha1`). Read without opening the source through gix.
/// `None` when the config cannot be read.
fn source_object_format(objects_dir: &Path) -> Option<String> {
    let config_path = objects_dir.parent()?.join("config");
    if !config_path.exists() {
        return Some("sha1".to_owned());
    }
    let config =
        gix::config::File::from_path_no_includes(config_path.clone(), gix::config::Source::Local)
            .inspect_err(|e| {
                tracing::debug!(path = %config_path.display(), error = %e, "cannot read source repository config");
            })
            .ok()?;
    Some(
        config
            .string("extensions.objectFormat")
            .map_or_else(|| "sha1".to_owned(), |v| v.to_string().to_ascii_lowercase()),
    )
}

/// Replace `path` with `content` through a temporary file in the same
/// directory, so a reader sees the old or the new content, never a mix.
fn write_atomic(path: &Path, content: &[u8]) -> SnapshotResult<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_err("create directory", dir))?;
    let mut tmp =
        tempfile::NamedTempFile::new_in(dir).map_err(io_err("create temporary file in", dir))?;
    tmp.write_all(content).map_err(io_err("write", path))?;
    tmp.persist(path)
        .map_err(|e| io_err("rename into place", path)(e.error))?;
    Ok(())
}

impl GitSnapshotBackend {
    /// Blocking body of [`SnapshotBackend::link_object_source`].
    ///
    /// [`SnapshotBackend::link_object_source`]: crate::snapshot::SnapshotBackend::link_object_source
    pub(super) fn link_object_source_blocking(
        &self,
        worktree: &Path,
        objects_dir: &Path,
        baseline_commit: &str,
    ) -> SnapshotResult<()> {
        let repo = self.open_or_init(worktree)?;

        let store_format = repo.object_hash().to_string();
        match source_object_format(objects_dir) {
            Some(source_format) if source_format == store_format => {}
            Some(source_format) => {
                tracing::debug!(
                    objects = %objects_dir.display(),
                    %source_format,
                    %store_format,
                    "source repository uses a different object format; snapshot store stays self-contained"
                );
                return Ok(());
            }
            None => return Ok(()),
        }
        let baseline = gix::ObjectId::from_hex(baseline_commit.trim().as_bytes())
            .map_err(|e| SnapshotError::Other(format!("invalid baseline commit: {e}")))?;
        if !objects_dir.is_dir() {
            tracing::debug!(objects = %objects_dir.display(), "source object directory is missing; snapshot store stays self-contained");
            return Ok(());
        }

        // Exclusive: no snapshot is in flight while the store switches from
        // writing everything to trusting the baseline.
        let Some(_lock) = exclusive_within_wait(repo.git_dir())? else {
            tracing::debug!(
                store = %repo.git_dir().display(),
                "snapshot store is busy; left unlinked for now"
            );
            return Ok(());
        };

        // The baseline file first: a store that lists an alternate but names no
        // baseline trusts nothing, while the reverse order could leave a window
        // in which the alternate is trusted wholesale.
        let baseline_path = repo.git_dir().join(BASELINE_FILE);
        let baseline_text = format!("{baseline}\n");
        if fs::read_to_string(&baseline_path).ok().as_deref() != Some(baseline_text.as_str()) {
            write_atomic(&baseline_path, baseline_text.as_bytes())?;
        }

        let alternates_path = repo.git_dir().join("objects/info/alternates");
        let wanted = objects_dir.to_string_lossy().into_owned();
        let existing = match fs::read_to_string(&alternates_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(io_err("read", &alternates_path)(e)),
        };
        if existing.lines().filter(|line| *line == wanted).count() != 1 {
            let mut lines: Vec<&str> = existing.lines().filter(|line| *line != wanted).collect();
            lines.push(&wanted);
            write_atomic(
                &alternates_path,
                format!("{}\n", lines.join("\n")).as_bytes(),
            )?;
        }
        Ok(())
    }
}
