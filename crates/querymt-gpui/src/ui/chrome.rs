//! Application-owned header: brand, page widgets, connection status, theme
//! toggle, and window controls. GPUI Kit supplies the outer window frame.

use gpui_kit::assets::IconName;
use gpui_kit::component::{InteractiveElementExt as _, StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Page, QmtApp, kit};
use crate::theme::{Palette, palette};
use crate::util::workspace_name;

pub const HEADER_HEIGHT: Pixels = px(44.);
/// Gap between the title bar's columns.
const HEADER_GAP: Pixels = px(12.);
/// Inset of the title bar's right edge.
const HEADER_PAD_RIGHT: Pixels = px(8.);
const IS_WEB: bool = cfg!(target_family = "wasm");
const IS_LINUX: bool = cfg!(target_os = "linux");
const IS_MACOS: bool = cfg!(target_os = "macos");
const IS_WINDOWS: bool = cfg!(target_os = "windows");

/// Linux client-side decorations need application-owned header controls.
/// Session title bar geometry: the width of the brand's lead block, and the
/// extra base width the right column needs, so the title centres over the
/// page area (right of a `sidebar` wide sidebar) rather than the window.
///
/// The left spacer starts at `pad_left + lead + HEADER_GAP` and the right
/// column ends `HEADER_PAD_RIGHT` from the window edge; both flex equally.
/// The lead puts the spacer's start `HEADER_PAD_RIGHT` past the sidebar. When
/// the left padding alone already reaches further (macOS window buttons over
/// the collapsed rail), the lead is empty and the right column gets the
/// difference instead.
fn session_title_insets(sidebar: Pixels, pad_left: Pixels) -> (Pixels, Pixels) {
    let lead = sidebar + HEADER_PAD_RIGHT - HEADER_GAP - pad_left;
    (lead.max(px(0.)), (-lead).max(px(0.)))
}

