//! Linux: compositor blur on Wayland desktops that provide it, else opaque.
//!
//! GPUI has no query for whether the compositor blurs, and a see-through
//! panel without blur behind it is unreadable. So `glass` is reported only on
//! a desktop known to blur windows that ask for it (KDE Plasma); X11 and GNOME
//! get an opaque panel.

use gpui_kit::component::window_paddings;
use gpui_kit::{Decorations, Edges, Pixels, Window, WindowBackgroundAppearance, px};

use super::Chrome;

/// The hairline gpui-component draws inside the shadow margin of a
/// client-decorated window, on each side not tiled.
const HAIRLINE: f32 = 1.;

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

/// The frame gpui-component's root draws inside a client-decorated window
/// (`window_border`): GNOME has no server-side decorations, and a layer-shell
/// surface never has them. Without it the panel would lose 42 px each way.
pub(super) fn frame(window: &Window) -> Edges<Pixels> {
    frame_for(window.window_decorations(), window_paddings(window))
}

/// Keep the window's bottom edge while the panel's height goes from `from`
/// to `to`, the height of the frame being drawn: the offset rides on that
/// frame's commit, which brings the new size with it. Nothing changes on X11.
pub(super) fn keep_bottom(window: &Window, from: f32, to: f32) {
    // Whole pixels, as GPUI truncates the window geometry.
    let dy = from as i32 - to as i32;
    if dy == 0 {
        return;
    }
    if let Err(error) = crate::quick::wayland::offset(window, dy) {
        tracing::debug!(%error, "could not keep the panel's bottom edge");
    }
}

/// The shadow margin and the hairline inside it, for `decorations`.
fn frame_for(decorations: Decorations, margin: Edges<Pixels>) -> Edges<Pixels> {
    let Decorations::Client { tiling } = decorations else {
        return Edges::default();
    };
    let side = |margin: Pixels, tiled: bool| {
        if tiled { margin } else { margin + px(HAIRLINE) }
    };
    Edges {
        top: side(margin.top, tiling.top),
        right: side(margin.right, tiling.right),
        bottom: side(margin.bottom, tiling.bottom),
        left: side(margin.left, tiling.left),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Decorations, Edges, Tiling, px};

    use super::{blurs, frame_for};

    #[test]
    fn only_wayland_kde_is_trusted_to_blur() {
        assert!(blurs(true, "KDE"));
        assert!(!blurs(false, "KDE"));
        assert!(!blurs(true, "GNOME"));
        assert!(!blurs(true, "ubuntu:GNOME"));
        assert!(!blurs(false, ""));
    }

    #[test]
    fn a_client_decorated_window_is_framed_by_its_margin_and_hairline() {
        let margin = Edges::all(px(20.));
        assert_eq!(frame_for(Decorations::Server, margin), Edges::default());
        let untiled = Decorations::Client {
            tiling: Tiling::default(),
        };
        assert_eq!(frame_for(untiled, margin), Edges::all(px(21.)));
        // gpui-component draws no margin and no hairline on a tiled side.
        let tiled_top = Decorations::Client {
            tiling: Tiling {
                top: true,
                ..Tiling::default()
            },
        };
        let margin = Edges {
            top: px(0.),
            ..Edges::all(px(20.))
        };
        let frame = frame_for(tiled_top, margin);
        assert_eq!(frame.top, px(0.));
        assert_eq!(frame.bottom, px(21.));
    }
}
