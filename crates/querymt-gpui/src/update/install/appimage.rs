//! Replace the running AppImage with a downloaded one (design D10).
//!
//! The download lands next to `$APPIMAGE` (the same file system) as a hidden
//! file. [`prepare`] checks it and makes it executable; [`Prepared::commit`]
//! renames it over `$APPIMAGE`. The rename is atomic, and the running image
//! stays mounted from the old inode, so the running process is unaffected.
//! Split in two so the caller can stop the local agents in between.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::InstallError;

/// Where the download goes: next to the AppImage, hidden.
pub fn staging_path(appimage: &Path, pid: u32) -> PathBuf {
    let name = appimage
        .file_name()
        .map_or_else(|| "AppImage".into(), |n| n.to_string_lossy().into_owned());
    appimage
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.update-{pid}"))
}

/// The one file-system step, injected so a test can make it fail.
pub struct Hooks<'a> {
    pub rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
}

fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

impl Hooks<'static> {
    pub fn real() -> Self {
        Self {
            rename: &real_rename,
        }
    }
}

/// A checked, executable download waiting to be moved into place. Dropping it
/// without a commit deletes the download.
pub struct Prepared {
    target: PathBuf,
    staged: PathBuf,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        // After a successful commit the file is gone and this does nothing.
        let _ = fs::remove_file(&self.staged);
    }
}

/// Check the download at `staged` and make it executable (`chmod 755`). On an
/// error the download is deleted and `target` was not touched.
pub fn prepare(target: &Path, staged: &Path) -> Result<Prepared, InstallError> {
    let guard = Prepared {
        target: target.to_path_buf(),
        staged: staged.to_path_buf(),
    };
    check_elf(staged)?;
    set_executable(staged)
        .map_err(|err| InstallError(format!("Could not make the download executable: {err}")))?;
    Ok(guard)
}

impl Prepared {
    /// Rename the download over the AppImage. On an error the AppImage is as it
    /// was and the download is deleted.
    pub fn commit(self, hooks: &Hooks<'_>) -> Result<(), InstallError> {
        (hooks.rename)(&self.staged, &self.target).map_err(|err| {
            InstallError(format!(
                "Could not replace `{}`: {err}. The current version was kept.",
                self.target.display()
            ))
        })
    }
}

/// An AppImage is an ELF executable. A download that is not (an HTML error
/// page that passed a lenient proxy, say) must never replace the app.
fn check_elf(path: &Path) -> Result<(), InstallError> {
    use io::Read as _;
    let mut magic = [0u8; 4];
    let read = fs::File::open(path).and_then(|mut file| file.read_exact(&mut magic));
    match read {
        Ok(()) if magic == *b"\x7fELF" => Ok(()),
        Ok(()) => Err(InstallError("The download is not an AppImage.".into())),
        Err(err) => Err(InstallError(format!("Could not read the download: {err}"))),
    }
}

fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    const PID: u32 = 4242;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The installed AppImage (mode 0644, to see the chmod) and a downloaded
    /// one next to it.
    fn setup(dir: &Path, download: &[u8]) -> (PathBuf, PathBuf) {
        let target = dir.join("QueryMT-Desktop.AppImage");
        fs::write(&target, b"\x7fELF old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let staged = staging_path(&target, PID);
        fs::write(&staged, download).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        (target, staged)
    }

    #[test]
    fn appimage_is_replaced_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (target, staged) = setup(dir.path(), b"\x7fELF new");
        assert_eq!(staged.parent(), Some(dir.path()));

        let prepared = prepare(&target, &staged).unwrap();
        // Nothing installed has changed before the commit.
        assert_eq!(fs::read(&target).unwrap(), b"\x7fELF old");
        prepared.commit(&Hooks::real()).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"\x7fELF new");
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(names(dir.path()), ["QueryMT-Desktop.AppImage"]);
    }

    #[test]
    fn a_failed_rename_keeps_the_old_appimage_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let (target, staged) = setup(dir.path(), b"\x7fELF new");
        let faulty = |_: &Path, _: &Path| Err(io::Error::other("injected fault"));
        let hooks = Hooks { rename: &faulty };

        let err = prepare(&target, &staged)
            .unwrap()
            .commit(&hooks)
            .unwrap_err();

        assert!(err.0.contains("injected fault"), "{err}");
        assert!(err.0.contains("current version was kept"), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"\x7fELF old");
        assert_eq!(names(dir.path()), ["QueryMT-Desktop.AppImage"]);
    }

    #[test]
    fn a_download_that_is_not_an_appimage_is_refused_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [&b"<html>Not found</html>"[..], b"", b"\x7fEL"] {
            let (target, staged) = setup(dir.path(), bad);
            let err = prepare(&target, &staged).err().unwrap();
            assert!(!err.0.is_empty());
            assert_eq!(fs::read(&target).unwrap(), b"\x7fELF old");
            assert_eq!(names(dir.path()), ["QueryMT-Desktop.AppImage"]);
        }
    }

    #[test]
    fn an_abandoned_install_deletes_its_download() {
        let dir = tempfile::tempdir().unwrap();
        let (target, staged) = setup(dir.path(), b"\x7fELF new");
        drop(prepare(&target, &staged).unwrap());
        assert_eq!(names(dir.path()), ["QueryMT-Desktop.AppImage"]);
        assert_eq!(fs::read(&target).unwrap(), b"\x7fELF old");
    }
}
