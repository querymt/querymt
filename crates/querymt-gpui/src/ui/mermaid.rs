//! Mermaid diagrams in transcript Markdown (change `render-mermaid-diagrams`).
//!
//! `limu` parses and lays a diagram out; [`paint`] draws the laid-out scene
//! with GPUI. Every call into limu lives in this module so an API change in a
//! 0.x release stays contained.
//!
//! - Layout runs once per (source, fonts) and is cached ([`scene`]); a failed
//!   parse is cached too, so a broken diagram is not re-parsed every frame.
//! - [`DiagramElement`] knows its size from the cached scene at first layout
//!   (so the transcript never jumps), scales down to fit the column and never
//!   up, and paints with the current palette, so a theme switch only recolors.
//! - [`look`] maps the app palette onto limu's theme and ignores colors the
//!   diagram's author set.

mod paint;
mod svg;

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui_kit::component::Theme;
use gpui_kit::*;
use limu::scene::Scene;
use limu::scene::style::{Color, Look, Theme as DiagramTheme};

pub(crate) use paint::Fonts;

use crate::theme::Palette;

/// Layouts kept, least recently used first out (shaped like `code.rs`).
const CACHE_LIMIT: usize = 128;
/// Size of text with no size of its own in a diagram.
const FONT_SIZE: f64 = 14.0;
/// Family of interface text; diagrams are drawn in it.
const SANS: &str = "IBM Plex Sans";

/// Why a closed `mermaid` fence is shown as code instead of a diagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The source does not parse (or asks for what cannot be drawn).
    Syntax,
    /// The diagram type is not one limu reads; names it where known.
    Unsupported(Option<String>),
    /// Over the renderer's size limits.
    TooLarge,
}

impl Unavailable {
    /// The reason in the `Diagram not drawn: <reason>` note.
    pub(crate) fn reason(&self) -> String {
        match self {
            Unavailable::Syntax => "syntax error".to_string(),
            Unavailable::Unsupported(Some(name)) => format!("unsupported diagram type `{name}`"),
            Unavailable::Unsupported(None) => "unsupported diagram type".to_string(),
            Unavailable::TooLarge => "diagram too large".to_string(),
        }
    }
}

/// A laid-out diagram, or why there is none.
pub(crate) type Layout = Result<SharedScene, Unavailable>;

/// A laid-out diagram, shared with the cache.
pub(crate) type SharedScene = Rc<Scene>;

#[derive(Default)]
struct Cache {
    entries: HashMap<u64, Layout>,
    /// Keys, least recently used first.
    order: Vec<u64>,
    /// Layouts computed (cache misses), for tests.
    #[cfg(test)]
    runs: usize,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// The families diagrams are measured and drawn in.
pub(crate) fn fonts(cx: &App) -> Fonts {
    Fonts {
        sans: SANS.into(),
        mono: Theme::global(cx).mono_font_family.clone(),
    }
}

fn key(source: &str, fonts: &Fonts) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    fonts.hash(&mut h);
    FONT_SIZE.to_bits().hash(&mut h);
    h.finish()
}

/// The laid-out diagram of `source` in the app fonts: from the cache, or
/// parsed and laid out now (measuring text needs the window).
pub(crate) fn scene(source: &str, window: &Window, cx: &App) -> Layout {
    let fonts = fonts(cx);
    let key = key(source, &fonts);
    let hit = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let found = c.entries.get(&key).cloned();
        if found.is_some() {
            c.order.retain(|k| *k != key);
            c.order.push(key);
        }
        found
    });
    if let Some(layout) = hit {
        return layout;
    }
    let layout = compute(source, paint::TextMeasurer::new(window, fonts));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        #[cfg(test)]
        {
            c.runs += 1;
        }
        if c.entries.len() >= CACHE_LIMIT && !c.order.is_empty() {
            let oldest = c.order.remove(0);
            c.entries.remove(&oldest);
        }
        c.entries.insert(key, layout.clone());
        c.order.push(key);
    });
    layout
}

/// Parse and lay out, mapping limu's errors to [`Unavailable`].
fn compute(source: &str, measurer: paint::TextMeasurer) -> Layout {
    let options = limu::layout::Options {
        measurer: &measurer,
        font_size: FONT_SIZE,
        ..limu::layout::Options::default()
    };
    limu::parse(source, &limu::ParseOptions::default())
        .and_then(|d| d.layout(&options))
        .map(Rc::new)
        .map_err(|e| match e.code {
            limu::ErrorCode::UnsupportedDiagram => Unavailable::Unsupported(
                e.mermaid_type
                    .map(str::to_string)
                    .or_else(|| first_word(source)),
            ),
            limu::ErrorCode::Limit => Unavailable::TooLarge,
            limu::ErrorCode::Syntax | limu::ErrorCode::Semantic => Unavailable::Syntax,
            #[allow(unreachable_patterns)]
            _ => Unavailable::Syntax,
        })
}

