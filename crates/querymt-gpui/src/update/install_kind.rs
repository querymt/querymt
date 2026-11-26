//! How the running app was installed, which decides whether it may replace
//! itself or only tell the user about a new version.
//!
//! The decision is a pure function of an [`InstallProbe`] (the facts the
//! runtime reads from the system), so every platform's rule is tested on any
//! host. Paths are handled as text for the same reason: a Windows path is not a
//! path on macOS.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    Linux,
}

/// The facts the install kind depends on.
#[derive(Clone, Debug)]
pub struct InstallProbe {
    pub os: Os,
    /// The running executable (`std::env::current_exe`).
    pub exe: String,
    /// `$APPIMAGE`, the AppImage file the app runs from (Linux).
    pub appimage_env: Option<String>,
    /// `InstallLocation` of the NSIS uninstall entry of QueryMT Desktop
    /// (Windows), when the entry exists.
    pub nsis_install_location: Option<String>,
    /// Whether the directory [`InstallProbe::install_parent`] names is
    /// writable: the one that holds the `.app` (macOS) or the AppImage (Linux).
    pub parent_writable: bool,
}

/// How the app replaces itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// A `.app` bundle: replaced in place, at `bundle`.
    MacApp { bundle: String },
    /// The per-user NSIS install in `dir`: the setup exe does the replacing.
    Nsis { dir: String },
    /// An AppImage file: replaced in place, at `path`.
    AppImage { path: String },
    /// The app does not replace itself: it shows the version and a link.
    NotifyOnly(NotifyReason),
}

/// Why an install is notify-only; the UI words each differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyReason {
    /// Running from macOS App Translocation: move the app to Applications.
    Translocated,
    /// A bundle or AppImage in a directory the user cannot write.
    ReadOnly,
    /// An install the app does not manage: deb, Nix, a bare binary, a
    /// Windows build not installed by the setup exe.
    Unmanaged,
}

pub fn detect_install_kind(probe: &InstallProbe) -> InstallKind {
    match probe.os {
        Os::Mac => mac_kind(probe),
        Os::Windows => windows_kind(probe),
        Os::Linux => linux_kind(probe),
    }
}

/// The `.app` bundle path inside an executable path
/// (`…/QueryMT Desktop.app/Contents/MacOS/querymt-gpui`).
fn mac_bundle(exe: &str) -> Option<&str> {
    let end = exe.find(".app/Contents/MacOS/")? + ".app".len();
    Some(&exe[..end])
}

fn mac_kind(probe: &InstallProbe) -> InstallKind {
    let Some(bundle) = mac_bundle(&probe.exe) else {
        return InstallKind::NotifyOnly(NotifyReason::Unmanaged);
    };
    if probe.exe.contains("/AppTranslocation/") {
        return InstallKind::NotifyOnly(NotifyReason::Translocated);
    }
    if !probe.parent_writable {
        return InstallKind::NotifyOnly(NotifyReason::ReadOnly);
    }
    InstallKind::MacApp {
        bundle: bundle.to_owned(),
    }
}

fn windows_kind(probe: &InstallProbe) -> InstallKind {
    let (Some(location), Some(dir)) = (&probe.nsis_install_location, parent_text(&probe.exe))
    else {
        return InstallKind::NotifyOnly(NotifyReason::Unmanaged);
    };
    if windows_same_dir(location, dir) {
        InstallKind::Nsis {
            dir: dir.to_owned(),
        }
    } else {
        InstallKind::NotifyOnly(NotifyReason::Unmanaged)
    }
}

fn linux_kind(probe: &InstallProbe) -> InstallKind {
    match probe.appimage_env.as_deref().filter(|p| !p.is_empty()) {
        // Everything else: deb (/usr), Nix (/nix/store), a bare binary.
        None => InstallKind::NotifyOnly(NotifyReason::Unmanaged),
        Some(_) if !probe.parent_writable => InstallKind::NotifyOnly(NotifyReason::ReadOnly),
        Some(path) => InstallKind::AppImage {
            path: path.to_owned(),
        },
    }
}

