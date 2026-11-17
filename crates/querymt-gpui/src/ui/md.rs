//! Markdown in the transcript, styled like the desktop app: inline code as a
//! soft rounded chip, fenced code as a card with a header (language on the
//! left, copy button on the right).

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::text::{MarkdownNode, TextView, markdown_ast};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::code::HighlightedLines;
use super::kit;
use crate::theme::{Palette, palette};

/// Name of the custom markdown block that replaces fenced code.
const CODE_BLOCK: &str = "qmt-code-block";
/// Name of the custom markdown block that wraps tables (adds a copy button).
const TABLE_BLOCK: &str = "qmt-table";
/// Language shown (and highlighted) for fences without one, as the desktop does.
const DEFAULT_LANG: &str = "text";
/// How long the copy button shows its check.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1200);

/// A fenced code block: language from the info string, and the code.
struct Code {
    lang: Option<String>,
    code: String,
}

fn code_lang(lang: Option<&str>) -> &str {
    lang.map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or(DEFAULT_LANG)
}

/// Markdown view for transcript text; `id` must be unique in the window.
pub fn markdown(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    p: &Palette,
) -> TextView {
    let id: SharedString = id.into();
    let copy_prefix = id.clone();
    let table_prefix = id.clone();
    TextView::markdown(id, text)
        .inline_code_style(inline_code_style(p))
        .markdown_block_parser(|node, cx| match node {
            markdown_ast::Node::Code(c) => {
                let block = Code {
                    lang: c.lang.clone(),
                    code: c.value.clone(),
                };
                let source = cx.node_source(node).unwrap_or(&c.value).to_string();
                Some(
                    MarkdownNode::new(CODE_BLOCK, block)
                        .text(c.value.clone())
                        .markdown(source),
                )
            }
            markdown_ast::Node::Table(_) => {
                let source = cx.node_source(node)?.to_string();
                Some(
                    MarkdownNode::new(TABLE_BLOCK, source.clone())
                        .text(source.clone())
                        .markdown(source),
                )
            }
            _ => None,
        })
        .markdown_block_renderer(TABLE_BLOCK, move |node, _, _| {
            let Some(source) = node.data::<String>() else {
                return div().into_any_element();
            };
            let start = node.source_range().map_or(0, |r| r.start);
            let id = format!("{table_prefix}-table-{start}");
            table_card(&id, source).into_any_element()
        })
        .markdown_block_renderer(CODE_BLOCK, move |node, _, cx| {
            let Some(code) = node.data::<Code>() else {
                return div().into_any_element();
            };
            let start = node.source_range().map_or(0, |r| r.start);
            let copy_id = SharedString::from(format!("{copy_prefix}-copy-{start}"));
            code_card(code, copy_id, cx).into_any_element()
        })
}

/// Desktop `.markdown-body code`: rounded, faint fill, padded.
fn inline_code_style(p: &Palette) -> StyleRefinement {
    let (bg, fg) = if p.dark {
        (rgba(0xffffff13), rgb(0xe5e5e5))
    } else {
        (rgba(0x5e5c641a), rgb(0x3d3846))
    };
    StyleRefinement::default()
        .bg(Hsla::from(bg))
        .text_color(Hsla::from(fg))
        .rounded(px(6.))
        .px(px(5.))
}

/// Desktop `.code-block-shell`: header row, then the highlighted code.
fn code_card(code: &Code, copy_id: SharedString, cx: &App) -> impl IntoElement {
    let p = *palette(cx);
    let theme = Theme::global(cx);
    let (bg, highlight) = (theme.muted, theme.highlight_theme.clone());
    let lang = code_lang(code.lang.as_deref()).to_string();
    let lines = HighlightedLines::new(&code.code, Some(&lang), &highlight);
    v_flex()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(8.))
        .border_1()
        .border_color(p.border)
        .bg(bg)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .h(px(30.))
                .pl_3()
                .pr_1()
                .border_b_1()
                .border_color(p.border)
                .text_xs()
                .text_color(p.muted)
                .child(div().min_w_0().truncate().child(lang))
                .child(CopyButton {
                    id: copy_id,
                    value: code.code.clone().into(),
                }),
        )
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .px_4()
                .py_3()
                .font_family(kit::MONO)
                .text_size(px(13.))
                .line_height(relative(1.6))
                .text_color(p.text)
                .children((0..lines.len()).map(|ix| lines.line(ix))),
        )
}

/// A markdown table (rendered by a nested view of its own source, without
/// this module's block hooks so it is not wrapped again) with a copy button
/// in its corner, like the transcript's message copy buttons. Copies the
/// table's markdown.
fn table_card(id: &str, source: &str) -> impl IntoElement {
    TableCard {
        id: SharedString::from(id.to_string()),
        source: source.to_string().into(),
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
}

impl RenderOnce for TableCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = *palette(cx);
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
            .inline_code_style(inline_code_style(&p));
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

/// Copy icon that turns into a check for a moment after copying.
#[derive(IntoElement)]
struct CopyButton {
    id: SharedString,
    value: SharedString,
}

impl RenderOnce for CopyButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let copied = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        let done = *copied.read(cx);
        let p = palette(cx);
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
    use super::{DEFAULT_LANG, code_lang, plain_text};

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

    #[test]
    fn fences_without_a_language_are_text() {
        assert_eq!(code_lang(None), DEFAULT_LANG);
        assert_eq!(code_lang(Some("  ")), DEFAULT_LANG);
        assert_eq!(code_lang(Some("rust")), "rust");
    }
}