/// The first word of the first line that is not blank, a comment or front
/// matter: what names the diagram type.
fn first_word(source: &str) -> Option<String> {
    let mut in_front = false;
    for line in source.lines().map(str::trim) {
        if line == "---" {
            in_front = !in_front;
            continue;
        }
        if in_front || line.is_empty() || line.starts_with("%%") {
            continue;
        }
        return line
            .split(|c: char| c.is_whitespace() || c == ';')
            .next()
            .map(str::to_string);
    }
    None
}

fn color(c: Hsla) -> Color {
    let c = Rgba::from(c);
    let b = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    Color::new(b(c.r), b(c.g), b(c.b), b(c.a))
}

/// limu's look for the app palette: the palette's colors only (an author's
/// colors are tuned for a white page and can be unreadable in dark mode).
pub(crate) fn look(p: &Palette) -> Look {
    let theme = DiagramTheme {
        // Also what edge labels are backed with, so it is the card's own fill.
        bg: color(p.card),
        fg: color(p.text),
        muted: color(p.muted),
        line: color(p.muted),
        surface: color(p.surface),
        surface_alt: color(p.card_hover),
        border: color(p.border_strong),
        accent: color(p.accent),
        critical: color(p.danger),
        series: [
            color(p.accent),
            color(p.success),
            color(p.running),
            color(p.warn),
            color(p.danger),
            color(p.accent_hover),
            color(p.dim),
            color(p.status_muted),
        ],
        ..DiagramTheme::default()
    };
    let mut look = Look::new(theme);
    look.theme_only = true;
    look
}

/// Tallest the diagram area grows before the diagram is scaled down to fit.
pub(crate) const MAX_H: f64 = 480.0;
/// Zoom (relative to Fit) every diagram can reach at least.
const MIN_MAX_ZOOM: f32 = 4.0;
/// Zoom per pixel of a precise wheel delta: `exp(dy * this)`.
const WHEEL_ZOOM_PER_PIXEL: f32 = 0.0025;
/// Zoom of one wheel notch (lines mode) per line: about 10%.
const WHEEL_ZOOM_PER_LINE: f32 = 1.1;
/// Zoom of one press of the header's `+` or `−`.
pub(crate) const BUTTON_STEP: f32 = 1.25;

/// Uniform scale of a scene `scene_w` by `scene_h` in `avail_w`: down to fit
/// the width and the [`MAX_H`] height, never up.
pub(crate) fn fit_scale(scene_w: f64, scene_h: f64, avail_w: f64) -> f64 {
    let mut scale: f64 = 1.0;
    if scene_w > 0.0 && avail_w.is_finite() && avail_w < scene_w {
        scale = scale.min((avail_w / scene_w).max(0.0));
    }
    if scene_h > MAX_H {
        scale = scale.min(MAX_H / scene_h);
    }
    scale
}

/// What the user did to a diagram: `zoom` is relative to the fitted scale
/// (so a resize keeps it), and `(x, y)` is where the diagram's top left is
/// drawn from the top left of its area. The default is Fit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Viewport {
    pub zoom: f32,
    pub x: f32,
    pub y: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            zoom: 1.0,
            x: 0.0,
            y: 0.0,
        }
    }
}

/// Greatest zoom for a diagram fitted at `fit`: the greater of four times
/// Fit and twice the natural size.
pub(crate) fn max_zoom(fit: f32) -> f32 {
    MIN_MAX_ZOOM.max(2.0 / fit.max(f32::EPSILON))
}

fn clamp_axis(offset: f32, content: f32, area: f32) -> f32 {
    if content <= area {
        // Smaller than the area: centred.
        (area - content) / 2.0
    } else {
        offset.clamp(area - content, 0.0)
    }
}

/// `vp` with its zoom in range and its offset kept so the diagram's edges
/// never pass the edges of its `area`; an axis the diagram does not fill
/// stays centred. The diagram is `area * zoom` big.
pub(crate) fn clamp(vp: Viewport, fit: f32, area: (f32, f32)) -> Viewport {
    let zoom = vp.zoom.clamp(1.0, max_zoom(fit));
    Viewport {
        zoom,
        x: clamp_axis(vp.x, area.0 * zoom, area.0),
        y: clamp_axis(vp.y, area.1 * zoom, area.1),
    }
}

