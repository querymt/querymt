//! Markdown in the transcript, styled like the desktop app: inline code as a
//! soft rounded chip, fenced code as a card with a header (language on the
//! left, copy button on the right).
//!
//! Code uses the native markdown block and one selection participant. Its
//! actions slot supplies the header without nesting another text view.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui_kit::base::{TextView, TextViewStyle};
use gpui_kit::component::text::{
    MarkdownNode, RangeHighlight, RenderedText, TextViewMotion, TextViewState, markdown_ast,
};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit::ButtonExt as _;
use super::{find, kit, mermaid};
use crate::theme::Palette;

/// Name of the custom markdown block that wraps tables (adds a copy button).
const TABLE_BLOCK: &str = "qmt-table";
/// Name of the custom markdown block that draws a closed `mermaid` fence.
const MERMAID_BLOCK: &str = "qmt-mermaid";
/// How the user changed Mermaid blocks from the default (the diagram, fitted
/// to the card), by block id.
#[derive(Default)]
struct CardModes {
    /// Blocks shown as their source.
    source: HashSet<String>,
    /// Zoom and pan of the diagrams, and what their elements measured.
    views: mermaid::SharedViews,
}

impl CardModes {
    fn clear(&mut self) {
        self.source.clear();
        self.views.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// [`CardModes`] shared by the block renderers of one [`Kept`] (cleared with
/// it).
type SourceShown = Arc<Mutex<CardModes>>;
/// Language shown (and highlighted) for fences without one, as the desktop does.
const DEFAULT_LANG: &str = "text";
/// How long the copy button shows its check.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1200);
/// How long one streamed word takes to reach full color.
const STREAM_FADE: std::time::Duration = std::time::Duration::from_millis(280);
/// Delay between consecutive words of one streamed chunk (the library's own
/// default). Longer staggers let later chunks show before earlier words of a
/// previous chunk have started fading, which reads as text out of order.
const STREAM_STAGGER: std::time::Duration = std::time::Duration::from_millis(10);

/// Motion for text that streams in (agent replies, reasoning): each appended
/// word fades in, staggered behind the one before it. The text view only
/// fades text that extends what it already shows, so history, reloads and
/// edits appear at once, and it shows everything at once under reduced motion.
/// A chunk too long to stagger within one fade fades in as a whole.
/// `enabled` is the Appearance setting; off shows streamed text at once.
pub fn stream_motion(enabled: bool) -> TextViewMotion {
    if !enabled {
        return TextViewMotion::default();
    }
    TextViewMotion::default()
        .with_stream_fade(STREAM_FADE)
        .with_stream_fade_stagger(STREAM_STAGGER)
        .with_stream_fade_easing(gpui_kit::base::Easing::EaseOut)
}

fn code_lang(lang: Option<&str>) -> &str {
    lang.map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(DEFAULT_LANG)
}

/// Markdown view for transcript text; `id` must be unique in the window.
/// The transcript itself uses [`Kept::markdown`].
#[cfg(test)]
pub fn markdown(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    p: &Palette,
) -> TextView {
    let id: SharedString = id.into();
    styled(
        TextView::markdown(id.clone(), text),
        id,
        p,
        SourceShown::default(),
        1.,
        px(kit::REM_BASE_PX),
    )
}

/// The style of a text view in `p`: every colour (text, muted text, links,
/// selection, code and table backgrounds, borders) comes from the palette
/// handed in, never from the app theme. The kit's own `TextView` wrapper
/// derives them from the theme, which painted dark text onto the quick panel
/// when the app was in a light theme, so this module builds the base view.
///
/// Fence colours are not part of the style: the base view takes them from the
/// highlighter installed with the theme (`highlight::install`), one `Arc` for
/// every frame, which is what keeps fences from being re-highlighted.
pub(crate) fn view_style(p: &Palette) -> TextViewStyle {
    view_style_scaled(p, 1.)
}

/// [`view_style`] with code blocks sized by `mono_scale` ([`kit::mono_scale`]).
pub(crate) fn view_style_scaled(p: &Palette, mono_scale: f32) -> TextViewStyle {
    let radius = px(8.);
    TextViewStyle::default()
        .with_foreground(p.text)
        .with_muted_foreground(p.muted)
        .with_link(p.accent)
        .with_selection(p.accent_dim)
        .with_code_background(p.card)
        .with_border(p.border)
        .with_code_block(
            StyleRefinement::default()
                .rounded(radius)
                // Room for the header row ([`CodeHeader`], rem-sized).
                .pt(kit::rpx(42.))
                .pb_3()
                .text_size(kit::mono_px_at(mono_scale, 13.)),
        )
        .with_table(StyleRefinement::default().rounded(radius))
        .with_table_head(StyleRefinement::default().bg(p.card).text_color(p.muted))
        .with_table_row_hover(p.card)
        .with_inline_code(HighlightStyle {
            background_color: Some(p.selected),
            ..Default::default()
        })
        .with_dark(p.dark)
}

/// Transcript styling and extensions; `id` prefixes the table ids.
fn styled(
    view: TextView,
    id: SharedString,
    p: &Palette,
    sources: SourceShown,
    mono_scale: f32,
    rem: Pixels,
) -> TextView {
    let table_prefix = id.clone();
    let mermaid_prefix = id;
    let p = *p;
    view.style(view_style_scaled(&p, mono_scale))
        .inline_code_style(inline_code_style(&p, mono_scale, rem))
        .code_block_actions(move |block, _, cx| {
            let lang = block.lang();
            let row = header_row("code-header", &p, cx)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(code_lang(lang.as_deref()).to_string()),
                )
                .child(CopyButton {
                    id: "copy".into(),
                    value: block.code(),
                    p,
                });
            #[cfg(test)]
            let row = {
                use gpui_kit::test::TestSupportExt as _;
                row.test_support()
            };
            CodeHeader {
                row: row.into_any_element(),
            }
        })
        .markdown_block_parser(|node, cx| match node {
            markdown_ast::Node::Table(_) => {
                let source = cx.node_source(node)?.to_string();
                Some(
                    MarkdownNode::new(TABLE_BLOCK, source.clone())
                        .text(source.clone())
                        .markdown(source),
                )
            }
            markdown_ast::Node::Code(code) => {
                let lang = code.lang.as_deref()?;
                if !lang.trim().eq_ignore_ascii_case("mermaid") {
                    return None;
                }
                let source = cx.node_source(node)?;
                if !fence_closed(source) {
                    return None;
                }
                Some(
                    MarkdownNode::new(MERMAID_BLOCK, code.value.clone())
                        .text(code.value.clone())
                        .markdown(source.to_string()),
                )
            }
            _ => None,
        })
        .markdown_block_renderer(MERMAID_BLOCK, move |node, _, _| {
            let Some(body) = node.data::<String>() else {
                return div().into_any_element();
            };
            let start = node.source_range().map_or(0, |r| r.start);
            MermaidCard {
                id: SharedString::from(format!("{mermaid_prefix}-mermaid-{start}")),
                source: body.clone().into(),
                p,
                sources: sources.clone(),
            }
            .into_any_element()
        })
        .markdown_block_renderer(TABLE_BLOCK, move |node, _, _| {
            let Some(source) = node.data::<String>() else {
                return div().into_any_element();
            };
            let start = node.source_range().map_or(0, |r| r.start);
            let id = format!("{table_prefix}-table-{start}");
            table_card(&id, source, p).into_any_element()
        })
}

/// The header strip of a code or diagram card: 30px, a hairline under it,
/// the label on the left and controls on the right.
fn header_row(id: impl Into<ElementId>, p: &Palette, cx: &App) -> Stateful<Div> {
    h_flex()
        .w_full()
        .id(id)
        .justify_between()
        .gap_3()
        .h(px(30.))
        .pl_3()
        .pr_1()
        .border_b_1()
        .border_color(p.border)
        .font_family(Theme::global(cx).font_family.clone())
        .text_xs()
        .text_color(p.muted)
}

/// Whether the source of a fenced code node ends with its closing fence: a
/// line of only the opener's fence character, at least as long as the opener.
/// mdast reports an unclosed fence as a node running to the end of the
/// document, so this is what tells a streamed fence from a finished one.
fn fence_closed(source: &str) -> bool {
    let source = source.trim_end();
    let mut lines = source.lines();
    let Some(open) = lines.next().map(str::trim_start) else {
        return false;
    };
    let Some(fence) = open.chars().next().filter(|c| matches!(c, '`' | '~')) else {
        return false;
    };
    let open_len = open.chars().take_while(|c| *c == fence).count();
    let Some(close) = lines.next_back().map(str::trim) else {
        return false;
    };
    close.len() >= open_len && close.chars().all(|c| c == fence)
}

/// Parsed Markdown of transcript rows, kept for the open session.
///
/// A [`TextView::markdown`] keeps its parsed document in element state, which
/// GPUI drops once the element skips a frame. The transcript list does not
/// render rows above the viewport, so a reply scrolled out at the top lost its
/// document, and on scrolling back a reply over the synchronous parse limit
/// first laid out empty: the list took that as the row's height. Keeping the
/// state here preserves the document; [`Self::row_ready`] reports a parse that
/// is still pending, so the list keeps such a height provisional.
#[derive(Clone, Default)]
pub struct Kept(Rc<RefCell<KeptInner>>);

#[derive(Default)]
struct KeptInner {
    views: HashMap<String, KeptView>,
    /// Transcript row key → ids of the views it built when last rendered.
    rows: HashMap<String, Vec<String>>,
    /// Row being rendered, which views built now belong to.
    row: Option<String>,
    /// Mermaid blocks shown as source (survives rows being rebuilt).
    sources: SourceShown,
    /// The find in session query painted into every view, while find is open.
    find: Option<FindQuery>,
    /// Bumped whenever `find` changes, so views repaint their highlights.
    find_generation: u64,
}

/// What find in session paints into the kept views (design D1).
#[derive(Clone, Debug, PartialEq)]
pub struct FindQuery {
    pub text: String,
    /// The current match: the view and which occurrence in its text.
    pub current: Option<(String, usize)>,
    /// Background of the other matches.
    pub weak: Hsla,
    /// Background of the current match.
    pub strong: Hsla,
}

impl FindQuery {
    /// The two highlight colours of `p`: the accent at low and at higher
    /// alpha, so text stays readable in both themes (design D5).
    pub fn new(text: impl Into<String>, current: Option<(String, usize)>, p: &Palette) -> Self {
        Self {
            text: text.into(),
            current,
            weak: p.accent.opacity(0.28),
            strong: p.accent.opacity(0.62),
        }
    }
}

/// When a view last painted find: the generation of the query and the
/// rendered text it searched.
#[derive(PartialEq)]
struct FindStamp {
    generation: u64,
    text: RenderedText,
}

struct KeptView {
    state: Entity<TextViewState>,
    stamp: Option<FindStamp>,
    /// Ranges last handed to the view and whether each is the current match
    /// (tests read what was painted).
    #[cfg(test)]
    painted: Vec<(std::ops::Range<usize>, bool)>,
    #[cfg(test)]
    paints: usize,
    text: SharedString,
    /// Set while the list was told the row is not ready; the next parse
    /// commit then asks for a frame so the list measures the row again.
    awaited: Rc<Cell<bool>>,
    _observe: Subscription,
}

impl KeptView {
    /// A full replacement (a new view, or new text) has not been parsed yet.
    /// Streamed appends are not pending: the committed document is a prefix.
    fn parse_pending(&self, cx: &App) -> bool {
        let rendered = self.state.read(cx).rendered_text();
        let source = rendered.source();
        !self.text.starts_with(source) || (source.is_empty() && !self.text.is_empty())
    }
}

impl Kept {
    /// Drop every kept view (another session opened).
    pub fn clear(&self) {
        let sources = self.0.borrow().sources.clone();
        sources.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.0.borrow_mut() = KeptInner {
            sources,
            ..KeptInner::default()
        };
    }

