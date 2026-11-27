//! macOS: the panel's material is an AppKit view placed behind GPUI's own.
//!
//! GPUI's drawing view is a subview of the window's content view and draws
//! into its own layer, so a material view added as a sibling **below** it
//! shows through the transparent window. It cannot be a subview of GPUI's
//! view: that would cover the UI. Every step is a no-op when the view
//! hierarchy is not what is expected, and the panel then stays opaque.

use gpui_kit::{App, Pixels, Size, Window};
use objc2::msg_send;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSGlassEffectView, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowOrderingMode,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSProcessInfo, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;

use super::{Chrome, RADIUS};

thread_local! {
    /// The material view of the panel that is open. There is one panel at a
    /// time, and AppKit is only touched on the main thread. Weak: the view
    /// goes with its window.
    static MATERIAL: RefCell<Option<Weak<NSView>>> = const { RefCell::new(None) };
}

/// GPUI's drawing view.
pub(super) fn gpui_view(window: &Window) -> Option<&NSView> {
    // The inherent `Window::window_handle` is GPUI's own; this is the
    // `raw-window-handle` one.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: the handle is the window's drawing `NSView`, alive as long as
    // the window, which `window` borrows.
    Some(unsafe { handle.ns_view.cast::<NSView>().as_ref() })
}

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    gpui_view(window)?.window()
}

/// Liquid Glass exists from macOS 26. `QMT_MATERIAL=vfx` forces the older
/// visual-effect material, to compare the two on one Mac (task 7.19).
fn glass_available() -> bool {
    if std::env::var("QMT_MATERIAL").is_ok_and(|m| m == "vfx") {
        return false;
    }
    AnyClass::get(c"NSGlassEffectView").is_some()
        && NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            >= 26
}

/// System Settings > Accessibility > Display > Reduce transparency
/// (`NSWorkspace.accessibilityDisplayShouldReduceTransparency`). Called
/// through the runtime: the typed binding needs two more `objc2-app-kit`
/// features for one getter.
pub(super) fn reduce_transparency() -> bool {
    let Some(class) = AnyClass::get(c"NSWorkspace") else {
        return false;
    };
    // SAFETY: `sharedWorkspace` returns the shared instance, and the getter
    // returns a `BOOL`; both exist since macOS 10.10.
    unsafe {
        let workspace: *mut AnyObject = msg_send![class, sharedWorkspace];
        if workspace.is_null() {
            return false;
        }
        msg_send![workspace, accessibilityDisplayShouldReduceTransparency]
    }
}

/// Turn off the window's system shadow, with or without a material.
///
/// GPUI makes every window titled (`Titled | FullSizeContentView`) and gives a
/// transparent one a background of black at 0.0001 alpha, so the system
/// shadows the **whole window** in the system's own rounded outline, not what
/// the panel draws in it. Over a bright window that outline shows: during the
/// opening as a fixed light shape the panel grows into, and at rest as a sliver
/// at each corner, where the system's corner is squarer than the panel's
/// [`RADIUS`]. A no-op when it is not an AppKit window (a test window).
pub(super) fn drop_window_shadow(window: &Window) {
    if let Some(ns_window) = ns_window(window) {
        ns_window.setHasShadow(false);
    }
}

/// Add the material below GPUI's view and set it to the app's appearance
/// (`dark`: dark aqua, else aqua). `animate`: the window starts invisible, for
/// [`open_step`] to bring in.
pub(super) fn install(window: &mut Window, animate: bool, dark: bool) -> Chrome {
    let Some(mtm) = MainThreadMarker::new() else {
        return Chrome::solid();
    };
    let Some(view) = gpui_view(window) else {
        return Chrome::solid();
    };
    // SAFETY: a plain getter on a live view, on the main thread.
    let Some(parent) = (unsafe { view.superview() }) else {
        return Chrome::solid();
    };
    let frame = parent.bounds();
    let glass = glass_available();
    let material: Retained<NSView> = if glass {
        let glass = NSGlassEffectView::initWithFrame(mtm.alloc(), frame);
        glass.setCornerRadius(RADIUS as f64);
        Retained::into_super(glass)
    } else {
        let effect = NSVisualEffectView::initWithFrame(mtm.alloc(), frame);
        effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        // The default follows the window's active state, and a non-activating
        // panel never becomes the active window: the material would stay grey.
        effect.setState(NSVisualEffectState::Active);
        effect.setWantsLayer(true);
        // SAFETY: `layer` is a `CALayer` once the view wants one.
        unsafe {
            let layer: *mut AnyObject = msg_send![&*effect, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: RADIUS as f64];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
        Retained::into_super(effect)
    };
    material.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    parent.addSubview_positioned_relativeTo(&material, NSWindowOrderingMode::Below, None);
    MATERIAL.with(|slot| *slot.borrow_mut() = Some(Weak::from_retained(&material)));
    set_appearance(window, dark);
    if animate && let Some(ns_window) = ns_window(window) {
        // Before the first frame: the material would otherwise show at once,
        // ahead of the animation.
        ns_window.setAlphaValue(0.);
    }
    // Liquid Glass draws its own edge; a visual-effect view has none.
    Chrome::material(glass)
}

/// `bounds` scaled by `scale` around its centre.
pub(super) fn scaled(bounds: NSRect, scale: f64) -> NSRect {
    let (dx, dy) = (
        bounds.size.width * (1. - scale) / 2.,
        bounds.size.height * (1. - scale) / 2.,
    );
    NSRect::new(
        NSPoint::new(bounds.origin.x + dx, bounds.origin.y + dy),
        NSSize::new(bounds.size.width - 2. * dx, bounds.size.height - 2. * dy),
    )
}

/// One step of the opening animation (design D14): the window, and so the
/// material and the content together, at `opacity`, and the material at
/// `scale` of the window, as GPUI insets the content. The window has no
/// shadow to follow ([`drop_window_shadow`]).
///
/// Called while GPUI paints. None of these calls resizes the window, so AppKit
/// does not call back into GPUI (see [`set_height`]).
pub(super) fn open_step(window: &Window, opacity: f32, scale: f32) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.setAlphaValue(opacity.clamp(0., 1.) as f64);
    let material = MATERIAL.with(|slot| slot.borrow().as_ref().and_then(Weak::load));
    if let Some(material) = material
        // SAFETY: a plain getter on a live view, on the main thread.
        && let Some(parent) = (unsafe { material.superview() })
    {
        // At rest this is the parent's bounds again, which the autoresizing
        // mask then keeps through later resizes.
        material.setFrame(scaled(parent.bounds(), scale.min(1.) as f64));
    }
    // The light's view goes with the material.
    super::rim::open_step(scale.min(1.) as f64);
}