/// `vp` zoomed by `factor` keeping the point `pivot` (from the top left of
/// the area) over the same part of the diagram.
pub(crate) fn zoom_about(
    vp: Viewport,
    factor: f32,
    pivot: (f32, f32),
    fit: f32,
    area: (f32, f32),
) -> Viewport {
    let zoom = (vp.zoom * factor).clamp(1.0, max_zoom(fit));
    let ratio = zoom / vp.zoom;
    clamp(
        Viewport {
            zoom,
            x: pivot.0 - (pivot.0 - vp.x) * ratio,
            y: pivot.1 - (pivot.1 - vp.y) * ratio,
        },
        fit,
        area,
    )
}

/// Zoom factor of a wheel event: scrolling up zooms in.
fn wheel_factor(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Pixels(d) => (f32::from(d.y) * WHEEL_ZOOM_PER_PIXEL).exp(),
        ScrollDelta::Lines(d) => WHEEL_ZOOM_PER_LINE.powf(d.y),
    }
}

/// What the element measured of its diagram, for the card's header.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Metrics {
    /// Scale of the diagram at Fit.
    pub fit: f32,
    /// Size of the diagram area.
    pub area: (f32, f32),
}

/// A drag in progress.
#[derive(Clone, Copy)]
struct Drag {
    /// Pointer position when the drag began (window coordinates).
    from: (f32, f32),
    /// Viewport when the drag began.
    start: Viewport,
}

/// The viewports and measures of the diagrams of one open session, by block
/// id. Kept apart from the elements so they survive rows being rebuilt.
#[derive(Default)]
pub(crate) struct Views {
    viewports: HashMap<String, Viewport>,
    metrics: HashMap<String, Metrics>,
    drag: Option<(String, Drag)>,
}

impl Views {
    pub(crate) fn clear(&mut self) {
        *self = Views::default();
    }
}

/// [`Views`] shared by the cards of one session.
pub(crate) type SharedViews = Arc<Mutex<Views>>;

/// One diagram's entry in [`SharedViews`].
#[derive(Clone)]
pub(crate) struct ViewHandle {
    views: SharedViews,
    id: String,
}

