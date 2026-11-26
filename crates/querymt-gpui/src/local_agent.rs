//! Which `qmtcode` a process agent runs (native only).
//!
//! A bare `qmtcode` command line runs the copy bundled with the app first,
//! then the first one on PATH. A program given with a path, or any other
//! program, is started as written. Every function takes its inputs (the app
//! executable, the PATH value, the executable suffix) so tests never touch the
//! process environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The program name a process agent uses for the bundled agent.
const PROGRAM: &str = "qmtcode";

/// Where a resolved program came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The copy shipped with the app.
    Bundled,
    /// The first `qmtcode` on PATH.
    Path,
    /// Not a bare `qmtcode`: started as written.
    AsWritten,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub source: Source,
}

fn file_name(suffix: &str) -> String {
    format!("{PROGRAM}{suffix}")
}

/// The qmtcode next to the app executable, or in `../lib/querymt-gpui/`
/// relative to it (deb, AppImage, Nix).
fn bundled_with_suffix(exe: &Path, suffix: &str) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let name = file_name(suffix);
    [
        dir.join(&name),
        dir.join("..").join("lib").join("querymt-gpui").join(&name),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// The first `qmtcode` in the directories of a PATH value.
fn find_on_path_with_suffix(path_var: &OsStr, suffix: &str) -> Option<PathBuf> {
    let name = file_name(suffix);
    std::env::split_paths(path_var)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// Whether the app can start a local qmtcode: bundled or on PATH.
pub(crate) fn available_with_suffix(
    exe: Option<&Path>,
    path_var: Option<&OsStr>,
    suffix: &str,
) -> bool {
    exe.and_then(|exe| bundled_with_suffix(exe, suffix))
        .is_some()
        || path_var
            .and_then(|p| find_on_path_with_suffix(p, suffix))
            .is_some()
}

/// Resolve the program of a process agent's command line.
pub fn resolve_program(
    program: &str,
    exe: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Result<Resolved, String> {
    resolve_with_suffix(program, exe, path_var, std::env::consts::EXE_SUFFIX)
}

fn resolve_with_suffix(
    program: &str,
    exe: Option<&Path>,
    path_var: Option<&OsStr>,
    suffix: &str,
) -> Result<Resolved, String> {
    if program != PROGRAM && program != file_name(suffix) {
        return Ok(Resolved {
            path: PathBuf::from(program),
            source: Source::AsWritten,
        });
    }
    if let Some(path) = exe.and_then(|exe| bundled_with_suffix(exe, suffix)) {
        return Ok(Resolved {
            path,
            source: Source::Bundled,
        });
    }
    if let Some(path) = path_var.and_then(|p| find_on_path_with_suffix(p, suffix)) {
        return Ok(Resolved {
            path,
            source: Source::Path,
        });
    }
    Err("qmtcode was not found (not bundled with the app and not on PATH).".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `<root>/app/querymt-gpui` plus whichever qmtcode files a test creates.
    struct Layout {
        root: tempfile::TempDir,
    }

    impl Layout {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let path = self.root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"").unwrap();
            path
        }

        fn exe(&self) -> PathBuf {
            self.root
                .path()
                .join("usr")
                .join("bin")
                .join("querymt-gpui")
        }

        fn path_var(&self, dirs: &[&str]) -> std::ffi::OsString {
            std::env::join_paths(dirs.iter().map(|d| self.root.path().join(d))).unwrap()
        }
    }

    const SUFFIX: &str = std::env::consts::EXE_SUFFIX;

    fn resolve(layout: &Layout, program: &str, path_dirs: &[&str]) -> Result<Resolved, String> {
        resolve_program(
            program,
            Some(&layout.exe()),
            Some(&layout.path_var(path_dirs)),
        )
    }

    #[test]
    fn bundled_copy_wins_over_path() {
        let l = Layout::new();
        let bundled = l.touch(&format!("usr/bin/qmtcode{SUFFIX}"));
        l.touch(&format!("onpath/qmtcode{SUFFIX}"));
        let got = resolve(&l, "qmtcode", &["onpath"]).unwrap();
        assert_eq!(got.source, Source::Bundled);
        assert_eq!(got.path, bundled);
    }

    #[test]
    fn path_copy_is_used_without_a_bundled_copy() {
        let l = Layout::new();
        let on_path = l.touch(&format!("onpath/qmtcode{SUFFIX}"));
        let got = resolve(&l, "qmtcode", &["empty", "onpath"]).unwrap();
        assert_eq!(got.source, Source::Path);
        assert_eq!(got.path, on_path);
    }

    #[test]
    fn linux_lib_layout_is_found() {
        let l = Layout::new();
        // The app executable exists, so `usr/bin/..` resolves as installed.
        l.touch("usr/bin/querymt-gpui");
        l.touch(&format!("usr/lib/querymt-gpui/qmtcode{SUFFIX}"));
        let got = resolve(&l, "qmtcode", &[]).unwrap();
        assert_eq!(got.source, Source::Bundled);
        assert!(
            got.path
                .ends_with(format!("lib/querymt-gpui/qmtcode{SUFFIX}"))
        );
    }

    #[test]
    fn windows_finds_qmtcode_exe() {
        // The suffix is injected, so this runs on every host; on Windows the
        // production functions use the same `.exe` suffix.
        let l = Layout::new();
        let bundled = l.touch("usr/bin/qmtcode.exe");
        // A suffix-less file must not match when the suffix is `.exe`.
        l.touch("onpath/qmtcode");
        let exe = l.exe();
        let got = resolve_with_suffix("qmtcode", Some(&exe), None, ".exe").unwrap();
        assert_eq!(got.path, bundled);
        // `qmtcode.exe` written out is also the bare name on Windows.
        let got = resolve_with_suffix("qmtcode.exe", Some(&exe), None, ".exe").unwrap();
        assert_eq!(got.source, Source::Bundled);
        // PATH lookup looks for the suffixed name only.
        let path = l.path_var(&["onpath"]);
        assert_eq!(find_on_path_with_suffix(&path, ".exe"), None);
        l.touch("onpath/qmtcode.exe");
        assert_eq!(
            find_on_path_with_suffix(&path, ".exe"),
            Some(l.root.path().join("onpath").join("qmtcode.exe"))
        );
        assert!(available_with_suffix(None, Some(&path), ".exe"));
    }

    #[test]
    fn no_qmtcode_anywhere() {
        let l = Layout::new();
        let err = resolve(&l, "qmtcode", &["onpath"]).unwrap_err();
        assert!(
            err.contains("qmtcode") && err.contains("not found"),
            "{err}"
        );
        assert!(!available_with_suffix(
            Some(&l.exe()),
            Some(&l.path_var(&["onpath"])),
            SUFFIX
        ));
    }

    #[test]
    fn path_to_qmtcode_is_not_replaced() {
        let l = Layout::new();
        l.touch(&format!("usr/bin/qmtcode{SUFFIX}"));
        let got = resolve(&l, "/opt/qmt/qmtcode", &[]).unwrap();
        assert_eq!(got.source, Source::AsWritten);
        assert_eq!(got.path, PathBuf::from("/opt/qmt/qmtcode"));
    }

    #[test]
    fn an_explicit_path_is_used_even_when_a_copy_is_bundled() {
        let l = Layout::new();
        l.touch(&format!("usr/bin/qmtcode{SUFFIX}"));
        l.touch(&format!("onpath/qmtcode{SUFFIX}"));
        let custom = l.touch(&format!("opt/custom/qmtcode{SUFFIX}"));
        let program = custom.to_str().unwrap();
        let got = resolve(&l, program, &["onpath"]).unwrap();
        assert_eq!(got.source, Source::AsWritten);
        assert_eq!(got.path, custom);
        // The bare name still selects the bundled copy.
        assert_eq!(
            resolve(&l, "qmtcode", &["onpath"]).unwrap().source,
            Source::Bundled
        );
    }

    #[test]
    fn other_programs_are_unaffected() {
        let l = Layout::new();
        l.touch(&format!("usr/bin/qmtcode{SUFFIX}"));
        let got = resolve(&l, "other-agent", &[]).unwrap();
        assert_eq!(got.source, Source::AsWritten);
        assert_eq!(got.path, PathBuf::from("other-agent"));
    }
}
