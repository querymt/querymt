//! Turn diff panel (desktop `SessionTurnDiffPanel`): every file one turn
//! changed, opened from the turn's change pill.
//!
//! Unlike the desktop app, the panel sits beside the chat on the same level
//! instead of floating over it: a hairline separates the two, and dragging
//! it resizes the panel. The width is remembered across sessions and runs.
//! A slim top bar replaces the desktop's floating close / file-count pills.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::session_view::{CHAT_MAX, ReadingQuip, TOOL_LINE_H, diff_rows, hunk_marker};
use super::{QmtApp, kit};
use crate::session::TurnDiffFile;
use crate::theme::{Palette, palette};

/// Narrowest panel, and the room always left for the chat.
const MIN_W: f32 = 320.;
const MIN_CHAT_W: f32 = 380.;
/// Share of the page the panel takes until the user drags it.
const DEFAULT_SHARE: f32 = 0.44;
/// Lines each change shows before "Show N more lines".
const CHANGE_LINES: usize = 400;
/// Height of a file header (sticky copy included).
const FILE_HEADER_H: f32 = 36.;
/// Height of the panel's top bar (file count, fold all, close).
const HEADER_H: f32 = 36.;
/// Hit area of the separator around its hairline.
const SEPARATOR_W: f32 = 9.;
/// Inset of the scroll area inside the rounded panel: square-clipped content
/// stays within the 12px corner arcs.
const PANEL_PAD: f32 = 4.;

#[derive(Default)]
pub(super) struct TurnDiff {
    /// `agent/session` and the key of the first item of the turn shown.
    pub(super) open: Option<(String, String)>,
    /// Files folded by the user.
    collapsed: HashSet<String>,
    /// Changes showing all their lines.
    expanded: HashSet<String>,
    scroll: ScrollHandle,
    /// Pointer x and panel width when the separator was pressed.
    drag: Option<(Pixels, f32)>,
    /// Width while dragging; saved on release.
    live_width: Option<f32>,
    /// Scroll offset of the last render, to re-render the sticky header.
    seen_offset: Rc<Cell<f32>>,
}

/// Drag payload of the separator.
struct ResizeTurnDiff;

impl Render for ResizeTurnDiff {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn session_key(agent_id: &str, session_id: &str) -> String {
    format!("{agent_id}/{session_id}")
}

impl QmtApp {
    /// Open the diff of the turn closing at item `end`, or close it when it
    /// is the one shown (desktop `toggleTurnDiff`).
    pub(super) fn toggle_turn_diff(&mut self, end: usize, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(view) = store.session.as_ref() else {
            return;
        };
        let Some(turn) = view.turn_key(end) else {
            return;
        };
        let target = (
            session_key(&view.agent_id, &view.session_id),
            turn.to_string(),
        );
        if self.turn_diff.open.as_ref() == Some(&target) {
            self.turn_diff.open = None;
        } else {
            self.turn_diff.open = Some(target);
            self.turn_diff.collapsed.clear();
            self.turn_diff.expanded.clear();
            self.turn_diff.scroll.set_offset(point(px(0.), px(0.)));
        }
        cx.notify();
    }

    /// Escape / close button.
    pub(super) fn close_turn_diff(&mut self) -> bool {
        self.turn_diff.open.take().is_some()
    }

    /// Items of the turn whose diff is shown in the open session; `None`
    /// when closed, or once that turn is gone (undo, reload).
    pub(super) fn turn_diff_range(&self, cx: &App) -> Option<std::ops::Range<usize>> {
        let (session, turn) = self.turn_diff.open.as_ref()?;
        let view = self.store.read(cx).session.as_ref()?;
        (session_key(&view.agent_id, &view.session_id) == *session)
            .then(|| view.turn_range(turn))
            .flatten()
    }

