//! Markdown in the transcript, styled like the desktop app: inline code as a
//! soft rounded chip, fenced code as a card with a header (language on the
//! left, copy button on the right).
//!
//! Code uses the native markdown block and one selection participant. Its
//! actions slot supplies the header without nesting another text view.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::text::{MarkdownNode, TextView, TextViewStyle, markdown_ast};
use gpui_kit::component::{Sizable as _, Theme, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit;
use crate::theme::{Palette, palette};

/// Name of the custom markdown block that wraps tables (adds a copy button).
const TABLE_BLOCK: &str = "qmt-table";
/// Language shown (and highlighted) for fences without one, as the desktop does.
const DEFAULT_LANG: &str = "text";
/// How long the copy button shows its check.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1200);

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
    let table_prefix = id.clone();
    TextView::markdown(id, text)
        .style(TextViewStyle {
            code_block: StyleRefinement::default()
                .rounded(px(8.))
                .pt(px(42.))
                .pb_3()
                .text_size(px(13.)),
            ..TextViewStyle::default()
        })
        .inline_code_style(inline_code_style(p))
        .code_block_actions(|block, _, cx| {
            let p = *palette(cx);
            let lang = block.lang();
            let row = h_flex()
                .w_full()
                .id("code-header")
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
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(code_lang(lang.as_deref()).to_string()),
                )
                .child(CopyButton {
                    id: "copy".into(),
                    value: block.code(),
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
}

/// Expands the built-in code block's corner action slot into a header.
/// This is presentation only: the native code block remains the sole owner
/// of text selection. The slot is inset by `top_2/right_2` in GPUI Kit;
/// expand that measured slot to the block's inner edges without retaining
/// geometry or creating a second selectable document.
struct CodeHeader {
    row: AnyElement,
}

impl IntoElement for CodeHeader {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
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
        let layout = window.request_measured_layout(Style::default(), |known, available, _, _| {
            let width = known.width.unwrap_or(match available.width {
                AvailableSpace::Definite(width) => width,
                _ => px(0.),
            });
            size(width, px(30.))
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
                AvailableSpace::Definite(px(30.)),
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
            crate::theme::apply(true, "orange", cx);
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
}