impl ViewHandle {
    pub(crate) fn new(views: SharedViews, id: impl Into<String>) -> Self {
        ViewHandle {
            views,
            id: id.into(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Views> {
        self.views.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn viewport(&self) -> Viewport {
        self.lock()
            .viewports
            .get(&self.id)
            .copied()
            .unwrap_or_default()
    }

    /// Store `vp`; Fit is stored as no entry.
    pub(crate) fn set_viewport(&self, vp: Viewport) {
        let mut views = self.lock();
        if vp == Viewport::default() {
            views.viewports.remove(&self.id);
        } else {
            views.viewports.insert(self.id.clone(), vp);
        }
    }

    pub(crate) fn metrics(&self) -> Option<Metrics> {
        self.lock().metrics.get(&self.id).copied()
    }

    /// Store the measure; whether it changed.
    fn set_metrics(&self, m: Metrics) -> bool {
        self.lock().metrics.insert(self.id.clone(), m) != Some(m)
    }

    /// Back to Fit.
    pub(crate) fn fit(&self) {
        self.set_viewport(Viewport::default());
    }

    /// Zoom by `factor` about the centre of the area (the header buttons).
    pub(crate) fn zoom_by(&self, factor: f32) {
        let Some(m) = self.metrics() else { return };
        let centre = (m.area.0 / 2.0, m.area.1 / 2.0);
        self.set_viewport(zoom_about(self.viewport(), factor, centre, m.fit, m.area));
    }

    /// The zoom as a percentage of the diagram's natural size.
    pub(crate) fn percent(&self, fit: f32) -> u32 {
        (fit * self.viewport().zoom * 100.0).round() as u32
    }

    /// Whether the diagram can zoom out, and in, from where it is.
    pub(crate) fn can_zoom(&self) -> (bool, bool) {
        let fit = self.metrics().map_or(1.0, |m| m.fit);
        let zoom = self.viewport().zoom;
        (zoom > 1.0 + 1e-4, zoom < max_zoom(fit) - 1e-4)
    }

    fn begin_drag(&self, from: (f32, f32)) {
        let start = self.viewport();
        self.lock().drag = Some((self.id.clone(), Drag { from, start }));
    }

    fn drag(&self) -> Option<Drag> {
        self.lock()
            .drag
            .as_ref()
            .filter(|(id, _)| *id == self.id)
            .map(|(_, d)| *d)
    }

    fn end_drag(&self) -> bool {
        let mut views = self.lock();
        let mine = views.drag.as_ref().is_some_and(|(id, _)| *id == self.id);
        if mine {
            views.drag = None;
        }
        mine
    }
}

/// The diagram of a laid-out scene, drawn in `p`'s colors. It is as big as
/// the scene scaled to fit its width and the height cap; zoom and pan (from
/// `view`) only change what is painted inside that area.
pub(crate) fn diagram(scene: Rc<Scene>, p: &Palette, view: ViewHandle) -> DiagramElement {
    DiagramElement { scene, p: *p, view }
}

/// See [`diagram`]. Its size comes from the cached scene, known at first
/// layout; resizing only changes the fit, zooming and panning never change
/// the size.
pub(crate) struct DiagramElement {
    scene: Rc<Scene>,
    p: Palette,
    view: ViewHandle,
}

impl DiagramElement {
    /// Scale at Fit of a diagram laid out in `bounds`.
    fn fit_in(&self, bounds: Bounds<Pixels>) -> f32 {
        if self.scene.width > 0.0 {
            (f64::from(bounds.size.width) / self.scene.width).min(1.0) as f32
        } else {
            1.0
        }
    }
}

impl Element for DiagramElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        let (w, h) = (self.scene.width, self.scene.height);
        let layout = window.request_measured_layout(
            Style {
                // Shrinks with the column; a diagram is never stretched. A
                // flex item is otherwise no narrower than its min-content
                // width, so a diagram says it can shrink to nothing.
                flex_shrink: 1.0,
                min_size: Size {
                    width: Length::Definite(px(0.).into()),
                    height: Length::Auto,
                },
                ..Style::default()
            },
            move |known, available, _, _| {
                let avail = known.width.map(f64::from).or(match available.width {
                    AvailableSpace::Definite(width) => Some(f64::from(width)),
                    AvailableSpace::MinContent => Some(0.0),
                    AvailableSpace::MaxContent => None,
                });
                let scale = fit_scale(w, h, avail.unwrap_or(f64::INFINITY));
                size(px((w * scale) as f32), px((h * scale) as f32))
            },
        );
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Hitbox {
        // The header's zoom controls read this; draw again when it changed.
        let measured = Metrics {
            fit: self.fit_in(bounds),
            area: (f32::from(bounds.size.width), f32::from(bounds.size.height)),
        };
        if self.view.set_metrics(measured) {
            window.refresh();
        }
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let fit = self.fit_in(bounds);
        let area = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let vp = clamp(self.view.viewport(), fit, area);
        let dragging = self.view.drag().is_some();
        let cursor = if dragging {
            Some(CursorStyle::ClosedHand)
        } else if vp.zoom > 1.0 {
            Some(CursorStyle::OpenHand)
        } else {
            None
        };
        if let Some(cursor) = cursor {
            if dragging {
                // The pointer may leave the diagram mid-drag.
                window.set_window_cursor_style(cursor);
            } else {
                window.set_cursor_style(cursor, hitbox);
            }
        }
        self.register_input(hitbox, bounds, fit, area, window);

        let fonts = fonts(cx);
        let look = look(&self.p);
        let origin = bounds.origin + point(px(vp.x), px(vp.y));
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint::paint(
                &self.scene,
                &look,
                &fonts,
                origin,
                fit * vp.zoom,
                window,
                cx,
            );
        });
    }
}

impl DiagramElement {
    /// Zoom with Cmd/Ctrl+wheel and pinch, pan by dragging, Fit on a
    /// double-click.
    fn register_input(
        &self,
        hitbox: &Hitbox,
        bounds: Bounds<Pixels>,
        fit: f32,
        area: (f32, f32),
        window: &mut Window,
    ) {
        let pivot = move |position: Point<Pixels>| {
            (
                f32::from(position.x - bounds.origin.x),
                f32::from(position.y - bounds.origin.y),
            )
        };

        // The transcript's list registers its handler before painting its
        // children, so this runs first in the bubble phase; stopping the
        // event keeps the list from scrolling. Without the modifier nothing
        // here runs and the event reaches the list.
        let (view, hit) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || !hit.should_handle_scroll(window)
                || !event.modifiers.secondary(cx)
            {
                return;
            }
            let factor = wheel_factor(event.delta);
            view.set_viewport(zoom_about(
                view.viewport(),
                factor,
                pivot(event.position),
                fit,
                area,
            ));
            window.refresh();
            cx.stop_propagation();
        });

