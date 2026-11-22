//! Log viewer: one full-size modal for an agent's log (desktop
//! `SidecarLogList`, reworked). Search, source and level filters, line
//! wrapping, tail following, copy and clear; each line opens to show its
//! full text and origin. Rows are virtualized, so thousands of lines stay
//! smooth.

use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, Selectable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agents::modal_scrim;
use super::{QmtApp, kit};
use crate::store::agent_log::{Level, LogEntry, LogSource, clock};
use crate::theme::{Palette, palette};

/// What the viewer shows.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LogScope {
    /// The log of one agent.
    Agent(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SourceFilter {
    #[default]
    All,
    Agent,
    App,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum LevelFilter {
    #[default]
    All,
    /// Warnings and errors.
    Warnings,
    Errors,
}

impl LevelFilter {
    fn admits(self, level: Level) -> bool {
        match self {
            Self::All => true,
            Self::Warnings => level <= Level::Warn,
            Self::Errors => level == Level::Error,
        }
    }
}

/// Inputs the shown rows were built from.
#[derive(Clone, PartialEq)]
struct Built {
    scope: LogScope,
    /// First and last entry seq of the log at build time.
    first: u64,
    last: u64,
    query: String,
    source: SourceFilter,
    level: LevelFilter,
}

pub(super) struct LogView {
    pub(super) scope: Option<LogScope>,
    pub(super) query: Entity<InputState>,
    pub(super) source: SourceFilter,
    pub(super) level: LevelFilter,
    pub(super) wrap: bool,
    /// Entries opened to their full text, by seq.
    expanded: HashSet<u64>,
    list: ListState,
    /// Seqs of the shown entries, oldest first.
    shown: Rc<Vec<u64>>,
    built: Option<Built>,
}

impl LogView {
    pub(super) fn new(window: &mut Window, cx: &mut Context<QmtApp>) -> (Self, Subscription) {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let sub = cx.subscribe_in(&query, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        let view = Self {
            scope: None,
            query,
            source: SourceFilter::All,
            level: LevelFilter::All,
            wrap: false,
            expanded: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(800.)),
            shown: Rc::default(),
            built: None,
        };
        (view, sub)
    }

    /// Keep `shown` in step with the log: appended lines splice in at the
    /// end (measurements and scroll stay), dropped ones leave the front,
    /// anything else rebuilds.
    fn sync(&mut self, entries: &std::collections::VecDeque<LogEntry>, built: Built) {
        if self.built.as_ref() == Some(&built) {
            return;
        }
        let matches = |e: &LogEntry| {
            let source = match built.source {
                SourceFilter::All => true,
                SourceFilter::Agent => e.source == LogSource::Agent,
                SourceFilter::App => e.source == LogSource::App,
            };
            source
                && built.level.admits(e.level)
                && (built.query.is_empty()
                    || e.text.to_lowercase().contains(&built.query)
                    || e.target
                        .as_ref()
                        .is_some_and(|t| t.to_lowercase().contains(&built.query)))
        };
        let same_view = self.built.as_ref().is_some_and(|old| {
            old.scope == built.scope
                && old.query == built.query
                && old.source == built.source
                && old.level == built.level
                && built.last >= old.last
        });
        if same_view {
            let old_last = self.built.as_ref().map_or(0, |b| b.last);
            let mut shown = (*self.shown).clone();
            let dropped = shown.iter().take_while(|s| **s < built.first).count();
            if dropped > 0 {
                shown.drain(..dropped);
                self.list.splice(0..dropped, 0);
            }
            let start = shown.len();
            shown.extend(
                entries
                    .iter()
                    .filter(|e| e.seq > old_last && matches(e))
                    .map(|e| e.seq),
            );
            self.list.splice(start..start, shown.len() - start);
            self.shown = Rc::new(shown);
        } else {
            let shown: Vec<u64> = entries
                .iter()
                .filter(|e| matches(e))
                .map(|e| e.seq)
                .collect();
            self.list.reset(shown.len());
            self.list.set_follow_mode(FollowMode::Tail);
            self.shown = Rc::new(shown);
        }
        self.built = Some(built);
    }
}

fn level_color(level: Level, p: &Palette) -> Hsla {
    match level {
        Level::Error => p.danger,
        Level::Warn => p.warn,
        Level::Info => p.muted,
        Level::Debug | Level::Trace => p.dim,
    }
}

/// `10:42:07.123 WARN [agent] target: text`, for the clipboard.
fn plain(e: &LogEntry) -> String {
    let source = match e.source {
        LogSource::Agent => "agent",
        LogSource::App => "app",
    };
    match &e.target {
        Some(t) => format!(
            "{} {} [{source}] {t}: {}",
            clock(e.at_ms),
            e.level.label(),
            e.text
        ),
        None => format!(
            "{} {} [{source}] {}",
            clock(e.at_ms),
            e.level.label(),
            e.text
        ),
    }
}

const COPIED_LOG: &str = "log-view-copy";

impl QmtApp {
    /// Open the log of agent `agent` in the full-size viewer.
    pub(super) fn open_agent_logs(
        &mut self,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One modal at a time: the details sheet this may come from closes.
        self.details = None;
        let v = &mut self.log_view;
        v.scope = Some(LogScope::Agent(agent.to_string()));
        v.expanded.clear();
        v.built = None;
        v.query
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        v.query.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn close_log_view(&mut self) -> bool {
        self.log_view.scope.take().is_some()
    }

    fn log_view_entries<'a>(
        &self,
        cx: &'a App,
    ) -> Option<&'a std::collections::VecDeque<LogEntry>> {
        match self.log_view.scope.as_ref()? {
            LogScope::Agent(agent) => Some(self.store.read(cx).runtime(agent)?.log.entries()),
        }
    }

    pub(super) fn render_log_view(
        &mut self,
        radii: Corners<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let scope = self.log_view.scope.clone()?;
        let p = *palette(cx);
        let LogScope::Agent(agent) = &scope;
        let agent = agent.clone();
        let query = self.log_view.query.read(cx).value().trim().to_lowercase();
        let (name, total, first, last) = {
            let store = self.store.read(cx);
            let name = store
                .config(&agent)
                .map_or_else(|| agent.clone(), |c| c.name.clone());
            let log = store.runtime(&agent).map(|rt| &rt.log);
            let first = log.and_then(|l| l.entries().front()).map_or(0, |e| e.seq);
            (
                name,
                log.map_or(0, |l| l.entries().len()),
                first,
                log.map_or(0, |l| l.last_seq()),
            )
        };
        let built = Built {
            scope: scope.clone(),
            first,
            last,
            query: query.clone(),
            source: self.log_view.source,
            level: self.log_view.level,
        };
        let store = self.store.clone();
        let empty = std::collections::VecDeque::new();
        {
            let store = store.read(cx);
            let entries = store.runtime(&agent).map_or(&empty, |rt| rt.log.entries());
            self.log_view.sync(entries, built);
        }
        let shown = self.log_view.shown.len();
        let filtered = !query.is_empty()
            || self.log_view.source != SourceFilter::All
            || self.log_view.level != LevelFilter::All;
        let following = self.log_view.list.is_following_tail();
        let wrap = self.log_view.wrap;
        let copied = self.copied.as_deref() == Some(COPIED_LOG);

        // ---- header: what this is, and the actions on the whole log
        let action =
            |id: &'static str, icon: IconName, tip: &'static str| kit::icon_button(id, icon, tip);
        let header = h_flex()
            .gap_3()
            .px_5()
            .pt_4()
            .pb_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_lg().font_semibold().child("Agent logs"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .truncate()
                            .child(format!("{name} · times in UTC")),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(tagged(
                        "log-wrap",
                        action("log-wrap", IconName::TextWrap, "Wrap long lines")
                            .selected(wrap)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.log_view.wrap = !this.log_view.wrap;
                                this.log_view.list.remeasure();
                                cx.notify();
                            })),
                    ))
                    .child(tagged(
                        "log-follow",
                        action("log-follow", IconName::ArrowDownToLine, "Follow new lines")
                            .selected(following)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let list = &this.log_view.list;
                                if list.is_following_tail() {
                                    list.pause_following_tail();
                                } else {
                                    list.set_follow_mode(FollowMode::Tail);
                                }
                                cx.notify();
                            })),
                    ))
                    .child(tagged(
                        "log-copy",
                        action(
                            "log-copy",
                            if copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            },
                            "Copy shown lines",
                        )
                        .disabled(shown == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let text = this.log_view_text(cx);
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            this.flash_copied(COPIED_LOG.into(), cx);
                        })),
                    ))
                    .child(tagged(
                        "log-clear",
                        action("log-clear", IconName::Eraser, "Clear log")
                            .disabled(total == 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let agent = agent.clone();
                                this.log_view.expanded.clear();
                                this.store.update(cx, |s, cx| s.clear_agent_log(&agent, cx));
                            })),
                    ))
                    .child(div().w(px(1.)).h(px(18.)).mx_1().bg(p.border))
                    .child(
                        kit::icon_button("log-close", IconName::X, "Close").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.close_log_view();
                                cx.notify();
                            },
                        )),
                    ),
            );

        // ---- toolbar: search, filters, count
        let focused = self
            .log_view
            .query
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search = kit::field_shell(focused, &p)
            .flex_1()
            .min_w(px(180.))
            .h(px(32.))
            .child(kit::icon(IconName::Search, 14., p.dim))
            .child(
                Input::new(&self.log_view.query)
                    .appearance(false)
                    .aria_label("Search logs")
                    .flex_1(),
            );
        let sources = [
            (SourceFilter::All, "All", "log-src-all"),
            (SourceFilter::Agent, "Agent", "log-src-agent"),
            (SourceFilter::App, "App", "log-src-app"),
        ];
        let levels = [
            (LevelFilter::All, "All levels", "log-lvl-all"),
            (LevelFilter::Warnings, "Warnings", "log-lvl-warn"),
            (LevelFilter::Errors, "Errors", "log-lvl-error"),
        ];
        let source_bar = kit::segment_bar(&p).children(sources.map(|(f, label, id)| {
            kit::segment(id, self.log_view.source == f, &p)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.log_view.source = f;
                    cx.notify();
                }))
        }));
        let level_bar = kit::segment_bar(&p).children(levels.map(|(f, label, id)| {
            kit::segment(id, self.log_view.level == f, &p)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.log_view.level = f;
                    cx.notify();
                }))
        }));
        let count = if filtered {
            format!(
                "{} of {}",
                kit::short_num(shown as u64),
                kit::short_num(total as u64)
            )
        } else {
            format!("{} lines", kit::short_num(total as u64))
        };
        let toolbar = h_flex()
            .gap_2()
            .px_5()
            .pb_3()
            .child(search)
            .child(source_bar)
            .child(level_bar)
            .child(
                div()
                    .flex_none()
                    .min_w(px(72.))
                    .text_right()
                    .text_xs()
                    .text_color(p.dim)
                    .child(count),
            );

        // ---- body: the lines, or why there are none
        let body = if shown == 0 {
            let (title, text) = if total == 0 {
                (
                    "No log lines yet",
                    "Lines appear as the agent starts and runs.",
                )
            } else {
                ("No lines match", "Change the search or the filters.")
            };
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(kit::icon(IconName::ScrollText, 22., p.dim))
                .child(div().mt_1().font_medium().child(title))
                .child(div().text_sm().text_color(p.muted).child(text))
                .when(total > 0, |el| {
                    el.child(
                        kit::flat_btn("log-reset", true, true, &p)
                            .mt_2()
                            .child("Clear filters")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.log_view.source = SourceFilter::All;
                                this.log_view.level = LevelFilter::All;
                                this.log_view.query.update(cx, |s, cx| {
                                    s.set_value(SharedString::default(), window, cx)
                                });
                                cx.notify();
                            })),
                    )
                })
                .into_any_element()
        } else {
            let render =
                cx.processor(|this: &mut Self, ix: usize, _, cx| this.render_log_row(ix, cx));
            list(self.log_view.list.clone(), move |ix, window, cx| {
                render(ix, window, cx)
            })
            .size_full()
            .py_1()
            .into_any_element()
        };
        let well = div()
            .relative()
            .flex_1()
            .min_h_0()
            .mx_5()
            .mb_5()
            .rounded(px(10.))
            .border_1()
            .border_color(p.border)
            .bg(p.bg)
            .child(body)
            .child(kit::corner_mask(10., p.surface));

        let card = v_flex()
            .id("log-view")
            .w(relative(0.92))
            .max_w(px(1120.))
            .h(relative(0.88))
            .max_h(px(900.))
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(toolbar)
            .child(well);
        Some(
            modal_scrim("scrim-log-view", radii, &p)
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_log_view();
                    cx.notify();
                }))
                .child(card)
                .into_any_element(),
        )
    }

    /// The shown lines as plain text.
    fn log_view_text(&self, cx: &App) -> String {
        let Some(entries) = self.log_view_entries(cx) else {
            return String::new();
        };
        let Some(first) = entries.front().map(|e| e.seq) else {
            return String::new();
        };
        self.log_view
            .shown
            .iter()
            .filter_map(|seq| entries.get((seq - first) as usize))
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_log_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = *palette(cx);
        let Some(&seq) = self.log_view.shown.get(ix) else {
            return div().into_any_element();
        };
        let entry = self.log_view_entries(cx).and_then(|entries| {
            let first = entries.front()?.seq;
            entries.get(seq.checked_sub(first)? as usize).cloned()
        });
        let Some(e) = entry else {
            return div().into_any_element();
        };
        let open = self.log_view.expanded.contains(&seq);
        let wrap = self.log_view.wrap || open;
        let hover = p.card_hover;
        let tint = match e.level {
            Level::Error => Some(p.danger.opacity(0.07)),
            Level::Warn => Some(p.warn.opacity(0.06)),
            _ => None,
        };
        let text_color = match e.level {
            Level::Debug | Level::Trace => p.muted,
            _ => p.text,
        };
        let row = h_flex()
            .id(("log-row", seq))
            .w_full()
            .items_start()
            .gap_3()
            .px_3()
            .py(px(2.))
            .rounded(px(6.))
            .cursor_pointer()
            .font_family(kit::MONO)
            .text_xs()
            .when_some(tint, |el, t| el.bg(t))
            .when(open, |el| el.bg(p.card))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.log_view.expanded.remove(&seq) {
                    this.log_view.expanded.insert(seq);
                }
                this.log_view.list.remeasure_items(ix..ix + 1);
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(96.))
                    .whitespace_nowrap()
                    .text_color(p.dim)
                    .child(clock(e.at_ms)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(44.))
                    .whitespace_nowrap()
                    .font_medium()
                    .text_color(level_color(e.level, &p))
                    .child(e.level.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(text_color)
                    .when(!wrap, |el| el.truncate())
                    .child(message(&e, &p)),
            );
        let detail = open.then(|| {
            let origin = match (&e.source, &e.target) {
                (LogSource::Agent, Some(t)) => format!("Agent · {t}"),
                (LogSource::Agent, None) => "Agent".to_string(),
                (LogSource::App, _) => "This app".to_string(),
            };
            h_flex()
                .ml(px(96. + 44. + 24.))
                .mr_3()
                .mb_1()
                .gap_3()
                .text_xs()
                .text_color(p.dim)
                .child(div().flex_1().min_w_0().child(origin))
                .child(kit::copy_button(
                    ("log-copy-line", seq),
                    plain(&e),
                    "Copy line",
                ))
        });
        // The list only offers its width as available space; claim it, or
        // the row sizes to its text and long lines run past the edge.
        v_flex()
            .w_full()
            .px_1()
            .child(row)
            .children(detail)
            .into_any_element()
    }
}