/// Set the window's appearance, and so the material's and everything in the
/// window that AppKit themes: dark aqua for a dark app theme, aqua for a light
/// one. The panel follows the app's theme, not the desktop's.
pub(super) fn set_appearance(window: &Window, dark: bool) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    // SAFETY: AppKit's constant appearance names.
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    ns_window.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
}

/// The frame the window takes for a `width` x `height` content size: the
/// **bottom edge stays fixed**, so a composer at the bottom does not move under
/// the cursor while the panel grows. AppKit's origin is the bottom left. If
/// that would push the top past the screen's visible area (under the menu bar),
/// the window moves down by just enough, and never below the visible area.
fn target_frame(frame: NSRect, visible: Option<NSRect>, width: f64, height: f64) -> NSRect {
    let mut origin = frame.origin;
    if let Some(visible) = visible {
        let top = visible.origin.y + visible.size.height;
        let over = origin.y + height - top;
        if over > 0. {
            origin.y = (origin.y - over).max(visible.origin.y);
        }
    }
    NSRect::new(origin, NSSize::new(width, height))
}

/// Low Power Mode is on.
pub(super) fn low_power() -> bool {
    NSProcessInfo::processInfo().isLowPowerModeEnabled()
}

/// Give the window `size`; false when it is not an AppKit window (a test
/// window), so the caller resizes it the generic way.
///
/// The change is **deferred to the foreground executor**, as GPUI's own
/// `resize` does. Resizing straight away, from inside a GPUI update (we are
/// called from a frame callback), makes AppKit call GPUI's resize handler while
/// the app is borrowed. That handler's `window.update` then fails, so GPUI keeps
/// laying out and painting at the old size while the window and its material
/// are already bigger: an empty blurred area. It is not animated for the same
/// reason: AppKit's animation runs the handler for every step, nested inside
/// whatever called us.
pub(super) fn set_height(window: &Window, size: Size<Pixels>, cx: &App) -> bool {
    let Some(ns_window) = ns_window(window) else {
        return false;
    };
    // Weak: the window may be closed before the task runs.
    let weak = Weak::from_retained(&ns_window);
    let (width, height) = (f32::from(size.width) as f64, f32::from(size.height) as f64);
    cx.foreground_executor()
        .spawn(async move {
            let Some(ns_window) = weak.load() else {
                return;
            };
            let visible = ns_window.screen().map(|screen| screen.visibleFrame());
            // Read now, not when scheduled: the window may have been moved.
            let target = target_frame(ns_window.frame(), visible, width, height);
            ns_window.setFrame_display(target, true);
            // The light's paths follow the new size.
            super::rim::relayout();
        })
        .detach();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_material_scales_around_the_centre_and_rests_on_the_bounds() {
        let bounds = rect(0., 0., 680., 100.);
        let small = scaled(bounds, 0.9);
        assert!((small.size.width - 612.).abs() < 1e-9);
        assert!((small.size.height - 90.).abs() < 1e-9);
        assert!((small.origin.x - 34.).abs() < 1e-9 && (small.origin.y - 5.).abs() < 1e-9);
        assert_eq!(scaled(bounds, 1.), bounds);
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    #[test]
    fn growing_keeps_the_bottom_edge() {
        let screen = rect(0., 0., 1440., 875.);
        let t = target_frame(rect(100., 300., 680., 260.), Some(screen), 680., 400.);
        assert_eq!(t.origin.y, 300.);
        assert_eq!(t.size.height, 400.);
        assert_eq!(t.origin.x, 100.);
    }

    #[test]
    fn growing_into_the_menu_bar_moves_the_window_down() {
        let screen = rect(0., 0., 1440., 875.);
        // 700 + 260 would end at 960, 85 above the visible area.
        let t = target_frame(rect(0., 700., 680., 175.), Some(screen), 680., 260.);
        assert_eq!(t.origin.y + t.size.height, 875.);
    }

    #[test]
    fn it_never_leaves_the_visible_area_at_the_bottom() {
        let screen = rect(0., 25., 1440., 800.);
        let t = target_frame(rect(0., 30., 680., 260.), Some(screen), 680., 2000.);
        assert_eq!(t.origin.y, 25.);
    }

    #[test]
    fn without_a_screen_only_the_size_changes() {
        let t = target_frame(rect(5., 6., 680., 260.), None, 680., 400.);
        assert_eq!((t.origin.x, t.origin.y), (5., 6.));
    }
}