        let (view, hit) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &PinchEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hit.is_hovered(window) {
                return;
            }
            view.set_viewport(zoom_about(
                view.viewport(),
                (1.0 + event.delta).max(0.01),
                pivot(event.position),
                fit,
                area,
            ));
            window.refresh();
            cx.stop_propagation();
        });

        let (view, hit) = (self.view.clone(), hitbox.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hit.is_hovered(window)
            {
                return;
            }
            // The press is ours: no window-level text selection from it.
            gpui_kit::base::GlobalState::suppress_text_selection(cx);
            if event.click_count >= 2 {
                view.fit();
                view.end_drag();
            } else if view.viewport().zoom > 1.0 {
                view.begin_drag((f32::from(event.position.x), f32::from(event.position.y)));
            }
            window.refresh();
        });

        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let Some(drag) = view.drag() else { return };
            if event.pressed_button != Some(MouseButton::Left) {
                view.end_drag();
                window.refresh();
                return;
            }
            if drag.start.zoom <= 1.0 {
                return;
            }
            let moved = (
                f32::from(event.position.x) - drag.from.0,
                f32::from(event.position.y) - drag.from.1,
            );
            view.set_viewport(clamp(
                Viewport {
                    x: drag.start.x + moved.0,
                    y: drag.start.y + moved.1,
                    ..drag.start
                },
                fit,
                area,
            ));
            window.refresh();
        });

        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, _| {
            if phase == DispatchPhase::Bubble
                && event.button == MouseButton::Left
                && view.end_drag()
            {
                window.refresh();
            }
        });
    }
}