    /// Panel width: dragged, remembered, or the default share, clamped so
    /// the chat keeps its room.
    fn turn_diff_width(&self, window: &Window, cx: &App) -> f32 {
        let page = f32::from(window.viewport_size().width - self.sidebar_width(cx));
        let default = (page * DEFAULT_SHARE).min(CHAT_MAX);
        let max = (page - MIN_CHAT_W - SEPARATOR_W).max(MIN_W);
        self.turn_diff
            .live_width
            .or(self.store.read(cx).prefs.turn_diff_width)
            .unwrap_or(default)
            .clamp(MIN_W, max)
    }

    fn finish_turn_diff_resize(&mut self, cx: &mut Context<Self>) {
        self.turn_diff.drag = None;
        if let Some(width) = self.turn_diff.live_width.take() {
            self.store.update(cx, |s, cx| {
                s.update_prefs(|p| p.turn_diff_width = Some(width.round()), cx)
            });
        }
        cx.notify();
    }

    /// Put the open turn diff beside `chat`, behind a draggable separator.
    pub(super) fn with_turn_diff(
        &mut self,
        chat: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(range) = self.turn_diff_range(cx) else {
            if self.turn_diff.open.is_some() {
                // The turn is gone: nothing to come back to.
                self.turn_diff.open = None;
            }
            return chat;
        };
        let files = self
            .store
            .read(cx)
            .session
            .as_ref()
            .map(|v| v.turn_diffs(range))
            .unwrap_or_default();
        let p = *palette(cx);
        let width = self.turn_diff_width(window, cx);
        let dragging = self.turn_diff.drag.is_some();
        h_flex()
            .size_full()
            .when(dragging, |el| el.cursor_col_resize())
            .child(div().flex_1().min_w_0().h_full().child(chat))
            .child(self.turn_diff_separator(width, dragging, &p, cx))
            .child(self.turn_diff_panel(files, width, &p, cx))
            .into_any_element()
    }

