//! Linux: compositor blur on Wayland desktops that provide it, else opaque.
//!
//! GPUI has no query for whether the compositor blurs, and a see-through
//! panel without blur behind it is unreadable. So `glass` is reported only on
//! a desktop known to blur windows that ask for it (KDE Plasma); X11 and GNOME
//! get an opaque panel.

use gpui_kit::{Window, WindowBackgroundAppearance};

use super::Chrome;

pub(super) fn install(window: &mut Window) -> Chrome {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if blurs(wayland, &desktop) {
        window.set_background_appearance(WindowBackgroundAppearance::Blurred);
        Chrome::glass()
    } else {
        Chrome::solid()
    }
}

/// Whether the desktop blurs the window behind a panel that asks for it.
fn blurs(wayland: bool, desktop: &str) -> bool {
    wayland && desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE"))
}

#[cfg(test)]
mod tests {
    use super::blurs;

    #[test]
    fn only_wayland_kde_is_trusted_to_blur() {
        assert!(blurs(true, "KDE"));
        assert!(!blurs(false, "KDE"));
        assert!(!blurs(true, "GNOME"));
        assert!(!blurs(true, "ubuntu:GNOME"));
        assert!(!blurs(false, ""));
    }
}
