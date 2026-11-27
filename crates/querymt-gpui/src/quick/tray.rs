//! The tray icon that keeps the app reachable while its windows are closed
//! (design D9): Open QueryMT, Quick panel, Quit.

use async_channel::Sender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::Signal;

/// The app icon, 256 px, as the tray shows it.
const ICON_PNG: &[u8] = include_bytes!("../../packaging/icons/128x128@2x.png");

/// The icon in the system tray. It goes away when this is dropped.
pub(crate) struct Tray {
    _icon: TrayIcon,
}

fn icon() -> Result<Icon, String> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(ICON_PNG))
        .read_info()
        .map_err(|e| e.to_string())?;
    let mut rgba = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut rgba).map_err(|e| e.to_string())?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return Err("the tray icon must be 8-bit RGBA".into());
    }
    rgba.truncate(frame.buffer_size());
    Icon::from_rgba(rgba, frame.width, frame.height).map_err(|e| e.to_string())
}

impl Tray {
    /// Create the icon and its menu. Must run on the main thread. Menu
    /// choices arrive as signals.
    pub(crate) fn new(signals: Sender<Signal>) -> Result<Self, String> {
        let open = MenuItem::new("Open QueryMT", true, None);
        let quick = MenuItem::new("Quick panel", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        for item in [&open, &quick, &quit] {
            menu.append(item).map_err(|e| e.to_string())?;
        }
        let (open_id, quick_id, quit_id) =
            (open.id().clone(), quick.id().clone(), quit.id().clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let signal = if *event.id() == open_id {
                Signal::OpenMain {
                    // ksni only exposes the token while handling this click.
                    #[cfg(target_os = "linux")]
                    activation_token: ksni::current_xdg_activation_token(),
                    #[cfg(not(target_os = "linux"))]
                    activation_token: None,
                }
            } else if *event.id() == quick_id {
                Signal::TogglePanel
            } else if *event.id() == quit_id {
                Signal::Quit
            } else {
                return;
            };
            // On the main thread the handler must not wait for room; a full
            // queue already holds requests for the app.
            let _ = signals.try_send(signal);
        }));
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon()?)
            .with_tooltip("QueryMT")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { _icon: tray })
    }
}

pub(super) fn activate_window(window: &gpui_kit::Window, activation_token: Option<String>) {
    #[cfg(target_os = "linux")]
    if let Some(token) = activation_token.filter(|token| !token.is_empty()) {
        match wayland::activate(window, token) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => tracing::debug!(%error, "could not use the tray activation token"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = activation_token;
    window.activate_window();
}

#[cfg(target_os = "linux")]
mod wayland {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{
        Request, XdgActivationV1,
    };

    use crate::quick::wayland::{Surface, surface};

    struct Activation;
    delegate_noop!(Activation: ignore XdgActivationV1);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Activation {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    pub(super) fn activate(window: &gpui_kit::Window, token: String) -> Result<bool, String> {
        let Some(Surface {
            connection,
            surface,
        }) = surface(window)?
        else {
            return Ok(false);
        };
        let (globals, queue) =
            registry_queue_init::<Activation>(&connection).map_err(|e| e.to_string())?;
        let activation = globals.bind::<XdgActivationV1, _, _>(&queue.handle(), 1..=1, ());
        globals.destroy();
        let activation = activation.map_err(|e| e.to_string())?;
        let sent = activation.send_request(Request::Activate { token, surface });
        activation.destroy();
        connection.flush().map_err(|e| e.to_string())?;
        sent.map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_icon_decodes() {
        assert!(super::icon().is_ok());
    }
}
