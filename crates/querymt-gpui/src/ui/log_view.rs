//! Log viewer: one full-size modal for an agent's log and for its debug
//! events (desktop `SidecarLogList`, reworked; it replaces the desktop's
//! `SessionTechnicalDetails`). Search, filters, line wrapping, tail
//! following, copy and clear; each row opens to show everything about it.
//! Rows are virtualized, so thousands of lines stay smooth.
//!
//! - Agent logs: this app's notes and the agent's stderr, by level.
//! - Debug events (Developer mode): the JSON-RPC messages of a session or of
//!   the whole agent, requests paired with their responses.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, Selectable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agents::modal_scrim;
use super::{QmtApp, kit};
use crate::rpc_log::{Body, WireEvent, WireLog};
use crate::store::agent_log::{Level, LogEntry, LogSource, clock};
use crate::theme::{Palette, palette};

/// What the viewer shows.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LogScope {
    /// The log of one agent.
    Agent(String),
    /// Debug events of one agent, or of one of its sessions.
    Events {
        agent: String,
        session: Option<String>,
    },
}

impl LogScope {
    fn agent(&self) -> &str {
        match self {
            Self::Agent(agent) | Self::Events { agent, .. } => agent,
        }
    }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum EventFilter {
    #[default]
    All,
    /// Requests with responses, either way.
    Calls,
    Notifications,
    /// Calls that failed.
    Errors,
}

impl EventFilter {
    fn admits(self, e: &WireEvent) -> bool {
        match self {
            Self::All => true,
            Self::Calls => e.kind.is_call(),
            Self::Notifications => !e.kind.is_call(),
            Self::Errors => e.failed(),
        }
    }
}

/// Inputs the shown rows were built from.
#[derive(Clone, PartialEq)]
struct Built {
    scope: LogScope,
    /// First and last seq of the source at build time.
    first: u64,
    last: u64,
    /// Recorder version, when the filter depends on in-place changes
    /// (a reply turning a call into an error).
    version: u64,
    query: String,
    source: SourceFilter,
    level: LevelFilter,
    events: EventFilter,
}

impl Built {
    /// Same rows, possibly with more appended.
    fn extends(&self, old: &Built) -> bool {
        self.scope == old.scope
            && self.version == old.version
            && self.query == old.query
            && self.source == old.source
            && self.level == old.level
            && self.events == old.events
            && self.last >= old.last
    }
}

pub(super) struct LogView {
    pub(super) scope: Option<LogScope>,
    pub(super) query: Entity<InputState>,
    pub(super) source: SourceFilter,
    pub(super) level: LevelFilter,
    pub(super) events: EventFilter,
    pub(super) wrap: bool,
    /// Rows opened to show everything about them, by seq.
    expanded: HashSet<u64>,
    list: ListState,
    /// Seqs of the shown rows, oldest first.
    pub(super) shown: Rc<Vec<u64>>,
    built: Option<Built>,
    /// Pretty-printed message bodies of opened events, by (seq, part).
    pretty: HashMap<(u64, u8), SharedString>,
    /// Recorder version the rows were last measured at.
    measured: u64,
    /// Re-renders while debug events are open: the recorder changes without
    /// telling the store.
    _poll: Option<Task<()>>,
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
            events: EventFilter::All,
            wrap: false,
            expanded: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(800.)),
            shown: Rc::default(),
            built: None,
            pretty: HashMap::new(),
            measured: 0,
            _poll: None,
        };
        (view, sub)
    }

    fn filtered(&self, query: &str) -> bool {
        !query.is_empty()
            || match self.scope {
                Some(LogScope::Agent(_)) => {
                    self.source != SourceFilter::All || self.level != LevelFilter::All
                }
                _ => self.events != EventFilter::All,
            }
    }

    /// Keep `shown` in step with the source: appended rows splice in at the
    /// end (measurements and scroll stay), dropped ones leave the front,
    /// anything else rebuilds. `matching(after)`: seqs past `after` that
    /// pass the filters.
    fn sync(&mut self, built: Built, matching: impl Fn(u64) -> Vec<u64>) {
        if self.built.as_ref() == Some(&built) {
            return;
        }
        match self.built.as_ref().filter(|old| built.extends(old)) {
            Some(old) => {
                let old_last = old.last;
                let mut shown = (*self.shown).clone();
                let dropped = shown.iter().take_while(|s| **s < built.first).count();
                if dropped > 0 {
                    shown.drain(..dropped);
                    self.list.splice(0..dropped, 0);
                }
                let start = shown.len();
                shown.extend(matching(old_last));
                self.list.splice(start..start, shown.len() - start);
                self.shown = Rc::new(shown);
            }
            None => {
                let fresh = self.built.as_ref().is_none_or(|b| b.scope != built.scope);
                let following = self.list.is_following_tail();
                let top = self.list.logical_scroll_top();
                let shown = matching(0);
                self.list.reset(shown.len());
                if fresh || following {
                    self.list.set_follow_mode(FollowMode::Tail);
                } else {
                    self.list.scroll_to(ListOffset {
                        item_ix: top.item_ix.min(shown.len()),
                        offset_in_item: px(0.),
                    });
                }
                self.shown = Rc::new(shown);
            }
        }
        self.built = Some(built);
    }

    /// Pretty JSON of an event body, made once per opened row.
    fn pretty(&mut self, seq: u64, part: u8, body: impl FnOnce() -> String) -> SharedString {
        self.pretty
            .entry((seq, part))
            .or_insert_with(|| body().into())
            .clone()
    }
}

