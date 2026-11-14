//! Image preview (lightbox) for attachments, ported from the desktop app's
//! `SessionAttachmentPreview`: click a thumbnail in the composer or in a chat
//! bubble to open it fitted to the window. Ctrl+wheel zooms (50–300%), the
//! arrow keys step through the gallery, Escape / the backdrop / ✕ close it.

use std::sync::Arc;

use base64::Engine as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::{StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::chrome::HEADER_HEIGHT;
use super::{QmtApp, kit};
use crate::session::{ItemKind, UserAttachment};
use crate::store::Attachment;
use crate::theme::Palette;
use crate::util::human_size;

const MIN_ZOOM: f32 = 0.5;
const MAX_ZOOM: f32 = 3.;
const ZOOM_STEP: f32 = 0.1;
/// Wheel travel (px) per zoom step, like the desktop's accumulated `deltaY`.
const WHEEL_STEP: f32 = 24.;
/// Lightbox inset from the window edges (below the title bar at the top).
const INSET: f32 = 16.;
/// Space kept above / below the image for the filename row and zoom label.
const TOP_BAR: f32 = 48.;
const BOTTOM_BAR: f32 = 44.;

pub(super) struct PreviewImage {
    pub name: String,
    pub image: Arc<Image>,
}

pub(super) struct Preview {
    images: Vec<PreviewImage>,
    index: usize,
    zoom: f32,
    wheel: f32,
    scroll: ScrollHandle,
}

/// Fit `natural` into `stage` without upscaling (desktop `calculateImageFit`).
pub(super) fn fit_scale(natural: (f32, f32), stage: (f32, f32)) -> Option<f32> {
    let ((w, h), (sw, sh)) = (natural, stage);
    (w > 0. && h > 0. && sw > 0. && sh > 0.).then(|| (sw / w).min(sh / h).min(1.))
}

pub(super) fn step_zoom(zoom: f32, up: bool) -> f32 {
    let next = if up { zoom + ZOOM_STEP } else { zoom - ZOOM_STEP };
    ((next * 100.).round() / 100.).clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Decoded image of a transcript attachment, when it carries inline bytes.
pub(super) fn user_attachment_image(a: &UserAttachment) -> Option<Arc<Image>> {
    let data = a.data.as_deref().filter(|_| a.is_image())?;
    let format = ImageFormat::from_mime_type(&a.mime)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

impl QmtApp {
    pub(super) fn open_preview(&mut self, images: Vec<PreviewImage>, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if images.is_empty() {
            return;
        }
        let index = index.min(images.len() - 1);
        self.preview = Some(Preview { images, index, zoom: 1., wheel: 0., scroll: ScrollHandle::new() });
        // Keys (arrows, Escape) go to the lightbox while it is open.
        self.preview_focus.focus(window, cx);
        cx.notify();
    }

    /// Open the composer's image attachment `id`, with the composer's images as the gallery.
    fn open_composer_image(&mut self, session: bool, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let list = if session { &self.session_attachments } else { &self.attachments };
        let images: Vec<_> = list.iter().filter_map(|a| Some((a.id.clone(), PreviewImage { name: a.name.clone(), image: a.image.clone()? }))).collect();
        let index = images.iter().position(|(aid, _)| aid == id).unwrap_or(0);
        self.open_preview(images.into_iter().map(|(_, i)| i).collect(), index, window, cx);
    }

    /// Open attachment `ix` of transcript item `key`; every inline image of
    /// the session's user messages is the gallery, like the desktop.
    pub(super) fn open_chat_image(&mut self, key: &str, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut images = Vec::new();
        let mut start = 0;
        if let Some(view) = self.store.read(cx).session.as_ref() {
            for item in &view.items {
                let ItemKind::User { attachments, .. } = &item.kind else { continue };
                for (i, a) in attachments.iter().enumerate() {
                    let Some(image) = user_attachment_image(a) else { continue };
                    if item.key == key && i == ix {
                        start = images.len();
                    }
                    images.push(PreviewImage { name: a.name.clone(), image });
                }
            }
        }
        self.open_preview(images, start, window, cx);
    }

    fn preview_step(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(pv) = self.preview.as_mut() else { return };
        let n = pv.images.len() as isize;
        if n < 2 {
            return;
        }
        pv.index = (pv.index as isize + offset).rem_euclid(n) as usize;
        pv.zoom = 1.;
        pv.wheel = 0.;
        pv.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn preview_wheel(&mut self, dy: f32, cx: &mut Context<Self>) {
        let Some(pv) = self.preview.as_mut() else { return };
        if dy == 0. {
            return;
        }
        if pv.wheel != 0. && pv.wheel.signum() != dy.signum() {
            pv.wheel = 0.;
        }
        pv.wheel += dy;
        if pv.wheel.abs() >= WHEEL_STEP {
            // Positive y is a wheel turned away from the user: zoom in.
            pv.zoom = step_zoom(pv.zoom, pv.wheel > 0.);
            pv.wheel = 0.;
            cx.notify();
        }
    }

    pub(super) fn close_preview(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.preview.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    /// Composer attachment strip: image thumbnails (click to preview) and file chips.
    pub(super) fn attachment_strip(&self, session: bool, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let list = if session { &self.session_attachments } else { &self.attachments };
        (!list.is_empty()).then(|| {
            h_flex().flex_wrap().items_end().gap_1p5().px_1().children(list.iter().map(|a| {
                let remove = {
                    let id = a.id.clone();
                    cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        let list = if session { &mut this.session_attachments } else { &mut this.attachments };
                        list.retain(|a| a.id != id);
                        cx.notify();
                    })
                };
                let tip = format!("{} · {}", a.name, human_size(a.size));
                match a.image.clone() {
                    Some(image) => composer_thumb(a, image, session, tip, remove, p, cx).into_any_element(),
                    None => h_flex()
                        .id(kit::keyed("att-file", a.id.clone()))
                        .gap_1p5()
                        .pl_2()
                        .pr_1()
                        .py_0p5()
                        .rounded(px(8.))
                        .bg(p.card)
                        .text_xs()
                        .child(kit::icon(if a.is_image() { IconName::Image } else { IconName::FileText }, 13., p.muted))
                        .child(div().max_w(px(200.)).truncate().font_medium().child(a.name.clone()))
                        .child(div().text_color(p.dim).child(human_size(a.size)))
                        .child(remove_button(kit::keyed("rm-attach-file", a.id.clone()), 18., p.muted, remove, p))
                        .into_any_element(),
                }
            }))
        })
    }

    /// Full-window lightbox; `None` while no preview is open.
    pub(super) fn render_preview(&mut self, radii: Corners<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pv = self.preview.as_ref()?;
        let current = pv.images.get(pv.index)?;
        let (count, index, zoom) = (pv.images.len(), pv.index, pv.zoom);
        let scroll = pv.scroll.clone();
        let image = current.image.clone();
        let name = current.name.clone();

        // Natural size once decoded (re-renders when ready), fitted to the stage.
        let natural = image.clone().use_render_image(window, cx).map(|r| {
            let s = r.size(0);
            (s.width.0 as f32, s.height.0 as f32)
        });
        let viewport = window.viewport_size();
        let stage = (
            f32::from(viewport.width) - INSET * 2.,
            f32::from(viewport.height) - f32::from(HEADER_HEIGHT) - INSET * 2. - TOP_BAR - BOTTOM_BAR,
        );
        let rendered = natural.and_then(|n| fit_scale(n, stage).map(|f| (n.0 * f * zoom, n.1 * f * zoom)));

        let weak = cx.entity().downgrade();
        let pill = |el: Div| el.px_2p5().py_1p5().rounded(px(8.)).bg(rgba(0x18181bb3)).text_color(white());
        let round = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .bg(rgba(0x18181bb3))
                .hover(|s| s.bg(rgba(0x18181be0)))
                .child(kit::icon(icon, 18., white()))
                .tooltip(move |window, cx| kit::wrap_tip(tip).build(window, cx))
        };

        let picture = match rendered {
            Some((w, h)) => img(image)
                .id("preview-image")
                .flex_none()
                .w(px(w))
                .h(px(h))
                .object_fit(ObjectFit::Fill)
                .on_click(|_, _, cx| cx.stop_propagation())
                .into_any_element(),
            None => kit::spinner("preview-loading", 20., white()).into_any_element(),
        };

        let lightbox = div()
            .absolute()
            .top(HEADER_HEIGHT + px(INSET))
            .left(px(INSET))
            .right(px(INSET))
            .bottom(px(INSET))
            // Stage: scrolls once zoomed past the window; clicking around the image closes.
            .child(
                div()
                    .id("preview-stage")
                    .absolute()
                    .top(px(TOP_BAR))
                    .bottom(px(BOTTOM_BAR))
                    .left_0()
                    .right_0()
                    .overflow_scroll()
                    .track_scroll(&scroll)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_preview(cx);
                    }))
                    .child(div().min_w_full().min_h_full().flex().items_center().justify_center().child(picture)),
            )
            .child(
                h_flex()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right(px(48.))
                    .child(pill(h_flex().max_w_full().min_w_0().gap_2().text_sm().font_medium())
                        .child(div().min_w_0().truncate().child(name))
                        .when(count > 1, |el| el.child(div().flex_none().text_color(white().opacity(0.7)).child(format!("{} / {count}", index + 1))))),
            )
            .child(
                div().absolute().top_0().right_0().child(round("preview-close", IconName::X, "Close image preview").on_click(cx.listener(|this, _, _, cx| {
                    this.close_preview(cx);
                }))),
            )
            .when(count > 1, |el| {
                el.child(
                    div().absolute().left_0().top(px(TOP_BAR)).bottom(px(BOTTOM_BAR)).flex().items_center().child(
                        round("preview-prev", IconName::ChevronLeft, "Previous image (←)").on_click(cx.listener(|this, _, _, cx| {
                            // Sibling clicks also reach the backdrop, which closes.
                            cx.stop_propagation();
                            this.preview_step(-1, cx)
                        })),
                    ),
                )
                .child(
                    div().absolute().right_0().top(px(TOP_BAR)).bottom(px(BOTTOM_BAR)).flex().items_center().child(
                        round("preview-next", IconName::ChevronRight, "Next image (→)").on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.preview_step(1, cx)
                        })),
                    ),
                )
            })
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .child(pill(div().text_xs().font_semibold()).child(format!("{}%", (zoom * 100.).round()))),
            );

        Some(
            div()
                .id("preview-backdrop")
                .track_focus(&self.preview_focus)
                .absolute()
                .inset_0()
                .rounded_tl(radii.top_left)
                .rounded_tr(radii.top_right)
                .rounded_bl(radii.bottom_left)
                .rounded_br(radii.bottom_right)
                .bg(black().opacity(0.78))
                .occlude()
                // Ctrl+wheel zooms. A capture-phase listener runs before the
                // stage's own scrolling (and sees the smooth-scroll layer's
                // re-dispatched steps, which keep the modifiers).
                .child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            let weak = weak.clone();
                            let line = window.line_height();
                            window.on_mouse_event(move |ev: &ScrollEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture || !(ev.modifiers.control || ev.modifiers.platform) {
                                    return;
                                }
                                cx.stop_propagation();
                                let dy: f32 = ev.delta.pixel_delta(line).y.into();
                                let _ = weak.update(cx, |this, cx| this.preview_wheel(dy, cx));
                            });
                        },
                    )
                    .absolute()
                    .size_0(),
                )
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| match ev.keystroke.key.as_str() {
                    "left" => this.preview_step(-1, cx),
                    "right" => this.preview_step(1, cx),
                    "escape" => {
                        this.close_preview(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_preview(cx);
                }))
                .child(lightbox)
                .into_any_element(),
        )
    }
}

