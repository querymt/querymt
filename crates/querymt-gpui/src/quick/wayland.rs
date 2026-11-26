//! Requests GPUI does not make, sent on the `wl_surface` of one of its
//! windows: GPUI lends the display and the surface through raw window handles.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

/// `wl_surface.offset` (wl_surface version 5).
const OFFSET_SINCE: u32 = 5;

/// A window's surface, on a connection of ours to GPUI's display.
pub(super) struct Surface {
    pub(super) connection: Connection,
    pub(super) surface: WlSurface,
}

/// The Wayland surface of `window`; `None` when it is not a Wayland window.
pub(super) fn surface(window: &gpui_kit::Window) -> Result<Option<Surface>, String> {
    let (Ok(display), Ok(surface)) = (
        window.display_handle(),
        HasWindowHandle::window_handle(window),
    ) else {
        return Ok(None);
    };
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
        (display.as_raw(), surface.as_raw())
    else {
        return Ok(None);
    };

    // SAFETY: GPUI lends both handles for the lifetime of this window borrow.
    // The backend shares the display without owning it and has its own event
    // queue; callers drop it, and the imported surface, before returning.
    let (connection, surface_id) = unsafe {
        let backend = Backend::from_foreign_display(display.display.as_ptr().cast());
        let id = ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast());
        (Connection::from_backend(backend), id)
    };
    let surface = WlSurface::from_id(&connection, surface_id.map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(Some(Surface {
        connection,
        surface,
    }))
}

/// Move the window by `dy` with the next commit that changes its size.
/// Wayland gives a toplevel no position of its own; this offset is how a
/// client says which way the new size grows. Mutter applies it with the
/// commit that changes the window geometry (`finish_move_resize`).
/// Returns whether it was sent.
pub(super) fn offset(window: &gpui_kit::Window, dy: i32) -> Result<bool, String> {
    let Some(Surface {
        connection,
        surface,
    }) = surface(window)?
    else {
        return Ok(false);
    };
    // Older compositors only take an offset with a buffer, which GPUI's
    // renderer attaches.
    if surface.version() < OFFSET_SINCE {
        return Ok(false);
    }
    surface.offset(0, dy);
    connection.flush().map_err(|e| e.to_string())?;
    Ok(true)
}