/// The directory of a path written with `/` or `\`.
fn parent_text(path: &str) -> Option<&str> {
    let end = path.rfind(['/', '\\'])?;
    Some(&path[..end])
}

/// Windows directories are equal ignoring case, separator style and a trailing
/// separator.
fn windows_same_dir(a: &str, b: &str) -> bool {
    let normal = |s: &str| {
        s.trim()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let a = normal(a);
    !a.is_empty() && a == normal(b)
}

impl InstallProbe {
    /// The directory whose write access decides self-update: the one that
    /// holds the `.app` (macOS) or the AppImage file (Linux).
    pub fn install_parent(os: Os, exe: &str, appimage_env: Option<&str>) -> Option<PathBuf> {
        match os {
            Os::Mac => Path::new(mac_bundle(exe)?).parent().map(Path::to_path_buf),
            Os::Linux => Path::new(appimage_env.filter(|p| !p.is_empty())?)
                .parent()
                .map(Path::to_path_buf),
            Os::Windows => None,
        }
    }
}

/// The uninstall key of the NSIS install (`installer.nsi`).
const NSIS_UNINSTALL_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\QueryMTDesktopGpui";

/// The value of `InstallLocation` in the output of `reg query <key> /v
/// InstallLocation` (`    InstallLocation    REG_SZ    C:\Path`).
// Only the Windows probe reads `reg` output; the parsing is the same
// everywhere and its tests run everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_reg_install_location(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("InstallLocation")?;
        let (kind, value) = rest.trim_start().split_once(char::is_whitespace)?;
        (kind == "REG_SZ")
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// Whether files can be created in `dir`.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".querymt-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl InstallProbe {
    /// Read the facts from the running system.
    pub fn detect_current() -> Option<Self> {
        let os = if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        };
        let exe = std::env::current_exe().ok()?.to_string_lossy().into_owned();
        let appimage_env = std::env::var("APPIMAGE").ok();
        let parent_writable = Self::install_parent(os, &exe, appimage_env.as_deref())
            .is_some_and(|dir| dir_is_writable(&dir));
        let nsis_install_location = if os == Os::Windows {
            windows_install_location()
        } else {
            None
        };
        Some(Self {
            os,
            exe,
            appimage_env,
            nsis_install_location,
            parent_writable,
        })
    }
}