    /// Views built from now until [`Self::end_row`] belong to row `key`.
    pub fn begin_row(&self, key: &str) {
        let mut inner = self.0.borrow_mut();
        inner.rows.insert(key.to_string(), Vec::new());
        inner.row = Some(key.to_string());
    }

    pub fn end_row(&self) {
        self.0.borrow_mut().row = None;
    }

    /// Whether every view of row `key` shows its current text, so the row's
    /// measured height is final.
    pub fn row_ready(&self, key: &str, cx: &App) -> bool {
        let inner = self.0.borrow();
        let Some(ids) = inner.rows.get(key) else {
            return true;
        };
        let mut ready = true;
        for view in ids.iter().filter_map(|id| inner.views.get(id)) {
            if view.parse_pending(cx) {
                view.awaited.set(true);
                ready = false;
            }
        }
        ready
    }

    /// Transcript Markdown whose parsed document outlives the element;
    /// `id` must be unique in the window.
    pub fn markdown(
        &self,
        id: impl Into<SharedString>,
        text: impl Into<SharedString>,
        p: &Palette,
    ) -> KeptMarkdown {
        let id: SharedString = id.into();
        {
            let mut inner = self.0.borrow_mut();
            if let Some(row) = inner.row.clone()
                && let Some(ids) = inner.rows.get_mut(&row)
            {
                ids.push(id.to_string());
            }
        }
        KeptMarkdown {
            kept: self.clone(),
            id,
            text: text.into(),
            p: *p,
            motion: None,
            sources: self.0.borrow().sources.clone(),
        }
    }

    /// Paint `query` into every view, or remove every highlight with
    /// `None`. Views that are not built are painted when they are.
    pub fn set_find(&self, query: Option<FindQuery>, cx: &mut App) {
        let clear: Vec<Entity<TextViewState>> = {
            let mut inner = self.0.borrow_mut();
            if inner.find == query {
                return;
            }
            inner.find_generation += 1;
            let clearing = query.is_none();
            inner.find = query;
            if !clearing {
                return;
            }
            inner
                .views
                .values_mut()
                .filter_map(|view| {
                    #[cfg(test)]
                    view.painted.clear();
                    view.stamp.take().map(|_| view.state.clone())
                })
                .collect()
        };
        for state in clear {
            state.update(cx, |state, cx| state.clear_range_highlights(cx));
        }
    }

    /// The query being painted (tests).
    #[cfg(test)]
    pub fn find(&self) -> Option<FindQuery> {
        self.0.borrow().find.clone()
    }

    /// How many times view `id` was handed highlights, each of which
    /// notifies it (tests).
    #[cfg(test)]
    pub fn paints(&self, id: &str) -> usize {
        self.0.borrow().views.get(id).map_or(0, |v| v.paints)
    }

    /// Highlights `id` last painted, as `(range, is current)` (tests).
    #[cfg(test)]
    pub fn painted(&self, id: &str) -> Vec<(std::ops::Range<usize>, bool)> {
        self.0
            .borrow()
            .views
            .get(id)
            .map(|v| v.painted.clone())
            .unwrap_or_default()
    }

    /// Every kept view as `(id, text)` (tests).
    #[cfg(test)]
    pub fn built(&self) -> Vec<(String, String)> {
        let mut views: Vec<_> = self
            .0
            .borrow()
            .views
            .iter()
            .map(|(id, v)| (id.clone(), v.text.to_string()))
            .collect();
        views.sort();
        views
    }

    /// The text view `id` renders (tests).
    #[cfg(test)]
    pub fn rendered(&self, id: &str, cx: &App) -> Option<String> {
        let state = self.0.borrow().views.get(id)?.state.clone();
        Some(state.read(cx).rendered_text().as_str().to_string())
    }

    /// Highlight the matches of the find query in view `id` and reveal the
    /// current one. A view paints once per query, current match and text
    /// revision, so rendering it again does not notify it again.
    fn paint_find(&self, id: &str, state: &Entity<TextViewState>, cx: &mut App) {
        let (query, generation) = {
            let inner = self.0.borrow();
            let Some(query) = inner.find.clone() else {
                return;
            };
            (query, inner.find_generation)
        };
        let rendered = state.read(cx).rendered_text();
        let stamp = FindStamp {
            generation,
            text: rendered.clone(),
        };
        if self
            .0
            .borrow()
            .views
            .get(id)
            .is_none_or(|view| view.stamp.as_ref() == Some(&stamp))
        {
            return;
        }
        let ranges = find::Matcher::new(&query.text)
            .map(|m| m.ranges(rendered.as_str()))
            .unwrap_or_default();
        let wanted = query
            .current
            .as_ref()
            .filter(|(view, _)| view == id)
            .map(|(_, k)| *k);
        // An occurrence the rendered text does not have (the index and the
        // view disagree) paints without a current match and reveals the
        // first one (design D2).
        let current = wanted.filter(|k| *k < ranges.len());
        let reveal = match wanted {
            Some(_) => current.or(Some(0)).and_then(|k| ranges.get(k).cloned()),
            None => None,
        };
        let highlights: Vec<RangeHighlight> = ranges
            .iter()
            .enumerate()
            .map(|(k, range)| {
                let colour = if Some(k) == current {
                    query.strong
                } else {
                    query.weak
                };
                RangeHighlight::new(range.clone(), colour)
            })
            .collect();
        let applied = state.update(cx, |state, cx| {
            let ok = state.set_range_highlights(highlights, cx).is_ok();
            if ok && let Some(range) = reveal {
                let _ = state.reveal_range(range, cx);
            }
            ok
        });
        if let Some(view) = self.0.borrow_mut().views.get_mut(id) {
            // A rejected set is retried when the text next changes.
            view.stamp = applied.then_some(stamp);
            #[cfg(test)]
            {
                view.paints += 1;
                view.painted = if applied {
                    ranges
                        .into_iter()
                        .enumerate()
                        .map(|(k, r)| (r, Some(k) == current))
                        .collect()
                } else {
                    Vec::new()
                };
            }
        }
    }

    fn state(&self, id: &str, text: &SharedString, cx: &mut App) -> Entity<TextViewState> {
        let state = self.resolve(id, text, cx);
        self.paint_find(id, &state, cx);
        state
    }