fn client_tiling(window: &Window) -> Option<Tiling> {
    match window.window_decorations() {
        Decorations::Client { tiling } if IS_LINUX => Some(tiling),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Header

#[derive(Clone, Copy)]
enum Control {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn window_control(control: Control, p: &Palette) -> impl IntoElement {
    let (id, icon, area) = match control {
        Control::Minimize => ("win-min", IconName::WindowMinimize, WindowControlArea::Min),
        Control::Maximize => ("win-max", IconName::WindowMaximize, WindowControlArea::Max),
        Control::Restore => (
            "win-restore",
            IconName::WindowRestore,
            WindowControlArea::Max,
        ),
        Control::Close => ("win-close", IconName::WindowClose, WindowControlArea::Close),
    };
    let close = matches!(control, Control::Close);
    let hover_bg = if close { p.danger } else { p.card_hover };
    let hover_fg = if close {
        if p.dark { p.bg } else { hsla(0., 0., 1., 1.) }
    } else {
        p.text
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(28.))
        .rounded_full()
        .cursor_pointer()
        .text_color(p.muted)
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .when(IS_WINDOWS, |el| el.window_control_area(area))
        .when(IS_LINUX, |el| {
            el.on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                match control {
                    Control::Minimize => window.minimize_window(),
                    Control::Maximize | Control::Restore => window.zoom_window(),
                    Control::Close => window.remove_window(),
                }
            })
        })
        // ~15% larger than before (14px), within the title bar's 25% budget.
        .child(gpui_kit::component::Icon::new(icon).size(px(16.)))
}

impl QmtApp {
    /// Widgets the current page contributes to the header.
    fn header_slot(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.page {
            Page::Start => {
                let cwd = self.cwd.read(cx).value().trim().to_string();
                let store = self.store.read(cx);
                let agent = store
                    .primary_agent()
                    .and_then(|id| store.config(&id).map(|c| c.name.clone()));
                let model = store
                    .primary_agent()
                    .and_then(|id| store.launch_model(&id).map(|m| m.label.clone()));
                let mut out = vec![
                    h_flex()
                        .gap_1p5()
                        .px_2()
                        .py_0p5()
                        .rounded(px(6.))
                        .bg(p.card)
                        .text_xs()
                        .child(kit::icon(IconName::FolderOpen, 12., p.dim))
                        .child(
                            div()
                                .text_color(if cwd.is_empty() { p.dim } else { p.text })
                                .child(if cwd.is_empty() {
                                    "No workspace".to_string()
                                } else {
                                    workspace_name(&cwd)
                                }),
                        )
                        .when_some(agent, |el, a| {
                            el.child(div().text_color(p.dim).child(format!("on {a}")))
                        })
                        .into_any_element(),
                ];
                if let Some(model) = model {
                    out.push(kit::chip(model, p).into_any_element());
                }
                out
            }
            // Page actions live in each page's own header (`kit::page_header`).
            _ => vec![],
        }
    }

    pub(super) fn render_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = *palette(cx);
        let dark = p.dark;
        let client_decorated = client_tiling(window).is_some();
        let show_controls = (IS_LINUX && client_decorated) || IS_WINDOWS;
        let controls = window.window_controls();
        let maximized = window.is_maximized();
        let page_title = match self.page {
            Page::Start => "Start",
            Page::Inbox => "Inbox",
            Page::Sessions => "Sessions",
            Page::Agents => "Agents",
            Page::Mesh => "Mesh",
            Page::Automations => "Automations",
            Page::Session => "Session",
            Page::Settings => "Settings",
        };
        let slot = self.header_slot(&p, cx);
        // The open session owns the title bar: back + status + title centred,
        // its actions on the right where the connection status used to be.
        let (session_title, session_actions) = match self.page {
            Page::Session => self
                .session_chrome(&p, cx)
                .map(|(t, r)| (Some(t), r))
                .unwrap_or((None, vec![])),
            _ => (None, vec![]),
        };
        let is_session = session_title.is_some();
        let page_title_center = session_title;
        let page_title_left = div().text_sm().text_color(p.muted).child(page_title);
        let narrow = window.viewport_size().width < px(760.);
        let pad_left = if IS_MACOS && !window.is_fullscreen() {
            px(80.)
        } else {
            px(14.)
        };
        // Session: the title group centres over the page area (right of the
        // sidebar), where the transcript column is centred, not over the
        // whole window. The brand sits in a lead block as wide as the
        // sidebar; the two flex columns after it then mirror each other
        // inside the page. The lead's width puts the left column's start
        // `HEADER_PAD_RIGHT` past the sidebar (minus the bar's gap), the same
        // inset the bar keeps on the right, so the centre lands exactly on
        // the page's. The block keeps that width even when the brand is wider
        // (collapsed rail): the brand overflows into the left spacer, which
        // keeps at least that overflow free, so the title never covers it.
        let (lead_w, right_extra) = session_title_insets(self.sidebar_width(cx), pad_left);
        let wordmark = !(is_session && narrow);
        let brand_overflow = if is_session {
            let mut brand_w = px(22.);
            if wordmark {
                let text = SharedString::from("QueryMT");
                let run = TextRun {
                    len: text.len(),
                    font: Font {
                        weight: FontWeight::SEMIBOLD,
                        ..font(gpui_kit::component::Theme::global(cx).font_family.clone())
                    },
                    color: p.text,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let size = window.rem_size() * 0.875;
                // `gap_2` between mark and wordmark, then the shaped text.
                brand_w += window.rem_size() * 0.5
                    + window
                        .text_system()
                        .shape_line(text, size, &[run], None)
                        .width();
            }
            (brand_w - lead_w).max(px(0.))
        } else {
            px(0.)
        };

        let brand = h_flex()
            .gap_2()
            .flex_none()
            .child(
                div()
                    .size(px(22.))
                    .rounded(px(6.))
                    .bg(p.accent)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        svg()
                            .path(if dark {
                                "brand/querymt-dark.svg"
                            } else {
                                "brand/querymt.svg"
                            })
                            .size(px(19.))
                            // GPUI renders SVGs as masks, so the tint must match the theme too.
                            .text_color(if dark {
                                rgb(0xfff7ed).into()
                            } else {
                                hsla(0., 0., 0.08, 1.)
                            }),
                    ),
            )
            // Narrow session windows drop the wordmark to give the title room.
            .when(wordmark, |el| {
                el.child(div().text_sm().font_semibold().child("QueryMT"))
            })
            .when(!is_session, |el| {
                el.child(div().text_sm().text_color(p.border_strong).child("/"))
            });

        h_flex()
            .id("app-header")
            .flex_none()
            .h(HEADER_HEIGHT)
            .w_full()
            .gap(HEADER_GAP)
            .pr(HEADER_PAD_RIGHT)
            .pl(pad_left)
            // No background here: the root paints a rounded `p.bg` underneath,
            // and GPUI's overflow mask is rectangular, so a square quad flush
            // with the top would overpaint the frame's rounded corners.
            // The whole bar is the drag handle. Controls inside it claim their
            // press with `prevent_default` (as GPUI Kit's `Button` does) instead
            // of stopping propagation, and the bar ignores claimed presses.
            .when(!IS_WEB, |el| {
                el.window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window: &mut Window, _| {
                            this.header_press_claimed = window.default_prevented();
                            this.header_drag = !this.header_press_claimed;
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.header_drag = false),
                    )
                    .on_mouse_down_out(cx.listener(|this, _, _, _| this.header_drag = false))
                    .on_mouse_move(cx.listener(|this, _, window, _| {
                        if this.header_drag {
                            this.header_drag = false;
                            window.start_window_move();
                        }
                    }))
                    // Clicks on controls bubble here too; only the bar itself zooms.
                    .on_double_click(cx.listener(|this, _, window, _| {
                        if this.header_press_claimed {
                            return;
                        }
                        if IS_MACOS {
                            window.titlebar_double_click();
                        } else {
                            window.zoom_window();
                        }
                    }))
            })
            .when(IS_LINUX && client_decorated, |el| {
                el.on_mouse_down(MouseButton::Right, |ev, window, _| {
                    window.show_window_menu(ev.position)
                })
            })
            // Session: two columns sharing one flex basis around the title, so
            // it is centred on the page area rather than on leftover space.
            // The sides never shrink below their content (no `min_w_0`): when
            // the actions need the room, the title moves left, and only then
            // gives way and truncates; icons never slide under each other.
            // Other pages keep the `QueryMT / Page` breadcrumb on the left.
            .map(|el| {
                if is_session {
                    el.child(h_flex().flex_none().w(lead_w).child(brand))
                        .child(div().flex_1().min_w(brand_overflow))
                } else {
                    el.child(
                        h_flex()
                            .flex_1()
                            .gap_3()
                            .child(brand)
                            .child(page_title_left)
                            .child(h_flex().gap_1p5().children(slot)),
                    )
                }
            })
            // The title stays a drag handle; it truncates before the sides shrink.
            .children(page_title_center.map(|title| {
                h_flex()
                    .flex_initial()
                    .max_w(relative(0.5))
                    .min_w_0()
                    .overflow_hidden()
                    .justify_center()
                    .child(title)
            }))
            .child(
                h_flex()
                    .flex_1()
                    .justify_end()
                    .gap_1()
                    // Leading padding adds to this column's flex base, so it
                    // takes `right_extra` more than the left spacer: that
                    // offsets a left side pushed past the sidebar edge.
                    .when(is_session, |el| el.pl(right_extra))
                    .children(session_actions)
                    .child(
                        kit::title_icon_button(
                            "hdr-theme",
                            if dark { IconName::Sun } else { IconName::Moon },
                            "Toggle theme",
                            &p,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.store.update(cx, |s, _| s.set_dark(!dark));
                                this.apply_theme(window, cx);
                            },
                        )),
                    )
                    .when(show_controls, |el| {
                        el.child(
                            h_flex()
                                .gap_1()
                                // 8px before minimise (gap 4 + 4), +50% → 12px.
                                .pl_2()
                                .when(controls.minimize, |el| {
                                    el.child(window_control(Control::Minimize, &p))
                                })
                                .when(controls.maximize, |el| {
                                    el.child(window_control(
                                        if maximized {
                                            Control::Restore
                                        } else {
                                            Control::Maximize
                                        },
                                        &p,
                                    ))
                                })
                                .child(window_control(Control::Close, &p)),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{HEADER_GAP, HEADER_PAD_RIGHT, session_title_insets};
    use gpui_kit::{Pixels, px};

    /// Centre of the title between the two equal flex columns, for a window
    /// `width` wide: halfway between the left spacer's start and the right
    /// column's end once its extra base width is taken off.
    fn title_centre(sidebar: f32, pad_left: f32, width: f32) -> Pixels {
        let (lead, extra) = session_title_insets(px(sidebar), px(pad_left));
        let start = px(pad_left) + lead + HEADER_GAP;
        let end = px(width) - HEADER_PAD_RIGHT - extra;
        (start + end) / 2.
    }

    #[test]
    fn session_title_centres_over_the_page_area() {
        // Expanded sidebar, collapsed rail, and macOS window buttons over the rail.
        for (sidebar, pad_left) in [(253., 14.), (56., 14.), (253., 80.), (56., 80.)] {
            for width in [1300., 1960.] {
                let page_centre = px((sidebar + width) / 2.);
                assert_eq!(title_centre(sidebar, pad_left, width), page_centre);
            }
        }
    }
}