#[cfg(windows)]
fn windows_install_location() -> Option<String> {
    use std::os::windows::process::CommandExt as _;
    let output = std::process::Command::new("reg")
        .args(["query", NSIS_UNINSTALL_KEY, "/v", "InstallLocation"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    parse_reg_install_location(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(windows))]
fn windows_install_location() -> Option<String> {
    let _ = NSIS_UNINSTALL_KEY;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(os: Os, exe: &str) -> InstallProbe {
        InstallProbe {
            os,
            exe: exe.into(),
            appimage_env: None,
            nsis_install_location: None,
            parent_writable: true,
        }
    }

    const MAC_EXE: &str = "/Applications/QueryMT Desktop.app/Contents/MacOS/querymt-gpui";
    const TRANSLOCATED: &str = "/private/var/folders/ab/xyz/AppTranslocation/6F1C-77/d/QueryMT Desktop.app/Contents/MacOS/querymt-gpui";

    #[test]
    fn deb_install_is_notify_only() {
        let mut deb = probe(Os::Linux, "/usr/bin/querymt-gpui");
        assert_eq!(
            detect_install_kind(&deb),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );
        // Even when root-owned `/usr/bin` happens to be writable, and when
        // `APPIMAGE` is set to nothing.
        deb.appimage_env = Some(String::new());
        assert_eq!(
            detect_install_kind(&deb),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );
    }

    #[test]
    fn translocated_macos_app_is_notify_only() {
        let kind = detect_install_kind(&probe(Os::Mac, TRANSLOCATED));
        assert_eq!(kind, InstallKind::NotifyOnly(NotifyReason::Translocated));
        // Not because of the directory: a writable one changes nothing.
        let mut writable = probe(Os::Mac, TRANSLOCATED);
        writable.parent_writable = true;
        assert_eq!(detect_install_kind(&writable), kind);
    }

    #[test]
    fn install_kind_table() {
        // macOS: a writable .app installs itself; a read-only parent does not;
        // a bare binary is not a bundle.
        assert_eq!(
            detect_install_kind(&probe(Os::Mac, MAC_EXE)),
            InstallKind::MacApp {
                bundle: "/Applications/QueryMT Desktop.app".into()
            }
        );
        let mut read_only = probe(Os::Mac, MAC_EXE);
        read_only.parent_writable = false;
        assert_eq!(
            detect_install_kind(&read_only),
            InstallKind::NotifyOnly(NotifyReason::ReadOnly)
        );
        assert_eq!(
            detect_install_kind(&probe(
                Os::Mac,
                "/Users/me/target/release-gpui/querymt-gpui"
            )),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );

        // Windows: the exe's directory is the NSIS install location (case and
        // separators do not matter); any other location is not managed.
        let exe = r"C:\Users\me\AppData\Local\Programs\QueryMT Desktop\querymt-gpui.exe";
        let mut nsis = probe(Os::Windows, exe);
        nsis.nsis_install_location =
            Some(r"c:/users/me/appdata/local/programs/querymt desktop\".into());
        assert_eq!(
            detect_install_kind(&nsis),
            InstallKind::Nsis {
                dir: r"C:\Users\me\AppData\Local\Programs\QueryMT Desktop".into()
            }
        );
        nsis.nsis_install_location = Some(r"D:\Apps\QueryMT Desktop".into());
        assert_eq!(
            detect_install_kind(&nsis),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );
        nsis.nsis_install_location = None;
        assert_eq!(
            detect_install_kind(&nsis),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );

        // Linux: an AppImage in a writable directory; a read-only one is not.
        let appimage_exe = "/tmp/.mount_QueryMTabc/usr/bin/querymt-gpui";
        let mut appimage = probe(Os::Linux, appimage_exe);
        appimage.appimage_env = Some("/home/me/Apps/QueryMT.AppImage".into());
        assert_eq!(
            detect_install_kind(&appimage),
            InstallKind::AppImage {
                path: "/home/me/Apps/QueryMT.AppImage".into()
            }
        );
        appimage.parent_writable = false;
        assert_eq!(
            detect_install_kind(&appimage),
            InstallKind::NotifyOnly(NotifyReason::ReadOnly)
        );

        // Nix: the wrapped binary in the store, whatever else is set.
        let nix = probe(
            Os::Linux,
            "/nix/store/abc-querymt-gpui/bin/.querymt-gpui-wrapped",
        );
        assert_eq!(
            detect_install_kind(&nix),
            InstallKind::NotifyOnly(NotifyReason::Unmanaged)
        );
    }

    #[test]
    fn reg_output_gives_the_install_location() {
        let output = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\QueryMTDesktopGpui\r\n    InstallLocation    REG_SZ    C:\\Users\\me\\AppData\\Local\\Programs\\QueryMT Desktop\r\n\r\n";
        assert_eq!(
            parse_reg_install_location(output).as_deref(),
            Some("C:\\Users\\me\\AppData\\Local\\Programs\\QueryMT Desktop")
        );
        assert_eq!(
            parse_reg_install_location(
                "ERROR: The system was unable to find the specified registry key"
            ),
            None
        );
        assert_eq!(
            parse_reg_install_location("    InstallLocation    REG_DWORD    0x1"),
            None
        );
    }

    #[test]
    fn install_parent_is_the_directory_that_needs_write_access() {
        assert_eq!(
            InstallProbe::install_parent(Os::Mac, MAC_EXE, None),
            Some(PathBuf::from("/Applications"))
        );
        assert_eq!(
            InstallProbe::install_parent(Os::Linux, "/x", Some("/home/me/Apps/Q.AppImage")),
            Some(PathBuf::from("/home/me/Apps"))
        );
        assert_eq!(
            InstallProbe::install_parent(Os::Linux, "/usr/bin/q", None),
            None
        );
        assert_eq!(
            InstallProbe::install_parent(Os::Windows, "C:\\q.exe", None),
            None
        );
    }
}