/// A line's text after its origin in a dim prefix: the agent's `tracing`
/// target, or `app` for this app's own notes. One run of text, so it wraps
/// and truncates as a whole.
fn message(e: &LogEntry, p: &Palette) -> StyledText {
    let prefix = match (e.source, &e.target) {
        (LogSource::App, _) => Some("app".to_string()),
        (LogSource::Agent, target) => target.clone(),
    };
    match prefix {
        Some(prefix) => {
            let head = prefix.len() + 2;
            StyledText::new(format!("{prefix}: {}", e.text)).with_highlights(vec![(
                0..head,
                HighlightStyle {
                    color: Some(p.dim),
                    ..Default::default()
                },
            )])
        }
        None => StyledText::new(e.text.clone()),
    }
}

/// `el` under a debug selector, for tests.
fn tagged(id: &'static str, el: impl IntoElement) -> Div {
    div().debug_selector(move || id.into()).child(el)
}

#[cfg(test)]
mod tests {
    use super::{LevelFilter, LogScope, SourceFilter};
    use crate::store::agent_log::Level;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Modifiers, VisualTestContext};

    #[gpui_kit::test]
    fn agent_logs_open_full_size_and_filter(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let log = &mut store.runtime_mut_for_test("test-agent").log;
        log.push_app(Level::Info, "Connecting to qmtcode --acp…");
        log.push_agent("2026-11-20T10:42:07.123Z ERROR querymt::session: prompt failed: boom");
        log.push_agent("2026-11-20T10:42:08.000Z  INFO querymt::mesh: joined");
        log.push_agent("2026-11-20T10:42:09.000Z DEBUG querymt::rpc: frame");
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        let render = |cx: &mut gpui_kit::TestAppContext| {
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Agents, cx);
                app.details = Some("test-agent".into());
            })
        });
        render(cx);

        // The details sheet links to the logs; they replace it.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let open = vcx.debug_bounds("details-open-logs").expect("Open logs…");
        vcx.simulate_click(open.center(), Modifiers::none());
        render(cx);
        let shown = |cx: &mut gpui_kit::TestAppContext| {
            app.read_with(cx, |app, _| app.log_view.shown.len())
        };
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.log_view.scope,
                Some(LogScope::Agent("test-agent".into()))
            );
            assert!(app.details.is_none(), "one modal at a time");
        });
        assert_eq!(shown(cx), 4);

        // Level and source filters, then search.
        cx.update(|cx| app.update(cx, |app, _| app.log_view.level = LevelFilter::Warnings));
        render(cx);
        assert_eq!(shown(cx), 1);
        cx.update(|cx| {
            app.update(cx, |app, _| {
                app.log_view.level = LevelFilter::All;
                app.log_view.source = SourceFilter::App;
            })
        });
        render(cx);
        assert_eq!(shown(cx), 1);
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.log_view.source = SourceFilter::All;
                app.log_view
                    .query
                    .update(cx, |s, cx| s.set_value("mesh", window, cx));
            })
        })
        .unwrap();
        render(cx);
        assert_eq!(shown(cx), 1);

        // New lines join while the viewer is open.
        let store = app.read_with(cx, |app, _| app.store.clone());
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.runtime_mut_for_test("test-agent")
                    .log
                    .push_agent(" WARN querymt::mesh: peer left")
            })
        });
        render(cx);
        assert_eq!(shown(cx), 2);

        // Copy takes the shown lines.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let copy = vcx.debug_bounds("log-copy").expect("Copy");
        vcx.simulate_click(copy.center(), Modifiers::none());
        let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
        let lines: Vec<&str> = copied.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "10:42:08.000 INFO [agent] querymt::mesh: joined");
        // No timestamp on the line: the time it arrived.
        assert!(lines[1].ends_with(" WARN [agent] querymt::mesh: peer left"));

        // Escape closes it.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        vcx.simulate_keystrokes("escape");
        app.read_with(cx, |app, _| assert!(app.log_view.scope.is_none()));
    }
}
