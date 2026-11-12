//! Filesystem operations confined to the worktree.
//!
//! Restore paths come from stored session history and snapshot trees, so they
//! are not trusted, and the worktree can change underneath a restore. On Unix
//! every operation is relative to an open directory handle and never follows a
//! symlink, so a parent directory swapped for a symlink cannot redirect a write
//! or delete outside the worktree. Elsewhere symlinked parents are rejected
//! with a check that is not race-free.

use crate::snapshot::backend::{SnapshotError, SnapshotResult, io_err};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Split `rel` into normal components, rejecting anything that could leave the
/// worktree (`..`, absolute paths, prefixes) and empty paths.
pub(super) fn validate(rel: &Path) -> SnapshotResult<Vec<&OsStr>> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => return Err(outside(rel)),
        }
    }
    if parts.is_empty() {
        return Err(outside(rel));
    }
    Ok(parts)
}

fn outside(rel: &Path) -> SnapshotError {
    SnapshotError::Filesystem(format!(
        "refusing to restore {}: not a path inside the worktree",
        rel.display()
    ))
}

fn symlinked(path: &Path) -> SnapshotError {
    SnapshotError::Filesystem(format!(
        "refusing to restore {}: a parent is a symlink or not a directory",
        path.display()
    ))
}

#[cfg(unix)]
pub(super) use unix::Root;