// ---- formatting ------------------------------------------------------------

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
    let head = format!("{} {} [{source}]", clock(e.at_ms), e.level.label());
    match &e.target {
        Some(t) => format!("{head} {t}: {}", e.text),
        None => format!("{head} {}", e.text),
    }
}

/// What a row of an event is called: the kind of a session update, or the
/// method.
fn event_label(e: &WireEvent) -> &str {
    e.update.as_deref().unwrap_or(&e.method)
}

fn duration(ms: f64) -> String {
    if ms < 1000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.)
    }
}

/// Status of an event as words: how its call went.
fn event_status(e: &WireEvent) -> Option<String> {
    if e.pending() {
        return Some("waiting".into());
    }
    let reply = e.reply.as_ref()?;
    Some(match &reply.error {
        Some(_) => "failed".into(),
        None => duration(e.duration_ms().unwrap_or(0.)),
    })
}

/// `10:42:07.123 → session/prompt {…} 120 ms`, for the clipboard.
fn plain_event(e: &WireEvent) -> String {
    let arrow = if e.kind.outgoing() { "→" } else { "←" };
    let preview: String = e.preview.chars().take(200).collect();
    let mut line = format!("{} {arrow} {}", clock(e.at_ms), event_label(e));
    if e.merged > 1 {
        line.push_str(&format!(" ×{}", e.merged));
    }
    if !preview.is_empty() {
        line.push(' ');
        line.push_str(&preview.replace('\n', " "));
    }
    if let Some(status) = event_status(e) {
        line.push_str(&format!(" [{status}]"));
    }
    line
}

/// Characters of a body shown in an opened row; Copy takes all of it.
const SHOWN_BODY: usize = 64 * 1024;

/// A message body as indented JSON, cut for display.
fn pretty_body(body: &Body) -> String {
    let text = serde_json::from_str::<serde_json::Value>(&body.text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| body.text.clone());
    cut(&text, SHOWN_BODY)
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… shown up to {} KB; Copy takes all of it.",
        &text[..end],
        max / 1024
    )
}

const COPIED_LOG: &str = "log-view-copy";

/// Width of the time column (`00:00:00.000`, 12 monospace characters, about
/// 87px at the row's text size, with room to spare); opened rows indent their
/// details past it.
const TIME_W: f32 = 96.;
/// Width of the level column (`ERROR`, `DEBUG`).
const LEVEL_W: f32 = 44.;

impl QmtApp {
    /// Open the log of agent `agent` in the full-size viewer.
    pub(super) fn open_agent_logs(
        &mut self,
        agent: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_log_view(LogScope::Agent(agent.to_string()), window, cx);
    }