    /// The hairline between chat and panel, inset like the sidebar's; its
    /// wider hit area drags the panel's width.
    fn turn_diff_separator(
        &self,
        width: f32,
        dragging: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (line, active) = (p.border, p.accent);
        div()
            .id("turn-diff-separator")
            .group("turn-diff-separator")
            .flex_none()
            .w(px(SEPARATOR_W))
            .h_full()
            .flex()
            .justify_center()
            .py_3()
            .cursor_col_resize()
            .occlude()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .bg(if dragging { active } else { line })
                    .group_hover("turn-diff-separator", move |s| s.bg(active)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.turn_diff.drag = Some((ev.position.x, width));
                    cx.notify();
                }),
            )
            .on_drag(ResizeTurnDiff, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| ResizeTurnDiff)
            })
            .on_drag_move(
                cx.listener(|this, ev: &DragMoveEvent<ResizeTurnDiff>, window, cx| {
                    let Some((x, start)) = this.turn_diff.drag else {
                        return;
                    };
                    // Dragging left widens the panel.
                    let wanted = start + f32::from(x - ev.event.position.x);
                    this.turn_diff.live_width = Some(wanted);
                    let clamped = this.turn_diff_width(window, cx);
                    this.turn_diff.live_width = Some(clamped);
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_turn_diff_resize(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.turn_diff.drag.is_some() {
                        this.finish_turn_diff_resize(cx);
                    }
                }),
            )
    }

    fn turn_diff_panel(
        &self,
        files: Vec<TurnDiffFile>,
        width: f32,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sticky = self.sticky_file(&files);
        let header = self.panel_header(&files, p, cx);
        let sections: Vec<AnyElement> = files
            .iter()
            .enumerate()
            .map(|(ix, file)| self.file_section(ix, file, p, cx))
            .collect();
        let body = v_flex()
            .id("turn-diff-body")
            .size_full()
            .p(px(PANEL_PAD))
            .gap(px(PANEL_PAD))
            .overflow_y_scroll()
            .track_scroll(&self.turn_diff.scroll)
            .when(files.is_empty(), |el| {
                el.child(div().px_3().py_2p5().text_xs().text_color(p.muted).child(
                    "Diff preview unavailable. The saved tool calls do not contain \
                             enough source content to display these changes.",
                ))
            })
            .children(sections);
        // The scroll offset moves without a render (wheel, smooth scroll):
        // re-render once it changed so the sticky header follows.
        let watch = {
            let (seen, handle, weak) = (
                self.turn_diff.seen_offset.clone(),
                self.turn_diff.scroll.clone(),
                cx.entity().downgrade(),
            );
            canvas(
                move |_, window, _| {
                    let y = f32::from(handle.offset().y);
                    if (seen.get() - y).abs() > 0.5 {
                        seen.set(y);
                        window.on_next_frame(move |_, cx| {
                            let _ = weak.update(cx, |_, cx| cx.notify());
                        });
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_0()
        };
        // Covers the inset strip above it too, where scrolled lines pass.
        let sticky = sticky.map(|(ix, top)| {
            div()
                .absolute()
                .top(px(top))
                .left_0()
                .right_0()
                .px(px(PANEL_PAD))
                .pt(px(PANEL_PAD))
                .bg(p.surface)
                .child(self.file_header(ix, &files[ix], true, p, cx))
        });
        div()
            .flex_none()
            .w(px(width))
            .h_full()
            .pr_1p5()
            .pb_3()
            .child(
                v_flex()
                    .id("turn-diff-panel")
                    .relative()
                    .size_full()
                    .rounded(px(12.))
                    .bg(p.surface)
                    .overflow_hidden()
                    .child(header)
                    // Clips the pinned header as the file's end pushes it
                    // up, so it never slides over the panel header.
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.smooth_scroll.region("turn-diff", body))
                            .children(sticky),
                    )
                    .child(watch)
                    // GPUI clips to rectangles: scrolled rows and the pinned
                    // header would show square corners past the rounding.
                    // Last child, so it paints over everything else.
                    .child(kit::corner_mask(12., p.bg)),
            )
    }

    /// Slim bar on top of the panel: what the turn changed, then fold-all
    /// and close, as the app's small icon buttons.
    fn panel_header(
        &self,
        files: &[TurnDiffFile],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = files.len();
        let (added, removed) = files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let all_open = files
            .iter()
            .all(|f| !self.turn_diff.collapsed.contains(&f.path));
        let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
        h_flex()
            .flex_none()
            .h(px(HEADER_H))
            .pl_3()
            .pr_1p5()
            .gap_2()
            .border_b_1()
            .border_color(p.border)
            .child(kit::icon(IconName::Diff, 14., p.dim))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .text_size(px(12.5))
                    .child(div().font_medium().text_color(p.text).child(match count {
                        0 => "Turn changes".to_string(),
                        1 => "1 file".to_string(),
                        n => format!("{n} files"),
                    }))
                    .when(count > 0, |el| {
                        el.child(
                            h_flex()
                                .gap_1p5()
                                .font_family(kit::MONO)
                                .text_size(px(11.))
                                .font_semibold()
                                .child(div().text_color(p.success).child(format!("+{added}")))
                                .child(div().text_color(p.danger).child(format!("-{removed}"))),
                        )
                    }),
            )
            .when(count > 1, |el| {
                el.child(
                    div().debug_selector(|| "turn-diff-fold".into()).child(
                        kit::icon_button(
                            "turn-diff-fold",
                            if all_open {
                                IconName::ChevronsDownUp
                            } else {
                                IconName::ChevronsUpDown
                            },
                            if all_open {
                                "Collapse all files"
                            } else {
                                "Expand all files"
                            },
                        )
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if all_open {
                                this.turn_diff.collapsed.extend(paths.iter().cloned());
                            } else {
                                this.turn_diff.collapsed.clear();
                            }
                            cx.notify();
                        })),
                    ),
                )
            })
            .child(
                div().debug_selector(|| "turn-diff-close".into()).child(
                    kit::icon_button("turn-diff-close", IconName::X, "Close turn diff (Esc)")
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.turn_diff.open = None;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    /// The file whose header has scrolled off the top, and the header's
    /// offset: it stays pinned until the file's end pushes it up.
    fn sticky_file(&self, files: &[TurnDiffFile]) -> Option<(usize, f32)> {
        let handle = &self.turn_diff.scroll;
        let scrolled = -f32::from(handle.offset().y);
        if scrolled <= 0. {
            return None;
        }
        let origin = f32::from(handle.bounds().top());
        (0..files.len()).find_map(|ix| {
            if self.turn_diff.collapsed.contains(&files[ix].path) {
                return None;
            }
            let b = handle.bounds_for_item(ix)?;
            // Child bounds are laid out unscrolled, relative to the viewport.
            let top = f32::from(b.top()) - origin - PANEL_PAD;
            let bottom = f32::from(b.bottom()) - origin - PANEL_PAD;
            (top < scrolled && bottom > scrolled)
                .then(|| (ix, (bottom - scrolled - FILE_HEADER_H).min(0.)))
        })
    }

    fn file_header(
        &self,
        ix: usize,
        file: &TurnDiffFile,
        sticky: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = !self.turn_diff.collapsed.contains(&file.path);
        let path = file.path.clone();
        let hover = p.card_hover;
        h_flex()
            .id(SharedString::from(format!(
                "turn-diff-file-{}{ix}",
                if sticky { "sticky-" } else { "" }
            )))
            .w_full()
            .h(px(FILE_HEADER_H))
            .flex_none()
            .gap(px(7.2))
            .px(px(11.2))
            .bg(p.card)
            .when(expanded, |el| el.rounded_t(px(8.)))
            .when(!expanded, |el| el.rounded(px(8.)))
            .when(expanded, |el| el.border_b_1().border_color(p.border))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .text_color(p.dim)
            .child(kit::icon(
                if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                14.,
                p.dim,
            ))
            .child(kit::icon(IconName::FileCode, 15., p.dim))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(kit::MONO)
                    .text_size(px(11.5))
                    .text_color(p.text)
                    .child(file.path.clone()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .font_family(kit::MONO)
                    .text_size(px(11.))
                    .font_semibold()
                    .child(
                        div()
                            .text_color(p.success)
                            .child(format!("+{}", file.added)),
                    )
                    .child(
                        div()
                            .text_color(p.danger)
                            .child(format!("-{}", file.removed)),
                    ),
            )
            .tooltip({
                let path = path.clone();
                move |window, cx| kit::wrap_tip(path.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.turn_diff.collapsed.remove(&path) {
                    this.turn_diff.collapsed.insert(path.clone());
                }
                cx.notify();
            }))
    }

    fn file_section(
        &self,
        ix: usize,
        file: &TurnDiffFile,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = !self.turn_diff.collapsed.contains(&file.path);
        let total = file.changes.len();
        let changes: Vec<AnyElement> = if expanded {
            file.changes
                .iter()
                .enumerate()
                .map(|(cix, change)| {
                    let key = format!("{}:{}", change.id, file.path);
                    let all = self.turn_diff.expanded.contains(&key);
                    let cap = if all { usize::MAX } else { CHANGE_LINES };
                    let mut rows: Vec<AnyElement> = Vec::new();
                    let mut lines = 0;
                    let multi = change.hunks.len() > 1;
                    for (hix, d) in change.hunks.iter().enumerate() {
                        let left = cap.saturating_sub(lines);
                        let (hunk_rows, _, _, shown) = diff_rows(d, left, p);
                        if (multi || d.old_start.is_some()) && left > 0 {
                            rows.push(hunk_marker(d, hix == 0, p));
                        }
                        rows.extend(hunk_rows);
                        lines += shown;
                    }
                    let hidden = lines.saturating_sub(cap);
                    v_flex()
                        .w_full()
                        .when(cix > 0, |el| el.border_t_1().border_color(p.border))
                        .when(total > 1, |el| {
                            el.child(
                                h_flex()
                                    .gap(px(7.2))
                                    .px_3()
                                    .py_2()
                                    .bg(p.chip.opacity(0.5))
                                    .border_b_1()
                                    .border_color(p.border.opacity(0.6))
                                    .font_family(kit::MONO)
                                    .text_size(px(10.6))
                                    .font_medium()
                                    .text_color(p.dim)
                                    .child(kit::icon(IconName::GitCommitHorizontal, 13., p.muted))
                                    .child(format!("Change {} of {total}", cix + 1)),
                            )
                        })
                        .child(
                            v_flex()
                                .w_full()
                                .line_height(px(TOOL_LINE_H))
                                .font_family(kit::MONO)
                                .text_xs()
                                .children(rows),
                        )
                        .when(hidden > 0, |el| {
                            // Same row as the transcript's "Show N more lines".
                            let quip_key = format!("turn-diff-more-{key}");
                            let hover = p.card_hover.opacity(0.55);
                            let row = h_flex()
                                .id(SharedString::from(format!("turn-diff-more-{key}")))
                                .gap_1p5()
                                .px_3()
                                .py_1()
                                .cursor_pointer()
                                .hover(move |s| s.bg(hover))
                                .text_xs()
                                .text_color(p.dim)
                                .child(kit::icon(IconName::ChevronsDown, 12., p.dim))
                                .child(format!(
                                    "Show {hidden} more {}",
                                    if hidden == 1 { "line" } else { "lines" }
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.turn_diff.expanded.insert(key.clone());
                                    cx.notify();
                                }));
                            el.child(ReadingQuip::new(quip_key, row, p.dim))
                        })
                        .into_any_element()
                })
                .collect()
        } else {
            Vec::new()
        };
        v_flex()
            .w_full()
            .flex_none()
            .rounded(px(8.))
            .bg(p.bg)
            .child(self.file_header(ix, file, false, p, cx))
            .when(expanded, |el| {
                // Bottom room keeps square rows inside the rounded corners.
                el.child(v_flex().w_full().pb_2().children(changes))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::AppContext as _;
    use serde_json::json;

    use super::super::{Page, QmtApp};
    use super::MIN_W;
    use crate::session::SessionView;

    struct Harness {
        handle: gpui_kit::AnyWindowHandle,
        app: gpui_kit::Entity<QmtApp>,
    }

    /// The open session: prompt u1, two edits, a reply (turn closing at item
    /// 2), then prompt u2 with its reply.
    fn harness(cx: &mut gpui_kit::TestAppContext) -> Harness {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut view = SessionView::new("test-agent", "s", "/w", "Source");
        let text = |kind: &str, id: &str, text: &str| {
            json!({ "sessionUpdate": kind, "messageId": id,
                "content": { "type": "text", "text": text } })
        };
        view.apply(&text("user_message_chunk", "u1", "change things"));
        view.apply(&crate::session::undo_tests::edit(
            "t1", "a.rs", "a\n", "b\n",
        ));
        view.apply(&crate::session::undo_tests::edit(
            "t2", "b.rs", "", "c\nd\n",
        ));
        view.apply(&text("agent_message_chunk", "r1", "done"));
        view.apply(&text("user_message_chunk", "u2", "thanks"));
        view.apply(&text("agent_message_chunk", "r2", "sure"));
        let mut store = crate::store::automations::schedule_ui_fixture();
        store.session = Some(view);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        Harness { handle, app }
    }

    #[gpui_kit::test]
    fn the_pill_toggles_the_turn_diff(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                // Item 2 closes the first turn.
                app.toggle_turn_diff(2, cx);
                assert_eq!(app.turn_diff_range(cx), Some(0..3));
            });
            window.render_frame(cx);
            h.app.update(cx, |app, cx| {
                // The same pill closes it.
                app.toggle_turn_diff(2, cx);
                assert!(app.turn_diff.open.is_none());
                app.toggle_turn_diff(2, cx);
                // Escape closes it once no popover or dialog is open.
                assert!(app.close_topmost());
                assert!(app.turn_diff.open.is_none());
                assert!(!app.close_topmost());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_running_turn_has_its_pill_and_live_diff(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.store.update(cx, |s, _| {
                    let view = s.session.as_mut().unwrap();
                    view.apply(
                        &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u3",
                        "content": { "type": "text", "text": "more" } }),
                    );
                    view.apply(&crate::session::undo_tests::edit("t3", "c.rs", "", "e\n"));
                    view.prompting = true;
                });
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(h.handle, cx);
        // Items: u1, edits, r1, u2, r2, u3, edit t3 (6, still running).
        assert!(
            vcx.debug_bounds("diff-pill-6").is_some(),
            "live turn shows its pill"
        );
        assert!(vcx.debug_bounds("diff-pill-2").is_some());
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.toggle_turn_diff(6, cx);
                assert_eq!(app.turn_diff_range(cx), Some(5..7));
                // A new edit while it runs joins the open panel.
                app.store.update(cx, |s, _| {
                    let view = s.session.as_mut().unwrap();
                    view.apply(&crate::session::undo_tests::edit("t4", "d.rs", "", "f\n"));
                });
                let range = app.turn_diff_range(cx).unwrap();
                let files = app
                    .store
                    .read(cx)
                    .session
                    .as_ref()
                    .unwrap()
                    .turn_diffs(range);
                assert_eq!(files.len(), 2);
            });
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_header_folds_every_file(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.toggle_turn_diff(2, cx);
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(h.handle, cx);
        let fold = vcx.debug_bounds("turn-diff-fold").expect("fold button");
        assert!(vcx.debug_bounds("turn-diff-close").is_some());
        vcx.simulate_click(fold.center(), gpui_kit::Modifiers::none());
        cx.update(|cx| {
            let collapsed = &h.app.read(cx).turn_diff.collapsed;
            assert!(collapsed.contains("a.rs") && collapsed.contains("b.rs"));
        });
        let fold = vcx.debug_bounds("turn-diff-fold").unwrap();
        vcx.simulate_click(fold.center(), gpui_kit::Modifiers::none());
        cx.update(|cx| assert!(h.app.read(cx).turn_diff.collapsed.is_empty()));
        let close = vcx.debug_bounds("turn-diff-close").unwrap();
        vcx.simulate_click(close.center(), gpui_kit::Modifiers::none());
        cx.update(|cx| assert!(h.app.read(cx).turn_diff.open.is_none()));
    }

    #[gpui_kit::test]
    fn hovering_show_more_asks_if_anyone_reads_this(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{Modifiers, point, px};
        let h = harness(cx);
        let long: String = (0..450).map(|i| format!("line {i}\n")).collect();
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.store.update(cx, |s, _| {
                    let view = s.session.as_mut().unwrap();
                    view.apply(
                        &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u3",
                        "content": { "type": "text", "text": "big" } }),
                    );
                    view.apply(&crate::session::undo_tests::edit("t9", "big.rs", "", &long));
                });
                app.toggle_turn_diff(6, cx);
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(h.handle, cx);
        let laid_out = vcx
            .debug_bounds("turn-diff-more-t9:big.rs-row")
            .expect("show more row");
        // The row ends a 400-line change: scroll it into view first.
        let app = h.app.clone();
        vcx.update(|window, cx| {
            app.update(cx, |app, _| {
                let top = app.turn_diff.scroll.bounds().top();
                app.turn_diff
                    .scroll
                    .set_offset(point(px(0.), top + px(200.) - laid_out.top()));
            });
            window.render_frame(cx);
        });
        let row = vcx
            .debug_bounds("turn-diff-more-t9:big.rs-row")
            .expect("show more row");
        assert!(
            row.top() > px(0.) && row.bottom() < vcx.update(|w, _| w.viewport_size().height),
            "row on screen: {row:?}"
        );
        assert!(
            vcx.debug_bounds("reading-quip").is_none(),
            "hidden until hovered"
        );
        let settle = |vcx: &mut gpui_kit::VisualTestContext| {
            // The fade runs on the wall clock.
            std::thread::sleep(std::time::Duration::from_millis(320));
            vcx.update(|window, cx| window.render_frame(cx));
            vcx.run_until_parked();
        };
        vcx.simulate_mouse_move(row.center(), None, Modifiers::none());
        settle(&mut vcx);
        assert!(
            vcx.debug_bounds("reading-quip").is_some(),
            "fades in on hover"
        );
        vcx.simulate_mouse_move(point(px(5.), px(5.)), None, Modifiers::none());
        settle(&mut vcx);
        assert!(
            vcx.debug_bounds("reading-quip").is_none(),
            "fades out on leave"
        );
    }

    #[gpui_kit::test]
    fn a_gone_turn_closes_the_panel(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.toggle_turn_diff(2, cx);
                let store = app.store.clone();
                store.update(cx, |s, _| {
                    s.session = Some(SessionView::new("test-agent", "other", "/w", "Other"))
                });
            });
            window.render_frame(cx);
            h.app
                .update(cx, |app, _| assert!(app.turn_diff.open.is_none()));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_mouse_drags_the_separator(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{Modifiers, MouseButton, point, px};
        let h = harness(cx);
        let (start, vw) = cx
            .update_window(h.handle, |_, window, cx| {
                h.app.update(cx, |app, cx| {
                    app.go(Page::Session, cx);
                    app.toggle_turn_diff(2, cx);
                });
                window.render_frame(cx);
                let vw = f32::from(window.viewport_size().width);
                (
                    h.app.update(cx, |app, cx| app.turn_diff_width(window, cx)),
                    vw,
                )
            })
            .unwrap();
        let mut vcx = gpui_kit::VisualTestContext::from_window(h.handle, cx);
        // The separator's hairline sits just left of the panel.
        let x = vw - start - super::SEPARATOR_W / 2.;
        let at = |x: f32| point(px(x), px(300.));
        vcx.simulate_mouse_move(at(x), None, Modifiers::none());
        vcx.simulate_mouse_down(at(x), MouseButton::Left, Modifiers::none());
        for step in 1..=4 {
            vcx.simulate_mouse_move(
                at(x - 20. * step as f32),
                MouseButton::Left,
                Modifiers::none(),
            );
        }
        vcx.simulate_mouse_up(at(x - 80.), MouseButton::Left, Modifiers::none());
        vcx.run_until_parked();
        cx.update(|cx| {
            let saved = h.app.read(cx).store.read(cx).prefs.turn_diff_width;
            assert_eq!(saved, Some((start + 80.).round()));
        });
    }

    #[gpui_kit::test]
    fn dragging_the_separator_resizes_and_remembers(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let h = harness(cx);
        cx.update_window(h.handle, |_, window, cx| {
            h.app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.toggle_turn_diff(2, cx);
                let page = f32::from(window.viewport_size().width - app.sidebar_width(cx));
                let default = app.turn_diff_width(window, cx);
                assert_eq!(default, (page * 0.44).clamp(MIN_W, super::CHAT_MAX));
                // A drag 60px to the left widens the panel by 60px…
                app.turn_diff.drag = Some((gpui_kit::px(500.), default));
                app.turn_diff.live_width = Some(default + 60.);
                let live = app.turn_diff_width(window, cx);
                app.finish_turn_diff_resize(cx);
                assert_eq!(app.store.read(cx).prefs.turn_diff_width, Some(live.round()));
                assert!(app.turn_diff.drag.is_none() && app.turn_diff.live_width.is_none());
                // …and is kept for the next panel; it never squeezes the chat.
                assert_eq!(app.turn_diff_width(window, cx), live.round());
                app.store.update(cx, |s, cx| {
                    s.update_prefs(|p| p.turn_diff_width = Some(99_999.), cx)
                });
                assert!(app.turn_diff_width(window, cx) < page);
            });
            window.render_frame(cx);
        })
        .unwrap();
    }
}