#[cfg(test)]
pub(crate) fn layout_runs() -> usize {
    CACHE.with(|c| c.borrow().runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    // `use super::*` glob-imports `gpui_kit::test`, which would shadow the
    // `#[test]` the gpui_kit test macro emits.
    use core::prelude::v1::test;

    pub(super) const FLOW: &str = "flowchart LR; a --> b";

    /// A column `width` wide holding one diagram of `source`.
    pub(super) struct Fixture {
        pub(super) source: SharedString,
        pub(super) width: Pixels,
    }

    impl Render for Fixture {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let p = *crate::theme::palette(cx);
            let column = div().id("column").test_support().w(self.width);
            match scene(&self.source, window, cx) {
                Ok(scene) => column.child(diagram(
                    scene,
                    &p,
                    ViewHandle::new(SharedViews::default(), "fixture"),
                )),
                Err(_) => column,
            }
        }
    }

    pub(super) fn setup(
        cx: &mut TestAppContext,
        source: &str,
        width: f32,
    ) -> (AnyWindowHandle, Entity<Fixture>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let source: SharedString = source.to_string().into();
        let (handle, view) = cx.update(|cx| {
            open_window(Default::default(), cx, |_, cx| {
                cx.new(|_| Fixture {
                    source,
                    width: px(width),
                })
            })
            .unwrap()
        });
        (handle, view)
    }

    /// Frame the window and give the column's size.
    pub(super) fn column(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Size<Pixels> {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.find("column").bounds().size
        })
        .unwrap()
    }

    pub(super) fn natural(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        source: &str,
    ) -> (f32, f32) {
        let s = cx
            .update_window(handle, |_, window, cx| scene(source, window, cx))
            .unwrap()
            .expect("lays out");
        (s.width as f32, s.height as f32)
    }

    fn count(s: &Scene, f: impl Fn(&limu::scene::Kind) -> bool) -> usize {
        s.items.iter().filter(|i| f(&i.kind)).count()
    }

    /// Spike (design D6): limu lays out and our painter paints a flowchart
    /// without panicking, and the `Scene` size and `Theme` fields D2 and D4
    /// rely on exist.
    #[gpui_kit::test]
    fn flowchart_lays_out_and_paints(cx: &mut TestAppContext) {
        let (handle, _view) = setup(cx, FLOW, 600.);
        let s = cx
            .update_window(handle, |_, window, cx| scene(FLOW, window, cx))
            .unwrap()
            .expect("lays out");
        let nodes = count(&s, |k| matches!(k, limu::scene::Kind::Node));
        let edges = count(&s, |k| matches!(k, limu::scene::Kind::Edge(_)));
        assert_eq!((nodes, edges), (2, 1));
        assert!(s.width > 0.0 && s.height > 0.0);
        let painted = column(cx, handle);
        assert!(painted.height > px(0.));
    }

    fn near(a: Pixels, b: f32) -> bool {
        (f32::from(a) - b).abs() < 0.6
    }

    fn set_width(cx: &mut TestAppContext, view: &Entity<Fixture>, width: f32) {
        cx.update(|cx| {
            view.update(cx, |f, cx| {
                f.width = px(width);
                cx.notify();
            })
        });
    }

    /// "Wide diagram": a column half the natural width draws at half scale.
    #[gpui_kit::test]
    fn wide_diagram_scales_down(cx: &mut TestAppContext) {
        let (handle, view) = setup(cx, FLOW, 4000.);
        let (w, h) = natural(cx, handle, FLOW);
        set_width(cx, &view, w / 2.);
        let size = column(cx, handle);
        assert!(near(size.width, w / 2.), "{:?} vs {}", size.width, w / 2.);
        assert!(near(size.height, h / 2.), "{:?} vs {}", size.height, h / 2.);
    }

    /// "Small diagram": a column wider than the diagram leaves it natural.
    #[gpui_kit::test]
    fn small_diagram_keeps_natural_size(cx: &mut TestAppContext) {
        let (handle, view) = setup(cx, FLOW, 4000.);
        let (w, h) = natural(cx, handle, FLOW);
        set_width(cx, &view, w * 3.);
        let size = column(cx, handle);
        assert!(near(size.height, h), "{:?} vs {h}", size.height);
        assert_eq!(
            fit_scale(f64::from(w), f64::from(h), f64::from(w) * 3.),
            1.0
        );
    }

    /// Spec: "Tall diagram": 1200px tall in a wide column is 480px tall at
    /// 40%; width still limits a wide one.
    #[test]
    fn fit_is_limited_by_width_and_by_the_height_cap() {
        assert_eq!(fit_scale(300., 1200., 600.), 0.4);
        assert_eq!(fit_scale(300., 100., 600.), 1.0);
        assert_eq!(fit_scale(1200., 200., 600.), 0.5);
        // Both: the smaller scale wins.
        assert_eq!(fit_scale(1200., 2400., 600.), 0.2);
        assert_eq!(fit_scale(1200., 960., 1200.), 0.5);
        assert_eq!(fit_scale(0., 0., 600.), 1.0);
    }

    const AREA: (f32, f32) = (400., 300.);

    fn vp(zoom: f32, x: f32, y: f32) -> Viewport {
        Viewport { zoom, x, y }
    }

    /// A diagram point's position in the area: where the diagram's own
    /// coordinate `(u, v)` (in units of the area at Fit) is drawn.
    fn at(vp: Viewport, u: f32, v: f32) -> (f32, f32) {
        (vp.x + u * vp.zoom, vp.y + v * vp.zoom)
    }

    #[test]
    fn zoom_range_is_four_fits_or_twice_natural() {
        assert_eq!(max_zoom(1.0), 4.0);
        assert_eq!(max_zoom(0.5), 4.0);
        // Fitted at 20%: twice the natural size is ten times Fit.
        assert!((max_zoom(0.2) - 10.0).abs() < 1e-4);
    }

    /// The point under the pointer stays put while zooming.
    #[test]
    fn zoom_keeps_the_pivot_fixed() {
        let start = Viewport::default();
        let pivot = (100., 80.);
        let zoomed = zoom_about(start, 2.0, pivot, 1.0, AREA);
        assert_eq!(zoomed.zoom, 2.0);
        // Diagram coordinate under the pivot before and after.
        let before = (pivot.0, pivot.1);
        let after = at(zoomed, pivot.0, pivot.1);
        let under = (
            (pivot.0 - zoomed.x) / zoomed.zoom,
            (pivot.1 - zoomed.y) / zoomed.zoom,
        );
        assert!((under.0 - before.0).abs() < 1e-3 && (under.1 - before.1).abs() < 1e-3);
        assert_eq!(after, (zoomed.x + 200., zoomed.y + 160.));
        // A second zoom from a panned state keeps its pivot too.
        let panned = clamp(vp(2.0, -150., -90.), 1.0, AREA);
        let again = zoom_about(panned, 1.5, (250., 120.), 1.0, AREA);
        let u = |v: Viewport| ((250. - v.x) / v.zoom, (120. - v.y) / v.zoom);
        assert!((u(panned).0 - u(again).0).abs() < 1e-3);
        assert!((u(panned).1 - u(again).1).abs() < 1e-3);
    }

    #[test]
    fn zoom_stops_at_both_limits() {
        let out = zoom_about(vp(1.0, 0., 0.), 0.5, (10., 10.), 1.0, AREA);
        assert_eq!(out, Viewport::default(), "cannot zoom out past Fit");
        let max = max_zoom(1.0);
        let top = zoom_about(vp(3.9, -10., -10.), 10.0, (100., 100.), 1.0, AREA);
        assert_eq!(top.zoom, max);
        let past = zoom_about(top, 1.5, (100., 100.), 1.0, AREA);
        assert_eq!(past, top, "cannot zoom in past the maximum");
        // The limit follows the fit scale.
        assert!((zoom_about(vp(1.0, 0., 0.), 100.0, (0., 0.), 0.2, AREA).zoom - 10.0).abs() < 1e-3);
    }

    #[test]
    fn offset_is_clamped_to_the_diagram_edges_on_both_axes() {
        // 2x: the diagram is 800x600 in a 400x300 area.
        let c = |x, y| clamp(vp(2.0, x, y), 1.0, AREA);
        assert_eq!((c(50., 40.).x, c(50., 40.).y), (0., 0.), "top left edge");
        assert_eq!((c(-900., -700.).x, c(-900., -700.).y), (-400., -300.));
        assert_eq!((c(-100., -50.).x, c(-100., -50.).y), (-100., -50.));
        // Each axis alone.
        assert_eq!((c(-900., -50.).x, c(-900., -50.).y), (-400., -50.));
        assert_eq!((c(-100., 70.).x, c(-100., 70.).y), (-100., 0.));
    }

    /// A diagram smaller than its area on an axis stays centred on it.
    #[test]
    fn a_small_axis_is_centred() {
        // Zoom 1 and an offset someone left: drawn from the origin.
        assert_eq!(clamp(vp(1.0, 30., -20.), 1.0, AREA), Viewport::default());
        // Zoom below 1 is out of range and comes back to 1.
        assert_eq!(clamp(vp(0.5, 30., -20.), 1.0, AREA).zoom, 1.0);
        assert_eq!(clamp_axis(99., 100., 300.), 100.);
        assert_eq!(clamp_axis(-5., 300., 300.), 0.);
    }

    #[test]
    fn wheel_up_zooms_in_and_down_zooms_out() {
        use gpui_kit::point;
        let up = wheel_factor(ScrollDelta::Pixels(point(px(0.), px(40.))));
        let down = wheel_factor(ScrollDelta::Pixels(point(px(0.), px(-40.))));
        assert!(up > 1.0 && down < 1.0);
        assert!((up * down - 1.0).abs() < 1e-5);
        let notch = wheel_factor(ScrollDelta::Lines(point(0., 1.)));
        assert!((notch - 1.1).abs() < 1e-5);
    }

    #[test]
    fn view_handles_share_one_store_and_drop_fit() {
        let views = SharedViews::default();
        let a = ViewHandle::new(views.clone(), "a");
        let b = ViewHandle::new(views.clone(), "b");
        assert_eq!(a.viewport(), Viewport::default());
        a.set_viewport(vp(2.0, -10., -5.));
        assert_eq!(a.viewport().zoom, 2.0);
        assert_eq!(b.viewport(), Viewport::default());
        a.fit();
        assert!(views.lock().unwrap().viewports.is_empty());
        // Buttons zoom about the centre of the measured area.
        assert!(a.set_metrics(Metrics {
            fit: 1.0,
            area: AREA
        }));
        assert!(!a.set_metrics(Metrics {
            fit: 1.0,
            area: AREA
        }));
        a.zoom_by(2.0);
        let z = a.viewport();
        assert_eq!(z.zoom, 2.0);
        assert_eq!((z.x, z.y), (-200., -150.));
        assert_eq!(a.can_zoom(), (true, true));
        a.zoom_by(10.0);
        assert_eq!(a.can_zoom(), (true, false));
        assert_eq!(a.percent(1.0), 400);
    }

    /// Laying the same source out again, or resizing, does not re-run limu.
    #[gpui_kit::test]
    fn layout_is_cached_across_frames_and_resizes(cx: &mut TestAppContext) {
        let source = "flowchart TD; cache1 --> cache2 --> cache3";
        let (handle, view) = setup(cx, source, 4000.);
        let (w, h) = natural(cx, handle, source);
        let runs = layout_runs();
        for width in [w, w / 2., w / 4., w * 2.] {
            set_width(cx, &view, width);
            let size = column(cx, handle);
            let scale = (width / w).min(1.);
            assert!(near(size.height, h * scale), "{:?} at {width}", size.height);
        }
        assert_eq!(layout_runs(), runs, "resizing re-ran layout");
    }

    /// Failures are cached too, and name the reason.
    #[gpui_kit::test]
    fn failures_are_cached_with_a_reason(cx: &mut TestAppContext) {
        let (handle, _view) = setup(cx, FLOW, 400.);
        let layout = |cx: &mut TestAppContext, src: &'static str| {
            cx.update_window(handle, |_, window, cx| scene(src, window, cx))
                .unwrap()
        };
        assert_eq!(
            layout(cx, "flowchart LR; a -->").err(),
            Some(Unavailable::Syntax)
        );
        let runs = layout_runs();
        assert_eq!(
            layout(cx, "flowchart LR; a -->").err(),
            Some(Unavailable::Syntax)
        );
        assert_eq!(layout_runs(), runs);
        let unsupported = layout(cx, "notadiagram\n  a b").err();
        assert!(
            matches!(unsupported, Some(Unavailable::Unsupported(_))),
            "{unsupported:?}"
        );
    }

    #[test]
    fn look_maps_the_palette_and_ignores_author_colors() {
        let cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let (light, dark) = cx.update(|cx| {
            crate::theme::apply(false, "orange", cx);
            let light = *crate::theme::palette(cx);
            crate::theme::apply(true, "orange", cx);
            (light, *crate::theme::palette(cx))
        });
        let (l, d) = (look(&light), look(&dark));
        assert_ne!(l, d);
        assert!(l.theme_only && d.theme_only);
        assert_eq!(d.theme.fg, color(dark.text));
        assert_eq!(d.theme.surface, color(dark.surface));
        assert_eq!(d.theme.bg, color(dark.card));
        assert_eq!(d.theme.border, color(dark.border_strong));
        assert_eq!(l.theme.line, color(light.muted));
        assert_ne!(l.theme.fg, d.theme.fg);
    }

    /// "Switch to dark mode": recolors only; size unchanged, no new layout.
    #[gpui_kit::test]
    fn theme_switch_does_not_relayout(cx: &mut TestAppContext) {
        let source = "flowchart TD; theme1 --> theme2";
        let (handle, _view) = setup(cx, source, 300.);
        let before = column(cx, handle);
        let runs = layout_runs();
        cx.update(|cx| crate::theme::apply(false, "orange", cx));
        cx.update_window(handle, |_, window, _| window.refresh())
            .unwrap();
        let after = column(cx, handle);
        assert_eq!(before, after);
        assert_eq!(layout_runs(), runs);
    }

    /// Task 2.3: a 400-edge flowchart lays out within a frame (16 ms) in a
    /// release build; debug builds only report the time.
    #[gpui_kit::test]
    fn four_hundred_edge_flowchart_lays_out_within_a_frame(cx: &mut TestAppContext) {
        let (handle, _view) = setup(cx, FLOW, 400.);
        let mut src = String::from("flowchart TD\n");
        for i in 0..400 {
            src.push_str(&format!("  n{i} --> n{}\n", i + 1));
        }
        let took = cx
            .update_window(handle, |_, window, cx| {
                let measurer = paint::TextMeasurer::new(window, fonts(cx));
                let start = std::time::Instant::now();
                let layout = compute(&src, measurer);
                (layout, start.elapsed())
            })
            .unwrap();
        assert!(took.0.is_ok(), "{:?}", took.0.err());
        eprintln!("400-edge flowchart layout: {:?}", took.1);
        if !cfg!(debug_assertions) {
            assert!(
                took.1 < std::time::Duration::from_millis(16),
                "{:?}",
                took.1
            );
        }
    }

    #[test]
    fn first_word_skips_front_matter_and_comments() {
        assert_eq!(
            first_word("%% hi\n---\ntitle: x\n---\n\n  gantt\n a"),
            Some("gantt".into())
        );
        assert_eq!(first_word("  \n"), None);
        assert_eq!(
            Unavailable::Unsupported(Some("x".into())).reason(),
            "unsupported diagram type `x`"
        );
    }
}