    fn resolve(&self, id: &str, text: &SharedString, cx: &mut App) -> Entity<TextViewState> {
        let existing = self.0.borrow_mut().views.get_mut(id).map(|view| {
            let same = view.text.as_ptr() == text.as_ptr() && view.text.len() == text.len();
            let changed = !same && view.text != *text;
            if changed {
                view.text = text.clone();
            }
            (view.state.clone(), changed)
        });
        if let Some((state, changed)) = existing {
            if changed {
                state.update(cx, |state, cx| state.set_text(text, cx));
            }
            return state;
        }
        let state = cx.new(|cx| TextViewState::markdown(text, cx));
        let awaited = Rc::new(Cell::new(false));
        let observe = cx.observe(&state, {
            let awaited = awaited.clone();
            move |_, cx| {
                if awaited.replace(false) {
                    cx.refresh_windows();
                }
            }
        });
        self.0.borrow_mut().views.insert(
            id.to_string(),
            KeptView {
                state: state.clone(),
                stamp: None,
                #[cfg(test)]
                painted: Vec::new(),
                #[cfg(test)]
                paints: 0,
                text: text.clone(),
                awaited,
                _observe: observe,
            },
        );
        state
    }
}

/// See [`Kept::markdown`].
#[derive(IntoElement)]
pub struct KeptMarkdown {
    kept: Kept,
    id: SharedString,
    text: SharedString,
    p: Palette,
    motion: Option<TextViewMotion>,
    sources: SourceShown,
}

impl KeptMarkdown {
    /// The palette this view is styled with.
    #[cfg(test)]
    pub fn palette(&self) -> &Palette {
        &self.p
    }

    pub fn motion(mut self, motion: TextViewMotion) -> Self {
        self.motion = Some(motion);
        self
    }
}

impl RenderOnce for KeptMarkdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = self.kept.state(&self.id, &self.text, cx);
        let scale = kit::mono_scale(cx);
        let view = styled(
            TextView::new(&state),
            self.id,
            &self.p,
            self.sources,
            scale,
            window.rem_size(),
        );
        match self.motion {
            Some(motion) => view.motion(motion),
            None => view,
        }
    }
}

/// Expands the built-in code block's corner action slot into a header.
/// This is presentation only: the native code block remains the sole owner
/// of text selection. The slot is inset by `top_2/right_2` in GPUI Kit;
/// expand that measured slot to the block's inner edges without retaining
/// geometry or creating a second selectable document.
struct CodeHeader {
    row: AnyElement,
}

impl Element for CodeHeader {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        // As tall as the row painted into it (rem-sized, see `prepaint`).
        let height = kit::rpx(30.).to_pixels(window.rem_size());
        let layout =
            window.request_measured_layout(Style::default(), move |known, available, _, _| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    _ => px(0.),
                });
                size(width, height)
            });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let inset = window.rem_size() * 0.5;
        self.row.prepaint_as_root(
            bounds.origin - point(px(0.), inset),
            size(
                AvailableSpace::Definite(bounds.size.width + inset),
                AvailableSpace::Definite(kit::rpx(30.).to_pixels(window.rem_size())),
            ),
            window,
            cx,
        );
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.row.paint(window, cx);
    }
}

/// The size of inline code in the transcript at the default code size:
/// the library sets it to 0.875 of the 14 px body text.
const INLINE_CODE_PX: f32 = 14. * 0.875;

/// Desktop `.markdown-body code`: rounded, faint fill, padded. With the
/// default sizes (code 13 px, `rem` 16 px) it keeps the library's size (0.875
/// of the surrounding text), so nothing changes. Otherwise the span gets an
/// explicit size, the default's 12.25 px times the code size: like the other
/// code text it follows the Code size setting only, not the interface size.
fn inline_code_style(p: &Palette, mono_scale: f32, rem: Pixels) -> StyleRefinement {
    let (bg, fg) = if p.dark {
        (rgba(0xffffff13), rgb(0xe5e5e5))
    } else {
        (rgba(0x5e5c641a), rgb(0x3d3846))
    };
    let style = StyleRefinement::default()
        .bg(Hsla::from(bg))
        .text_color(Hsla::from(fg))
        .rounded(px(6.))
        .px(px(5.));
    if mono_scale == 1. && rem == px(kit::REM_BASE_PX) {
        style
    } else {
        style.text_size(kit::mono_px_at(mono_scale, INLINE_CODE_PX))
    }
}

/// A markdown table (rendered by a nested view of its own source, without
/// this module's block hooks so it is not wrapped again) with a copy button
/// in its corner, like the transcript's message copy buttons. Copies the
/// table's markdown.
fn table_card(id: &str, source: &str, p: Palette) -> impl IntoElement {
    TableCard {
        id: SharedString::from(id.to_string()),
        source: source.to_string().into(),
        p,
    }
}

/// Hover and copy state of one [`TableCard`]. The fade is retargeted only
/// from the hover and copy handlers, never during render.
#[derive(Default)]
struct TableChrome {
    hovered: bool,
    copied: bool,
    fade: kit::Fade,
}

impl TableChrome {
    /// The check keeps the button up after the pointer leaves.
    fn retarget(&mut self, reduce_motion: bool) -> bool {
        let shown = self.hovered || self.copied;
        self.fade.show(shown, crate::util::now_ms(), reduce_motion)
    }
}

/// See [`table_card`]. The button and its backing chip fade in together
/// while the pointer is over the table (or the check still shows) and are
/// fully gone otherwise; with reduced motion they appear and vanish at once.
#[derive(IntoElement)]
struct TableCard {
    id: SharedString,
    source: SharedString,
    p: Palette,
}

impl RenderOnce for TableCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = self.p;
        let chrome = window.use_keyed_state(
            SharedString::from(format!("{}-chrome", self.id)),
            cx,
            |_, _| TableChrome::default(),
        );
        let state = chrome.read(cx);
        let (copied, fade) = (state.copied, state.fade);
        let (opacity, running) = fade.value(crate::util::now_ms(), cx.reduce_motion());
        if running {
            // Only an animating chip asks for frames.
            window.request_animation_frame();
        }

        let table = TextView::markdown(self.id.clone(), self.source.clone())
            .style(view_style(&p))
            .inline_code_style(inline_code_style(
                &p,
                kit::mono_scale(cx),
                window.rem_size(),
            ));
        let (hover, click, value) = (chrome.clone(), chrome, self.source);
        let button = kit::icon_button(
            SharedString::from(format!("{}-copy", self.id)),
            if copied {
                IconName::Check
            } else {
                IconName::Copy
            },
            if copied { "Copied" } else { "Copy table" },
        )
        .xsmall()
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(value.to_string()));
            let reduce = cx.reduce_motion();
            click.update(cx, |c, cx| {
                c.copied = true;
                c.retarget(reduce);
                cx.notify();
            });
            let click = click.clone();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(COPIED_FOR).await;
                let reduce = cx.update(|cx| cx.reduce_motion());
                click.update(cx, |c, cx| {
                    c.copied = false;
                    c.retarget(reduce);
                    cx.notify();
                });
            })
            .detach();
        });
        div()
            .id(SharedString::from(format!("{}-card", self.id)))
            .relative()
            .w_full()
            .min_w_0()
            .on_hover(move |hovered, _, cx| {
                let reduce = cx.reduce_motion();
                hover.update(cx, |c, cx| {
                    c.hovered = *hovered;
                    if c.retarget(reduce) {
                        cx.notify();
                    }
                })
            })
            .child(table)
            // Fully transparent means no chip at all, and nothing to click.
            .when(opacity > 0., |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .rounded(px(6.))
                        .bg(p.surface)
                        .opacity(opacity)
                        .child(button),
                )
            })
    }
}

/// A closed `mermaid` fence: the diagram in a card framed like a code block,
/// its header showing `mermaid`, a Diagram/Source toggle and the copy button.
/// A source that cannot be drawn shows as plain source with a note instead,
/// and no toggle.
#[derive(IntoElement)]
struct MermaidCard {
    id: SharedString,
    source: SharedString,
    p: Palette,
    sources: SourceShown,
}

impl MermaidCard {
    fn shown_as_source(&self) -> bool {
        self.sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .contains(self.id.as_ref())
    }

    /// This diagram's zoom and pan, kept for the open session.
    fn view(&self) -> mermaid::ViewHandle {
        let views = self
            .sources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .views
            .clone();
        mermaid::ViewHandle::new(views, self.id.to_string())
    }

