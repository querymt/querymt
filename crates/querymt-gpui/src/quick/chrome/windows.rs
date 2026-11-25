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

/// Settings > Personalisation > Colours > Transparency effects is off.
/// Read from the registry's `EnableTransparency` value; no value means on.
pub(super) fn reduce_transparency() -> bool {
    use std::os::windows::process::CommandExt as _;
    const NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "EnableTransparency",
        ])
        .creation_flags(NO_WINDOW)
        .output()
        .ok()
        .is_some_and(|out| transparency_off(&String::from_utf8_lossy(&out.stdout)))
}

/// `reg query` output for `EnableTransparency`: a `REG_DWORD` of 0 is off.
fn transparency_off(reg_output: &str) -> bool {
    reg_output
        .lines()
        .find(|l| l.trim_start().starts_with("EnableTransparency"))
        .and_then(|l| l.split_whitespace().last())
        .is_some_and(|v| matches!(v, "0x0" | "0"))
}

/// `DWMWA_SYSTEMBACKDROP_TYPE` came with Windows 11 22H2 (build 22621).
fn mica_available() -> bool {
    windows_build().is_some_and(|build| build >= 22621)
}

#[cfg(test)]
mod tests {
    use super::transparency_off;

    #[test]
    fn the_registry_value_tells_whether_transparency_is_off() {
        let key = "\r\nHKEY_CURRENT_USER\\Software\\X\r\n    EnableTransparency    REG_DWORD    ";
        assert!(transparency_off(&format!("{key}0x0\r\n")));
        assert!(!transparency_off(&format!("{key}0x1\r\n")));
        assert!(!transparency_off(""), "no value: on");
    }
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