    /// Open the debug events of `agent`, or of one of its sessions.
    pub(super) fn open_debug_events(
        &mut self,
        agent: &str,
        session: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scope = LogScope::Events {
            agent: agent.to_string(),
            session,
        };
        self.open_log_view(scope, window, cx);
        let Some(wire) = self.wire_log(agent, cx) else {
            return;
        };
        self.log_view._poll = Some(cx.spawn(async move |this, cx| {
            let mut seen = wire.version();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if wire.version() != seen {
                    seen = wire.version();
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }
        }));
    }

    fn open_log_view(&mut self, scope: LogScope, window: &mut Window, cx: &mut Context<Self>) {
        // One modal at a time: the sheet or menu this comes from closes.
        self.details = None;
        self.session_menu_open = false;
        let v = &mut self.log_view;
        v.scope = Some(scope);
        v.expanded.clear();
        v.pretty.clear();
        v.built = None;
        v._poll = None;
        v.query
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        v.query.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(super) fn close_log_view(&mut self) -> bool {
        self.log_view._poll = None;
        self.log_view.pretty.clear();
        self.log_view.scope.take().is_some()
    }

    fn wire_log(&self, agent: &str, cx: &App) -> Option<Rc<WireLog>> {
        Some(self.store.read(cx).runtime(agent)?.wire.clone())
    }

    fn log_entries<'a>(&self, cx: &'a App) -> Option<&'a std::collections::VecDeque<LogEntry>> {
        let agent = self.log_view.scope.as_ref()?.agent();
        Some(self.store.read(cx).runtime(agent)?.log.entries())
    }

    /// Bring the shown rows up to date; returns (rows in the source, title,
    /// subtitle).
    fn sync_log_view(
        &mut self,
        scope: &LogScope,
        query: &str,
        cx: &App,
    ) -> (usize, String, String) {
        let store = self.store.read(cx);
        let name = store
            .config(scope.agent())
            .map_or_else(|| scope.agent().to_string(), |c| c.name.clone());
        let v = &self.log_view;
        let mut built = Built {
            scope: scope.clone(),
            first: 0,
            last: 0,
            version: 0,
            query: query.to_string(),
            source: v.source,
            level: v.level,
            events: v.events,
        };
        match scope {
            LogScope::Agent(agent) => {
                let empty = std::collections::VecDeque::new();
                let log = store.runtime(agent).map(|rt| &rt.log);
                let entries = log.map_or(&empty, |l| l.entries());
                built.first = entries.front().map_or(0, |e| e.seq);
                built.last = log.map_or(0, |l| l.last_seq());
                let (source, level) = (v.source, v.level);
                let matches = |e: &LogEntry| {
                    let from = match source {
                        SourceFilter::All => true,
                        SourceFilter::Agent => e.source == LogSource::Agent,
                        SourceFilter::App => e.source == LogSource::App,
                    };
                    from && level.admits(e.level)
                        && (query.is_empty()
                            || e.text.to_lowercase().contains(query)
                            || e.target
                                .as_ref()
                                .is_some_and(|t| t.to_lowercase().contains(query)))
                };
                let total = entries.len();
                self.log_view.sync(built, |after| {
                    entries
                        .iter()
                        .filter(|e| e.seq > after && matches(e))
                        .map(|e| e.seq)
                        .collect()
                });
                (total, "Agent logs".into(), format!("{name} · times in UTC"))
            }
            LogScope::Events { agent, session } => {
                let Some(wire) = store.runtime(agent).map(|rt| rt.wire.clone()) else {
                    return (0, "Debug events".into(), name);
                };
                let events = wire.events();
                built.first = events.front().map_or(0, |e| e.seq);
                built.last = events.back().map_or(0, |e| e.seq);
                if v.events == EventFilter::Errors {
                    built.version = wire.version();
                }
                let filter = v.events;
                let in_scope =
                    |e: &WireEvent| session.is_none() || e.session.as_ref() == session.as_ref();
                let matches = |e: &WireEvent| {
                    in_scope(e)
                        && filter.admits(e)
                        && (query.is_empty()
                            || [e.method.as_str(), &e.preview, &e.body.text]
                                .iter()
                                .chain(e.update.as_deref().iter())
                                .chain(e.session.as_deref().iter())
                                .chain(e.reply.as_ref().map(|r| r.body.text.as_str()).iter())
                                .any(|t| t.to_lowercase().contains(query)))
                };
                let total = events.iter().filter(|e| in_scope(e)).count();
                self.log_view.sync(built, |after| {
                    events
                        .iter()
                        .filter(|e| e.seq > after && matches(e))
                        .map(|e| e.seq)
                        .collect()
                });
                // Replies and merged chunks change rows in place.
                let version = wire.version();
                if self.log_view.measured != version {
                    self.log_view.measured = version;
                    let n = self.log_view.shown.len();
                    if n > 0 {
                        self.log_view.list.remeasure_items(n - 1..n);
                    }
                    for (ix, seq) in self.log_view.shown.iter().enumerate() {
                        if self.log_view.expanded.contains(seq) {
                            self.log_view.list.remeasure_items(ix..ix + 1);
                        }
                    }
                }
                let subtitle = match session {
                    Some(s) => format!("Session {s} · {name} · times in UTC"),
                    None => format!("{name} · all sessions · times in UTC"),
                };
                (total, "Debug events".into(), subtitle)
            }
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
        let events = matches!(scope, LogScope::Events { .. });
        let query = self.log_view.query.read(cx).value().trim().to_lowercase();
        let (total, title, subtitle) = self.sync_log_view(&scope, &query, cx);
        let shown = self.log_view.shown.len();
        let filtered = self.log_view.filtered(&query);
        let following = self.log_view.list.is_following_tail();
        let wrap = self.log_view.wrap;
        let copied = self.copied.as_deref() == Some(COPIED_LOG);
        let noun = if events { "events" } else { "lines" };

        // ---- header: what this is, and the actions on all of it
        let action =
            |id: &'static str, icon: IconName, tip: &'static str| kit::icon_button(id, icon, tip);
        let agent = scope.agent().to_string();
        let header = h_flex()
            .gap_3()
            .px_5()
            .pt_4()
            .pb_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_lg().font_semibold().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .truncate()
                            .child(subtitle),
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
                        action(
                            "log-follow",
                            IconName::ArrowDownToLine,
                            if events {
                                "Follow new events"
                            } else {
                                "Follow new lines"
                            },
                        )
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
                            if events {
                                "Copy shown events"
                            } else {
                                "Copy shown lines"
                            },
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
                        action(
                            "log-clear",
                            IconName::Eraser,
                            if events {
                                "Clear the agent's events"
                            } else {
                                "Clear log"
                            },
                        )
                        .disabled(total == 0)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let agent = agent.clone();
                            this.log_view.expanded.clear();
                            this.log_view.pretty.clear();
                            this.store.update(cx, |s, cx| {
                                if events {
                                    s.clear_wire_log(&agent, cx)
                                } else {
                                    s.clear_agent_log(&agent, cx)
                                }
                            });
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
                    .aria_label(if events {
                        "Search events"
                    } else {
                        "Search logs"
                    })
                    .flex_1(),
            );
        let filters: Vec<Div> = if events {
            let options = [
                (EventFilter::All, "All", "ev-all"),
                (EventFilter::Calls, "Requests", "ev-calls"),
                (EventFilter::Notifications, "Notifications", "ev-notes"),
                (EventFilter::Errors, "Errors", "ev-errors"),
            ];
            vec![kit::segment_bar(&p).children(options.map(|(f, label, id)| {
                kit::segment(id, self.log_view.events == f, &p)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.log_view.events = f;
                        cx.notify();
                    }))
            }))]
        } else {
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
            vec![
                kit::segment_bar(&p).children(sources.map(|(f, label, id)| {
                    kit::segment(id, self.log_view.source == f, &p)
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.log_view.source = f;
                            cx.notify();
                        }))
                })),
                kit::segment_bar(&p).children(levels.map(|(f, label, id)| {
                    kit::segment(id, self.log_view.level == f, &p)
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.log_view.level = f;
                            cx.notify();
                        }))
                })),
            ]
        };
        let count = if filtered {
            format!(
                "{} of {}",
                kit::short_num(shown as u64),
                kit::short_num(total as u64)
            )
        } else {
            format!("{} {noun}", kit::short_num(total as u64))
        };
        let toolbar = h_flex()
            .gap_2()
            .px_5()
            .pb_3()
            .child(search)
            .children(filters)
            .child(
                div()
                    .flex_none()
                    .min_w(px(72.))
                    .text_right()
                    .text_xs()
                    .text_color(p.dim)
                    .child(count),
            );

        // ---- body: the rows, or why there are none
        let body = if shown == 0 {
            let (icon, title, text) = match (events, total) {
                (false, 0) => (
                    IconName::ScrollText,
                    "No log lines yet",
                    "Lines appear as the agent starts and runs.",
                ),
                (true, 0) => (
                    IconName::Bug,
                    "No events yet",
                    "Messages between this app and the agent appear here. They are recorded \
                     while Developer mode is on.",
                ),
                (false, _) => (
                    IconName::ScrollText,
                    "No lines match",
                    "Change the search or the filters.",
                ),
                (true, _) => (
                    IconName::Bug,
                    "No events match",
                    "Change the search or the filter.",
                ),
            };
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .px_8()
                .child(kit::icon(icon, 22., p.dim))
                .child(div().mt_1().font_medium().child(title))
                .child(
                    div()
                        .max_w(px(420.))
                        .text_center()
                        .text_sm()
                        .text_color(p.muted)
                        .child(text),
                )
                .when(total > 0, |el| {
                    el.child(
                        kit::flat_btn("log-reset", true, true, &p)
                            .mt_2()
                            .child("Clear filters")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.log_view.source = SourceFilter::All;
                                this.log_view.level = LevelFilter::All;
                                this.log_view.events = EventFilter::All;
                                this.log_view.query.update(cx, |s, cx| {
                                    s.set_value(SharedString::default(), window, cx)
                                });
                                cx.notify();
                            })),
                    )
                })
                .into_any_element()
        } else {
            let render = cx.processor(move |this: &mut Self, ix: usize, _, cx| {
                if events {
                    this.render_event_row(ix, cx)
                } else {
                    this.render_log_row(ix, cx)
                }
            });
            list(self.log_view.list.clone(), move |ix, window, cx| {
                render(ix, window, cx)
            })
            .size_full()
            .py_1()
            .into_any_element()
        };
        let well = div()
            .debug_selector(|| "log-well".into())
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

    /// The shown rows as plain text.
    fn log_view_text(&self, cx: &App) -> String {
        let lines: Vec<String> = match self.log_view.scope.as_ref() {
            Some(LogScope::Agent(_)) => {
                let Some(entries) = self.log_entries(cx) else {
                    return String::new();
                };
                let first = entries.front().map_or(0, |e| e.seq);
                self.log_view
                    .shown
                    .iter()
                    .filter_map(|seq| entries.get(seq.checked_sub(first)? as usize))
                    .map(plain)
                    .collect()
            }
            Some(LogScope::Events { agent, .. }) => {
                let Some(wire) = self.wire_log(agent, cx) else {
                    return String::new();
                };
                let events = wire.events();
                let first = events.front().map_or(0, |e| e.seq);
                self.log_view
                    .shown
                    .iter()
                    .filter_map(|seq| events.get(seq.checked_sub(first)? as usize))
                    .map(plain_event)
                    .collect()
            }
            None => Vec::new(),
        };
        lines.join("\n")
    }

    /// Open or close row `ix` (`seq`).
    fn toggle_log_row(&mut self, ix: usize, seq: u64, cx: &mut Context<Self>) {
        if !self.log_view.expanded.remove(&seq) {
            self.log_view.expanded.insert(seq);
        }
        self.log_view.list.remeasure_items(ix..ix + 1);
        cx.notify();
    }

    fn render_log_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = *palette(cx);
        let Some(&seq) = self.log_view.shown.get(ix) else {
            return div().into_any_element();
        };
        let entry = self.log_entries(cx).and_then(|entries| {
            let first = entries.front()?.seq;
            entries.get(seq.checked_sub(first)? as usize).cloned()
        });
        let Some(e) = entry else {
            return div().into_any_element();
        };
        let open = self.log_view.expanded.contains(&seq);
        let wrap = self.log_view.wrap || open;
        let tint = match e.level {
            Level::Error => Some(p.danger.opacity(0.07)),
            Level::Warn => Some(p.warn.opacity(0.06)),
            _ => None,
        };
        let text_color = match e.level {
            Level::Debug | Level::Trace => p.muted,
            _ => p.text,
        };
        let row = row_shell(("log-row", seq), open, tint, &p)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_log_row(ix, seq, cx)))
            .child(time_cell(e.at_ms, &p))
            .child(
                div()
                    .flex_none()
                    .w(px(LEVEL_W))
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
                .ml(px(TIME_W + LEVEL_W + 24.))
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
        list_item(ix, row, detail)
    }

    fn render_event_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = *palette(cx);
        let Some(&seq) = self.log_view.shown.get(ix) else {
            return div().into_any_element();
        };
        let Some(LogScope::Events { agent, session }) = self.log_view.scope.clone() else {
            return div().into_any_element();
        };
        let Some(wire) = self.wire_log(&agent, cx) else {
            return div().into_any_element();
        };
        let events = wire.events();
        let Some(e) = events
            .front()
            .and_then(|f| events.get(seq.checked_sub(f.seq)? as usize))
        else {
            return div().into_any_element();
        };
        let open = self.log_view.expanded.contains(&seq);
        let wrap = self.log_view.wrap || open;
        let failed = e.failed();
        let out = e.kind.outgoing();
        let label = event_label(e).to_string();
        // Rows show a line; an opened row shows the merged text below.
        let preview: String = e.preview.chars().take(2000).collect();
        let status = event_status(e);
        let row = row_shell(
            ("event-row", seq),
            open,
            failed.then(|| p.danger.opacity(0.07)),
            &p,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_log_row(ix, seq, cx)))
        .child(time_cell(e.at_ms, &p))
        .child(
            div()
                .id(("event-dir", seq))
                .flex_none()
                .w(px(14.))
                .pt(px(1.))
                .child(kit::icon(
                    if out {
                        IconName::ArrowUpRight
                    } else {
                        IconName::ArrowDownLeft
                    },
                    12.,
                    if out { p.dim } else { p.accent },
                ))
                .tooltip(move |window, cx| {
                    kit::wrap_tip(if out {
                        "Sent by this app"
                    } else {
                        "Sent by the agent"
                    })
                    .build(window, cx)
                }),
        )
        .child(
            div()
                .flex_none()
                .w(px(200.))
                .truncate()
                .font_medium()
                .text_color(p.text)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(p.muted)
                .when(!wrap, |el| el.truncate())
                .when(open && e.merged > 1, |el| el.truncate())
                .child(preview),
        )
        .when(e.merged > 1, |el| {
            el.child(kit::chip(format!("×{}", e.merged), &p))
        })
        .when_some(e.session.clone().filter(|_| session.is_none()), |el, s| {
            el.child(kit::chip(s.chars().take(8).collect::<String>(), &p))
        })
        .child(
            div()
                .flex_none()
                .w(px(64.))
                .whitespace_nowrap()
                .text_right()
                .text_color(if failed { p.danger } else { p.dim })
                .children(status),
        );

        let detail = open.then(|| {
            let (seq, merged) = (e.seq, e.merged);
            let first_label = match (e.kind, merged > 1) {
                (_, true) => "First chunk",
                (k, _) if k.outgoing() => "Sent",
                _ => "Received",
            };
            let body_note = |b: &Body| {
                b.cut().then(|| {
                    format!(
                        "kept {} KB of {} KB",
                        b.text.len() / 1024,
                        b.full_len.div_ceil(1024)
                    )
                })
            };
            let request = (
                first_label.to_string(),
                body_note(&e.body),
                e.body.text.clone(),
            );
            let reply = e.reply.as_ref().map(|r| {
                let title = match &r.error {
                    Some(err) => format!("Error · {err}"),
                    None => format!("Response · {}", duration(e.duration_ms().unwrap_or(0.))),
                };
                (
                    title,
                    body_note(&r.body),
                    r.body.text.clone(),
                    r.error.is_some(),
                )
            });
            let joined = (merged > 1).then(|| e.preview.clone());
            let bodies = (e.body.clone(), e.reply.as_ref().map(|r| r.body.clone()));
            (seq, request, reply, joined, bodies)
        });
        drop(events);

        let detail = detail.map(|(seq, request, reply, joined, (body, reply_body))| {
            let request_text = self.log_view.pretty(seq, 0, || pretty_body(&body));
            let reply_text = reply_body
                .as_ref()
                .map(|b| self.log_view.pretty(seq, 1, || pretty_body(b)));
            let joined_text = joined.map(|j| self.log_view.pretty(seq, 2, || cut(&j, SHOWN_BODY)));
            v_flex()
                .ml(px(TIME_W + 14. + 24.))
                .mr_3()
                .mt_1()
                .mb_2()
                .gap_2()
                .when_some(joined_text, |el, text| {
                    el.child(body_block(
                        ("ev-joined", seq),
                        "Merged text".into(),
                        None,
                        text,
                        None,
                        false,
                        &p,
                    ))
                })
                .child(body_block(
                    ("ev-body", seq),
                    request.0,
                    request.1,
                    request_text,
                    Some(request.2),
                    false,
                    &p,
                ))
                .when_some(
                    reply.zip(reply_text),
                    |el, ((title, note, raw, error), text)| {
                        el.child(body_block(
                            ("ev-reply", seq),
                            title,
                            note,
                            text,
                            Some(raw),
                            error,
                            &p,
                        ))
                    },
                )
        });
        list_item(ix, row, detail)
    }
}