/// 56px image thumbnail with a corner remove button (desktop `session-image-preview`).
#[allow(clippy::too_many_arguments)]
fn composer_thumb(
    a: &Attachment,
    image: Arc<Image>,
    session: bool,
    tip: String,
    remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    p: &Palette,
    cx: &mut Context<QmtApp>,
) -> impl IntoElement {
    let id = a.id.clone();
    let (border, hover) = (p.border, p.accent);
    div()
        .id(kit::keyed("att-thumb", a.id.clone()))
        .relative()
        .flex_none()
        .size(px(56.))
        .rounded(px(10.))
        .border_1()
        .border_color(border)
        .bg(p.card)
        .cursor_pointer()
        .hover(move |s| s.border_color(hover))
        .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
        .on_click(cx.listener(move |this, _, window, cx| this.open_composer_image(session, &id, window, cx)))
        .child(div().size_full().rounded(px(9.)).overflow_hidden().child(img(image).size_full().rounded(px(9.)).object_fit(ObjectFit::Cover)))
        // Desktop: inside the thumbnail's top-right corner, so the composer
        // never clips it; as the last child it paints over the image.
        .child(div().absolute().top(px(2.)).right(px(2.)).child(remove_button(kit::keyed("att-thumb-rm", a.id.clone()), 18., p.text, remove, p)))
}

