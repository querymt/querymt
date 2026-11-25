//! Windows: Mica where the system has it, else acrylic-style blur.

use gpui_kit::{Window, WindowBackgroundAppearance};

use super::Chrome;

pub(super) fn install(window: &mut Window) -> Chrome {
    // DWM ignores an unsupported backdrop type (before Windows 11 22H2), so
    // Mica is asked first on Windows 11 and blur on older builds.
    if mica_available() {
        window.set_background_appearance(WindowBackgroundAppearance::MicaBackdrop);
    } else {
        window.set_background_appearance(WindowBackgroundAppearance::Blurred);
    }
    Chrome::glass()
}

/// `DWMWA_SYSTEMBACKDROP_TYPE` came with Windows 11 22H2 (build 22621).
fn mica_available() -> bool {
    windows_build().is_some_and(|build| build >= 22621)
}

/// The Windows build number, read from `ver`.
fn windows_build() -> Option<u32> {
    use std::os::windows::process::CommandExt as _;
    /// `CREATE_NO_WINDOW`: no console flashes up from a GUI app.
    const NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("cmd")
        .args(["/C", "ver"])
        .creation_flags(NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split("Version ").nth(1)?;
    version
        .trim_end_matches([']', '\r', '\n', ' '])
        .split('.')
        .nth(2)?
        .parse()
        .ok()
}