    /// The Diagram / Source toggle. Segment ids are the block id and the
    /// option's id.
    fn toggle(&self, as_source: bool) -> impl IntoElement {
        let p = self.p;
        let option = |slug: &'static str, name: &'static str, source: bool| {
            let (sources, id) = (self.sources.clone(), self.id.to_string());
            kit::segment(
                SharedString::from(format!("{}-{slug}", self.id)),
                as_source == source,
                &p,
            )
            .h(px(20.))
            .label(name)
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                {
                    let mut modes = sources.lock().unwrap_or_else(|e| e.into_inner());
                    if source {
                        modes.source.insert(id.clone());
                    } else {
                        modes.source.remove(&id);
                    }
                }
                cx.refresh_windows();
            })
        };
        kit::segment_bar(&p)
            .h(px(24.))
            .child(option("diagram", "Diagram", false))
            .child(option("source", "Source", true))
    }

    /// `−`, the zoom, `+` and `Fit`. Disabled at the limits; the tooltips
    /// name the gestures.
    fn zoom_controls(&self, window: &Window) -> impl IntoElement {
        let p = self.p;
        let view = self.view();
        let (can_out, can_in) = view.can_zoom();
        let fit = view.metrics().map_or(1.0, |m| m.fit);
        let percent = view.percent(fit);
        let _ = window;
        let control = |slug: &'static str,
                       label: &'static str,
                       tip: &'static str,
                       enabled: bool,
                       act: fn(&mermaid::ViewHandle)| {
            let view = view.clone();
            kit::button(SharedString::from(format!("{}-{slug}", self.id)))
                .ghost()
                .xsmall()
                .label(label)
                .tooltip(tip)
                .inactive(!enabled)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    act(&view);
                    cx.refresh_windows();
                })
        };
        let readout = div()
            .id(SharedString::from(format!("{}-zoom", self.id)))
            .min_w(px(36.))
            .text_center()
            .text_color(p.muted)
            .child(format!("{percent}%"));
        #[cfg(test)]
        let readout = {
            use gpui_kit::test::TestSupportExt as _;
            readout.test_support()
        };
        h_flex()
            .items_center()
            .child(control(
                "zoom-out",
                "\u{2212}",
                "Zoom out (Cmd+scroll or pinch)",
                can_out,
                |v| v.zoom_by(1.0 / mermaid::BUTTON_STEP),
            ))
            .child(readout)
            .child(control(
                "zoom-in",
                "+",
                "Zoom in (Cmd+scroll or pinch)",
                can_in,
                |v| v.zoom_by(mermaid::BUTTON_STEP),
            ))
            .child(control(
                "fit",
                "Fit",
                "Fit to the card (double-click)",
                can_out,
                |v| v.fit(),
            ))
    }

    /// The diagram, centred in the card. It is fitted to the card's width and
    /// a 480px height; zooming and panning stay inside that area.
    fn diagram(&self, scene: &mermaid::SharedScene) -> AnyElement {
        let fitted = div()
            .id(SharedString::from(format!("{}-fitted", self.id)))
            .min_w_0()
            .child(mermaid::diagram(scene.clone(), &self.p, self.view()));
        #[cfg(test)]
        let fitted = {
            use gpui_kit::test::TestSupportExt as _;
            fitted.test_support()
        };
        h_flex()
            .w_full()
            .min_w_0()
            .justify_center()
            .p_3()
            .child(fitted)
            .into_any_element()
    }
}

/// The Mermaid source as plain monospace lines, as a code block shows it.
fn source_lines(source: &str, p: &Palette, cx: &App) -> Div {
    v_flex()
        .w_full()
        .px_3()
        .py_3()
        .font_family(kit::mono_family(cx))
        .text_size(kit::mono_px(cx, 13.))
        .line_height(relative(1.6))
        .text_color(p.text)
        .children(source.lines().map(|line| {
            div().child(if line.is_empty() {
                " ".to_string()
            } else {
                line.replace('\t', "    ")
            })
        }))
}

impl RenderOnce for MermaidCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = self.p;
        let layout = mermaid::scene(&self.source, window, cx);
        let as_source = layout.is_err() || self.shown_as_source();
        let header = header_row(SharedString::from(format!("{}-header", self.id)), &p, cx)
            .child(div().min_w_0().truncate().child("mermaid"))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .when(layout.is_ok() && !as_source, |el| {
                        el.child(self.zoom_controls(window))
                    })
                    .when(layout.is_ok(), |el| el.child(self.toggle(as_source)))
                    .child(CopyButton {
                        id: SharedString::from(format!("{}-copy", self.id)),
                        value: self.source.clone(),
                        p,
                    }),
            );
        let body = match (&layout, as_source) {
            (Ok(scene), false) => self.diagram(scene),
            _ => source_lines(&self.source, &p, cx).into_any_element(),
        };
        let note = layout.as_ref().err().map(|why| {
            div()
                .px_3()
                .pb_2()
                .id(SharedString::from(format!("{}-note", self.id)))
                .text_xs()
                .text_color(p.muted)
                .font_family(Theme::global(cx).font_family.clone())
                .child(format!("Diagram not drawn: {}", why.reason()))
        });
        #[cfg(test)]
        let note = {
            use gpui_kit::test::TestSupportExt as _;
            note.map(|n| n.test_support())
        };
        let card = v_flex()
            .id(SharedString::from(format!("{}-card", self.id)))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .rounded(px(8.))
            .border_1()
            .border_color(p.border)
            .bg(p.card)
            .child(header)
            .child(body)
            .children(note);
        #[cfg(test)]
        let card = {
            use gpui_kit::test::TestSupportExt as _;
            card.test_support()
        };
        card
    }
}

/// Copy icon that turns into a check for a moment after copying.
#[derive(IntoElement)]
struct CopyButton {
    id: SharedString,
    value: SharedString,
    p: Palette,
}

impl RenderOnce for CopyButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let copied = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        let done = *copied.read(cx);
        let p = &self.p;
        let (icon, color) = if done {
            (IconName::Check, p.accent)
        } else {
            (IconName::Copy, p.muted)
        };
        kit::button(self.id)
            .ghost()
            .xsmall()
            .icon(kit::icon(icon, 13., color))
            .tooltip(if done { "Copied" } else { "Copy code" })
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(self.value.to_string()));
                copied.update(cx, |c, cx| {
                    *c = true;
                    cx.notify();
                });
                let copied = copied.clone();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(COPIED_FOR).await;
                    copied.update(cx, |c, cx| {
                        *c = false;
                        cx.notify();
                    });
                })
                .detach();
            })
    }
}