/// One item of the virtual list. The list only offers its width as
/// available space, so the item claims it: without a width, a row sizes to
/// its text, long lines run past the edge and short ones pull the right-hand
/// columns in.
fn list_item(ix: usize, row: impl IntoElement, detail: Option<impl IntoElement>) -> AnyElement {
    v_flex()
        .w_full()
        .px_1()
        .debug_selector(move || format!("log-item-{ix}"))
        .child(row)
        .children(detail)
        .into_any_element()
}

/// Frame of one row: hover, the opened fill, a state tint.
fn row_shell(
    id: impl Into<ElementId>,
    open: bool,
    tint: Option<Hsla>,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.card_hover;
    h_flex()
        .id(id)
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
}

fn time_cell(at_ms: f64, p: &Palette) -> Div {
    // Never wraps: a time on two lines doubles the row.
    div()
        .flex_none()
        .w(px(TIME_W))
        .whitespace_nowrap()
        .text_color(p.dim)
        .child(clock(at_ms))
}

/// One message of an opened event: a title, the JSON, and a copy button
/// that takes the message as kept.
fn body_block(
    id: (&'static str, u64),
    title: String,
    note: Option<String>,
    text: SharedString,
    raw: Option<String>,
    error: bool,
    p: &Palette,
) -> Div {
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .font_medium()
                        .text_color(if error { p.danger } else { p.muted })
                        .child(title),
                )
                .children(note.map(|n| div().text_color(p.dim).child(n)))
                .child(div().flex_1())
                .children(raw.map(|raw| kit::copy_button(id, raw, "Copy message"))),
        )
        .child(
            div()
                .p_2()
                .rounded(px(6.))
                .bg(p.card)
                .font_family(kit::MONO)
                .text_xs()
                .text_color(p.text)
                .child(text),
        )
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
    use super::{Duration, EventFilter, LevelFilter, LogScope, SourceFilter};
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

    #[gpui_kit::test]
    fn debug_events_follow_one_session(cx: &mut gpui_kit::TestAppContext) {
        use serde_json::json;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        store.prefs.developer_mode = true;
        let (rpc, _requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.wire.set_enabled(true);
        rpc.set_wire_log(rt.wire.clone());
        store.session = Some(crate::session::SessionView::new(
            "test-agent",
            "s1",
            "/w",
            "S",
        ));
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
                app.go(Page::Session, cx);
                app.session_menu_open = true;
            })
        });
        render(cx);
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let item = vcx.debug_bounds("m-debug-events").expect("Debug events…");
        vcx.simulate_click(item.center(), Modifiers::none());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.log_view.scope,
                Some(LogScope::Events {
                    agent: "test-agent".into(),
                    session: Some("s1".into())
                })
            );
            assert!(!app.session_menu_open);
        });

        // Traffic: a prompt that fails, a streamed reply, another session.
        let _prompt = rpc.request("session/prompt", json!({ "sessionId": "s1", "prompt": [] }));
        let chunk = |s: &str, t: &str| {
            json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": s,
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": t } } } })
            .to_string()
        };
        for (s, t) in [("s1", "Hel"), ("s1", "lo"), ("s2", "other")] {
            rpc.handle(&chunk(s, t));
        }
        rpc.handle(
            &json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -1, "message": "boom" } })
                .to_string(),
        );
        // The viewer polls the recorder.
        cx.executor().advance_clock(Duration::from_millis(250));
        render(cx);
        let shown = |cx: &mut gpui_kit::TestAppContext| {
            app.read_with(cx, |app, _| app.log_view.shown.len())
        };
        assert_eq!(shown(cx), 2, "the prompt and one merged reply");

        // Copy reads as a list of what happened.
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let copy = vcx.debug_bounds("log-copy").expect("Copy");
        vcx.simulate_click(copy.center(), Modifiers::none());
        let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
        let lines: Vec<&str> = copied.lines().collect();
        assert!(lines[0].contains("→ session/prompt") && lines[0].ends_with("[failed]"));
        assert!(lines[1].contains("← agent_message_chunk ×2 Hello"));

        // Errors only; an opened row renders its messages.
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                app.log_view.events = EventFilter::Errors;
                cx.notify();
            })
        });
        render(cx);
        assert_eq!(shown(cx), 1);
        cx.update(|cx| {
            app.update(cx, |app, cx| {
                let seq = app.log_view.shown[0];
                app.toggle_log_row(0, seq, cx);
            })
        });
        render(cx);
        app.read_with(cx, |app, _| {
            assert!(app.log_view.pretty.values().any(|t| t.contains("\"boom\"")));
        });

        // The whole agent: every session.
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.close_log_view();
                app.log_view.events = EventFilter::All;
                app.open_debug_events("test-agent", None, window, cx);
            })
        })
        .unwrap();
        render(cx);
        assert_eq!(shown(cx), 3);
    }

    #[gpui_kit::test]
    fn debug_events_need_developer_mode(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        store.session = Some(crate::session::SessionView::new(
            "test-agent",
            "s1",
            "/w",
            "S",
        ));
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(Page::Session, cx);
                app.session_menu_open = true;
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("m-debug-events").is_none());
        let store = app.read_with(cx, |app, _| app.store.clone());
        assert!(store.read_with(cx, |s, _| !s.runtime("test-agent").unwrap().wire.enabled()));
        cx.update(|cx| store.update(cx, |s, cx| s.set_developer_mode(true, cx)));
        assert!(store.read_with(cx, |s, _| s.runtime("test-agent").unwrap().wire.enabled()));
    }

    #[gpui_kit::test]
    fn rows_fit_the_viewer(cx: &mut gpui_kit::TestAppContext) {
        use serde_json::json;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        store.prefs.developer_mode = true;
        let (rpc, _requests) = store.rpc_for_test("test-agent");
        let rt = store.runtime_mut_for_test("test-agent");
        rt.wire.set_enabled(true);
        rpc.set_wire_log(rt.wire.clone());
        // A long line, then a short one.
        let long = "x".repeat(4000);
        let _a = rpc.request("session/list", json!({ "cursor": long }));
        let _b = rpc.request("_querymt/capabilities", json!({}));
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.open_debug_events("test-agent", None, window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
        let mut vcx = VisualTestContext::from_window(handle, cx);
        let well = vcx.debug_bounds("log-well").expect("well");
        for (ix, selector) in ["log-item-0", "log-item-1"].into_iter().enumerate() {
            let item = vcx.debug_bounds(selector).expect("row");
            assert!(
                item.right() <= well.right(),
                "row {ix} runs past the viewer"
            );
            assert!(
                item.size.width > well.size.width * 0.9,
                "row {ix} is content-sized"
            );
        }
    }
}