#[cfg(not(unix))]
pub(super) use fallback::Root;

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;

    /// An open handle on the worktree root.
    pub(in super::super) struct Root {
        dir: OwnedFd,
        path: PathBuf,
    }

    fn open_dir(base: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Errno> {
        rustix::fs::openat(
            base,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
    }

    impl Root {
        pub(in super::super) fn open(worktree: &Path) -> SnapshotResult<Self> {
            let dir = rustix::fs::open(
                worktree,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| io_err("open", worktree)(e.into()))?;
            Ok(Self {
                dir,
                path: worktree.to_path_buf(),
            })
        }

        pub(in super::super) fn path(&self) -> &Path {
            &self.path
        }

        /// Open the directory holding the last component of a path, one
        /// no-follow step at a time. `Ok(None)` when a directory is missing
        /// and `create` is false.
        fn open_parent(
            &self,
            parents: &[&OsStr],
            create: bool,
            rel: &Path,
        ) -> SnapshotResult<Option<OwnedFd>> {
            let target = self.path.join(rel);
            let mut current: Option<OwnedFd> = None;
            for name in parents {
                let base = current.as_ref().map_or(self.dir.as_fd(), |fd| fd.as_fd());
                let next = match open_dir(base, name) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) if create => {
                        let mode = Mode::RWXU | Mode::RWXG | Mode::RWXO;
                        match rustix::fs::mkdirat(base, *name, mode) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(e) => return Err(io_err("create directory", &target)(e.into())),
                        }
                        open_dir(base, name).map_err(|e| step_error(e, &target))?
                    }
                    Err(Errno::NOENT) => return Ok(None),
                    Err(e) => return Err(step_error(e, &target)),
                };
                current = Some(next);
            }
            Ok(Some(match current {
                Some(fd) => fd,
                None => rustix::io::fcntl_dupfd_cloexec(&self.dir, 0)
                    .map_err(|e| io_err("open", &self.path)(e.into()))?,
            }))
        }

        /// Delete the file at `rel`; a path that is already gone is fine.
        pub(in super::super) fn remove_file(&self, rel: &Path) -> SnapshotResult<()> {
            let parts = validate(rel)?;
            let (name, parents) = parts.split_last().ok_or_else(|| outside(rel))?;
            let target = self.path.join(rel);
            let Some(dir) = self.open_parent(parents, false, rel)? else {
                return Ok(());
            };
            match rustix::fs::unlinkat(&dir, *name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(io_err("delete", &target)(e.into())),
            }
        }

        /// Atomically replace the file at `rel`: `fill` writes a temporary file
        /// next to it, which gets the restored permissions and is renamed into
        /// place. The temporary file is removed on every failure.
        pub(in super::super) fn replace_file(
            &self,
            rel: &Path,
            executable: bool,
            fill: impl FnOnce(&mut fs::File) -> SnapshotResult<()>,
        ) -> SnapshotResult<()> {
            let parts = validate(rel)?;
            let (name, parents) = parts.split_last().ok_or_else(|| outside(rel))?;
            let target = self.path.join(rel);
            let dir = self
                .open_parent(parents, true, rel)?
                .ok_or_else(|| outside(rel))?;

            // `st_mode` is `u16` on macOS/BSD but already `u32` on Linux, where the
            // conversion is a no-op that clippy flags.
            #[allow(clippy::useless_conversion)]
            let existing = match rustix::fs::statat(&dir, *name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile => {
                    Some(u32::from(st.st_mode))
                }
                _ => None,
            };

            let tmp_name = format!(".qmt-restore-{}", uuid::Uuid::new_v4().simple());
            let fd = rustix::fs::openat(
                &dir,
                tmp_name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|e| io_err("create temporary file in", &target)(e.into()))?;
            let mut file = fs::File::from(fd);

            let result = (|| {
                fill(&mut file)?;
                let mode = super::super::restore::restored_mode(existing, executable);
                file.set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(io_err("set permissions of", &target))?;
                rustix::fs::renameat(&dir, tmp_name.as_str(), &dir, *name)
                    .map_err(|e| io_err("write", &target)(e.into()))
            })();
            if result.is_err() {
                // Best effort: the restore error is the one to report.
                let _ = rustix::fs::unlinkat(&dir, tmp_name.as_str(), AtFlags::empty());
            }
            result
        }
    }

    fn step_error(e: Errno, target: &Path) -> SnapshotError {
        if matches!(e, Errno::LOOP | Errno::NOTDIR) {
            symlinked(target)
        } else {
            io_err("open", target)(e.into())
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use super::*;
    use std::io::ErrorKind;

    pub(in super::super) struct Root {
        path: PathBuf,
    }

    impl Root {
        pub(in super::super) fn open(worktree: &Path) -> SnapshotResult<Self> {
            Ok(Self {
                path: worktree.to_path_buf(),
            })
        }

        pub(in super::super) fn path(&self) -> &Path {
            &self.path
        }

        /// Reject a symlink among the existing parents of `rel`.
        fn check_parents(&self, parents: &[&OsStr]) -> SnapshotResult<()> {
            let mut current = self.path.clone();
            for name in parents {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(meta) if meta.file_type().is_symlink() => return Err(symlinked(&current)),
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => break,
                    Err(e) => return Err(io_err("stat", &current)(e)),
                }
            }
            Ok(())
        }

        pub(in super::super) fn remove_file(&self, rel: &Path) -> SnapshotResult<()> {
            let parts = validate(rel)?;
            let (_, parents) = parts.split_last().ok_or_else(|| outside(rel))?;
            self.check_parents(parents)?;
            let target = self.path.join(rel);
            match fs::remove_file(&target) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err("delete", &target)(e)),
            }
        }

        pub(in super::super) fn replace_file(
            &self,
            rel: &Path,
            _executable: bool,
            fill: impl FnOnce(&mut fs::File) -> SnapshotResult<()>,
        ) -> SnapshotResult<()> {
            let parts = validate(rel)?;
            let (_, parents) = parts.split_last().ok_or_else(|| outside(rel))?;
            self.check_parents(parents)?;
            let target = self.path.join(rel);
            let parent = target.parent().unwrap_or(&self.path);
            fs::create_dir_all(parent).map_err(io_err("create directory", parent))?;
            let mut tmp = tempfile::NamedTempFile::new_in(parent)
                .map_err(io_err("create temporary file in", parent))?;
            fill(tmp.as_file_mut())?;
            tmp.persist(&target)
                .map_err(|e| io_err("write", &target)(e.error))?;
            Ok(())
        }
    }
}