/// Opaque round remove button (desktop `.session-attachment-remove`): a solid
/// panel fill and border, so nothing behind it shows through.
fn remove_button(
    id: impl Into<ElementId>,
    size: f32,
    fg: Hsla,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    p: &Palette,
) -> Stateful<Div> {
    let (hover_bg, hover_fg) = (p.card_hover, p.text);
    div()
        .id(id)
        .flex_none()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .shadow(vec![BoxShadow { color: hsla(0., 0., 0., 0.3), offset: point(px(0.), px(1.)), blur_radius: px(3.), spread_radius: px(0.), inset: false }])
        .text_color(fg)
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .child(gpui_kit::component::Icon::new(IconName::X).size(px(11.)))
        .tooltip(|window, cx| kit::wrap_tip("Remove").build(window, cx))
        .on_click(on_click)
}

#[cfg(test)]
mod tests {
    use super::{fit_scale, step_zoom};

    #[test]
    fn fits_without_upscaling() {
        assert_eq!(fit_scale((2000., 1000.), (1000., 1000.)), Some(0.5));
        assert_eq!(fit_scale((100., 100.), (1000., 1000.)), Some(1.));
        assert_eq!(fit_scale((0., 10.), (100., 100.)), None);
    }

    #[test]
    fn zoom_steps_and_clamps() {
        assert_eq!(step_zoom(1., true), 1.1);
        assert_eq!(step_zoom(0.5, false), 0.5);
        assert_eq!(step_zoom(3., true), 3.);
        assert_eq!(step_zoom(1.1, false), 1.);
    }
}