/// Text of `source` with the markdown syntax removed and whitespace collapsed,
/// e.g. `**Checking scope**` → `Checking scope` (desktop `stripHtml` of the
/// rendered markdown).
pub fn plain_text(source: &str) -> String {
    let text = match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(root) => match root.children() {
            Some(blocks) => blocks
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" "),
            None => root.to_string(),
        },
        Err(_) => source.to_string(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LANG, code_lang, plain_text, stream_motion};
    use gpui_kit::AppContext as _;

    /// A reply over the synchronous parse limit is not ready until its parse
    /// lands, and its document survives being rebuilt (scrolled back in).
    #[gpui_kit::test]
    fn kept_row_waits_for_large_parse_and_keeps_document(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let kept = super::Kept::default();
        let text: gpui_kit::SharedString = "word ".repeat(2000).into();
        let p = cx.update(|cx| *crate::theme::palette(cx));
        kept.begin_row("row");
        let _ = kept.markdown("md-row", text.clone(), &p);
        kept.end_row();
        let state = cx.update(|cx| kept.state("md-row", &text, cx));
        assert!(
            !cx.update(|cx| kept.row_ready("row", cx)),
            "parse still pending"
        );
        cx.run_until_parked();
        assert!(cx.update(|cx| kept.row_ready("row", cx)), "parse landed");
        // Rebuilding the row reuses the parsed state instead of starting over.
        let again = cx.update(|cx| kept.state("md-row", &text, cx));
        assert_eq!(state.entity_id(), again.entity_id());
        assert!(cx.update(|cx| kept.row_ready("row", cx)));
        // Rows the store never saw do not hold the list back.
        assert!(cx.update(|cx| kept.row_ready("other", cx)));
    }

    #[test]
    fn closing_fence_decides_whether_a_fence_is_finished() {
        use super::fence_closed;
        assert!(fence_closed("```mermaid\nflowchart LR; a --> b\n```"));
        assert!(fence_closed("```mermaid\nflowchart LR; a --> b\n```\n"));
        assert!(fence_closed("~~~mermaid\na\n~~~~~"));
        assert!(fence_closed("````mermaid\na\n````"));
        assert!(fence_closed("  ```mermaid\n  a\n  ```"));
        assert!(fence_closed("```mermaid\n```"));
        // Still open: the body runs to the end of the document.
        assert!(!fence_closed("```mermaid"));
        assert!(!fence_closed("```mermaid\nflowchart LR\n  a --> b"));
        // A shorter or different fence does not close it.
        assert!(!fence_closed("````mermaid\na\n```"));
        assert!(!fence_closed("```mermaid\na\n~~~"));
        assert!(!fence_closed("```mermaid\na\n``` x"));
        assert!(!fence_closed(""));
    }

    /// Fences take their colours from the highlighter installed with the
    /// theme, and keep the same one between frames: the kit re-highlights every
    /// code block when the highlighter changes.
    #[gpui_kit::test]
    fn fences_use_the_installed_highlighter_for_every_theme(cx: &mut gpui_kit::TestAppContext) {
        use crate::theme::{Accent, Appearance, ThemeId};
        use gpui_kit::base::TextViewDefaults;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        for (key, dark) in [
            ("tokyo-night", true),
            ("solarized-light", false),
            ("qmt-dark", true),
        ] {
            let theme = ThemeId::resolve(key, dark);
            cx.update(|cx| {
                crate::theme::apply(
                    Appearance {
                        theme,
                        accent: Accent::Theme,
                    },
                    cx,
                )
            });
            // The view sets no highlighter of its own: the base view uses the
            // one installed with the theme, the same one every frame.
            assert!(
                cx.update(|cx| TextViewDefaults::global(cx).has_code_block_highlighter()),
                "{key}"
            );
        }
    }

    // appearance / Code size: code blocks and inline code follow the code
    // size, in absolute px (not the interface size); at the default they are
    // exactly what they were.
    #[test]
    fn code_text_follows_the_code_size() {
        use crate::theme::Palette;
        use gpui_kit::{AbsoluteLength, px};
        let p = Palette::dark();
        let block = |scale: f32| {
            super::view_style_scaled(&p, scale)
                .code_block()
                .text
                .font_size
        };
        let at = |v: f32| Some(AbsoluteLength::from(px(v)));
        assert_eq!(block(1.), at(13.));
        assert_eq!(block(1.), super::view_style(&p).code_block().text.font_size);
        assert_eq!(block(16. / 13.), at(16.));
        assert_eq!(block(8. / 13.), at(8.));
        // Inline code keeps the library's size (0.875 of the text around it)
        // at the default sizes, and gets an absolute size of its own off
        // them: the code size's, whatever the interface size.
        let inline =
            |scale: f32, rem: f32| super::inline_code_style(&p, scale, px(rem)).text.font_size;
        assert_eq!(inline(1., 16.), None);
        assert_eq!(inline(2., 16.), at(super::INLINE_CODE_PX * 2.));
        assert_eq!(inline(1., 20.), at(super::INLINE_CODE_PX));
        assert_eq!(inline(2., 20.), inline(2., 16.));
    }

    /// Every colour of a text view comes from the palette it is given, whatever
    /// theme the app is in: the quick panel's text was dark on dark when the
    /// app was light, because the kit's wrapper took them from the theme.
    #[test]
    fn a_views_style_takes_every_colour_from_the_palette() {
        use crate::quick::hud::hud_palette;
        use crate::theme::Palette;
        for p in [
            Palette::light(),
            Palette::dark(),
            hud_palette(&Palette::light(), true),
            hud_palette(&Palette::dark(), false),
        ] {
            let style = super::view_style(&p);
            assert_eq!(style.foreground(), p.text);
            assert_eq!(style.muted_foreground(), p.muted);
            assert_eq!(style.link(), p.accent);
            assert_eq!(style.selection(), p.accent_dim);
            assert_eq!(style.code_background(), p.card);
            assert_eq!(style.border(), p.border);
            assert_eq!(style.table_row_hover(), Some(p.card));
            assert_eq!(style.is_dark(), p.dark);
        }
    }

    /// A transcript reply kept like the session view keeps it.
    struct Doc {
        kept: super::Kept,
        text: gpui_kit::SharedString,
        shown: bool,
    }

    impl gpui_kit::Render for Doc {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::{IntoElement as _, ParentElement as _, Styled as _, div, px};
            let p = *crate::theme::palette(cx);
            let column = div().w(px(600.));
            if self.shown {
                column
                    .child(self.kept.markdown("m", self.text.clone(), &p))
                    .into_any_element()
            } else {
                column.into_any_element()
            }
        }
    }

    const BLOCK: &str = "m-mermaid-0";
    const FLOW: &str = "```mermaid\nflowchart LR; a --> b\n```";

    fn open_doc(
        cx: &mut gpui_kit::TestAppContext,
        text: &str,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<Doc>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let text: gpui_kit::SharedString = text.to_string().into();
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|_| Doc {
                    kept: super::Kept::default(),
                    text,
                    shown: true,
                })
            })
            .unwrap()
        });
        frames(cx, handle);
        (handle, view)
    }

    /// Let parsing land and draw a couple of frames.
    fn frames(cx: &mut gpui_kit::TestAppContext, handle: gpui_kit::AnyWindowHandle) {
        use gpui_kit::test::TestWindowExt as _;
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
    }

    fn has(cx: &mut gpui_kit::TestAppContext, handle: gpui_kit::AnyWindowHandle, id: &str) -> bool {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(handle, |_, window, _| {
            window
                .try_find(gpui_kit::SharedString::from(id.to_string()))
                .is_some()
        })
        .unwrap()
    }

    fn height(
        cx: &mut gpui_kit::TestAppContext,
        handle: gpui_kit::AnyWindowHandle,
        id: &str,
    ) -> gpui_kit::Pixels {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(handle, |_, window, _| {
            window
                .find(gpui_kit::SharedString::from(id.to_string()))
                .bounds()
                .size
                .height
        })
        .unwrap()
    }

    fn set_text(cx: &mut gpui_kit::TestAppContext, view: &gpui_kit::Entity<Doc>, text: &str) {
        let text: gpui_kit::SharedString = text.to_string().into();
        cx.update(|cx| {
            view.update(cx, |d, cx| {
                d.text = text;
                cx.notify();
            })
        });
    }

    /// Spec: "Flowchart in a reply" and "Other languages untouched".
    #[gpui_kit::test]
    fn mermaid_fence_is_a_diagram_and_other_fences_are_code(cx: &mut gpui_kit::TestAppContext) {
        let (handle, _view) = open_doc(cx, FLOW);
        assert!(has(cx, handle, &format!("{BLOCK}-card")));
        assert!(
            !has(cx, handle, "code-header"),
            "no code block for a diagram"
        );
        let (handle, _view) = open_doc(cx, "```rust\nfn main() {}\n```");
        assert!(has(cx, handle, "code-header"));
        assert!(!has(cx, handle, &format!("{BLOCK}-card")));
        // Language is case-insensitive.
        let (handle, _view) = open_doc(cx, &FLOW.replace("mermaid", "Mermaid"));
        assert!(has(cx, handle, &format!("{BLOCK}-card")));
    }

    /// Spec: "Diagram streams in".
    #[gpui_kit::test]
    fn diagram_streams_in_when_the_fence_closes(cx: &mut gpui_kit::TestAppContext) {
        let open = "```mermaid\nflowchart LR\n  a --> b";
        let (handle, view) = open_doc(cx, open);
        assert!(has(cx, handle, "code-header"), "open fence stays code");
        assert!(!has(cx, handle, &format!("{BLOCK}-card")));
        set_text(cx, &view, &format!("{open}\n```"));
        frames(cx, handle);
        assert!(
            has(cx, handle, &format!("{BLOCK}-card")),
            "closed fence draws"
        );
        assert!(!has(cx, handle, "code-header"));
        // The diagram's height is final as soon as it is first laid out.
        let first = height(cx, handle, &format!("{BLOCK}-card"));
        frames(cx, handle);
        assert_eq!(first, height(cx, handle, &format!("{BLOCK}-card")));
    }

    /// Spec: "Copy the source".
    #[gpui_kit::test]
    fn copy_puts_the_source_on_the_clipboard(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (handle, _view) = open_doc(cx, FLOW);
        cx.update_window(handle, |_, window, cx| {
            window.click(gpui_kit::SharedString::from(format!("{BLOCK}-copy")), cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()).unwrap(),
                "flowchart LR; a --> b"
            );
        })
        .unwrap();
    }

    /// Spec: "Toggle to source" and "Scroll away and back".
    #[gpui_kit::test]
    fn source_toggle_survives_the_row_being_rebuilt(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (handle, view) = open_doc(cx, FLOW);
        let card = format!("{BLOCK}-card");
        let diagram = height(cx, handle, &card);
        let click = |cx: &mut gpui_kit::TestAppContext, what: &str| {
            let id = gpui_kit::SharedString::from(format!("{BLOCK}-{what}"));
            cx.update_window(handle, |_, window, cx| window.click(id, cx))
                .unwrap();
            frames(cx, handle);
        };
        click(cx, "source");
        let source = height(cx, handle, &card);
        assert_ne!(source, diagram, "source replaces the diagram");
        // The row scrolls out of view (not built) and back.
        for shown in [false, true] {
            cx.update(|cx| {
                view.update(cx, |d, cx| {
                    d.shown = shown;
                    cx.notify();
                })
            });
            frames(cx, handle);
        }
        assert_eq!(height(cx, handle, &card), source, "still showing source");
        click(cx, "diagram");
        assert_eq!(height(cx, handle, &card), diagram);
    }

    /// A flowchart far wider than the 600px column.
    fn wide() -> String {
        let nodes: Vec<String> = (0..24).map(|i| format!("node{i}")).collect();
        format!("```mermaid\nflowchart LR; {}\n```", nodes.join(" --> "))
    }

    fn width(
        cx: &mut gpui_kit::TestAppContext,
        handle: gpui_kit::AnyWindowHandle,
        id: &str,
    ) -> gpui_kit::Pixels {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(handle, |_, window, _| {
            window
                .find(gpui_kit::SharedString::from(id.to_string()))
                .bounds()
                .size
                .width
        })
        .unwrap()
    }

    fn click(cx: &mut gpui_kit::TestAppContext, handle: gpui_kit::AnyWindowHandle, what: &str) {
        use gpui_kit::test::TestWindowExt as _;
        let id = gpui_kit::SharedString::from(format!("{BLOCK}-{what}"));
        cx.update_window(handle, |_, window, cx| window.click(id, cx))
            .unwrap();
        frames(cx, handle);
    }

    /// A diagram wider than the card shrinks to fit it, inside the card's
    /// flex row, instead of overflowing and being cut off.
    #[gpui_kit::test]
    fn wide_diagram_fits_the_card(cx: &mut gpui_kit::TestAppContext) {
        let (handle, _view) = open_doc(cx, &wide());
        let card = width(cx, handle, &format!("{BLOCK}-card"));
        let fitted = width(cx, handle, &format!("{BLOCK}-fitted"));
        // Card, minus its border and the body's padding.
        let room = card - gpui_kit::px(2. + 24.);
        assert!(fitted > gpui_kit::px(100.), "{fitted:?}");
        assert!(
            fitted <= room + gpui_kit::px(1.),
            "{fitted:?} overflows {room:?}"
        );
    }

    /// Spec: "Syntax error" and "Unsupported type": the source with a note,
    /// no toggle, and copy still copies it.
    #[gpui_kit::test]
    fn undrawable_diagrams_fall_back_to_source(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        for body in ["flowchart LR; a -->", "notadiagram\n  a b"] {
            let (handle, _view) = open_doc(cx, &format!("```mermaid\n{body}\n```"));
            assert!(has(cx, handle, &format!("{BLOCK}-card")), "{body}");
            assert!(has(cx, handle, &format!("{BLOCK}-note")), "{body}");
            assert!(
                !has(cx, handle, &format!("{BLOCK}-source")),
                "toggle hidden: {body}"
            );
            cx.update_window(handle, |_, window, cx| {
                window.click(gpui_kit::SharedString::from(format!("{BLOCK}-copy")), cx);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|c| c.text()).unwrap(),
                    body
                );
            })
            .unwrap();
        }
    }

    /// Zoom and pan (spec: "Zoom a diagram", "Pan a zoomed diagram", "Zoom
    /// and pan leave the transcript alone"), in the real card inside a
    /// transcript `list`.
    mod zoom {
        use super::{BLOCK, FLOW, click, frames, has, height, width};
        use crate::ui::mermaid;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{
            AnyWindowHandle, AppContext as _, Bounds, Context, Entity, InputEvent as _,
            IntoElement as _, ListAlignment, ListOffset, ListState, Modifiers, MouseMoveEvent,
            ParentElement as _, PinchEvent, Pixels, Render, ScrollDelta, ScrollEvent, SharedString,
            Styled as _, TestAppContext, TouchPhase, Window, div, list, point, px,
        };

        /// A transcript: a short row, the reply holding the diagram, then
        /// rows tall enough to scroll.
        struct Transcript {
            state: ListState,
            kept: super::super::Kept,
            text: SharedString,
        }

        impl Render for Transcript {
            fn render(
                &mut self,
                _: &mut Window,
                cx: &mut Context<Self>,
            ) -> impl gpui_kit::IntoElement {
                let p = *crate::theme::palette(cx);
                let (kept, text) = (self.kept.clone(), self.text.clone());
                list(self.state.clone(), move |ix, _, _| {
                    let key = format!("row-{ix}");
                    kept.begin_row(&key);
                    let row = match ix {
                        0 => div().h(px(40.)).w_full().into_any_element(),
                        1 => div()
                            .w_full()
                            .child(kept.markdown("m", text.clone(), &p))
                            .into_any_element(),
                        _ => div().h(px(400.)).w_full().into_any_element(),
                    };
                    kept.end_row();
                    row
                })
                .size_full()
            }
        }

        struct Open {
            handle: AnyWindowHandle,
            state: ListState,
            kept: super::super::Kept,
            _view: Entity<Transcript>,
        }

        fn open(cx: &mut TestAppContext, text: &str) -> Open {
            cx.update(|cx| {
                gpui_kit::init(cx);
                crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
            });
            let state = ListState::new(6, ListAlignment::Top, px(1200.));
            let kept = super::super::Kept::default();
            let text: SharedString = text.to_string().into();
            let (handle, view) = cx.update(|cx| {
                let (state, kept) = (state.clone(), kept.clone());
                gpui_kit::open_window(Default::default(), cx, |_, cx| {
                    cx.new(|_| Transcript { state, kept, text })
                })
                .unwrap()
            });
            frames(cx, handle);
            Open {
                handle,
                state,
                kept,
                _view: view,
            }
        }

        impl Open {
            fn frames(&self, cx: &mut TestAppContext) {
                frames(cx, self.handle);
            }

            /// This diagram's zoom and pan.
            fn view(&self) -> mermaid::ViewHandle {
                let views = self.kept.0.borrow().sources.lock().unwrap().views.clone();
                mermaid::ViewHandle::new(views, BLOCK)
            }

            /// The diagram area.
            fn area(&self, cx: &mut TestAppContext) -> Bounds<Pixels> {
                cx.update_window(self.handle, |_, window, _| {
                    window
                        .find(SharedString::from(format!("{BLOCK}-fitted")))
                        .bounds()
                })
                .unwrap()
            }

            fn centre(&self, cx: &mut TestAppContext) -> gpui_kit::Point<Pixels> {
                self.area(cx).center()
            }

            fn card_height(&self, cx: &mut TestAppContext) -> Pixels {
                height(cx, self.handle, &format!("{BLOCK}-card"))
            }

            fn move_to(&self, cx: &mut TestAppContext, position: gpui_kit::Point<Pixels>) {
                cx.update_window(self.handle, |_, window, cx| {
                    let modifiers = window.modifiers();
                    window.dispatch_event(
                        MouseMoveEvent {
                            position,
                            pressed_button: None,
                            modifiers,
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.render_frame(cx);
                })
                .unwrap();
            }

            fn wheel(
                &self,
                cx: &mut TestAppContext,
                position: gpui_kit::Point<Pixels>,
                dy: f32,
                modifiers: Modifiers,
            ) {
                self.move_to(cx, position);
                cx.update_window(self.handle, |_, window, cx| {
                    window.dispatch_event(
                        ScrollEvent {
                            position,
                            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                            modifiers,
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.render_frame(cx);
                })
                .unwrap();
            }

            fn pinch(
                &self,
                cx: &mut TestAppContext,
                position: gpui_kit::Point<Pixels>,
                delta: f32,
            ) {
                self.move_to(cx, position);
                cx.update_window(self.handle, |_, window, cx| {
                    window.dispatch_event(
                        PinchEvent {
                            position,
                            delta,
                            modifiers: Modifiers::default(),
                            phase: TouchPhase::Moved,
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.render_frame(cx);
                })
                .unwrap();
            }

            fn secondary(&self, cx: &mut TestAppContext) -> Modifiers {
                cx.update(|cx| Modifiers::secondary_key(cx))
            }
        }

        fn near(a: f32, b: f32) -> bool {
            (a - b).abs() < 0.01
        }

        /// Spec: "Modifier-wheel zoom".
        #[gpui_kit::test]
        fn modifier_wheel_zooms_about_the_pointer(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let area = t.area(cx);
            let rel = point(area.size.width * 0.3, area.size.height * 0.4);
            let cmd = t.secondary(cx);
            t.wheel(cx, area.origin + rel, 120., cmd);
            let vp = t.view().viewport();
            assert!(vp.zoom > 1.0, "{vp:?}");
            // The diagram point that was under the pointer still is.
            let (rx, ry) = (f32::from(rel.x), f32::from(rel.y));
            assert!(near((rx - vp.x) / vp.zoom, rx), "{vp:?}");
            assert!(near((ry - vp.y) / vp.zoom, ry), "{vp:?}");
            // Scrolling down zooms back out.
            t.wheel(cx, area.origin + rel, -1000., cmd);
            assert_eq!(t.view().viewport(), mermaid::Viewport::default());
        }

        /// Spec: "Pinch zoom".
        #[gpui_kit::test]
        fn pinch_zooms_in_around_the_pinch_point(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let area = t.area(cx);
            let rel = point(area.size.width * 0.7, area.size.height * 0.5);
            t.pinch(cx, area.origin + rel, 0.5);
            let vp = t.view().viewport();
            assert!(near(vp.zoom, 1.5), "{vp:?}");
            assert!(near((f32::from(rel.x) - vp.x) / vp.zoom, f32::from(rel.x)));
            assert!(near((f32::from(rel.y) - vp.y) / vp.zoom, f32::from(rel.y)));
        }

        /// Spec: "Plain scroll passes through": the transcript scrolls, the
        /// diagram does not change.
        #[gpui_kit::test]
        fn plain_scroll_passes_through_a_zoomed_diagram(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            t.view().zoom_by(2.0);
            t.frames(cx);
            let zoomed = t.view().viewport();
            assert_eq!(zoomed.zoom, 2.0);
            let before = t.state.logical_scroll_top();
            let at = t.centre(cx);
            t.wheel(cx, at, -30., Modifiers::default());
            assert_ne!(t.state.logical_scroll_top(), before, "the list scrolled");
            assert_eq!(t.view().viewport(), zoomed);
        }

        /// Spec: "Zooming does not scroll the transcript".
        #[gpui_kit::test]
        fn zooming_does_not_scroll_the_transcript(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            // The harness scrolls: a plain wheel moves the list.
            let at = t.centre(cx);
            t.wheel(cx, at, -20., Modifiers::default());
            let top = t.state.logical_scroll_top();
            assert_ne!(top, ListOffset::default());
            let cmd = t.secondary(cx);
            let at = t.centre(cx);
            t.wheel(cx, at, 80., cmd);
            assert_eq!(t.state.logical_scroll_top(), top, "Cmd+wheel");
            let zoomed = t.view().viewport().zoom;
            assert!(zoomed > 1.0);
            let at = t.centre(cx);
            t.pinch(cx, at, 0.2);
            assert_eq!(t.state.logical_scroll_top(), top, "pinch");
            assert!(t.view().viewport().zoom > zoomed);
        }

        /// Spec: "Drag to pan": the diagram follows the pointer, no text is
        /// selected, and it stops at its edges.
        #[gpui_kit::test]
        fn dragging_pans_within_the_edges_without_selecting(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let view = t.view();
            view.zoom_by(3.0);
            t.frames(cx);
            let area = t.area(cx);
            let (w, h) = (f32::from(area.size.width), f32::from(area.size.height));
            let from = area.center();
            let selected = |cx: &mut TestAppContext| {
                cx.update_window(t.handle, |_, window, cx| {
                    gpui_kit::base::TextSelection::selected_text(window, cx)
                })
                .unwrap()
            };
            let drag = |cx: &mut TestAppContext, dx: f32, dy: f32| {
                cx.update_window(t.handle, |_, window, cx| {
                    window.drag(from, from + point(px(dx), px(dy)), cx)
                })
                .unwrap();
            };
            let start = view.viewport();
            drag(cx, -10., -5.);
            let moved = view.viewport();
            assert!(near(moved.x, start.x - 10.) && near(moved.y, start.y - 5.));
            assert_eq!(moved.zoom, 3.0);
            assert!(selected(cx).trim().is_empty(), "{:?}", selected(cx));
            // Far past either edge stops at the edge.
            drag(cx, -10_000., -10_000.);
            let end = view.viewport();
            assert!(
                near(end.x, w - w * 3.0) && near(end.y, h - h * 3.0),
                "{end:?}"
            );
            drag(cx, 10_000., 10_000.);
            let top = view.viewport();
            assert!(near(top.x, 0.) && near(top.y, 0.), "{top:?}");
            assert!(selected(cx).trim().is_empty());
        }

        /// Spec: "Back to Fit", by the button and by double-click.
        #[gpui_kit::test]
        fn fit_button_and_double_click_return_to_fit(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let view = t.view();
            view.zoom_by(2.0);
            t.frames(cx);
            click(cx, t.handle, "fit");
            assert_eq!(view.viewport(), mermaid::Viewport::default());
            view.zoom_by(2.0);
            t.frames(cx);
            assert_eq!(view.viewport().zoom, 2.0);
            cx.update_window(t.handle, |_, window, cx| {
                window.double_click(SharedString::from(format!("{BLOCK}-fitted")), cx)
            })
            .unwrap();
            assert_eq!(view.viewport(), mermaid::Viewport::default());
        }

        /// Spec: "Zoom limits", and the controls are hidden in Source mode
        /// and in the fallback.
        #[gpui_kit::test]
        fn zoom_controls_stop_at_the_limits_and_hide_with_the_diagram(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let view = t.view();
            let controls = ["zoom-out", "zoom-in", "fit", "zoom"];
            for what in controls {
                assert!(has(cx, t.handle, &format!("{BLOCK}-{what}")), "{what}");
            }
            // At Fit: out and Fit are off, in is on.
            assert_eq!(view.can_zoom(), (false, true));
            click(cx, t.handle, "zoom-out");
            click(cx, t.handle, "fit");
            assert_eq!(view.viewport(), mermaid::Viewport::default());
            // In steps by 25% until the maximum, where it is off.
            click(cx, t.handle, "zoom-in");
            assert!(near(view.viewport().zoom, 1.25), "{:?}", view.viewport());
            for _ in 0..12 {
                click(cx, t.handle, "zoom-in");
            }
            let fit = view.metrics().expect("measured").fit;
            let max = mermaid::max_zoom(fit);
            assert!(near(view.viewport().zoom, max), "{:?}", view.viewport());
            assert_eq!(view.can_zoom(), (true, false));
            click(cx, t.handle, "zoom-in");
            assert!(near(view.viewport().zoom, max));
            click(cx, t.handle, "zoom-out");
            assert!(view.viewport().zoom < max);

            // Source mode hides them.
            click(cx, t.handle, "source");
            for what in controls {
                assert!(!has(cx, t.handle, &format!("{BLOCK}-{what}")), "{what}");
            }
            click(cx, t.handle, "diagram");
            assert!(has(cx, t.handle, &format!("{BLOCK}-zoom-in")));

            // So does the fallback.
            let t = open(cx, "```mermaid\nflowchart LR; a -->\n```");
            assert!(has(cx, t.handle, &format!("{BLOCK}-note")));
            for what in controls {
                assert!(!has(cx, t.handle, &format!("{BLOCK}-{what}")), "{what}");
            }
        }

        /// Spec: "Zoom keeps the row height".
        #[gpui_kit::test]
        fn zooming_keeps_the_card_and_the_rows_below(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let card = t.card_height(cx);
            let below = t.state.bounds_for_item(2);
            let area = t.area(cx);
            let cmd = t.secondary(cx);
            t.wheel(cx, area.center(), 200., cmd);
            click(cx, t.handle, "zoom-in");
            assert!(t.view().viewport().zoom > 1.5);
            t.frames(cx);
            assert_eq!(t.area(cx), area, "the area keeps its size");
            assert_eq!(t.card_height(cx), card);
            assert_eq!(t.state.bounds_for_item(2), below);
            click(cx, t.handle, "fit");
            assert_eq!(t.card_height(cx), card);
        }

        /// Spec: "Zoomed diagram scrolls away and back".
        #[gpui_kit::test]
        fn zoom_and_pan_survive_the_row_scrolling_away(cx: &mut TestAppContext) {
            let t = open(cx, FLOW);
            let view = t.view();
            view.zoom_by(2.0);
            t.frames(cx);
            let from = t.centre(cx);
            cx.update_window(t.handle, |_, window, cx| {
                window.drag(from, from + point(px(-12.), px(-6.)), cx)
            })
            .unwrap();
            let shown = view.viewport();
            assert!(
                near(shown.zoom, 2.0) && shown.x < 0. && shown.y < 0.,
                "{shown:?}"
            );
            for item_ix in [5, 0] {
                t.state.scroll_to(ListOffset {
                    item_ix,
                    offset_in_item: px(0.),
                });
                t.frames(cx);
            }
            assert!(has(cx, t.handle, &format!("{BLOCK}-card")));
            assert_eq!(view.viewport(), shown);
        }

        /// Spec: "Tall diagram": the area is 480px tall and the whole diagram
        /// is visible, at the scale that makes it so.
        #[gpui_kit::test]
        fn tall_diagram_fits_the_height_cap(cx: &mut TestAppContext) {
            let nodes: Vec<String> = (0..20).map(|i| format!("step{i}")).collect();
            let source = format!("flowchart TD; {}", nodes.join(" --> "));
            let t = open(cx, &format!("```mermaid\n{source}\n```"));
            let natural = cx
                .update_window(t.handle, |_, window, cx| {
                    mermaid::scene(&source, window, cx)
                })
                .unwrap()
                .expect("lays out");
            let (w, h) = (natural.width as f32, natural.height as f32);
            assert!(h > 480., "the diagram is taller than the cap: {h}");
            let area = t.area(cx);
            let scale = 480. / h;
            assert!(near(f32::from(area.size.height), 480.), "{:?}", area.size);
            assert!(
                (f32::from(area.size.width) - w * scale).abs() < 0.6,
                "{:?} vs {}",
                area.size,
                w * scale
            );
            assert!(near(t.view().metrics().unwrap().fit, scale));
            // The width is not what limits it.
            assert!(width(cx, t.handle, &format!("{BLOCK}-card")) > area.size.width);
        }
    }

    #[test]
    fn stream_motion_follows_the_setting() {
        assert!(stream_motion(false).stream_fade().is_zero());
        let on = stream_motion(true);
        assert!(!on.stream_fade().is_zero());
        assert_eq!(on.stream_fade_stagger().as_millis(), 10);
    }

    #[test]
    fn plain_text_drops_markdown() {
        assert_eq!(
            plain_text("**Checking scope filters**"),
            "Checking scope filters"
        );
        assert_eq!(
            plain_text("**Title**\n\nSome `code` and a [link](https://x.y)."),
            "Title Some code and a link."
        );
        assert_eq!(plain_text("  "), "");
    }

    struct SelectionFixture;

    impl gpui_kit::Render for SelectionFixture {
        fn render(
            &mut self,
            _: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Self>,
        ) -> impl gpui_kit::IntoElement {
            use gpui_kit::component::v_flex;
            use gpui_kit::test::TestSupportExt as _;
            use gpui_kit::{InteractiveElement as _, ParentElement as _, Styled as _, div, px};
            let p = *crate::theme::palette(cx);
            v_flex()
                .w(gpui_kit::px(600.))
                .gap_4()
                .children([
                    div().id("before").test_support().child(super::markdown(
                        "before-md",
                        "Before code",
                        &p,
                    )),
                    div().id("code").test_support().child(super::markdown(
                        "code-md",
                        "```rust\nfn main() {\n    println!(\"Hello, world!\");\n}\n```",
                        &p,
                    )),
                    div().id("after").test_support().child(super::markdown(
                        "after-md",
                        "After code",
                        &p,
                    )),
                ])
                .p(px(16.))
        }
    }

    #[gpui_kit::test]
    fn cross_message_copy_contains_code_once_and_header_spans_card(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, point, px};
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let (handle, _view) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| SelectionFixture))
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let before = window.find("before").bounds();
            let after = window.find("after").bounds();
            let code = window.find("code").bounds();
            let header = window.find("code-header").bounds();
            assert!((header.left() - code.left()).abs() <= px(2.));
            assert!((header.right() - code.right()).abs() <= px(2.));
            assert!((header.top() - code.top()).abs() <= px(2.));
            window.drag(
                point(before.left() + px(1.), before.top() + px(8.)),
                point(after.right() - px(1.), after.top() + px(8.)),
                cx,
            );
            let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
            assert_eq!(selected.matches("fn main()").count(), 1, "{selected}");
            assert!(
                selected.contains("Before code") && selected.contains("After code"),
                "{selected}"
            );
            window.press("secondary-c", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()).unwrap(),
                selected.trim_end()
            );
            assert_eq!(
                selected.trim_end(),
                "Before code\n\nfn main() {\n    println!(\"Hello, world!\");\n}\n\nAfter code"
            );

            // Select one word within the native block, not its whole card.
            use gpui_kit::test::ClickOptions;
            window.click_with_options(
                "code",
                ClickOptions::new()
                    .with_offset(point(px(19.), px(52.)))
                    .with_count(2),
                cx,
            );
            assert_eq!(
                gpui_kit::base::TextSelection::selected_text(window, cx).trim(),
                "fn"
            );
            window.press("secondary-c", cx);
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|c| c.text())
                    .unwrap()
                    .trim(),
                "fn"
            );

            // The header's copy button still copies the entire block.
            window.click("copy", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()).unwrap(),
                "fn main() {\n    println!(\"Hello, world!\");\n}"
            );
        })
        .unwrap();
    }

    #[test]
    fn fences_without_a_language_are_text() {
        assert_eq!(code_lang(None), DEFAULT_LANG);
        assert_eq!(code_lang(Some("  ")), DEFAULT_LANG);
        assert_eq!(code_lang(Some("rust")), "rust");
    }

    // add-session-find: the searchable text agrees with the rendered text.
    #[gpui_kit::test]
    fn searchable_text_finds_what_the_view_renders(cx: &mut gpui_kit::TestAppContext) {
        use crate::ui::find::{match_ranges, searchable_text};
        let corpus: &[(&str, &[&str])] = &[
            (
                "the **retry** loop and _soft_ *word* ~~gone~~",
                &[
                    "retry loop",
                    "retry",
                    "soft",
                    "word",
                    "**",
                    "loop and",
                    "gone",
                ],
            ),
            (
                "see [the docs](http://x.io/a) now",
                &["see the docs now", "docs", "http", "the docs", "now"],
            ),
            (
                "run `cargo test` now",
                &["cargo test", "run cargo", "test now", "`"],
            ),
            (
                "- one two\n- three\n  - four five\n\n1. six\n2. seven",
                &["one", "two", "four five", "six", "three", "-", "seven"],
            ),
            (
                "# Title here\n\nbody text",
                &["title here", "body", "here body", "#"],
            ),
            (
                "```rust\nlet x = 1;\nlet y = 2;\n```",
                &["let", "x = 1", "let y", "1; let", "rust", "```"],
            ),
            (
                "| a | b |\n|---|---|\n| c | d |",
                &["a", "c | d", "| a", "c d", "a b", "---"],
            ),
            (
                "line one\nline two\n\nnext",
                &["one line", "one", "line two", "two", "next"],
            ),
            ("first  \nsecond", &["first second", "second", "first"]),
            ("中文字符 和 English", &["中文", "字符 和", "english", "和"]),
            (
                "> quoted text\n> more\n\nafter",
                &["quoted", "text more", "more", ">"],
            ),
            ("x <b>bold</b> y", &["bold", "x bold", "<b>", "y"]),
            (
                "```mermaid\nflowchart LR; a --> b\n```",
                &["flowchart", "a --> b", "mermaid"],
            ),
            ("![alt text](u.png) after", &["alt", "after", "u.png"]),
            ("Straße and STRASSE", &["straße", "strasse", "and"]),
        ];
        for (source, queries) in corpus {
            let (_handle, view) = open_doc(cx, source);
            let rendered = cx
                .update(|cx| view.read(cx).kept.rendered("m", cx))
                .expect("the view is kept");
            let searchable = searchable_text(source);
            for query in *queries {
                assert_eq!(
                    match_ranges(&searchable, query).len(),
                    match_ranges(&rendered, query).len(),
                    "{query:?} in {source:?}: searchable {searchable:?}, rendered {rendered:?}"
                );
            }
        }
    }

    /// A kept view with `text`, and the palette it is painted with.
    fn found(
        cx: &mut gpui_kit::TestAppContext,
        text: &str,
    ) -> (
        gpui_kit::AnyWindowHandle,
        gpui_kit::Entity<Doc>,
        super::Kept,
        crate::theme::Palette,
    ) {
        let (handle, view) = open_doc(cx, text);
        let kept = cx.update(|cx| view.read(cx).kept.clone());
        let p = cx.update(|cx| *crate::theme::palette(cx));
        (handle, view, kept, p)
    }

    fn redraw(
        cx: &mut gpui_kit::TestAppContext,
        handle: gpui_kit::AnyWindowHandle,
        view: &gpui_kit::Entity<Doc>,
    ) {
        cx.update(|cx| view.update(cx, |_, cx| cx.notify()));
        frames(cx, handle);
    }

    // add-session-find / Matches are highlighted
    #[gpui_kit::test]
    fn a_kept_view_paints_matches_and_the_current_one_stronger(cx: &mut gpui_kit::TestAppContext) {
        let (handle, view, kept, p) = found(cx, "alpha beta Alpha");
        let query = super::FindQuery::new("alpha", Some(("m".into(), 1)), &p);
        cx.update(|cx| kept.set_find(Some(query), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m"), [(0..5, false), (11..16, true)]);
        // A current match in another view leaves this one with weak matches.
        let other = super::FindQuery::new("alpha", Some(("elsewhere".into(), 0)), &p);
        cx.update(|cx| kept.set_find(Some(other), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m"), [(0..5, false), (11..16, false)]);
    }

    // add-session-find: a stamp keeps render from notifying the view again.
    #[gpui_kit::test]
    fn painting_the_same_stamp_does_not_notify_again(cx: &mut gpui_kit::TestAppContext) {
        let (handle, view, kept, p) = found(cx, "alpha beta alpha");
        assert_eq!(kept.paints("m"), 0);
        let query = super::FindQuery::new("alpha", Some(("m".into(), 0)), &p);
        cx.update(|cx| kept.set_find(Some(query.clone()), cx));
        redraw(cx, handle, &view);
        // Each paint notifies the view once; a stamp stops the rest.
        assert_eq!(kept.paints("m"), 1);
        redraw(cx, handle, &view);
        redraw(cx, handle, &view);
        assert_eq!(kept.paints("m"), 1);
        // The same query again changes nothing either.
        cx.update(|cx| kept.set_find(Some(query), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.paints("m"), 1);
    }

    #[gpui_kit::test]
    fn clearing_the_query_removes_the_highlights(cx: &mut gpui_kit::TestAppContext) {
        let (handle, view, kept, p) = found(cx, "alpha beta alpha");
        let query = super::FindQuery::new("alpha", None, &p);
        cx.update(|cx| kept.set_find(Some(query), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m").len(), 2);
        cx.update(|cx| kept.set_find(None, cx));
        redraw(cx, handle, &view);
        assert!(kept.painted("m").is_empty());
        assert!(kept.find().is_none());
    }

    // add-session-find D2: the fallback for an occurrence the view lacks.
    #[gpui_kit::test]
    fn a_missing_occurrence_paints_without_a_current_match(cx: &mut gpui_kit::TestAppContext) {
        let (handle, view, kept, p) = found(cx, "alpha beta alpha");
        let query = super::FindQuery::new("alpha", Some(("m".into(), 7)), &p);
        cx.update(|cx| kept.set_find(Some(query), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m"), [(0..5, false), (11..16, false)]);
    }

    // add-session-find: appended text gets highlighted as it streams in.
    #[gpui_kit::test]
    fn streamed_text_is_searched_as_it_arrives(cx: &mut gpui_kit::TestAppContext) {
        let (handle, view, kept, p) = found(cx, "test one");
        let query = super::FindQuery::new("test", Some(("m".into(), 0)), &p);
        cx.update(|cx| kept.set_find(Some(query), cx));
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m"), [(0..4, true)]);
        set_text(cx, &view, "test one, test two");
        frames(cx, handle);
        redraw(cx, handle, &view);
        assert_eq!(kept.painted("m"), [(0..4, true), (10..14, false)]);
    }
}
