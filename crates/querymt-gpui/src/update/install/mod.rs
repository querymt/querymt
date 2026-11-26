//! The installers, one per install kind (design D10, D11).
//!
//! Each works on a file the download step already verified (size and SHA-256)
//! and changes the installed files in as few, undoable steps as it can. The
//! file system and the platform tools are injected, so the tests run on
//! temporary directories and can make a step fail.

use std::fmt;
use std::io;
use std::path::PathBuf;

use super::install_kind::InstallKind;

// Unix, not only Linux: the AppImage installer is plain file operations, and
// its tests run on a developer's Mac too.
#[cfg(unix)]
pub mod appimage;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod restart;
pub mod windows;

/// Why an install did not happen. The message is shown in the Updates card;
/// the installed version is intact whenever an installer returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallError(pub String);

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstallError {}

/// A program to start, detached from the app, so it outlives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// A last argument passed exactly as written, never quoted. The NSIS
    /// setup needs this for `/D=<dir>`: it must be last and must have no
    /// quotes, even when the directory has spaces.
    pub raw_tail: Option<String>,
}

impl Launch {
    /// Every argument in order, as the program sees them.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = self.args.clone();
        argv.extend(self.raw_tail.clone());
        argv
    }
}

/// The new version to start for `kind` after an install (macOS, AppImage).
/// Windows starts the setup instead ([`windows::launch`]); a notify-only
/// install has nothing to start.
pub fn launch_for(kind: &InstallKind) -> Option<Launch> {
    match kind {
        // `-n`: a new instance, while this one is still on its way out.
        InstallKind::MacApp { bundle } => Some(Launch {
            program: "open".into(),
            args: vec!["-n".into(), bundle.clone()],
            raw_tail: None,
        }),
        InstallKind::AppImage { path } => Some(Launch {
            program: path.into(),
            args: Vec::new(),
            raw_tail: None,
        }),
        InstallKind::Nsis { .. } | InstallKind::NotifyOnly(_) => None,
    }
}

/// Start `launch` detached from this process: its own process group on Unix,
/// no console and not tied to ours on Windows. Standard streams are closed.
pub fn spawn_detached(launch: &Launch) -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(&launch.program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.args(launch.argv()).process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.args(&launch.args);
        if let Some(tail) = &launch.raw_tail {
            command.raw_arg(tail);
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    // The child is not waited for: this process quits right after.
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::install_kind::NotifyReason;

    #[test]
    fn the_new_version_is_started_the_way_its_install_kind_needs() {
        let mac = launch_for(&InstallKind::MacApp {
            bundle: "/Applications/QueryMT Desktop.app".into(),
        })
        .unwrap();
        assert_eq!(mac.program, PathBuf::from("open"));
        assert_eq!(mac.argv(), ["-n", "/Applications/QueryMT Desktop.app"]);

        let appimage = launch_for(&InstallKind::AppImage {
            path: "/home/me/Apps/QueryMT.AppImage".into(),
        })
        .unwrap();
        assert_eq!(
            appimage.program,
            PathBuf::from("/home/me/Apps/QueryMT.AppImage")
        );
        assert!(appimage.argv().is_empty());

        // The setup does the starting on Windows; notify-only installs never
        // install.
        assert_eq!(
            launch_for(&InstallKind::Nsis {
                dir: "C:\\x".into()
            }),
            None
        );
        assert_eq!(
            launch_for(&InstallKind::NotifyOnly(NotifyReason::Unmanaged)),
            None
        );
    }
}
