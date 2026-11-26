//! Install on Windows by running the downloaded NSIS setup (design D11).
//!
//! The running exe and `qmtcode.exe` are locked, so the app cannot replace its
//! own files. The setup does it: `installer.nsi`, given `/UPDATE`, waits for
//! the app to exit, installs silently and starts the new app. The app's part
//! is to download and check the setup, stop its agents, free the
//! single-instance socket, start the setup detached and quit
//! (see [`super::restart`]).
//!
//! The command line is pure data, so it is tested on every platform; only
//! [`super::spawn_detached`] is Windows-specific.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::{InstallError, Launch};

/// Where the setup is downloaded: `%TEMP%\QueryMT-Desktop-update-<version>.exe`.
pub fn setup_path(temp: &Path, version: &str) -> PathBuf {
    // The version comes from a signed manifest; still, never let it add a
    // path separator to a file name.
    let version: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    temp.join(format!("QueryMT-Desktop-update-{version}.exe"))
}

/// Run `setup` silently as an update of the install in `dir`.
///
/// `/D=<dir>` must be the last argument and must not be quoted (NSIS reads
/// the rest of the command line as the directory), so it is the raw tail.
pub fn launch(setup: &Path, dir: &str) -> Launch {
    Launch {
        program: setup.to_path_buf(),
        args: vec!["/S".into(), "/UPDATE".into()],
        raw_tail: Some(format!("/D={dir}")),
    }
}

/// A download that is not a Windows executable must never be run. On an error
/// the file is deleted.
pub fn check_setup(setup: &Path) -> Result<(), InstallError> {
    let mut magic = [0u8; 2];
    let read = fs::File::open(setup).and_then(|mut file| file.read_exact(&mut magic));
    let result = match read {
        Ok(()) if &magic == b"MZ" => Ok(()),
        Ok(()) => Err(InstallError("The download is not a Windows setup.".into())),
        Err(err) => Err(InstallError(format!("Could not read the download: {err}"))),
    };
    if result.is_err() {
        let _ = fs::remove_file(setup);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_installer_runs_after_the_app_exits() {
        // A directory with spaces: the case where quoting would be tempting.
        let dir = r"C:\Users\Jo Doe\AppData\Local\QueryMT Desktop";
        let setup = setup_path(Path::new(r"C:\Temp"), "0.7.0");
        let launch = launch(&setup, dir);

        assert_eq!(launch.program, setup);
        assert_eq!(
            launch.argv(),
            ["/S", "/UPDATE", &format!("/D={dir}")],
            "silent, update mode, and the directory last"
        );
        // `/D` is the last argument and is not quoted.
        let last = launch.argv().pop().unwrap();
        assert!(last.starts_with("/D="), "{last}");
        assert!(!last.contains('"'), "{last}");
        // It travels as the raw tail, which the process builder never quotes;
        // an ordinary argument with spaces would be.
        assert_eq!(launch.raw_tail.as_deref(), Some(last.as_str()));
        assert!(launch.args.iter().all(|arg| !arg.starts_with("/D")));
    }

    #[test]
    fn the_setup_is_saved_under_a_safe_name() {
        let temp = Path::new("tmp");
        assert_eq!(
            setup_path(temp, "0.7.0"),
            temp.join("QueryMT-Desktop-update-0.7.0.exe")
        );
        assert_eq!(
            setup_path(temp, "0.7.0-nightly.3+abc"),
            temp.join("QueryMT-Desktop-update-0.7.0-nightly.3+abc.exe")
        );
        let odd = setup_path(temp, r"..\..\evil/x");
        assert_eq!(odd.parent(), Some(temp));
    }

    #[test]
    fn only_a_windows_executable_may_be_run() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.exe");
        fs::write(&good, b"MZ\x90\x00").unwrap();
        check_setup(&good).unwrap();
        assert!(good.exists());

        for (name, bytes) in [("html.exe", &b"<html></html>"[..]), ("empty.exe", b"")] {
            let bad = dir.path().join(name);
            fs::write(&bad, bytes).unwrap();
            assert!(check_setup(&bad).is_err(), "{name}");
            assert!(!bad.exists(), "{name} was left behind");
        }
    }
}
