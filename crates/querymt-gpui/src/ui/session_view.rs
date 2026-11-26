//! Session page, modelled on the desktop app's chat view: header with status
//! and context ring, a virtualized transcript (user bubbles, markdown replies,
//! collapsible work groups of reasoning + tool calls with diffs / terminal
//! output), an activity bar, a scroll-to-latest pill, pending requests, the
//! plan, and a composer that mirrors the desktop one (+ / model / options,
//! stop, steer / queue, attachments).

use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use super::{find, md};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Enter, IndentInline, OutdentInline, Textarea};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarAxis, ScrollbarMode};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Selectable as _, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::connectors::oauth_flow_box;
use super::kit;
use super::kit::ButtonExt as _;
use super::kit::badged_tone;
use super::kit::{accent_btn, flat_btn};
use super::session_pane::{SessionPane, SessionPaneEvent};
use super::start::{mode_tone, toolbar_button};
use crate::session::open::{Delivery, SessionKey};
use crate::session::{
    COMPACTING_LABEL, Compaction, CompactionStatus, Diff, Item, ItemKind, SelectOption,
    SessionView, Thought, ToolCall, UserAttachment, WorkEntry, is_compact_command,
};
use crate::store::Model;
use crate::store::worktree::WorktreeAction;
use crate::theme::Palette;
use crate::usage::{self, TokenKey};
use crate::util::{human_size, workspace_name};

const OUTPUT_LINES: usize = 80;
/// Floating composer: height of the fade above it, and its background-colored halo.
const COMPOSER_FADE_H: f32 = 28.;
/// Height of the slot the activity bar, waiting bubble and scroll pill float
/// in above the composer (their pill height).
const DOCK_ROW_H: f32 = 32.;
const COMPOSER_HALO_BLUR: f32 = 10.;
const COMPOSER_HALO_SPREAD: f32 = 2.;
/// Stacked copies of the halo: more layers = denser edge.
const COMPOSER_HALO_LAYERS: usize = 5;
/// How much the page background veils text that scrolled under the floating
/// area (beside and below the composer): 1.0 hides it, 0.0 shows it fully.
const COMPOSER_VEIL: f32 = 0.72;
/// Fade along the transcript's top edge, under the title bar: as tall as the
/// composer halo reaches (spread + blur) and stacked the same number of times,
/// so both edges dissolve text with the same size and density.
const TRANSCRIPT_TOP_FADE_H: f32 = COMPOSER_HALO_SPREAD + COMPOSER_HALO_BLUR;
/// Attached tool cards scroll inside fixed-height blocks, like the desktop:
/// read and terminal output `max-height: 18rem`, diffs 40 lines tall.
pub(super) const TOOL_LINE_H: f32 = 18.;
const READ_BLOCK_MAX_H: f32 = 288.;
const TERMINAL_BLOCK_MAX_H: f32 = 288.;
const DIFF_BLOCK_MAX_H: f32 = TOOL_LINE_H * 40.;
const DIFF_LINES: usize = 160;
/// Code lines shown for a read result before "N more lines".
const READ_LINES: usize = 200;
/// `QmtApp::copied` keys of the title bar / session menu copy targets.
const COPIED_TITLE_PATH: &str = "@title-path";
const COPIED_TITLE_ID: &str = "@title-id";
const COPIED_MENU_PATH: &str = "@menu-path";
const COPIED_MENU_ID: &str = "@menu-id";
const COPIED_MENU_CHECKOUT: &str = "@menu-checkout";
const COPIED_DETAILS_CHECKOUT: &str = "@details-checkout";
/// Copied flash key and expanded-state key of the load-error report.
const COPIED_ERROR_DETAILS: &str = "@load-error-details";
const ERROR_DETAILS_KEY: &str = "@load-error-details-open";

/// Desktop `.copy-chip-copied`: an accent check + "copied" laid over the chip,
/// at least as wide as it (so it hides the label), growing if the label is shorter.
fn copied_badge(p: &Palette) -> Div {
    h_flex()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .min_w_full()
        .px_1()
        .gap_1()
        .justify_center()
        .rounded(px(4.))
        .bg(p.card)
        .text_color(p.text)
        .whitespace_nowrap()
        .child(kit::icon(IconName::Check, 11., p.accent))
        .child("copied")
}

/// Tooltip of a click-to-copy target: the value muted (monospace for IDs)
/// above the hint; just "Copied" while the copy feedback shows (`value: None`).
fn copy_tooltip(value: Option<String>, hint: &'static str, dim: Hsla, mono: bool) -> Tooltip {
    Tooltip::element(move |_, _| match &value {
        Some(v) => v_flex()
            .max_w(px(kit::TOOLTIP_MAX_W))
            .gap_0p5()
            .child(
                div()
                    .text_color(dim)
                    .when(mono, |el| el.font_family(kit::MONO).text_xs())
                    .child(v.clone()),
            )
            .child(div().child(hint))
            .into_any_element(),
        None => div().child("Copied").into_any_element(),
    })
}

/// Desktop `--session-chat-max` (52rem).
pub(super) const CHAT_MAX: f32 = 832.;
/// Transcript column of a compact pane (the quick panel is 680 px wide).
const COMPACT_CHAT_MAX: f32 = 640.;
/// Size of the conversation's text in the HUD.
const COMPACT_PROSE_PX: f32 = 14.5;

/// How much a session pane shows. The row renderers are the same in both;
/// compact changes defaults and limits only (design D6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Density {
    /// The main window.
    #[default]
    Comfortable,
    /// The quick panel: collapsed rows, small caps, hover actions, a one-line
    /// composer, no usage ring, details or options panel.
    // Built by the quick panel (tasks 6.x) and by tests until then.
    #[cfg_attr(not(test), allow(dead_code))]
    Compact,
}

impl Density {
    pub(crate) fn is_compact(self) -> bool {
        self == Self::Compact
    }

    /// Lines of tool output and parameters shown before "N more lines".
    fn output_lines(self) -> usize {
        match self {
            Self::Comfortable => OUTPUT_LINES,
            Self::Compact => 20,
        }
    }

    /// Code lines of a read result shown before "N more lines".
    fn read_lines(self) -> usize {
        match self {
            Self::Comfortable => READ_LINES,
            Self::Compact => 40,
        }
    }

    /// Lines of each diff hunk shown before "N more lines".
    fn diff_lines(self) -> usize {
        match self {
            Self::Comfortable => DIFF_LINES,
            Self::Compact => 40,
        }
    }

    /// Height of the scrolling block of read results.
    fn read_block_h(self) -> f32 {
        match self {
            Self::Comfortable => READ_BLOCK_MAX_H,
            Self::Compact => TOOL_LINE_H * 10.,
        }
    }

    /// Height of the scrolling block of terminal output.
    fn terminal_block_h(self) -> f32 {
        match self {
            Self::Comfortable => TERMINAL_BLOCK_MAX_H,
            Self::Compact => TOOL_LINE_H * 10.,
        }
    }

    /// Height of the scrolling block of a diff.
    fn diff_block_h(self) -> f32 {
        match self {
            Self::Comfortable => DIFF_BLOCK_MAX_H,
            Self::Compact => TOOL_LINE_H * 16.,
        }
    }

    /// Widest the transcript column and the composer grow.
    pub(super) fn column_max(self) -> f32 {
        match self {
            Self::Comfortable => CHAT_MAX,
            Self::Compact => COMPACT_CHAT_MAX,
        }
    }

    /// Horizontal padding of a transcript row, in px.
    fn row_pad(self) -> f32 {
        match self {
            Self::Comfortable => 24.,
            Self::Compact => 12.,
        }
    }

    /// A running work group with more than `.0` blocks shows only its last
    /// `.1` blocks behind a "+N previous" toggle.
    fn live_tail(self) -> (usize, usize) {
        match self {
            Self::Comfortable => (3, 2),
            Self::Compact => (2, 1),
        }
    }

    /// Copy, undo, redo and fork show while the pointer is over the turn.
    fn hover_actions(self) -> bool {
        self.is_compact()
    }

    /// The context ring, the details panel and the options panel exist.
    pub(super) fn has_panels(self) -> bool {
        !self.is_compact()
    }

    /// Lines the composer grows between.
    pub(super) fn composer_lines(self) -> (usize, usize) {
        match self {
            Self::Comfortable => (2, 12),
            // One line at rest: the placeholder, the attach button, the model
            // chip and the send button then share one centre line.
            Self::Compact => (1, 6),
        }
    }
}
/// Hover group of a tool row (status ↔ chevron swap).
const TOOL_ROW_GROUP: &str = "tool-row";

/// Virtualized transcript: a gpui `ListState` plus per-item snapshots, re-measured
/// only where an item's signature (revision, expansion, turn state) changed.
pub struct Transcript {
    pub list: ListState,
    items: Rc<RefCell<Vec<Rc<Row>>>>,
    sigs: Vec<u64>,
    /// Item keys, to recognise older history prepended above the current first item.
    keys: Vec<String>,
    session: String,
    scroll_hooked: bool,
    /// Scroll positions of the fixed-height tool blocks, by block key. They
    /// outlive re-renders (the list re-creates items) so the scrollbars can
    /// read the real overflow.
    block_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Measured height of the floating composer area, so the list can pad
    /// its end and the last message can scroll clear of the composer.
    composer_h: Rc<Cell<f32>>,
    /// Previous frame's telemetry sample (only used while export is on).
    probe: Rc<RefCell<ScrollProbe>>,
    /// Parsed Markdown of the rows, kept while they are off screen.
    pub(super) md: md::Kept,
}

/// Last transcript frame seen by the telemetry sampler.
#[derive(Default)]
struct ScrollProbe {
    /// First visible row and its top edge on screen.
    anchor: Option<(usize, f32)>,
    following: bool,
    item_count: usize,
    layout_seq: u64,
    /// Session shown, for slow-frame reports.
    session: String,
    /// This frame so far: when its first row started building, rows built,
    /// and the slowest row (key, kind, work entries, ms).
    frame_started: Option<f64>,
    rows_built: usize,
    slowest: Option<(String, &'static str, usize, f64)>,
}

/// A transcript frame slower than this is reported (two 60 Hz frames).
const SLOW_FRAME_MS: f64 = 33.;

impl ScrollProbe {
    /// Row `row` was built, starting at `started` (ms).
    fn row_built(&mut self, row: &Row, started: f64) {
        let ms = crate::util::now_ms() - started;
        let (kind, entries) = match (&row.part, &row.item.kind) {
            (Part::Head, ItemKind::Work(e)) => ("work_head", e.len()),
            (Part::Block { range, .. }, _) => ("work_block", range.len()),
            (_, ItemKind::Work(e)) => ("work", e.len()),
            (_, ItemKind::User { .. }) => ("user", 0),
            (_, ItemKind::Agent { .. }) => ("reply", 0),
            (_, ItemKind::Notice { .. }) => ("notice", 0),
            (_, ItemKind::Compaction(_)) => ("compaction", 0),
        };
        crate::telemetry::record_row(ms, kind, entries);
        self.frame_started.get_or_insert(started);
        self.rows_built += 1;
        if self.slowest.as_ref().is_none_or(|s| ms > s.3) {
            self.slowest = Some((row.key.clone(), kind, entries, ms));
        }
    }
}

/// Frame movement beyond which content moving with the input is still a jump.
const JUMP_SLACK_PX: f32 = 40.;

/// After the transcript list's layout: record where the content is, and flag
/// frames whose visible movement disagrees with the injected scroll input.
fn sample_transcript(
    list: &ListState,
    probe: &RefCell<ScrollProbe>,
    smooth: &super::smooth_scroll::SmoothScroll,
    composer_h: f32,
) {
    let (input, gesture) = smooth.take_frame_input();
    let top = list.logical_scroll_top();
    let count = list.item_count();
    let following = list.is_following_tail();
    let offset = -f32::from(list.scroll_px_offset_for_scrollbar().y);
    let max_offset = f32::from(list.max_offset_for_scrollbar().y);
    let viewport = list.viewport_bounds();
    let anchor_top = list
        .bounds_for_item(top.item_ix)
        .map(|b| f32::from(b.top()));
    let mut p = probe.borrow_mut();
    p.layout_seq += 1;
    let layout_seq = p.layout_seq;
    // Rows built this frame: from the first one to after the list's layout
    // and prepaint (this sampler prepaints after the list).
    if let Some(started) = p.frame_started.take() {
        let frame_ms = crate::util::now_ms() - started;
        let rows = std::mem::take(&mut p.rows_built);
        let slowest = p.slowest.take();
        crate::telemetry::record_frame(frame_ms, rows);
        if frame_ms > SLOW_FRAME_MS {
            let (row_key, row_kind, row_entries, row_ms) = slowest.unwrap_or_default();
            tracing::warn!(
                session.id = p.session.as_str(),
                frame.ms = frame_ms,
                frame.rows = rows,
                row.key = row_key.as_str(),
                row.kind = row_kind,
                row.entries = row_entries,
                row.ms = row_ms,
                list.scroll_top_ix = list.logical_scroll_top().item_ix,
                list.item_count = list.item_count(),
                "slow transcript frame"
            );
        }
    }
    // Content movement of the row that was first on screen last frame, when
    // it is still laid out and no rows were inserted above it.
    let shift = p
        .anchor
        .filter(|_| p.item_count == count)
        .and_then(|(ix, was)| list.bounds_for_item(ix).map(|b| f32::from(b.top()) - was));
    let span = gesture.unwrap_or_else(tracing::Span::current);
    let engaged = following && !p.following;
    if engaged {
        crate::telemetry::record_follow_engaged();
    }
    if let Some(shift) = shift {
        crate::telemetry::record_shift(f64::from(shift));
        let against = input != 0. && shift * input < 0. && shift.abs() > 2.;
        // Content moving further than the input asked (a stalled list at an
        // edge is not a jump).
        let past = shift.abs() - input.abs() > JUMP_SLACK_PX;
        if against || past {
            let kind = if against {
                "against_input"
            } else {
                "past_input"
            };
            crate::telemetry::record_jump(kind);
            tracing::warn!(
                parent: &span,
                layout_seq,
                jump.kind = kind,
                jump.shift_px = shift,
                scroll.input_px = input,
                list.offset_px = offset,
                list.max_offset_px = max_offset,
                list.scroll_top_ix = top.item_ix,
                list.scroll_top_offset_px = f32::from(top.offset_in_item),
                list.follow_tail = following,
                list.follow_tail_engaged = engaged,
                list.item_count = count,
                list.viewport_h = f32::from(viewport.size.height),
                list.content_h = max_offset + f32::from(viewport.size.height),
                composer_h,
                "transcript scroll jump"
            );
        }
    }
    if input != 0. || shift.is_some_and(|s| s != 0.) || engaged || following != p.following {
        tracing::debug!(
            parent: &span,
            scroll.input_px = input,
            layout_seq,
            frame.shift_px = shift,
            list.content_h = max_offset + f32::from(viewport.size.height),
            list.offset_px = offset,
            list.max_offset_px = max_offset,
            list.scroll_top_ix = top.item_ix,
            list.scroll_top_offset_px = f32::from(top.offset_in_item),
            list.follow_tail = following,
            list.follow_tail_engaged = engaged,
            list.scrolled_to_end = list.is_scrolled_to_end().unwrap_or(false),
            list.item_count = count,
            list.viewport_h = f32::from(viewport.size.height),
            composer_h,
            "transcript frame"
        );
    }
    p.anchor = anchor_top.map(|t| (top.item_ix, t));
    p.following = following;
    p.item_count = count;
}

/// Per-render facts the item renderer needs besides the item itself.
struct RenderCtx {
    p: Palette,
    /// Syntax colors of the active component theme (dark/light).
    syntax: std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>,
    weak: WeakEntity<SessionPane>,
    density: Density,
    smooth_scroll: super::smooth_scroll::SmoothScroll,
    expanded: HashSet<String>,
    running: bool,
    /// Index of the first item of the current (possibly running) turn.
    last_turn_start: usize,
    /// Index of the transcript's last item: only it can still be streaming.
    last_ix: usize,
    /// Items that close a turn → (added, removed, files) of that turn's edits.
    turn_end: HashMap<usize, (usize, usize, usize)>,
    copied: Option<String>,
    block_scrolls: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Workspace of the session: paths inside it are shown relative.
    cwd: String,
    /// Items that close a turn → message id of that turn's user prompt.
    turn_user: HashMap<usize, String>,
    /// Workspace undo/redo can be offered now (supported, idle, nothing pending).
    undo_ready: bool,
    /// User turns a workspace undo can go back to.
    undoable: HashSet<String>,
    /// User turn that redo restores (top of the undo stack).
    redo_target: Option<String>,
    /// Turn-closing items whose turn can be forked now (supported, idle,
    /// not rolled back, with an agent-identified fork point).
    fork_points: HashSet<usize>,
    /// Turn-closing item of the turn whose diff panel is open.
    diff_open: Option<usize>,
    /// Items from this index on were rolled back by an undo (shown dimmed).
    reverted_from: Option<usize>,
    /// Agent of the open session; delegated child sessions live on it too.
    agent_id: String,
    /// Appearance setting: fade streamed reply and reasoning text in.
    stream_fade: bool,
    md: md::Kept,
    /// The agent returns complete tool content a load shortened.
    tool_content: bool,
    /// Developer mode: message metadata, `exit 0` of shell calls.
    developer: bool,
}

impl RenderCtx {
    /// Item `ix` belongs to the turn still running.
    fn live_turn(&self, ix: usize) -> bool {
        self.running && ix >= self.last_turn_start
    }

    fn block_scroll(&self, key: &str) -> ScrollHandle {
        self.block_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    fn sig(&self, ix: usize, item: &Item) -> u64 {
        let mut h = DefaultHasher::new();
        item.rev.hash(&mut h);
        match &item.kind {
            ItemKind::Work(entries) => {
                self.expanded.contains(&item.key).hash(&mut h);
                // Only the visible blocks' expansion shows (a running group
                // can hold thousands of hidden entries).
                let plan = WorkPlan::of(ix, item, entries, self);
                for block in plan.visible() {
                    self.hash_entries(&entries[block.clone()], &mut h);
                }
                (self.running && ix >= self.last_turn_start).hash(&mut h);
                (self.running && ix == self.last_ix).hash(&mut h);
            }
            ItemKind::User { .. } => {
                self.expanded.contains(&item.key).hash(&mut h);
                self.expanded
                    .contains(&front_matter_key(&item.key))
                    .hash(&mut h);
            }
            _ => self.expanded.contains(&item.key).hash(&mut h),
        }
        self.hash_turn(ix, item, &mut h);
        h.finish()
    }

    /// Expansion state of work entries.
    fn hash_entries(&self, entries: &[WorkEntry], h: &mut DefaultHasher) {
        for e in entries {
            self.expanded.contains(e.id()).hash(h);
            if let WorkEntry::Tool(call) = e {
                // Details, "Show N more lines" of the output and of each
                // diff file: every key naming the call (the set is small).
                let mut keys: Vec<&String> = self
                    .expanded
                    .iter()
                    .filter(|k| k.contains(call.id.as_str()))
                    .collect();
                keys.sort();
                keys.hash(h);
            }
        }
    }

    /// What the closing row of a turn shows besides its content: response
    /// actions, undo/redo, dimming after an undo, the copied flash, theme.
    fn hash_turn(&self, ix: usize, item: &Item, h: &mut DefaultHasher) {
        self.turn_end.get(&ix).hash(h);
        self.live_turn(ix).hash(h);
        if let Some(user) = self.turn_user.get(&ix) {
            self.undo_ready.hash(h);
            self.undoable.contains(user).hash(h);
            (self.redo_target.as_ref() == Some(user)).hash(h);
        }
        self.fork_points.contains(&ix).hash(h);
        (self.diff_open == Some(ix)).hash(h);
        self.reverted_from.is_some_and(|r| ix >= r).hash(h);
        (self.copied.as_deref() == Some(item.key.as_str())).hash(h);
        self.hash_globals(h);
    }

    /// What every row depends on: theme and agent abilities.
    fn hash_globals(&self, h: &mut DefaultHasher) {
        self.density.hash(h);
        // A pane with a palette of its own (the quick panel) can change it
        // while the rows stay: the accent follows the user's setting.
        for c in [self.p.text, self.p.accent, self.p.bg, self.p.card] {
            [c.h, c.s, c.l, c.a].map(f32::to_bits).hash(h);
        }
        (std::sync::Arc::as_ptr(&self.syntax) as usize).hash(h);
        self.tool_content.hash(h);
        self.developer.hash(h);
    }

    /// Signature of one transcript row: it changes exactly when the row's
    /// content or height may.
    fn row_sig(&self, item: &Item, row: &RowSpec) -> u64 {
        let ix = row.ix;
        let ItemKind::Work(entries) = &item.kind else {
            return self.sig(ix, item);
        };
        let mut h = DefaultHasher::new();
        match &row.part {
            Part::Whole => return self.sig(ix, item),
            Part::Head => {
                // Counts and the hidden total follow the item's revision.
                ("head", item.rev).hash(&mut h);
                self.expanded.contains(&item.key).hash(&mut h);
                (self.running && ix >= self.last_turn_start).hash(&mut h);
                self.reverted_from.is_some_and(|r| ix >= r).hash(&mut h);
                self.hash_globals(&mut h);
            }
            Part::Block {
                range,
                live,
                first,
                last,
            } => {
                (range.start, range.len(), live, first, last).hash(&mut h);
                for e in &entries[range.clone()] {
                    match e {
                        // A changed call is a new allocation (`Arc::make_mut`
                        // while the transcript holds the old one).
                        WorkEntry::Tool(call) => (
                            std::sync::Arc::as_ptr(call) as usize,
                            call.status.as_str(),
                            call.output().len(),
                            call.truncated_result,
                            call.truncated_args,
                        )
                            .hash(&mut h),
                        WorkEntry::Thought(t) => (t.id.as_str(), t.text.len()).hash(&mut h),
                    }
                }
                self.hash_entries(&entries[range.clone()], &mut h);
                if *last {
                    self.hash_turn(ix, item, &mut h);
                } else {
                    self.reverted_from.is_some_and(|r| ix >= r).hash(&mut h);
                    self.hash_globals(&mut h);
                }
            }
        }
        h.finish()
    }
}

impl Transcript {
    pub fn new() -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(1200.));
        list.set_follow_mode(FollowMode::Tail);
        let items: Rc<RefCell<Vec<Rc<Row>>>> = Rc::default();
        let md = md::Kept::default();
        // A row whose Markdown is still parsing measures short: keep that
        // height provisional so the list neither anchors to it nor re-engages
        // tail-follow from it.
        list.set_item_measurement_readiness({
            let (items, md) = (items.clone(), md.clone());
            move |ix, cx| {
                let key = items.borrow().get(ix).map(|i| i.key.clone());
                key.is_none_or(|key| md.row_ready(&key, cx))
            }
        });
        Self {
            list,
            items,
            sigs: vec![],
            keys: vec![],
            session: String::new(),
            scroll_hooked: false,
            block_scrolls: Rc::default(),
            composer_h: Rc::new(Cell::new(0.)),
            probe: Rc::default(),
            md,
        }
    }

    fn sync(&mut self, session: &str, view: &[Item], rc: &RenderCtx) {
        let specs = transcript_rows(view, rc);
        let sigs: Vec<u64> = specs.iter().map(|r| rc.row_sig(&view[r.ix], r)).collect();
        // One snapshot per item, shared by its rows; taken only for rows
        // that changed.
        let mut snapshots: HashMap<usize, Rc<Item>> = HashMap::new();
        let mut row = |spec: &RowSpec| -> Rc<Row> {
            let item = snapshots
                .entry(spec.ix)
                .or_insert_with(|| Rc::new(view[spec.ix].clone()))
                .clone();
            Rc::new(Row {
                item,
                ix: spec.ix,
                part: spec.part.clone(),
                key: spec.key.clone(),
            })
        };
        let keys: Vec<String> = specs.iter().map(|r| r.key.clone()).collect();
        if session != self.session {
            self.session = session.to_string();
            if crate::telemetry::enabled() {
                let mut probe = self.probe.borrow_mut();
                *probe = ScrollProbe {
                    layout_seq: probe.layout_seq,
                    session: session.to_string(),
                    ..Default::default()
                };
            }
            self.block_scrolls.borrow_mut().clear();
            self.md.clear();
            *self.items.borrow_mut() = specs.iter().map(&mut row).collect();
            self.list.reset(specs.len());
            self.sigs = sigs;
            self.keys = keys;
            self.list.set_follow_mode(FollowMode::Tail);
            self.list.scroll_to_end();
            return;
        }
        // Rows kept at both ends (same key) update in place; the rows between
        // them are replaced with one splice. Older history prepended above,
        // or a large work group opening into one row per block, is a single
        // splice, so the list's scroll anchor shifts with it.
        let (old_len, new_len) = (self.keys.len(), keys.len());
        let common = old_len.min(new_len);
        let head = (0..common).take_while(|&i| self.keys[i] == keys[i]).count();
        let tail = (0..common - head)
            .take_while(|&i| self.keys[old_len - 1 - i] == keys[new_len - 1 - i])
            .count();
        let mut items = self.items.borrow_mut();
        let kept = (0..head).chain(new_len - tail..new_len);
        for new_ix in kept {
            let old_ix = if new_ix < head {
                new_ix
            } else {
                new_ix + old_len - new_len
            };
            if self.sigs[old_ix] != sigs[new_ix] {
                items[old_ix] = row(&specs[new_ix]);
                // Height changed, not identity: keep the reader's offset
                // inside a potentially tall row.
                self.list.remeasure_items(old_ix..old_ix + 1);
                crate::telemetry::record_remeasure();
            }
        }
        let (old_mid, new_mid) = (head..old_len - tail, head..new_len - tail);
        if !old_mid.is_empty() || !new_mid.is_empty() {
            let fresh: Vec<Rc<Row>> = specs[new_mid.clone()].iter().map(&mut row).collect();
            items.splice(old_mid.clone(), fresh);
            self.list.splice(old_mid.clone(), new_mid.len());
            if head == 0 && tail == old_len && old_len > 0 {
                tracing::debug!(rows = new_mid.len(), "older history rows prepended");
            }
        }
        self.sigs = sigs;
        self.keys = keys;
    }

    /// Index, in the view's items, of the item at the top of the viewport.
    pub(super) fn top_item(&self) -> usize {
        let row = self.list.logical_scroll_top().item_ix;
        self.items.borrow().get(row).map_or(0, |r| r.ix)
    }

    /// The first list row of the item `key`.
    pub(super) fn row_of_item(&self, key: &str) -> Option<usize> {
        self.items.borrow().iter().position(|r| r.item.key == key)
    }

    /// Tail-following stops when the user scrolls up and re-engages at the
    /// bottom; `is_scrolled_to_end` is `None` while items are unmeasured.
    fn at_bottom(&self) -> bool {
        self.list.is_following_tail()
    }
}

// ---------------------------------------------------------------------------
// Tool presentation

// Tool names, labels and icons follow the desktop's `session-tool-presentation.ts`.
const READ_TOOLS: [&str; 6] = [
    "read_tool",
    "get_function",
    "get_symbol",
    "index",
    "ls",
    "read_shared",
];
const READ_FILE_TOOLS: [&str; 5] = [
    "read_tool",
    "get_function",
    "get_symbol",
    "read",
    "read_shared",
];
const EDIT_TOOLS: [&str; 4] = ["edit", "multiedit", "replace_symbol", "write_file"];
const SEARCH_TOOLS: [&str; 6] = [
    "search_text",
    "glob",
    "find_references",
    "find_symbol_references",
    "mdq",
    "language_query",
];
const WEB_TOOLS: [&str; 2] = ["browse", "web_fetch"];
const TASK_TOOLS: [&str; 3] = ["create_task", "todowrite", "todoread"];

fn strip_run(value: &str) -> String {
    let lower = value.trim().to_lowercase();
    lower
        .strip_prefix("run ")
        .unwrap_or(&lower)
        .trim()
        .to_string()
}

/// Name used for the label and icon: the ACP kind, unless it is `other` or an
/// edit whose title names a concrete edit tool (desktop `normalizeToolName`).
fn normalized_tool_name(call: &ToolCall) -> String {
    let (kind, title) = (strip_run(&call.kind), strip_run(&call.title));
    if kind == "edit" && EDIT_TOOLS.contains(&title.as_str()) {
        title
    } else if !kind.is_empty() && kind != "other" {
        kind
    } else {
        title
    }
}

/// Concrete tool name, deciding which output view to use (desktop `getSessionToolName`).
fn concrete_tool_name(call: &ToolCall) -> String {
    let (kind, title) = (strip_run(&call.kind), strip_run(&call.title));
    if kind == "execute" || kind == "shell" {
        return kind;
    }
    if kind == "read" && !READ_TOOLS.contains(&title.as_str()) {
        return kind;
    }
    if kind == "edit" && !EDIT_TOOLS.contains(&title.as_str()) {
        return kind;
    }
    if !title.is_empty() {
        return title;
    }
    normalized_tool_name(call)
}

fn is_delegate(name: &str) -> bool {
    name == "delegate" || name == "route_delegation_to_peer"
}

/// `mcp_foo-bar.baz` → `Foo bar baz`.
fn humanize_tool_name(value: &str) -> String {
    // Same steps as the desktop: drop `mcp` / `run ` prefixes, separators to spaces.
    let mut s = value.trim();
    if s.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("mcp")) {
        s = &s[3..];
        s = s.strip_prefix(['_', ':', '.', '/', '-']).unwrap_or(s);
    }
    if s.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("run"))
        && s[3..].starts_with(char::is_whitespace)
    {
        s = s[3..].trim_start();
    }
    let words: Vec<&str> = s
        .split(|c: char| matches!(c, '_' | '.' | ':' | '/' | '-') || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Tool call".into(),
    }
}

fn tool_label(call: &ToolCall) -> String {
    let name = normalized_tool_name(call);
    let label = match name.as_str() {
        "shell" | "execute" => "Shell",
        "read_tool" => "Read file",
        "get_function" => "Read function",
        "get_symbol" => "Read symbol",
        "index" => "Inspect file structure",
        "ls" => "List directory",
        "read_shared" => "Read shared context",
        "edit" | "multiedit" => "Edit",
        "replace_symbol" => "Replace symbol",
        "write_file" => "Write file",
        "delete_file" => "Delete file",
        "search_text" => "Search text",
        "glob" => "Find files",
        "find_references" | "find_symbol_references" => "Find references",
        "mdq" => "Search Markdown",
        "language_query" => "Query language server",
        "browse" | "web_fetch" => "Fetch URL",
        "question" => "Ask question",
        n if is_delegate(n) => "Delegate",
        "create_task" => "Create task",
        "todowrite" => "Update tasks",
        "todoread" => "Read tasks",
        "skill" => "Load skill",
        other => return humanize_tool_name(other),
    };
    label.to_string()
}

fn tool_icon(call: &ToolCall) -> IconName {
    let name = normalized_tool_name(call);
    let n = name.as_str();
    if n == "shell" || n == "execute" {
        IconName::SquareTerminal
    } else if READ_TOOLS.contains(&n) {
        IconName::Eye
    } else if EDIT_TOOLS.contains(&n) {
        IconName::FilePenLine
    } else if n == "delete_file" {
        // Desktop uses Lucide `Trash2`; the bundled set only has `Trash`.
        IconName::Trash
    } else if SEARCH_TOOLS.contains(&n) {
        IconName::Search
    } else if WEB_TOOLS.contains(&n) {
        IconName::Globe
    } else if n == "question" {
        IconName::MessageCircleQuestionMark
    } else if is_delegate(n) {
        IconName::Network
    } else if TASK_TOOLS.contains(&n) {
        IconName::ListTodo
    } else if n == "skill" {
        IconName::Sparkles
    } else {
        IconName::Wrench
    }
}

/// Whitespace collapsed, at most 110 chars (desktop `compact`).
fn compact(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > 110 {
        format!("{}…", normalized.chars().take(109).collect::<String>())
    } else {
        normalized
    }
}

/// First line of a non-JSON result.
fn result_preview(output: &str) -> String {
    let t = output.trim();
    if t.starts_with('{') || t.starts_with('[') {
        return String::new();
    }
    compact(t.lines().find(|l| !l.trim().is_empty()).unwrap_or_default())
}

/// Short argument shown after the label (desktop `toolPreview`): never raw JSON.
/// `text` with paths inside the workspace `cwd` made relative (`cwd/src/a.rs`
/// → `src/a.rs`, bare `cwd` → `.`); paths outside it stay absolute.
fn workspace_relative(text: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if cwd.len() < 2 || !cwd.starts_with('/') {
        return text.to_string();
    }
    let path_char = |c: char| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(cwd) {
        let (before, tail) = rest.split_at(at);
        let after = &tail[cwd.len()..];
        let starts_path = !before.chars().next_back().is_some_and(path_char);
        out.push_str(before);
        match after.chars().next() {
            // `cwd/…`: drop the prefix (`cwd/` alone is the workspace itself).
            Some('/') if starts_path => {
                let inner = &after[1..];
                if inner.chars().next().is_none_or(|c| !path_char(c)) {
                    out.push('.');
                }
                rest = inner;
            }
            next if starts_path && next.is_none_or(|c| !path_char(c)) => {
                out.push('.');
                rest = after;
            }
            // Another path that merely starts with the same characters.
            _ => {
                out.push_str(cwd);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn tool_preview(call: &ToolCall, cwd: &str) -> String {
    workspace_relative(&tool_preview_raw(call), cwd)
}

fn tool_preview_raw(call: &ToolCall) -> String {
    let name = concrete_tool_name(call);
    if is_skill(call)
        && let Some(skill) = SkillContent::parse(call.output())
    {
        return skill.name.to_string();
    }
    let Some(args) = call.input.as_ref().filter(|v| v.is_object()) else {
        return call
            .locations
            .first()
            .cloned()
            .unwrap_or_else(|| result_preview(call.output()));
    };
    let s = |k: &str| {
        args.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let count = |k: &str| {
        args.get(k)
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    };
    let file = || {
        s("path")
            .or_else(|| s("filePath"))
            .or_else(|| s("file_path"))
            .or_else(|| s("file"))
    };
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    match name.as_str() {
        "shell" | "execute" => {
            let extra: Vec<String> = args
                .get("args")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            s("command")
                .into_iter()
                .chain(extra)
                .collect::<Vec<_>>()
                .join(" ")
        }
        "search_text" => match (s("pattern"), s("include").or_else(|| s("path"))) {
            (Some(pat), Some(loc)) => format!("\"{pat}\" in {loc}"),
            (Some(pat), None) => format!("\"{pat}\""),
            (None, loc) => loc.unwrap_or_default(),
        },
        "glob" => match (s("pattern"), s("path")) {
            (Some(pat), Some(path)) => format!("{pat} in {path}"),
            (pat, path) => pat.or(path).unwrap_or_default(),
        },
        "question" if count("questions") > 0 => plural(count("questions"), "question"),
        "todowrite" if count("todos") > 0 => plural(count("todos"), "task"),
        "create_task" => s("expected_deliverable")
            .or_else(|| s("kind"))
            .unwrap_or_default(),
        "skill" => s("name").unwrap_or_default(),
        "browse" | "web_fetch" => s("url").unwrap_or_default(),
        // The target is the pill at the end of the row.
        n if is_delegate(n) => s("objective").unwrap_or_default(),
        "replace_symbol" => {
            let reps = args
                .get("replacements")
                .and_then(serde_json::Value::as_array);
            let first = reps
                .and_then(|r| r.first())
                .and_then(|f| f.get("path"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            match reps.map_or(0, Vec::len) {
                n if n > 1 => format!(
                    "{} +{} more",
                    if first.is_empty() {
                        "symbols".into()
                    } else {
                        first
                    },
                    n - 1
                ),
                _ => first,
            }
        }
        "read_tool" | "read" | "read_shared" => match file().or_else(|| s("root")) {
            Some(path) => {
                let n = |k: &str| args.get(k).and_then(serde_json::Value::as_u64);
                match (
                    n("offset").filter(|o| *o > 0),
                    n("limit").filter(|l| *l > 0),
                ) {
                    (Some(o), Some(l)) => format!("{path} (lines {}-{})", o + 1, o + l),
                    (Some(o), None) => format!("{path} (lines {}+)", o + 1),
                    (None, Some(l)) => format!("{path} (first {l} lines)"),
                    (None, None) => path,
                }
            }
            None => result_preview(call.output()),
        },
        _ => file()
            .or_else(|| s("root"))
            .or_else(|| s("symbol"))
            .or_else(|| s("pattern"))
            .or_else(|| s("action"))
            .or_else(|| {
                let paths: Vec<&str> = args
                    .get("paths")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .collect();
                (!paths.is_empty()).then(|| paths.join(", "))
            })
            .unwrap_or_else(|| result_preview(call.output())),
    }
}

fn is_shell(call: &ToolCall) -> bool {
    matches!(
        concrete_tool_name(call).as_str(),
        "shell" | "execute" | "bash"
    )
}

fn is_read(call: &ToolCall) -> bool {
    READ_FILE_TOOLS.contains(&concrete_tool_name(call).as_str())
}

fn is_skill(call: &ToolCall) -> bool {
    concrete_tool_name(call) == "skill" || normalized_tool_name(call) == "skill"
}

/// The skill tool returns an XML-like envelope, not escaped XML: markdown
/// inside `<content>` can contain arbitrary tags. Read metadata in order and
/// split the closing content tag from the end so examples stay untouched.
/// History may clip a result inside the body, leaving its metadata intact.
struct SkillContent<'a> {
    name: &'a str,
    description: &'a str,
    base_path: &'a str,
    content: &'a str,
    sample_files: Option<&'a str>,
    incomplete: bool,
}

impl<'a> SkillContent<'a> {
    fn parse(output: &'a str) -> Option<Self> {
        let output = output.trim();
        let closed = output.strip_suffix("</skill_content>");
        let inner = closed.unwrap_or(output);
        let (name, rest) = inner
            .strip_prefix("<skill_content name=\"")?
            .split_once("\">")?;
        if name.trim().is_empty() {
            return None;
        }
        let (description, rest) = rest
            .trim_start()
            .strip_prefix("<description>")?
            .split_once("</description>")?;
        let (base_path, rest) = rest
            .trim_start()
            .strip_prefix("<base_path>")?
            .split_once("</base_path>")?;
        let body = rest.trim_start().strip_prefix("<content>")?;
        let (content, sample_files) = if closed.is_some() {
            let (content, rest) = body.rsplit_once("</content>")?;
            let files = if rest.trim().is_empty() {
                None
            } else {
                Some(
                    rest.trim()
                        .strip_prefix("<sample_files>")?
                        .strip_suffix("</sample_files>")?
                        .trim_matches(['\r', '\n']),
                )
            };
            (content, files)
        } else {
            // Do not treat a closing tag in a markdown example as the wrapper's
            // end. Without the complete envelope, keep the available body intact.
            (body, None)
        };
        Some(Self {
            name: name.trim(),
            description: description.trim(),
            base_path: base_path
                .trim()
                .strip_prefix("file://")
                .unwrap_or(base_path.trim()),
            content: content.trim_matches(['\r', '\n']),
            sample_files: sample_files.filter(|s| !s.is_empty()),
            incomplete: closed.is_none(),
        })
    }
}

/// Nested disclosure inside an open tool card (skill instructions, delegation details).
fn tool_details_key(id: &str) -> String {
    format!("tool-details-{id}")
}

fn is_delegate_call(call: &ToolCall) -> bool {
    is_delegate(&concrete_tool_name(call)) || is_delegate(&normalized_tool_name(call))
}

/// A delegation's arguments: the objective up front, the rest behind a disclosure.
#[derive(Debug, PartialEq)]
struct DelegateBrief {
    target: Option<String>,
    objective: String,
    /// Remaining arguments as (label, text), in the order the agent sent them.
    details: Vec<(String, String)>,
}

impl DelegateBrief {
    fn parse(call: &ToolCall) -> Option<Self> {
        if !is_delegate_call(call) {
            return None;
        }
        let args = call.input.as_ref()?.as_object()?;
        let text = |v: &serde_json::Value| match v {
            serde_json::Value::String(s) => s.trim().to_string(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        let target = ["target_agent_id", "peer"]
            .iter()
            .filter_map(|k| args.get(*k).map(text))
            .find(|t| !t.is_empty());
        let objective = args.get("objective").map(text).unwrap_or_default();
        let details = args
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "target_agent_id" | "peer" | "objective"))
            .map(|(k, v)| (humanize_tool_name(k), text(v)))
            .filter(|(_, t)| !t.is_empty() && t != "null")
            .collect();
        Some(Self {
            target,
            objective,
            details,
        })
    }
}

/// URL fetches (`browse`, `web_fetch`, or the ACP `fetch` kind).
fn is_fetch(call: &ToolCall) -> bool {
    strip_run(&call.kind) == "fetch"
        || WEB_TOOLS.contains(&concrete_tool_name(call).as_str())
        || WEB_TOOLS.contains(&normalized_tool_name(call).as_str())
}

/// The URL a fetch asked for.
fn fetch_url(call: &ToolCall) -> Option<String> {
    let url = call.input.as_ref()?.get("url")?.as_str()?.trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// Byte ranges of `http(s)://` links in `text`, without trailing punctuation
/// or the brackets and quotes around them (markdown `[a](url)`, `<url>`).
fn link_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(off) = text[at..].find("http") {
        let start = at + off;
        let rest = &text[start..];
        let scheme = ["https://", "http://"]
            .into_iter()
            .find(|s| rest.starts_with(s))
            .map(str::len);
        let glued = text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let Some(scheme) = scheme.filter(|_| !glued) else {
            at = start + 4;
            continue;
        };
        let len = rest
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | ')' | ']' | '}')
            })
            .unwrap_or(rest.len());
        let mut end = start + len;
        while end > start + scheme && text[..end].ends_with(['.', ',', ';', ':', '!', '?']) {
            end -= 1;
        }
        if end > start + scheme {
            out.push(start..end);
        }
        at = start + len.max(4);
    }
    out
}

/// `text` with its links underlined in the accent color, opening on click.
fn linked_text(id: impl Into<ElementId>, text: String, p: &Palette) -> InteractiveText {
    let ranges = link_ranges(&text);
    let urls: Vec<String> = ranges.iter().map(|r| text[r.clone()].to_string()).collect();
    let link = HighlightStyle {
        color: Some(p.accent),
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(p.accent.opacity(0.6)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let styled = StyledText::new(text).with_highlights(ranges.iter().map(|r| (r.clone(), link)));
    InteractiveText::new(id, styled).on_click(ranges, move |ix, _, cx| {
        if let Some(url) = urls.get(ix) {
            cx.open_url(url);
        }
    })
}

/// The first `cap` lines of `text` (see [`more_lines`]).
fn clipped(text: &str, cap: usize) -> String {
    match text.match_indices('\n').nth(cap.saturating_sub(1)) {
        Some((end, _)) if cap > 0 => text[..end].trim_end().to_string(),
        _ => text.trim_end().to_string(),
    }
}

/// Expanded-state key of a block's "Show N more lines".
fn more_key(key: &str) -> String {
    format!("{key}#more")
}

/// Lines block `key` shows: `max`, or all once its rest is shown.
fn line_cap(key: &str, max: usize, rc: &RenderCtx) -> usize {
    if rc.expanded.contains(&more_key(key)) {
        usize::MAX
    } else {
        max
    }
}

/// "Show N more lines" under a block cut at `max` of its `total` lines, or
/// "Show fewer lines" once it shows them all (desktop shows results whole).
fn more_lines(key: &str, total: usize, max: usize, rc: &RenderCtx) -> Option<AnyElement> {
    if total <= max {
        return None;
    }
    let p = &rc.p;
    let more = more_key(key);
    let open = rc.expanded.contains(&more);
    let hover = p.card_hover.opacity(0.55);
    let weak = rc.weak.clone();
    let row = h_flex()
        .id(SharedString::from(format!("more-{key}")))
        .gap_1p5()
        .px_3()
        .py_1()
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .text_xs()
        .text_color(p.dim)
        .child(kit::icon(
            if open {
                IconName::ChevronsUp
            } else {
                IconName::ChevronsDown
            },
            12.,
            p.dim,
        ))
        .child(if open {
            "Show fewer lines".to_string()
        } else {
            let hidden = total - max;
            format!(
                "Show {hidden} more {}",
                if hidden == 1 { "line" } else { "lines" }
            )
        })
        .on_click(move |_, _, cx| toggle_expanded(&weak, more.clone(), cx));
    Some(if open {
        row.into_any_element()
    } else {
        ReadingQuip::new(format!("more-{key}"), row, p.dim).into_any_element()
    })
}

/// Fade of the [`ReadingQuip`]: slower than hover chrome, so it drifts in.
const QUIP_FADE_IN_MS: f64 = 260.;
const QUIP_FADE_OUT_MS: f64 = 200.;

/// A "Show N more lines" row that, while hovered, fades in a small quip on
/// its right asking whether anyone still reads all that code.
#[derive(IntoElement)]
pub(super) struct ReadingQuip {
    key: String,
    row: Stateful<Div>,
    color: Hsla,
}

impl ReadingQuip {
    pub(super) fn new(key: impl Into<String>, row: Stateful<Div>, color: Hsla) -> Self {
        Self {
            key: key.into(),
            row,
            color,
        }
    }
}

impl RenderOnce for ReadingQuip {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let fade = window.use_keyed_state(
            SharedString::from(format!("{}-quip", self.key)),
            cx,
            |_, _| kit::Fade::default(),
        );
        let opacity = kit::fade_opacity_ms(&fade, QUIP_FADE_IN_MS, QUIP_FADE_OUT_MS, window, cx);
        let weak = fade.downgrade();
        let key = self.key.clone();
        self.row
            .debug_selector(move || format!("{key}-row"))
            .on_hover(move |hovered, _, cx| {
                let reduce = cx.reduce_motion();
                let _ = weak.update(cx, |f, cx| {
                    if f.show(*hovered, crate::util::now_ms(), reduce) {
                        cx.notify();
                    }
                });
            })
            .child(div().flex_1())
            .when(opacity > 0., |el| {
                el.child(
                    div()
                        .debug_selector(|| "reading-quip".into())
                        .flex_none()
                        .italic()
                        .text_color(self.color)
                        .opacity(opacity)
                        .child("Are you going to read this?"),
                )
            })
    }
}

/// A text block cut at `max` lines, with its "Show N more lines".
fn clipped_with_more(
    key: &str,
    text: &str,
    max: usize,
    rc: &RenderCtx,
) -> (String, Option<AnyElement>) {
    let total = text.trim_end().lines().count();
    (
        clipped(text, line_cap(key, max, rc)),
        more_lines(key, total, max, rc),
    )
}

fn code_block(text: String, p: &Palette) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2()
        .rounded(px(8.))
        .bg(p.bg)
        .border_1()
        .border_color(p.border)
        .font_family(kit::MONO)
        .text_xs()
        .text_color(p.muted)
        .child(text)
}

/// `Parameters` / `Result` block with a header and copy button.
fn detail_section(
    id: impl Into<ElementId>,
    title: &'static str,
    text: String,
    danger: bool,
    rc: &RenderCtx,
) -> Div {
    let p = &rc.p;
    let copy = text.clone();
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .font_semibold()
                .text_color(p.dim)
                .child(title)
                .child(
                    kit::icon_button(id, IconName::Copy, "Copy")
                        .xsmall()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                ),
        )
        .child(
            code_block(clipped(&text, rc.density.output_lines()), p)
                .when(danger, |el| el.text_color(p.danger)),
        )
}

/// Unified diff card: file header with +/- counts, then gutter line numbers
/// (old | new), a sign column and the line, tinted per change. Snippet diffs
/// (edit arguments) show every line; whole-file diffs collapse unchanged runs.
/// Diff rows of one hunk plus its (added, removed) counts.
/// Rows of a hunk, at most `cap`; also the lines it adds, removes and has.
pub(super) fn diff_rows(
    d: &Diff,
    cap: usize,
    p: &Palette,
) -> (Vec<AnyElement>, usize, usize, usize) {
    let old = d.old.clone().unwrap_or_default();
    let diff = similar::TextDiff::from_lines(&old, &d.new);
    let (mut adds, mut dels) = (0usize, 0usize);
    let mut rows: Vec<AnyElement> = Vec::new();
    let mut shown = 0usize;
    let (add_bg, del_bg) = (
        p.success.opacity(if p.dark { 0.14 } else { 0.12 }),
        p.danger.opacity(if p.dark { 0.14 } else { 0.12 }),
    );
    let (add_gutter, del_gutter) = (p.success.opacity(0.28), p.danger.opacity(0.28));
    let gutter_fg = p.dim.opacity(0.8);
    // Offsets turn snippet-relative indices into file line numbers.
    // Unanchored snippets count from 1, like the desktop.
    let old_base = d.old_start.unwrap_or(1).saturating_sub(1);
    let new_base = d.new_start.unwrap_or(1).saturating_sub(1);
    // One number column: the new line, or the old one for removed lines.
    let last = (old_base + old.lines().count())
        .max(new_base + d.new.lines().count())
        .max(1);
    let gutter_w = px(last.to_string().len().max(2) as f32 * 7.5 + 14.);
    let ellipsis = |p: &Palette| {
        h_flex()
            .px_2()
            .py_0p5()
            .bg(p.card.opacity(0.6))
            .text_color(p.dim)
            .child(div().w(gutter_w + px(18.)).flex_none())
            .child("⋯")
            .into_any_element()
    };
    let groups = if d.snippet {
        vec![diff.ops().to_vec()]
    } else {
        diff.grouped_ops(3)
    };
    for (gix, group) in groups.iter().enumerate() {
        if gix > 0 && shown < cap {
            rows.push(ellipsis(p));
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (sign, bg, gutter_bg, fg) = match change.tag() {
                    similar::ChangeTag::Insert => {
                        adds += 1;
                        ("+", add_bg, add_gutter, p.text)
                    }
                    similar::ChangeTag::Delete => {
                        dels += 1;
                        ("-", del_bg, del_gutter, p.text)
                    }
                    similar::ChangeTag::Equal => {
                        (" ", transparent_black(), transparent_black(), p.muted)
                    }
                };
                shown += 1;
                if shown > cap {
                    continue;
                }
                let old_no = change
                    .old_index()
                    .map(|i| (i + 1 + old_base).to_string())
                    .unwrap_or_default();
                let new_no = change
                    .new_index()
                    .map(|i| (i + 1 + new_base).to_string())
                    .unwrap_or_default();
                let line = change.value().trim_end_matches('\n').replace('\t', "    ");
                rows.push(
                    h_flex()
                        .w_full()
                        .items_start()
                        .bg(bg)
                        .text_color(fg)
                        .child(
                            div()
                                .flex_none()
                                .w(gutter_w)
                                .pr_2()
                                .text_right()
                                .bg(gutter_bg)
                                .text_color(gutter_fg)
                                .child(if new_no.is_empty() { old_no } else { new_no }),
                        )
                        .child(
                            div()
                                .w(px(18.))
                                .flex_none()
                                .text_center()
                                .font_semibold()
                                .text_color(match change.tag() {
                                    similar::ChangeTag::Insert => p.success,
                                    similar::ChangeTag::Delete => p.danger,
                                    similar::ChangeTag::Equal => p.dim.opacity(0.5),
                                })
                                .child(sign),
                        )
                        // Long lines wrap (no horizontal scrolling).
                        .child(div().flex_1().min_w_0().pr_2().child(line))
                        .into_any_element(),
                );
            }
        }
    }
    (rows, adds, dels, shown)
}

/// GitHub-style hunk marker (`@@ -10,2 +10,3 @@`); between hunks it doubles as
/// a dashed divider so one file's edits read as a single block.
pub(super) fn hunk_marker(d: &Diff, first: bool, p: &Palette) -> AnyElement {
    let count = |s: &str| {
        if s.is_empty() {
            0
        } else {
            s.trim_end_matches('\n').split('\n').count()
        }
    };
    let label = match (d.old_start, d.new_start) {
        (Some(o), Some(n)) => format!(
            "@@ -{o},{} +{n},{} @@",
            count(d.old.as_deref().unwrap_or_default()),
            count(&d.new)
        ),
        (None, Some(n)) => format!("@@ +{n},{} @@", count(&d.new)),
        _ => "@@ … @@".to_string(),
    };
    h_flex()
        .w_full()
        .px_3()
        .py_1()
        .when(!first, |el| {
            el.border_t_1()
                .border_dashed()
                .border_color(p.border_strong)
        })
        .bg(p.accent_dim.opacity(if p.dark { 0.35 } else { 0.5 }))
        .text_color(p.dim)
        .child(label)
        .into_any_element()
}

/// One card per file: optional file header, then every hunk of that file,
/// separated by dashed hunk markers.
fn render_diff_file(
    key: String,
    path: &str,
    hunks: &[&Diff],
    with_header: bool,
    rc: &RenderCtx,
) -> Div {
    let p = &rc.p;
    let (mut adds, mut dels) = (0usize, 0usize);
    let mut body: Vec<AnyElement> = Vec::new();
    let multi = hunks.len() > 1;
    // Each hunk is cut at the density's diff line cap; one toggle shows the rest of all.
    let cap = line_cap(&key, rc.density.diff_lines(), rc);
    let mut hidden = 0;
    for (ix, d) in hunks.iter().enumerate() {
        let (rows, a, r, lines) = diff_rows(d, cap, p);
        hidden += lines.saturating_sub(rc.density.diff_lines());
        adds += a;
        dels += r;
        if multi || d.old_start.is_some() {
            body.push(hunk_marker(d, ix == 0, p));
        }
        body.extend(rows);
    }
    let more = more_lines(
        &key,
        rc.density.diff_lines() + hidden,
        rc.density.diff_lines(),
        rc,
    );
    // The tool card attached to the header draws the frame.
    v_flex()
        .w_full()
        // The tool row already names the file and counts lines; only repeat
        // that when this file differs from the one the row shows.
        .when(with_header, |el| {
            el.child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .bg(p.card)
                    .border_b_1()
                    .border_color(p.border)
                    .text_xs()
                    .child(kit::icon(IconName::FilePenLine, 12., p.dim))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(kit::MONO)
                            .child(path.to_string()),
                    )
                    .child(
                        div()
                            .font_semibold()
                            .text_color(p.success)
                            .child(format!("+{adds}")),
                    )
                    .when(dels > 0, |el| {
                        el.child(
                            div()
                                .font_semibold()
                                .text_color(p.danger)
                                .child(format!("-{dels}")),
                        )
                    }),
            )
        })
        // A fixed-height block that scrolls vertically only, flush to the card's bottom edge.
        .child(scroll_area(
            key,
            rc.density.diff_block_h(),
            ScrollbarAxis::Vertical,
            v_flex()
                .w_full()
                .line_height(px(TOOL_LINE_H))
                .font_family(kit::MONO)
                .text_xs()
                .children(body),
            rc,
        ))
        .children(more)
}

/// Structured results open as a card attached to the tool row; unknown or
/// failed skill results keep the generic parameters/error view.
fn attaches_to_header(call: &ToolCall) -> bool {
    !call.diffs.is_empty()
        || is_shell(call)
        || is_read(call)
        || is_fetch(call)
        || (is_skill(call) && !call.failed() && SkillContent::parse(call.output()).is_some())
        || (!call.failed() && DelegateBrief::parse(call).is_some())
}

/// Frame of a borderless output block inside an attached tool card.
fn flat_frame(p: &Palette) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2()
        .bg(p.bg)
        .font_family(kit::MONO)
        .text_xs()
        .line_height(px(TOOL_LINE_H))
        .text_color(p.muted)
}

/// Borderless output block inside an attached tool card.
fn flat_block(text: String, p: &Palette) -> Div {
    flat_frame(p).child(text)
}

/// First line of an attached card (the shell command, the fetched URL):
/// a dim prefix, the full value, and the desktop's thin rule
/// (`.session-tool-terminal-command-line::after`) under it.
fn card_heading(prefix: &'static str, value: impl IntoElement, p: &Palette) -> Div {
    v_flex()
        .w_full()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .px_3()
                .pt_2()
                .bg(p.bg)
                .font_family(kit::MONO)
                .text_xs()
                .child(div().flex_none().text_color(p.dim).child(prefix))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_semibold()
                        .text_color(p.text.opacity(0.88))
                        .child(value),
                ),
        )
        .child(
            div()
                .w_full()
                .bg(p.bg)
                .px(px(17.6))
                .pt(px(6.4))
                .child(div().w_full().h(px(1.)).bg(p.dim.opacity(0.35))),
        )
}

/// Fixed-height scroll block of an attached card, with always-visible
/// scrollbars on the axes that overflow (none when the content fits).
///
/// It also keeps the wheel to itself while it can scroll on that axis (CSS
/// `overscroll-behavior: contain`, as in the desktop): GPUI bubbles the wheel
/// to every scroll container under the pointer, so the transcript would
/// otherwise scroll too. Its own scroll listener is registered after this one
/// and runs first. The overflow comes from the scroll handle, i.e. the real
/// layout, so blocks that fit let the wheel through to the transcript.
fn scroll_area(
    key: String,
    max_h: f32,
    axis: ScrollbarAxis,
    child: impl IntoElement,
    rc: &RenderCtx,
) -> FadingScroll {
    let handle = rc.block_scroll(&key);
    let watch = handle.clone();
    let area = div()
        .id(SharedString::from(key.clone()))
        .w_full()
        .max_h(px(max_h))
        .track_scroll(&handle)
        .map(|el| match axis {
            ScrollbarAxis::Vertical => el.overflow_y_scroll(),
            ScrollbarAxis::Horizontal => el.overflow_x_scroll(),
            ScrollbarAxis::Both => el.overflow_scroll(),
        })
        .on_scroll(move |ev, _, cx| {
            let (d, max) = (ev.delta.pixel_delta(px(16.)), watch.max_offset());
            if (d.y != px(0.) && max.y > px(0.)) || (d.x != px(0.) && max.x > px(0.)) {
                cx.stop_propagation();
            }
        })
        .child(child);
    let area = rc
        .smooth_scroll
        .region(SharedString::from(key.clone()), area)
        .into_any_element();
    let bar = Scrollbar::new(&handle)
        .id(SharedString::from(format!("{key}-bar")))
        .axis(axis)
        .mode(ScrollbarMode::Always)
        .viewport_from_layout();
    FadingScroll {
        key,
        area,
        bar: Some(bar),
        bg: rc.p.bg,
    }
}

/// A scroll block whose scrollbars fade in while the pointer is anywhere over
/// it and fade out when it leaves (the library's own Hover mode only reacts
/// to the thin bar itself, and switches without easing).
#[derive(IntoElement)]
struct FadingScroll {
    key: String,
    area: AnyElement,
    bar: Option<Scrollbar>,
    bg: Hsla,
}

impl RenderOnce for FadingScroll {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let fade = window.use_keyed_state(
            SharedString::from(format!("{}-fade", self.key)),
            cx,
            |_, _| kit::Fade::default(),
        );
        let opacity = kit::fade_opacity(&fade, window, cx);
        let weak = fade.downgrade();
        div()
            .id(SharedString::from(format!("{}-wrap", self.key)))
            .relative()
            .w_full()
            .bg(self.bg)
            .on_hover(move |hovered, _, cx| {
                let reduce = cx.reduce_motion();
                let _ = weak.update(cx, |f, cx| {
                    if f.show(*hovered, crate::util::now_ms(), reduce) {
                        cx.notify();
                    }
                });
            })
            .child(self.area)
            .children(
                self.bar
                    .take()
                    .map(|bar| div().absolute().inset_0().opacity(opacity).child(bar)),
            )
    }
}

fn flat_note(text: &'static str, p: &Palette) -> Div {
    div()
        .px_3()
        .py_2()
        .bg(p.bg)
        .text_xs()
        .text_color(p.dim)
        .child(text)
}

/// Skill metadata stays visible when the tool card is open; the long markdown
/// instructions use a second disclosure, like the compaction summary.
fn render_skill_content(call: &ToolCall, skill: &SkillContent<'_>, rc: &RenderCtx) -> Div {
    let p = &rc.p;
    let key = tool_details_key(&call.id);
    let open = rc.expanded.contains(&key);
    let weak = rc.weak.clone();
    let copy_label = if skill.incomplete {
        "Copy available skill instructions"
    } else {
        "Copy skill instructions"
    };
    v_flex()
        .min_w_0()
        .gap_3()
        .p_3()
        .when(!skill.description.is_empty(), |el| {
            el.child(
                div()
                    .text_sm()
                    .text_color(p.muted)
                    .child(skill.description.to_string()),
            )
        })
        .when(!skill.base_path.is_empty(), |el| {
            el.child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(p.dim).child("Location"))
                    .child(
                        div()
                            .font_family(kit::MONO)
                            .text_color(p.muted)
                            .child(skill.base_path.to_string()),
                    ),
            )
        })
        .when_some(skill.sample_files, |el, files| {
            el.child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(p.dim).child("Sample files"))
                    .child(
                        div()
                            .font_family(kit::MONO)
                            .text_color(p.muted)
                            .child(files.to_string()),
                    ),
            )
        })
        .when(skill.incomplete, |el| {
            el.child(div().text_xs().text_color(p.dim).child(
                "This skill result is incomplete. Only the available instructions are shown.",
            ))
        })
        .map(|el| {
            if skill.content.trim().is_empty() {
                return el.child(div().text_xs().text_color(p.dim).child("No instructions."));
            }
            el.child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        kit::button(kit::keyed("skill-toggle", call.id.clone()))
                            .ghost()
                            .xsmall()
                            .label(if open {
                                "Hide instructions"
                            } else {
                                "Show instructions"
                            })
                            .icon(if open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx)),
                    )
                    .child(
                        Clipboard::new(kit::keyed("skill-copy", call.id.clone()))
                            .value(skill.content.to_string())
                            .accessibility_label(copy_label)
                            .tooltip(copy_label)
                            .xsmall(),
                    ),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .p_4()
                        .rounded_lg()
                        .bg(p.card)
                        .text_sm()
                        .child(rc.md.markdown(
                            format!("skill-md-{}", call.id),
                            skill.content.to_string(),
                            p,
                        )),
                )
            })
        })
}

/// Delegation card: the objective, with context, constraints and the rest of
/// the brief behind a disclosure. The result is a generic "queued" notice and
/// is not shown.
fn render_delegate_content(call: &ToolCall, brief: &DelegateBrief, rc: &RenderCtx) -> Div {
    let p = &rc.p;
    let key = tool_details_key(&call.id);
    let open = rc.expanded.contains(&key);
    let weak = rc.weak.clone();
    v_flex()
        .min_w_0()
        .gap_3()
        .p_3()
        .child(if brief.objective.is_empty() {
            div()
                .text_sm()
                .text_color(p.dim)
                .child("No objective.")
                .into_any_element()
        } else {
            div()
                .min_w_0()
                .text_sm()
                .child(rc.md.markdown(
                    format!("delegate-objective-{}", call.id),
                    brief.objective.clone(),
                    p,
                ))
                .into_any_element()
        })
        .when(!brief.details.is_empty(), |el| {
            el.child(
                h_flex().child(
                    kit::button(kit::keyed("delegate-toggle", call.id.clone()))
                        .ghost()
                        .xsmall()
                        .label(if open { "Hide details" } else { "Show details" })
                        .icon(if open {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx)),
                ),
            )
            .when(open, |el| {
                el.children(brief.details.iter().enumerate().map(|(ix, (label, text))| {
                    v_flex()
                        .gap_1()
                        .child(div().text_xs().text_color(p.dim).child(label.clone()))
                        .child(div().min_w_0().text_sm().child(rc.md.markdown(
                            format!("delegate-detail-{}-{ix}", call.id),
                            text.clone(),
                            p,
                        )))
                }))
            })
        })
}

/// Desktop `session-tool-session-pill`: arrow + delegate name; opens the
/// delegated child session when the agent has reported it.
fn delegate_pill(call: &ToolCall, target: String, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    let pill = h_flex()
        .id(SharedString::from(format!("delegate-pill-{}", call.id)))
        .flex_none()
        .gap_1()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(p.chip)
        .font_family(kit::MONO)
        .text_xs()
        .text_color(p.muted)
        .child(kit::icon(IconName::ArrowUpRight, 12., p.muted))
        .child(target.clone());
    let Some(child) = call.child_session.clone() else {
        return pill.into_any_element();
    };
    let (hover_bg, hover_fg) = (p.card_hover, p.text);
    let tip = format!("Open {target}");
    let (weak, agent) = (rc.weak.clone(), rc.agent_id.clone());
    pill.cursor_pointer()
        .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
        .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
        // The rest of the row toggles the card; the pill only navigates.
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let _ = weak.update(cx, |this, cx| this.open_child(&agent, &child, cx));
        })
        .into_any_element()
}

/// Body of an attached tool card, flush with the header.
fn attached_content(call: &ToolCall, rc: &RenderCtx) -> Div {
    let p = &rc.p;
    if is_skill(call)
        && !call.failed()
        && let Some(skill) = SkillContent::parse(call.output())
    {
        return render_skill_content(call, &skill, rc);
    }
    if !call.failed()
        && let Some(brief) = DelegateBrief::parse(call)
    {
        return render_delegate_content(call, &brief, rc);
    }
    let output = call.pretty_output();
    let body = v_flex().w_full();
    if !call.diffs.is_empty() {
        // Group hunks by file, keeping first-seen order.
        let mut files: Vec<(&str, Vec<&Diff>)> = Vec::new();
        for d in &call.diffs {
            match files.iter_mut().find(|(path, _)| *path == d.path.as_str()) {
                Some((_, hunks)) => hunks.push(d),
                None => files.push((d.path.as_str(), vec![d])),
            }
        }
        // A single edited file is already named by the tool row.
        let with_header = files.len() > 1;
        return body
            .children(files.iter().enumerate().map(|(ix, (path, hunks))| {
                render_diff_file(
                    format!("diff-{}-{ix}", call.id),
                    &workspace_relative(path, &rc.cwd),
                    hunks,
                    with_header,
                    rc,
                )
                .when(ix > 0, |el| el.border_t_1().border_color(p.border))
            }))
            .when(call.failed() && !output.trim().is_empty(), |el| {
                let (text, more) =
                    clipped_with_more(&call.id, output, rc.density.output_lines(), rc);
                el.child(
                    flat_block(text, p)
                        .border_t_1()
                        .border_color(p.border)
                        .text_color(p.danger),
                )
                .children(more)
            });
    }
    if is_shell(call) {
        // The header shows the command truncated; the full one heads the output.
        let cmd = call.command().unwrap_or_default();
        return body
            .when(!cmd.is_empty(), |el| el.child(card_heading("$", cmd, p)))
            .child(if output.trim().is_empty() {
                flat_note(
                    if call.running() {
                        "Waiting for output…"
                    } else {
                        "No output."
                    },
                    p,
                )
                .into_any_element()
            } else {
                let (text, more) =
                    clipped_with_more(&call.id, output, rc.density.output_lines(), rc);
                let block = flat_block(text, p).when(call.failed(), |el| el.text_color(p.danger));
                v_flex()
                    .w_full()
                    .child(scroll_area(
                        format!("term-{}", call.id),
                        rc.density.terminal_block_h(),
                        ScrollbarAxis::Vertical,
                        block,
                        rc,
                    ))
                    .children(more)
                    .into_any_element()
            });
    }
    if is_fetch(call) {
        // The URL is the (clickable) row header; the parameters only repeat
        // it, so the card shows just the page, with clickable links.
        let id = &call.id;
        return body.child(if output.trim().is_empty() {
            flat_note(
                if call.running() {
                    "Fetching…"
                } else {
                    "No content."
                },
                p,
            )
            .into_any_element()
        } else {
            let (text, more) = clipped_with_more(id, output, rc.density.read_lines(), rc);
            let block = flat_frame(p)
                .when(call.failed(), |el| el.text_color(p.danger))
                .child(linked_text(
                    SharedString::from(format!("fetch-out-{id}")),
                    text,
                    p,
                ));
            v_flex()
                .w_full()
                .child(scroll_area(
                    format!("fetch-scroll-{id}"),
                    rc.density.read_block_h(),
                    ScrollbarAxis::Vertical,
                    block,
                    rc,
                ))
                .children(more)
                .into_any_element()
        });
    }
    if output.trim().is_empty() {
        return body.child(flat_note(
            if call.running() {
                "Waiting for read output…"
            } else {
                "No read output available."
            },
            p,
        ));
    }
    if call.failed() {
        let (text, more) = clipped_with_more(&call.id, output, rc.density.output_lines(), rc);
        return body
            .child(flat_block(text, p).text_color(p.danger))
            .children(more);
    }
    let path = call.summary();
    let code = super::code::read_output(
        &format!("read-{}", call.id),
        output,
        Some(path.as_str()).filter(|s| !s.is_empty()),
        line_cap(&call.id, rc.density.read_lines(), rc),
        true,
        &rc.syntax,
        p,
    );
    body.child(scroll_area(
        format!("read-scroll-{}", call.id),
        rc.density.read_block_h(),
        ScrollbarAxis::Vertical,
        code,
        rc,
    ))
    .children(more_lines(
        &call.id,
        super::code::read_line_count(output),
        rc.density.read_lines(),
        rc,
    ))
}

/// Expanded content of any other tool call: raw parameters + result.
fn tool_content(call: &ToolCall, rc: &RenderCtx) -> Div {
    let p = &rc.p;
    let output = call.pretty_output();
    let input = call
        .input
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_default();
    let has_input = !input.is_empty() && input != "{}" && input != "null";
    let output_empty = output.trim().is_empty();
    let body = v_flex().gap_2();
    body.when(has_input, |el| {
        el.child(detail_section(
            SharedString::from(format!("cp-in-{}", call.id)),
            "Parameters",
            input,
            false,
            rc,
        ))
    })
    .when(!output_empty, |el| {
        el.child(detail_section(
            SharedString::from(format!("cp-out-{}", call.id)),
            if call.failed() { "Error" } else { "Result" },
            output.to_string(),
            call.failed(),
            rc,
        ))
    })
    .when(!has_input && output_empty, |el| {
        el.child(div().text_xs().text_color(p.dim).child(if call.running() {
            "Running…"
        } else {
            "No output."
        }))
    })
}

/// `expanded` key of a user message's front-matter panel.
fn front_matter_key(item_key: &str) -> String {
    format!("front-matter:{item_key}")
}

/// Front matter of a user message as highlighted YAML, with its own copy
/// button (desktop `SessionMessageMetadata`).
fn render_front_matter(
    item_key: &str,
    front: &crate::util::FrontMatter,
    rc: &RenderCtx,
) -> AnyElement {
    let p = &rc.p;
    // A fenced YAML block in the transcript's Markdown: the native code
    // block gives selection, highlighting and the header copy button, as
    // in replies. The fence outruns any backtick run in the YAML.
    let source = fenced("yaml", front.yaml.trim_end());
    // Width comes from the column shared with the message bubble.
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(div().text_xs().text_color(p.dim).child("Message metadata"))
        .child(div().min_w_0().text_sm().child(rc.md.markdown(
            format!("ufm-md-{item_key}"),
            source,
            p,
        )))
        .into_any_element()
}

/// `code` as a Markdown fenced block whose fence outruns any backtick run
/// inside it, so the code cannot close it early.
fn fenced(lang: &str, code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{code}\n{fence}")
}

fn toggle_expanded(weak: &WeakEntity<SessionPane>, key: String, cx: &mut App) {
    let _ = weak.update(cx, |this, cx| {
        // Inspecting a block is a reading action, not new content to follow.
        this.transcript.list.pause_following_tail();
        if !this.expanded.remove(&key) {
            this.expanded.insert(key);
        }
        cx.notify();
    });
}

/// Left-bordered, indented detail area under a work row (desktop `.session-tool-content`).
fn detail_well(p: &Palette) -> Div {
    div()
        .ml(px(28.))
        .mt_1()
        .mb_2()
        .pl_3()
        .border_l_1()
        .border_color(p.border.opacity(0.8))
}

fn render_tool(call: &ToolCall, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    let hover = p.card_hover.opacity(0.55);
    let open = rc.expanded.contains(&call.id);
    let output = call.pretty_output();
    let expandable = !call.diffs.is_empty()
        || !output.trim().is_empty()
        || call
            .input
            .as_ref()
            .is_some_and(|v| !v.is_null() && v != &serde_json::json!({}));
    let status: AnyElement = if call.running() {
        kit::spinner(
            SharedString::from(format!("tspin-{}", call.id)),
            13.,
            p.running,
        )
        .into_any_element()
    } else if call.failed() {
        // Yellow like the desktop's `.session-tool-status-failed` and the
        // group's failed count.
        kit::icon(IconName::TriangleAlert, 13., p.running).into_any_element()
    } else {
        kit::icon(IconName::Check, 13., p.success).into_any_element()
    };
    // A clean exit is noise except to developers (desktop `terminalExitCode`).
    let exit = call.exit_code().filter(|c| *c != 0 || rc.developer);
    let (adds, dels) = call.stats;
    let (id, weak) = (call.id.clone(), rc.weak.clone());
    let attached = open && expandable && attaches_to_header(call);
    // Opening an edit whose arguments a load shortened fetches them, so its
    // diff can be built (desktop `editNeedsFullArguments`).
    let fetch_args = !open
        && rc.tool_content
        && call.is_edit()
        && call.truncated_args.is_some()
        && call.fetch.is_none();
    let strip = (open && expandable)
        .then(|| truncation_strip(call, rc))
        .flatten();
    let row_selector = format!("tool-row-{}", call.id);
    // The HUD shows a tool call as a one-line chip: a faint white fill that
    // is as wide as its text, with no border (design D15).
    let chip = rc.density.is_compact() && !attached;
    let row = h_flex()
        .id(SharedString::from(format!("tool-{}", call.id)))
        .debug_selector(move || row_selector)
        .gap_2()
        .px_1p5()
        .min_h(px(if rc.density.is_compact() { 26. } else { 29. }))
        .when(!attached, |el| {
            el.rounded(px(if rc.density.is_compact() { 10. } else { 8. }))
        })
        .when(chip, |el| el.bg(p.chip).max_w_full())
        // Open + attached: the row becomes the card's header strip.
        .when(attached, |el| {
            el.bg(p.card.opacity(0.88))
                .border_b_1()
                .border_color(p.border.opacity(0.8))
        })
        .when(expandable, |el| {
            el.cursor_pointer().hover(move |s| s.bg(hover))
        })
        .text_xs()
        .child(
            div()
                .w(px(20.))
                .flex_none()
                .flex()
                .justify_center()
                .child(kit::icon(tool_icon(call), 14., p.dim)),
        )
        .child(
            div()
                .flex_none()
                .font_medium()
                .text_color(p.text.opacity(0.88))
                .child(tool_label(call)),
        )
        .child(match fetch_url(call).filter(|_| is_fetch(call)) {
            // A fetch's URL opens in the browser; the rest of the row toggles.
            Some(url) => h_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .id(SharedString::from(format!("fetch-link-{}", call.id)))
                        .min_w_0()
                        .truncate()
                        .font_family(kit::MONO)
                        .text_color(p.dim)
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .child(compact(&url))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cx.open_url(&url);
                        }),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(kit::MONO)
                .text_color(p.dim)
                .child(compact(&tool_preview(call, &rc.cwd)))
                .into_any_element(),
        })
        .when(adds > 0 || dels > 0, |el| {
            el.child(
                h_flex()
                    .gap_1()
                    .font_family(kit::MONO)
                    .when(adds > 0, |el| {
                        el.child(div().text_color(p.success).child(format!("+{adds}")))
                    })
                    .when(dels > 0, |el| {
                        el.child(div().text_color(p.danger).child(format!("-{dels}")))
                    }),
            )
        })
        .when_some(exit, |el, code| {
            el.child(if code == 0 {
                kit::chip("exit 0", p)
            } else {
                kit::tone_chip(format!("exit {code}"), p.danger)
            })
        })
        // The delegate's name lives only here (desktop session pill); once the
        // child session is known, the pill opens it.
        .when_some(
            DelegateBrief::parse(call).and_then(|b| b.target),
            |el, target| el.child(delegate_pill(call, target, rc)),
        )
        // The trailing slot shows the status; hovering an expandable row swaps
        // it for the expand chevron, in the same place.
        .map(|el| {
            if !expandable {
                el.child(status)
            } else {
                el.group(TOOL_ROW_GROUP)
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .size(px(13.))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .group_hover(TOOL_ROW_GROUP, |s| s.invisible())
                                    .child(status),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .invisible()
                                    .group_hover(TOOL_ROW_GROUP, |s| s.visible())
                                    .child(kit::icon(
                                        if open {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        },
                                        13.,
                                        p.dim,
                                    )),
                            ),
                    )
                    .on_click(move |_, _, cx| {
                        toggle_expanded(&weak, id.clone(), cx);
                        if fetch_args {
                            let id = id.clone();
                            let _ = weak.update(cx, |this, cx| {
                                this.load_tool_content(&id, "arguments", cx)
                            });
                        }
                    })
            }
        });
    let (card_selector, detail_selector) = (
        format!("tool-card-{}", call.id),
        format!("tool-detail-{}", call.id),
    );
    if attached {
        return v_flex()
            .debug_selector(move || card_selector)
            .mt_1()
            .mb_2()
            .rounded(px(8.))
            .border_1()
            .border_color(p.border.opacity(0.8))
            .overflow_hidden()
            .bg(p.bg)
            .child(row)
            .children(strip)
            .child(attached_content(call, rc))
            // GPUI clips children to rectangles, so tinted diff rows would show
            // square corners past the rounded border: paint the transcript
            // background over them (inner radius = 8 - 1px border). The
            // border itself paints after the children, on top.
            .child(kit::corner_mask(7., p.bg))
            .into_any_element();
    }
    v_flex()
        .when(rc.density.is_compact(), |el| el.items_start())
        .child(row)
        .when(open && expandable, |el| {
            el.child(
                detail_well(p)
                    .w_full()
                    .debug_selector(move || detail_selector)
                    .children(strip)
                    .child(tool_content(call, rc)),
            )
        })
        .into_any_element()
}

/// "Output shortened" with "Show full output" (desktop
/// `session-tool-truncated`): a windowed load cut this call's result or
/// arguments, and the agent can return them whole.
fn truncation_strip(call: &ToolCall, rc: &RenderCtx) -> Option<AnyElement> {
    use crate::session::ContentFetch;

    let (part, bytes, noun) = match (call.truncated_result, call.truncated_args) {
        (Some(bytes), _) => ("result", bytes, "output"),
        (None, Some(bytes)) => ("arguments", bytes, "parameters"),
        (None, None) => return None,
    };
    let p = &rc.p;
    let id = call.id.clone();
    let weak = rc.weak.clone();
    let load = move |id: String| {
        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let id = id.clone();
            let _ = weak.update(cx, |this, cx| this.load_tool_content(&id, part, cx));
        }
    };
    let action: AnyElement = match &call.fetch {
        Some(ContentFetch::Loading(_)) => h_flex()
            .gap_1p5()
            .child(kit::spinner(
                SharedString::from(format!("tc-spin-{}", call.id)),
                12.,
                p.muted,
            ))
            .child(format!("Loading full {noun}…"))
            .into_any_element(),
        _ if !rc.tool_content => div().into_any_element(),
        Some(ContentFetch::Failed(_)) => kit::flat_btn(
            SharedString::from(format!("tc-retry-{}", call.id)),
            true,
            true,
            p,
        )
        .h(px(24.))
        .child("Try again")
        .on_click(load(id))
        .into_any_element(),
        None => kit::flat_btn(
            SharedString::from(format!("tc-load-{}", call.id)),
            true,
            true,
            p,
        )
        .h(px(24.))
        .child(kit::icon(IconName::ChevronsDown, 13., p.text))
        .child(format!("Show full {noun}"))
        .on_click(load(id))
        .into_any_element(),
    };
    let error = match &call.fetch {
        Some(ContentFetch::Failed(error)) => Some(error.clone()),
        _ => None,
    };
    Some(
        v_flex()
            .gap_1()
            .px_2p5()
            .py_1p5()
            .text_xs()
            .text_color(p.muted)
            .child(
                h_flex()
                    .gap_2()
                    .child(kit::icon(IconName::Scissors, 12., p.dim))
                    .child(div().flex_1().min_w_0().child(format!(
                        "{} shortened · {} in full",
                        if part == "result" {
                            "Output"
                        } else {
                            "Parameters"
                        },
                        crate::util::human_size(bytes)
                    )))
                    .child(action),
            )
            .children(error.map(|e| div().text_color(p.danger).child(e)))
            .into_any_element(),
    )
}

/// Longest reasoning title, in chars (desktop `previewMaxLength`).
const REASONING_TITLE_MAX: usize = 122;
/// Source bytes parsed for a raw-reasoning title; the title only shows its start.
const REASONING_TITLE_SOURCE: usize = 2048;

/// Plain-text title of a reasoning entry, without markdown syntax.
fn reasoning_title(text: &str) -> String {
    let mut end = text.len().min(REASONING_TITLE_SOURCE);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let plain = md::plain_text(&text[..end]);
    if plain.chars().count() > REASONING_TITLE_MAX {
        let cut: String = plain.chars().take(REASONING_TITLE_MAX - 3).collect();
        format!("{cut}...")
    } else {
        plain
    }
}

/// Splits work entries into tools and reasoning blocks, as `start..end`
/// ranges: consecutive summary thoughts form one block, any other thought is
/// a block of its own (desktop `groupConsecutiveReasoning`).
fn work_blocks(entries: &[WorkEntry]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let joins = match e {
            WorkEntry::Thought(t) if t.is_summary() => out.last().is_some_and(|b| {
                entries[b.clone()]
                    .iter()
                    .all(|x| matches!(x, WorkEntry::Thought(r) if r.is_summary()))
            }),
            _ => false,
        };
        match out.last_mut() {
            Some(last) if joins => last.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// Expanded work groups with more visible blocks than this become one
/// transcript row per block, so the list builds and lays out only the
/// blocks on screen.
const SPLIT_BLOCKS: usize = 12;

/// How a work group shows: its blocks, the visible ones, and its header.
struct WorkPlan {
    /// A settled turn's group: a summary row, open or closed.
    settled: bool,
    open: bool,
    /// Blocks a running group shows whole; more are folded to its tail.
    collapse_above: usize,
    blocks: Vec<Range<usize>>,
    /// Blocks shown: `blocks[visible_from..]` (none for a closed summary).
    visible_from: usize,
    /// The last visible block is the live end of a running turn.
    live_tail: bool,
}

impl WorkPlan {
    fn of(ix: usize, item: &Item, entries: &[WorkEntry], rc: &RenderCtx) -> Self {
        let settled = !(rc.running && ix >= rc.last_turn_start);
        let open = rc.expanded.contains(&item.key);
        let blocks = work_blocks(entries);
        let (collapse_above, tail) = rc.density.live_tail();
        // Summary runs show as one block, so collapsing counts blocks, not entries.
        let visible_from = if settled && !open {
            blocks.len()
        } else if settled || open || blocks.len() <= collapse_above {
            0
        } else {
            blocks.len() - tail
        };
        Self {
            settled,
            open,
            collapse_above,
            visible_from,
            live_tail: rc.running && ix == rc.last_ix,
            blocks,
        }
    }

    fn visible(&self) -> &[Range<usize>] {
        &self.blocks[self.visible_from..]
    }

    fn hidden(&self) -> usize {
        if self.settled { 0 } else { self.visible_from }
    }

    /// A header row: the settled summary, or the running group's toggle.
    fn has_header(&self) -> bool {
        self.settled || self.hidden() > 0 || self.open && self.blocks.len() > self.collapse_above
    }

    fn split(&self) -> bool {
        self.visible().len() > SPLIT_BLOCKS
    }
}

/// A reasoning block (desktop `SessionReasoningBlock`). A run of provider
/// summaries is titled by the latest one and opens as a `-` list; a single
/// summary is a plain row. Raw reasoning opens as markdown.
fn render_reasoning(thoughts: &[&Thought], live: bool, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    let Some(first) = thoughts.first() else {
        return div().into_any_element();
    };
    let id = first.id.as_str();
    let hover = p.card_hover.opacity(0.55);
    let summary_run = thoughts.iter().all(|t| t.is_summary());
    let expandable = !summary_run || thoughts.len() > 1;
    let open = expandable && rc.expanded.contains(id);
    let title = thoughts
        .iter()
        .rev()
        .map(|t| reasoning_title(&t.text))
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| "Thinking".into());
    let (key, weak) = (id.to_string(), rc.weak.clone());
    let row = h_flex()
        .id(SharedString::from(format!("thought-{id}")))
        .gap_2()
        .px_1p5()
        .min_h(px(29.))
        .rounded(px(8.))
        .text_xs()
        .child(
            div()
                .w(px(20.))
                .flex_none()
                .flex()
                .justify_center()
                .child(if live {
                    kit::spinner(SharedString::from(format!("thspin-{id}")), 13., p.running)
                        .into_any_element()
                } else {
                    kit::icon(IconName::Sparkles, 14., p.dim).into_any_element()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_medium()
                .text_color(p.muted)
                .child(title),
        )
        .when(expandable, |el| {
            el.cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(kit::icon(
                    if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                    13.,
                    p.dim,
                ))
                .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx))
        });
    if !open {
        return row.into_any_element();
    }
    let body = if summary_run {
        // Desktop `.session-reasoning-summary-list`: one `-` line per summary.
        v_flex()
            .gap_0p5()
            .children(thoughts.iter().map(|t| {
                h_flex()
                    .items_start()
                    .gap_1p5()
                    .child(div().flex_none().child("-"))
                    .child(div().flex_1().min_w_0().child(rc.md.markdown(
                        format!("th-md-{}", t.id),
                        t.text.trim().to_string(),
                        p,
                    )))
            }))
            .into_any_element()
    } else {
        v_flex()
            .gap_2()
            .children(thoughts.iter().map(|t| {
                rc.md
                    .markdown(format!("th-md-{}", t.id), t.text.trim().to_string(), p)
                    .motion(md::stream_motion(rc.stream_fade))
            }))
            .into_any_element()
    };
    v_flex()
        .child(row)
        .child(
            detail_well(p)
                .opacity(0.78)
                .text_xs()
                .text_color(p.muted)
                .child(body),
        )
        .into_any_element()
}

/// A whole work group in one row: its header and visible blocks.
fn render_work(ix: usize, item: &Item, entries: &[WorkEntry], rc: &RenderCtx) -> AnyElement {
    let plan = WorkPlan::of(ix, item, entries, rc);
    let visible = plan.visible();
    let n = visible.len();
    v_flex()
        .w_full()
        .children(work_header(item, entries, &plan, rc))
        .children(visible.iter().enumerate().map(|(i, block)| {
            render_block(&entries[block.clone()], plan.live_tail && i + 1 == n, rc)
        }))
        .into_any_element()
}

/// One block of a work group: a tool call or a run of reasoning. `live`:
/// the last block of a running turn (only it can still stream).
fn render_block(entries: &[WorkEntry], live: bool, rc: &RenderCtx) -> AnyElement {
    match entries {
        [WorkEntry::Tool(call)] => render_tool(call, rc),
        _ => {
            let thoughts: Vec<&Thought> = entries
                .iter()
                .filter_map(|e| match e {
                    WorkEntry::Thought(t) => Some(t),
                    WorkEntry::Tool(_) => None,
                })
                .collect();
            render_reasoning(&thoughts, live, rc)
        }
    }
}

/// Header of a work group: the settled turn's summary (closed or open), or
/// the running group's "+N previous work entries" / "Show fewer" toggle.
fn work_header(
    item: &Item,
    entries: &[WorkEntry],
    plan: &WorkPlan,
    rc: &RenderCtx,
) -> Option<AnyElement> {
    if !plan.has_header() {
        return None;
    }
    let p = &rc.p;
    let (key, weak) = (item.key.clone(), rc.weak.clone());
    let hover = p.card_hover.opacity(0.55);
    let id = SharedString::from(format!("work-{}", item.key));
    if !plan.settled {
        let hidden = plan.hidden();
        return Some(
            h_flex()
                .id(id)
                .gap_1p5()
                .px_1p5()
                .py_1()
                .rounded(px(8.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .text_xs()
                .text_color(p.dim)
                .child(kit::icon(
                    if plan.open {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    },
                    12.,
                    p.dim,
                ))
                .child(if plan.open {
                    "Show fewer work entries".to_string()
                } else {
                    format!(
                        "+{hidden} previous work {}",
                        if hidden == 1 { "entry" } else { "entries" }
                    )
                })
                .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx))
                .into_any_element(),
        );
    }
    let tools = entries
        .iter()
        .filter(|e| matches!(e, WorkEntry::Tool(_)))
        .count();
    let failed = entries
        .iter()
        .filter(|e| matches!(e, WorkEntry::Tool(c) if c.failed()))
        .count();
    let thoughts = entries.len() - tools;
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    if tools > 0 {
        parts.push(plural(tools, "tool call"));
    }
    if thoughts > 0 {
        parts.push(plural(thoughts, "reasoning step"));
    }
    let summary = if parts.is_empty() {
        "Worked".to_string()
    } else {
        format!("Worked through {}", parts.join(" and "))
    };
    Some(
        h_flex()
            .id(id)
            .gap_2()
            .px_1p5()
            .min_h(px(29.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .text_xs()
            .child(
                div()
                    .w(px(20.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(kit::icon(IconName::Wrench, 14., p.dim)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_medium()
                    .text_color(p.muted)
                    .child(summary),
            )
            .when(failed > 0 && !plan.open, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        // Yellow like the desktop app (`--status-running`); the
                        // icon says what the count is.
                        .text_color(p.running)
                        .child(kit::icon(IconName::TriangleAlert, 12., p.running))
                        .child(failed.to_string()),
                )
            })
            .child(kit::icon(
                if plan.open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                13.,
                p.dim,
            ))
            .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx))
            .into_any_element(),
    )
}

/// Hover group of the turn a compact pane shows its response actions for.
fn turn_group(ix: usize) -> SharedString {
    SharedString::from(format!("turn-{ix}"))
}

/// Copy / change-stats row under the last block of a turn (desktop "response actions").
fn turn_actions(
    ix: usize,
    item: &Item,
    stats: (usize, usize, usize),
    rc: &RenderCtx,
) -> AnyElement {
    let p = &rc.p;
    let copied = rc.copied.as_deref() == Some(item.key.as_str());
    let (adds, dels, files) = stats;
    let key = item.key.clone();
    let weak = rc.weak.clone();
    let user = rc.turn_user.get(&ix).filter(|_| rc.undo_ready);
    let undo_to = user.filter(|u| rc.undoable.contains(*u)).cloned();
    let redo = user.is_some_and(|u| rc.redo_target.as_ref() == Some(u));
    let fork = rc.fork_points.contains(&ix);
    let diff_open = rc.diff_open == Some(ix);
    let live = rc.live_turn(ix);
    // Compact: the buttons wait for the pointer over the turn (the change
    // pill stays, it is information).
    let hover_only = rc.density.hover_actions() && !copied;
    let buttons = h_flex()
        .gap_1()
        .when(!live, |el| {
            el.child(
                kit::icon_button(
                    SharedString::from(format!("copy-{}", item.key)),
                    if copied {
                        IconName::Check
                    } else {
                        IconName::Copy
                    },
                    if copied { "Copied" } else { "Copy response" },
                )
                .xsmall()
                .on_click(move |_, _, cx| {
                    let key = key.clone();
                    let _ = weak.update(cx, |this, cx| this.copy_turn(ix, key, cx));
                }),
            )
        })
        .when_some(undo_to, |el, message_id| {
            let weak = rc.weak.clone();
            el.child(
                kit::icon_button(
                    SharedString::from(format!("undo-{}", item.key)),
                    IconName::Undo2,
                    "Undo workspace to this prompt",
                )
                .xsmall()
                .on_click(move |_, _, cx| {
                    let message_id = message_id.clone();
                    let _ = weak.update(cx, |this, cx| this.request_undo(message_id, cx));
                }),
            )
        })
        .when(redo, |el| {
            let weak = rc.weak.clone();
            el.child(
                kit::icon_button(
                    SharedString::from(format!("redo-{}", item.key)),
                    IconName::Redo2,
                    "Redo workspace changes for this turn",
                )
                .xsmall()
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| this.redo(cx));
                }),
            )
        })
        .when(fork, |el| {
            let weak = rc.weak.clone();
            el.child(
                kit::icon_button(
                    SharedString::from(format!("fork-{}", item.key)),
                    IconName::GitFork,
                    "Fork into a new session from this response",
                )
                .xsmall()
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |_, cx| cx.emit(SessionPaneEvent::Fork { end: ix }));
                }),
            )
        })
        .when(hover_only, |el| {
            el.invisible().group_hover(turn_group(ix), |s| s.visible())
        });
    h_flex()
        .gap_1()
        .pl_1()
        .pt_0p5()
        .text_color(p.muted)
        .child(buttons)
        .child(div().flex_1())
        .when(adds > 0 || dels > 0, |el| {
            // Desktop `.session-turn-diff-pill`: 0.28rem/0.5rem padding,
            // 0.68rem mono at line-height 1, 0.35rem gap, 13px icon. It
            // opens (and closes) the turn diff panel.
            let (weak, hover) = (rc.weak.clone(), p.card_hover);
            let plural = |n: usize, one: &str, many: &str| {
                format!("{n} {}", if n == 1 { one } else { many })
            };
            let tip = format!(
                "Turn changes: {} added, {} deleted{}",
                plural(adds, "line", "lines"),
                plural(dels, "line", "lines"),
                if files > 0 {
                    format!(", {} changed", plural(files, "file", "files"))
                } else {
                    String::new()
                }
            );
            el.child(
                h_flex()
                    .id(SharedString::from(format!("diff-pill-{}", item.key)))
                    .debug_selector(|| format!("diff-pill-{ix}"))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .when(diff_open, |el| el.bg(hover))
                    .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                    .on_mouse_down(MouseButton::Left, kit::claim_press)
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| this.toggle_turn_diff(ix, cx));
                    })
                    .flex_none()
                    .gap(px(5.6))
                    .px(px(8.))
                    .py(px(4.5))
                    .rounded_full()
                    .bg(p.chip)
                    .font_family(kit::MONO)
                    .text_size(px(10.9))
                    .line_height(px(10.9))
                    .font_semibold()
                    .text_color(p.dim)
                    .child(kit::icon(IconName::Diff, 13., p.dim))
                    .when(files > 0, |el| {
                        el.child(div().text_color(p.muted).child(files.to_string()))
                    })
                    .child(div().text_color(p.success).child(format!("+{adds}")))
                    .when(dels > 0, |el| {
                        el.child(div().text_color(p.danger).child(format!("-{dels}")))
                    })
                    // Desktop: the arrow turns around, in the accent, while open.
                    .child(if diff_open {
                        kit::icon(IconName::ArrowDownLeft, 12., p.accent)
                    } else {
                        kit::icon(IconName::ArrowUpRight, 12., p.dim)
                    }),
            )
        })
        .into_any_element()
}

/// Image thumbnail or file card for an attachment of a user message (desktop
/// `SessionAttachmentPreview`).
fn render_user_attachment(key: &str, ix: usize, a: &UserAttachment, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    let tip = match a.size {
        Some(size) => format!("{} · {}", a.name, human_size(size)),
        None => a.name.clone(),
    };
    let id = SharedString::from(format!("uatt-{key}-{ix}"));
    let image = super::preview::user_attachment_image(a);
    let frame = div()
        .id(id)
        .flex_none()
        .rounded(px(10.))
        .border_1()
        .border_color(p.border)
        .bg(p.card)
        .overflow_hidden()
        .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx));
    match image {
        // Click opens the lightbox over every image of the session.
        Some(image) => {
            let (weak, key, hover) = (rc.weak.clone(), key.to_string(), p.accent);
            frame
                .size(px(48.))
                .cursor_pointer()
                .hover(move |s| s.border_color(hover))
                .on_click(move |_, _window, cx| {
                    let _ = weak.update(cx, |this, cx| this.open_chat_image(&key, ix, cx));
                })
                .child(
                    img(image)
                        .size_full()
                        .rounded(px(9.))
                        .object_fit(ObjectFit::Cover),
                )
                .into_any_element()
        }
        None => frame
            .h_flex()
            .gap_2()
            .px_2p5()
            .py_1p5()
            .max_w(px(240.))
            .text_xs()
            .child(kit::icon(
                if a.is_image() {
                    IconName::Image
                } else {
                    IconName::FileText
                },
                14.,
                p.muted,
            ))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_medium()
                    .child(a.name.clone()),
            )
            .when_some(a.size, |el, size| {
                el.child(div().flex_none().text_color(p.dim).child(human_size(size)))
            })
            .into_any_element(),
    }
}

fn render_item(ix: usize, item: &Item, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    // Response actions appear once the turn has settled, like the desktop app.
    let actions = turn_actions_of(ix, rc);
    let body = match &item.kind {
        ItemKind::User {
            text,
            attachments,
            local,
        } => {
            let copied = rc.copied.as_deref() == Some(item.key.as_str());
            let group = SharedString::from(format!("ug-{}", item.key));
            let (key, weak, text2) = (item.key.clone(), rc.weak.clone(), text.clone());
            // Front matter (often from automated messages) stays out of the
            // bubble; a toggle beside Copy shows it as YAML.
            // The toggle is a developer tool (desktop `SessionTurn`).
            let front = crate::util::split_front_matter(text)
                .and_then(|(front, _)| rc.developer.then_some(front));
            let fm_key = front_matter_key(&item.key);
            let fm_open = front.is_some() && rc.expanded.contains(&fm_key);
            // The same text find searches (`find::views_of`).
            let text = find::user_body(text).to_string();
            v_flex()
                .group(group.clone())
                .w_full()
                .items_end()
                .pt_2()
                .gap_1p5()
                .when(!attachments.is_empty(), |el| {
                    el.child(
                        h_flex()
                            .max_w(relative(0.86))
                            .flex_wrap()
                            .justify_end()
                            .gap_1p5()
                            .when(*local, |el| el.opacity(0.7))
                            .children(
                                attachments
                                    .iter()
                                    .enumerate()
                                    .map(|(ix, a)| render_user_attachment(&item.key, ix, a, rc)),
                            ),
                    )
                })
                // Metadata reads before the message it describes. One column
                // holds both and stretches them to the wider one, so the
                // panel and the bubble share their width.
                .when(
                    !text.is_empty() || attachments.is_empty() || fm_open,
                    |el| {
                        el.child(
                            v_flex()
                                .max_w(relative(0.86))
                                .min_w_0()
                                .items_stretch()
                                .gap_1p5()
                                .when_some(front.clone().filter(|_| fm_open), |el, front| {
                                    el.child(render_front_matter(&item.key, &front, rc))
                                })
                                .when(!text.is_empty() || attachments.is_empty(), |el| {
                                    el.child(
                                        // Right-aligned and tinted with the
                                        // accent; the HUD's is a full pill.
                                        prose(
                                            div()
                                                .min_w_0()
                                                .px_3p5()
                                                .py(px(if rc.density.is_compact() {
                                                    6.
                                                } else {
                                                    8.
                                                }))
                                                .rounded(px(if rc.density.is_compact() {
                                                    18.
                                                } else {
                                                    14.
                                                }))
                                                .bg(p.accent_dim),
                                            rc.density,
                                        )
                                        .when(*local, |el| el.opacity(0.7))
                                        .when(text.is_empty(), |el| {
                                            el.text_color(p.dim).child("No message body")
                                        })
                                        // Markdown like the desktop app's user messages.
                                        .when(
                                            !text.is_empty(),
                                            |el| {
                                                el.min_w_0().child(rc.md.markdown(
                                                    find::user_view_id(&item.key),
                                                    text.clone(),
                                                    p,
                                                ))
                                            },
                                        ),
                                    )
                                }),
                        )
                    },
                )
                .child(
                    h_flex()
                        .h(px(26.))
                        .pr_1()
                        .gap_0p5()
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .when(copied || fm_open, |el| el.visible())
                        .when(front.is_some(), |el| {
                            let (weak, fm_key) = (rc.weak.clone(), fm_key.clone());
                            let label = if fm_open {
                                "Hide metadata"
                            } else {
                                "Show metadata"
                            };
                            el.child(
                                kit::icon_button(
                                    SharedString::from(format!("ufm-{}", item.key)),
                                    IconName::Braces,
                                    label,
                                )
                                .xsmall()
                                .selected(fm_open)
                                .on_click(move |_, _, cx| {
                                    toggle_expanded(&weak, fm_key.clone(), cx)
                                }),
                            )
                        })
                        .child(
                            kit::icon_button(
                                SharedString::from(format!("ucopy-{}", item.key)),
                                if copied {
                                    IconName::Check
                                } else {
                                    IconName::Copy
                                },
                                if copied { "Copied" } else { "Copy prompt" },
                            )
                            .xsmall()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text2.clone()));
                                let key = key.clone();
                                let _ = weak.update(cx, |this, cx| this.flash_copied(key, cx));
                            }),
                        ),
                )
                .into_any_element()
        }
        ItemKind::Agent { text } => agent_message(ix, item, text, actions, rc).into_any_element(),
        ItemKind::Work(entries) => v_flex()
            .w_full()
            .when(rc.density.hover_actions(), |el| el.group(turn_group(ix)))
            .child(render_work(ix, item, entries, rc))
            .when_some(actions, |el, stats| {
                el.child(turn_actions(ix, item, stats, rc))
            })
            .into_any_element(),
        ItemKind::Notice { text, error } => h_flex()
            .w_full()
            .justify_center()
            .gap_1p5()
            .py_1()
            .text_xs()
            .text_color(if *error { p.danger } else { p.dim })
            .when(*error, |el| {
                el.child(kit::icon(IconName::CircleAlert, 12., p.danger))
            })
            .child(text.clone())
            .into_any_element(),
        ItemKind::Compaction(c) => render_compaction(item, c, rc),
    };
    row_frame(ix, body, (true, true), rc)
}

/// Type of the conversation: the HUD reads in the system UI font at 14.5 px
/// (design D15); the main window keeps the theme's font at its small size.
fn prose<E: Styled>(el: E, density: Density) -> E {
    if density.is_compact() {
        el.text_size(px(COMPACT_PROSE_PX))
            .font_family(kit::SYSTEM_UI)
    } else {
        el.text_sm()
    }
}

/// An assistant reply: text straight on the surface behind the transcript, in
/// both densities, with no card, border or fill of its own. The turn's actions
/// (copy, undo, fork) follow it.
fn agent_message(
    ix: usize,
    item: &Item,
    text: &str,
    actions: Option<(usize, usize, usize)>,
    rc: &RenderCtx,
) -> Div {
    prose(v_flex().w_full().px_1(), rc.density)
        .when(rc.density.hover_actions(), |el| el.group(turn_group(ix)))
        .child(
            rc.md
                .markdown(find::reply_view_id(&item.key), text.to_owned(), &rc.p)
                .motion(md::stream_motion(rc.stream_fade)),
        )
        .when_some(actions, |el, stats| {
            el.child(turn_actions(ix, item, stats, rc))
        })
}

/// The document column around a row. `(top, bottom)`: the row starts or
/// ends its item, so it takes the gap between items on that side (rows of a
/// split work group sit flush against each other).
fn row_frame(
    ix: usize,
    body: AnyElement,
    (top, bottom): (bool, bool),
    rc: &RenderCtx,
) -> AnyElement {
    // Turns rolled back by an undo stay visible, dimmed, until the next prompt.
    let reverted = rc.reverted_from.is_some_and(|r| ix >= r);
    // Center the column like a document; the list itself spans the page.
    div()
        .w_full()
        .flex()
        .justify_center()
        .px(px(rc.density.row_pad()))
        .when(top, |el| el.pt_1())
        .when(bottom, |el| el.pb_1())
        .when(reverted, |el| el.opacity(0.5))
        .child(
            div()
                .w_full()
                .max_w(px(rc.density.column_max()))
                .child(body),
        )
        .into_any_element()
}

/// One transcript list row: a whole view item, or part of a large work
/// group (see [`SPLIT_BLOCKS`]).
#[derive(Clone)]
struct Row {
    item: Rc<Item>,
    /// Index of the item in the view.
    ix: usize,
    part: Part,
    key: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Whole,
    /// The group's header row.
    Head,
    /// One visible block: `entries[range]`.
    Block {
        range: Range<usize>,
        live: bool,
        /// First row of the group (no header) / last row of the group.
        first: bool,
        last: bool,
    },
}

/// A row before its item is snapshotted: view index, part and key.
struct RowSpec {
    ix: usize,
    part: Part,
    key: String,
}

/// Rows of the view: one per item, and a large expanded work group as its
/// header plus one row per visible block.
fn transcript_rows(view: &[Item], rc: &RenderCtx) -> Vec<RowSpec> {
    let mut rows = Vec::with_capacity(view.len());
    for (ix, item) in view.iter().enumerate() {
        let plan = match &item.kind {
            ItemKind::Work(entries) => Some(WorkPlan::of(ix, item, entries, rc)),
            _ => None,
        };
        let (Some(plan), ItemKind::Work(entries)) = (plan.filter(WorkPlan::split), &item.kind)
        else {
            rows.push(RowSpec {
                key: item.key.clone(),
                ix,
                part: Part::Whole,
            });
            continue;
        };
        if plan.has_header() {
            rows.push(RowSpec {
                key: format!("{}#head", item.key),
                ix,
                part: Part::Head,
            });
        }
        let visible = plan.visible();
        let n = visible.len();
        for (i, block) in visible.iter().enumerate() {
            rows.push(RowSpec {
                key: format!("{}#{}", item.key, entries[block.start].id()),
                ix,
                part: Part::Block {
                    range: block.clone(),
                    live: plan.live_tail && i + 1 == n,
                    first: i == 0 && !plan.has_header(),
                    last: i + 1 == n,
                },
            });
        }
    }
    rows
}

fn render_row(row: &Row, rc: &RenderCtx) -> AnyElement {
    let (ix, item) = (row.ix, row.item.as_ref());
    match (&row.part, &item.kind) {
        (Part::Head, ItemKind::Work(entries)) => {
            let plan = WorkPlan::of(ix, item, entries, rc);
            let body =
                work_header(item, entries, &plan, rc).unwrap_or_else(|| div().into_any_element());
            row_frame(ix, body, (true, false), rc)
        }
        (
            Part::Block {
                range,
                live,
                first,
                last,
            },
            ItemKind::Work(entries),
        ) => {
            let actions = last.then(|| turn_actions_of(ix, rc)).flatten();
            let body = v_flex()
                .w_full()
                .when(rc.density.hover_actions(), |el| el.group(turn_group(ix)))
                .child(render_block(&entries[range.clone()], *live, rc))
                .when_some(actions, |el, stats| {
                    el.child(turn_actions(ix, item, stats, rc))
                })
                .into_any_element();
            row_frame(ix, body, (*first, *last), rc)
        }
        _ => render_item(ix, item, rc),
    }
}

/// Response actions of a turn's closing item. The running turn gets only
/// its change pill, once it changed something (desktop: the pill is there
/// while the turn works; copy, undo and fork wait until it settles).
fn turn_actions_of(ix: usize, rc: &RenderCtx) -> Option<(usize, usize, usize)> {
    rc.turn_end
        .get(&ix)
        .copied()
        .filter(|&(adds, dels, _)| !rc.live_turn(ix) || adds > 0 || dels > 0)
}

/// Context-compaction divider (desktop `SessionCompactionDivider`): a rule
/// with status, token counts and an expandable summary.
fn render_compaction(item: &Item, c: &Compaction, rc: &RenderCtx) -> AnyElement {
    let p = &rc.p;
    let running = c.status == CompactionStatus::InProgress;
    let (tone, mark): (Hsla, AnyElement) = match c.status {
        CompactionStatus::InProgress => (
            p.muted,
            kit::spinner(SharedString::from(format!("cspin-{}", c.id)), 13., p.accent)
                .into_any_element(),
        ),
        CompactionStatus::Failed => (
            p.danger,
            kit::icon(IconName::TriangleAlert, 13., p.danger).into_any_element(),
        ),
        CompactionStatus::Cancelled => (
            p.dim,
            kit::icon(IconName::FoldVertical, 13., p.dim).into_any_element(),
        ),
        CompactionStatus::Completed => (
            p.muted,
            kit::icon(IconName::FoldVertical, 13., p.muted).into_any_element(),
        ),
    };
    let has_summary = !c.summary.trim().is_empty();
    let open = has_summary && rc.expanded.contains(&item.key);
    let rule = || div().flex_1().h(px(1.)).bg(p.border);
    let (hover_fg, weak, key) = (p.text, rc.weak.clone(), item.key.clone());
    let header = h_flex()
        .w_full()
        .gap_2()
        .child(rule())
        .child(
            h_flex()
                .flex_none()
                .max_w(relative(0.8))
                .gap_1p5()
                .text_xs()
                .child(mark)
                .child(
                    div()
                        .font_semibold()
                        .text_color(if running { p.text } else { tone })
                        .child(c.title()),
                )
                .children(c.detail().map(|d| {
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(if c.status == CompactionStatus::Failed {
                            p.danger
                        } else {
                            p.dim
                        })
                        .child(d)
                }))
                .when(has_summary, |el| {
                    el.child(
                        h_flex()
                            .id(SharedString::from(format!("ctoggle-{}", item.key)))
                            .gap_0p5()
                            .px_1()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_color(p.dim)
                            .hover(move |s| s.text_color(hover_fg))
                            .child(if open { "Hide summary" } else { "Show summary" })
                            .child(kit::icon(
                                if open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                },
                                12.,
                                p.dim,
                            ))
                            .on_click(move |_, _, cx| toggle_expanded(&weak, key.clone(), cx)),
                    )
                }),
        )
        .child(rule());
    // Streaming summaries stay plain text until settled, like live replies.
    let summary = open.then(|| {
        let body = if running {
            div()
                .text_sm()
                .text_color(p.muted)
                .child(c.summary.clone())
                .into_any_element()
        } else {
            div()
                .text_sm()
                .child(
                    rc.md
                        .markdown(format!("cmd-{}", item.key), c.summary.clone(), p),
                )
                .into_any_element()
        };
        div()
            .mt_2()
            .px_4()
            .py_3()
            .rounded(px(10.))
            .bg(p.card)
            .child(body)
    });
    v_flex()
        .w_full()
        .py_1()
        .child(header)
        .children(summary)
        .into_any_element()
}

/// What the agent is doing right now, for the activity bar.
fn activity(view: &SessionView) -> (String, Option<String>) {
    // The runtime only knows the turn is busy; the compaction knows why.
    if view.active_compaction().is_some() {
        return (COMPACTING_LABEL.into(), None);
    }
    match view.items.last().map(|i| &i.kind) {
        Some(ItemKind::User {
            text, local: true, ..
        }) if is_compact_command(text) => (COMPACTING_LABEL.into(), None),
        Some(ItemKind::Work(entries)) => match entries.last() {
            Some(WorkEntry::Tool(call)) if call.running() => {
                let more = entries
                    .iter()
                    .filter(|e| matches!(e, WorkEntry::Tool(c) if c.running()))
                    .count();
                let preview = compact(&tool_preview(call, &view.cwd));
                let title = if more > 1 {
                    format!("{} (+{} active)", tool_label(call), more - 1)
                } else {
                    tool_label(call)
                };
                (title, (!preview.is_empty()).then_some(preview))
            }
            Some(WorkEntry::Thought(_)) => ("Thinking…".into(), None),
            _ => ("Working…".into(), None),
        },
        Some(ItemKind::Agent { .. }) => ("Responding…".into(), None),
        Some(ItemKind::User { local: true, .. }) => ("Sending prompt…".into(), None),
        _ => ("Working…".into(), None),
    }
}

fn segmented(
    id: &'static str,
    option: &SelectOption,
    pending: bool,
    p: &Palette,
    cx: &mut Context<SessionPane>,
) -> Div {
    let config_id = option.id.clone();
    kit::segment_bar(p).children(
        option
            .choices
            .iter()
            .enumerate()
            .map(|(ix, (value, label))| {
                let (config_id, value) = (config_id.clone(), value.clone());
                kit::segment((id, ix), option.current == value, p)
                    .flex_1()
                    .label(label.clone())
                    .inactive(pending)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_option(&config_id, &value, cx);
                        this.restore_focus(window, cx);
                    }))
            }),
    )
}

/// One usage column: segmented donut with the total (or the selected
/// category's exact count) in the centre, a caption, and a clickable color key.
fn usage_ring(
    chart: &usage::Chart,
    selected: Option<TokenKey>,
    pinned: Option<TokenKey>,
    p: &Palette,
    cx: &mut Context<SessionPane>,
) -> Div {
    const SIZE: f32 = 76.;
    let active = selected.and_then(|k| chart.segment(k));
    let segments: Vec<(f32, Hsla)> = chart
        .segments
        .iter()
        .map(|s| {
            let color: Hsla = rgb(s.key.color(p.dark)).into();
            let dimmed = active.is_some_and(|a| a.key != s.key);
            (s.share, if dimmed { color.opacity(0.25) } else { color })
        })
        .collect();
    let (value, exact) = match active {
        Some(a) => (
            a.count
                .map_or("Not reported".to_string(), usage::format_exact),
            true,
        ),
        None => (usage::format_tokens(chart.total), false),
    };
    let label = match active {
        Some(a) => a.key.short().to_string(),
        None if chart.complete => chart.label.to_string(),
        None => format!("Reported {}", chart.label.to_lowercase()),
    };
    let caption = match active {
        Some(a) if a.count.is_none() => "Count unavailable".to_string(),
        Some(a) if chart.complete && chart.total > 0 => {
            format!("{:.1}% of {}", a.share * 100., chart.label.to_lowercase())
        }
        Some(_) => "Reported count".to_string(),
        None => chart.takeaway.clone(),
    };
    let key = h_flex()
        .flex_wrap()
        .justify_center()
        .gap_0p5()
        .children(chart.segments.iter().map(|s| {
            let k = s.key;
            let on = pinned == Some(k);
            let hover = p.card_hover;
            let hint = k.hint();
            let color: Hsla = rgb(k.color(p.dark)).into();
            h_flex()
                .id(SharedString::from(format!("usage-key-{}", k.short())))
                .gap_1()
                .px_1()
                .py_0p5()
                .rounded(px(6.))
                .cursor_pointer()
                .text_xs()
                .text_color(if on { p.text } else { p.muted })
                .when(on, |el| el.bg(hover))
                .hover(move |s| s.bg(hover))
                .child(div().size(px(7.)).rounded_full().bg(color))
                .child(k.short())
                .tooltip(move |window, cx| kit::wrap_tip(hint).build(window, cx))
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    this.usage_hovered = hovered
                        .then_some(k)
                        .or(this.usage_hovered.filter(|h| *h != k));
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.usage_pinned == Some(k) {
                        this.usage_pinned = None;
                        this.usage_hovered = None;
                    } else {
                        this.usage_pinned = Some(k);
                    }
                    cx.notify();
                }))
        }));
    v_flex()
        .min_w_0()
        .items_center()
        .gap_1()
        .px_1()
        .child(
            div()
                .relative()
                .size(px(SIZE))
                .child(kit::donut(
                    segments,
                    SIZE,
                    8.,
                    p.card_hover,
                    !chart.complete,
                ))
                .child(
                    v_flex()
                        .absolute()
                        .inset_0()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .font_medium()
                                .text_color(p.text)
                                .when(exact, |el| el.text_size(px(11.)))
                                .when(!exact, |el| el.text_size(px(16.)))
                                .child(value),
                        )
                        .child(div().text_size(px(11.)).text_color(p.muted).child(label)),
                ),
        )
        .child(
            div()
                .h(px(16.))
                .text_size(px(11.))
                .font_family(kit::MONO)
                .text_color(p.dim)
                .truncate()
                .child(caption),
        )
        .child(key)
}

/// Strength of the composer's outline in the mode's tone. A starting value,
/// tuned by hand over a bright and a dark window (design D20).
const MODE_OUTLINE_STRENGTH: f32 = 0.45;

/// What a conversation runs with, as the quick panel's header shows it
/// (design D20).
#[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
pub(crate) struct RunLabels {
    /// The model's name.
    pub model: String,
    /// A model change is in flight.
    pub model_pending: bool,
    /// The agent lists models to choose from.
    pub can_pick_model: bool,
    /// The reasoning effort's name; `None` when the agent offers none.
    pub effort: Option<String>,
    /// The names of every effort the agent offers, so the effort's box can
    /// be as wide as the widest and changing it moves nothing.
    pub efforts: Vec<String>,
    /// An effort change is in flight.
    pub effort_pending: bool,
}

impl SessionPane {
    /// Tokens used and the context size of the session shown, as the usage
    /// ring draws them: what the agent reports, else the last request's
    /// tokens against the model list's size. `None` when the size is unknown.
    pub(crate) fn context_usage(&self, cx: &App) -> Option<(u64, u64)> {
        let session = self.view(cx)?;
        let model_context = self
            .store
            .read(cx)
            .session_model(session)
            .and_then(|m| m.context)
            .filter(|size| *size > 0);
        session
            .usage
            .or_else(|| session.last_used.zip(model_context))
            .filter(|(_, size)| *size > 0)
    }

    /// Tab / Shift+Tab in the session composer: next / previous agent mode.
    pub(super) fn cycle_session_mode(&mut self, back: bool, cx: &mut Context<Self>) {
        let Some(option) = self.view(cx).and_then(|v| {
            let key = |o: &SelectOption| {
                if o.id.is_empty() {
                    "mode".to_string()
                } else {
                    o.id.clone()
                }
            };
            v.mode_option()
                .filter(|o| !o.choices.is_empty() && !v.pending_options.contains(&key(o)))
        }) else {
            return;
        };
        let n = option.choices.len();
        let at = option
            .choices
            .iter()
            .position(|(v, _)| *v == option.current)
            .unwrap_or(0);
        let next = if back { (at + n - 1) % n } else { (at + 1) % n };
        let value = option.choices[next].0.clone();
        self.set_option(&option.id, &value, cx);
    }

    /// The shown session can take a prompt now: its agent is connected, it
    /// has loaded without error, and a running turn can be steered or queued.
    /// Typed prompts and palette commands share this check.
    pub(crate) fn session_ready(&self, cx: &App) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let open = session.read(cx);
        let view = &open.view;
        // Without a connection the prompt would be dropped: keep it in the
        // composer until the agent is back.
        let connected = self
            .store
            .read(cx)
            .runtime(&view.agent_id)
            .is_some_and(|rt| rt.is_connected());
        // A read-only session (archived, handed off) takes no prompts.
        connected
            && view.access.is_none()
            && !view.loading
            && view.error.is_none()
            && (!view.running() || open.can_control_run())
    }

    pub(crate) fn send_session_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt.read(cx).value().trim().to_string();
        if (text.is_empty() && self.attachments.is_empty()) || !self.session_ready(cx) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let attachments = std::mem::take(&mut self.attachments);
        self.attach_error = None;
        let delivery = self.delivery;
        self.store.update(cx, |s, cx| {
            s.send_prompt(&session, text, attachments, delivery, cx)
        });
        self.prompt
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
        self.transcript.list.set_follow_mode(FollowMode::Tail);
        self.transcript.list.scroll_to_end();
    }

    pub(super) fn flash_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied = Some(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied.as_deref() == Some(key.as_str()) {
                    this.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ask before rolling the workspace back to `message_id` (desktop
    /// "Undo workspace changes?" dialog).
    pub(super) fn request_undo(&mut self, message_id: String, cx: &mut Context<Self>) {
        let ok = self.session.as_ref().is_some_and(|session| {
            let open = session.read(cx);
            let v = &open.view;
            open.supports_undo()
                && !v.running()
                && !v.undo_pending
                && v.undo_affected_turns(&message_id).is_some()
        });
        if ok {
            cx.emit(SessionPaneEvent::ConfirmUndo(message_id));
        }
    }

    fn copy_turn(&mut self, ix: usize, key: String, cx: &mut Context<Self>) {
        let text = self.view(cx).map(|v| v.turn_text(ix)).unwrap_or_default();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.flash_copied(key, cx);
    }

    fn scroll_to_latest(&mut self, cx: &mut Context<Self>) {
        self.transcript.list.set_follow_mode(FollowMode::Tail);
        self.transcript.list.scroll_to_end();
        cx.notify();
    }

    /// Above the composer: MCP servers this session left out, with sign-in
    /// (desktop `session-mcp-auth-notice`). Hidden for read-only sessions.
    pub(super) fn mcp_notice(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (agent, cwd, oauth, skipped, skey) = {
            let store = self.store.read(cx);
            let view = self.view(cx).filter(|v| v.access.is_none())?;
            let (agent, cwd) = (view.agent_id.clone(), view.cwd.clone());
            let skey = SessionKey::new(&view.agent_id, &view.session_id);
            let skipped: Vec<_> = store
                .skipped_mcp(&skey)
                .into_iter()
                .map(|s| {
                    let signed_in = store.mcp_signed_in(&agent, &s.provider);
                    let busy = store.is_pending(&agent, &format!("oauth:{}", s.provider));
                    (s, signed_in, busy)
                })
                .collect();
            let oauth = store.runtime(&agent).and_then(|rt| rt.oauth.clone());
            (agent, cwd, oauth, skipped, skey)
        };
        if skipped.is_empty() {
            return None;
        }
        let rows: Vec<AnyElement> = skipped
            .iter()
            .map(|(server, signed_in, busy)| {
                let (signed_in, busy) = (*signed_in, *busy);
                let key: SharedString = server.server.clone().into();
                let signing = oauth.as_ref().filter(|f| f.provider == server.provider);
                let text = if signed_in {
                    format!(
                        "Signed in to \"{}\". Start a new session to use its tools.",
                        server.server
                    )
                } else {
                    server.describe()
                };
                let tone = if server.error.is_some() {
                    p.warn
                } else {
                    p.accent
                };
                let actions = h_flex()
                    .flex_none()
                    .gap_1p5()
                    .when(signed_in, |el| {
                        let (agent, cwd) = (agent.clone(), cwd.clone());
                        el.child(
                            accent_btn(kit::keyed("mcp-notice-new", key.clone()), true, true, p)
                                .child("New session")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SessionPaneEvent::NewSessionIn {
                                        cwd: cwd.clone(),
                                        agent: agent.clone(),
                                    })
                                })),
                        )
                    })
                    .when(!signed_in, |el| {
                        let enabled = signing.is_none() && oauth.is_none() && !busy;
                        let (agent, provider) = (agent.clone(), server.provider.clone());
                        let selector = format!("mcp-notice-signin-{}", server.server);
                        el.child(
                            flat_btn(
                                kit::keyed("mcp-notice-signin", key.clone()),
                                enabled,
                                true,
                                p,
                            )
                            .debug_selector(move || selector)
                            .child(kit::icon(IconName::LogIn, 14., p.text))
                            .child(if server.error.is_some() {
                                "Retry sign-in"
                            } else {
                                "Sign in"
                            })
                            .when(enabled, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| {
                                    this.store
                                        .update(cx, |s, cx| s.start_oauth(&agent, &provider, cx))
                                }))
                            }),
                        )
                        .when(server.error.is_some(), |el| {
                            el.child(
                                flat_btn(
                                    kit::keyed("mcp-notice-settings", key.clone()),
                                    true,
                                    true,
                                    p,
                                )
                                .child("Settings")
                                .on_click(cx.listener(
                                    |_, _, _, cx| cx.emit(SessionPaneEvent::ConnectorSettings),
                                )),
                            )
                        })
                    })
                    .child({
                        let name = server.server.clone();
                        let skey = skey.clone();
                        kit::icon_button(
                            kit::keyed("mcp-notice-dismiss", key.clone()),
                            IconName::X,
                            "Dismiss",
                        )
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |s, cx| s.dismiss_skipped_mcp(&skey, &name, cx))
                        }))
                    });
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2p5()
                            .child(kit::icon(
                                if signed_in {
                                    IconName::CircleCheck
                                } else {
                                    IconName::Plug
                                },
                                15.,
                                if signed_in { p.success } else { tone },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .text_color(p.text)
                                    .child(text),
                            )
                            .child(actions),
                    )
                    .children(signing.map(|flow| {
                        oauth_flow_box(&self.store, &self.oauth_code, &agent, flow, key.clone(), p)
                    }))
                    .into_any_element()
            })
            .collect();
        Some(
            v_flex()
                .id("mcp-notice")
                .debug_selector(|| "mcp-notice".into())
                .w_full()
                .gap_2()
                .px_3()
                .py_2p5()
                .rounded(px(12.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .children(rows)
                .into_any_element(),
        )
    }

    /// "Configure · N" of Session options, when the session has delegates.
    pub(super) fn delegate_option_row(&self, p: &Palette, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        let skey = self.key(cx)?;
        if !store.supports_delegate_routing(skey.agent.as_str()) {
            return None;
        }
        let state = store.delegate_state(&skey).cloned().unwrap_or_default();
        if !state.has_routes() {
            return None;
        }
        let count = state.routes.as_ref().map_or(0, |r| r.assignments.len());
        Some(
            h_flex()
                .gap_2()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().child("Delegate routing"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("Model per delegate."),
                        ),
                )
                .child(
                    kit::flat_btn("open-delegates", !state.loading, true, p)
                        .child(if state.loading {
                            kit::spinner("delegates-row-spin", 13., p.muted).into_any_element()
                        } else {
                            kit::icon(IconName::Waypoints, 14., p.text).into_any_element()
                        })
                        .child(if state.error.is_some() {
                            "Retry".to_string()
                        } else if count > 0 {
                            format!("Configure · {count}")
                        } else {
                            "Configure".to_string()
                        })
                        .when(!state.loading, |el| {
                            el.on_click(
                                cx.listener(|_, _, _, cx| cx.emit(SessionPaneEvent::Delegates)),
                            )
                        }),
                ),
        )
    }

    /// The composer's model pill: the chosen model (with the mesh icon when
    /// another machine serves it), the reasoning badge, and the picker on click.
    fn model_picker_button(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let view = self.view(cx);
        let has_models = view.is_some_and(|v| !store.models(&v.agent_id).is_empty());
        let model = view.and_then(|v| store.session_model(v)).cloned();
        let label = model
            .as_ref()
            .map(|m| m.label.clone())
            .or_else(|| view.and_then(|v| v.model_label()))
            .unwrap_or_else(|| {
                if has_models {
                    "Choose model"
                } else {
                    "Model unknown"
                }
                .into()
            });
        let reasoning = view
            .and_then(|v| v.reasoning_option())
            .map(|r| (r.current_label(), r.current))
            .filter(|(label, _)| !label.is_empty());
        let pending = view.is_some_and(|v| v.pending_options.contains("model"));
        let tip = model.as_ref().map(Model::describe);
        let button =
            super::start::model_toolbar_button("s-model", label, model.as_ref(), pending, p)
                .when_some(tip, |el, tip| {
                    el.tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                })
                .when(has_models && !pending, |el| {
                    el.on_click(cx.listener(|_, _, _, cx| cx.emit(SessionPaneEvent::ModelPicker)))
                });
        match reasoning {
            Some((text, level)) => {
                badged_tone(button, &text, super::start::reasoning_color(&level, p), p)
                    .into_any_element()
            }
            None => button.into_any_element(),
        }
    }

    /// What the session shown runs with, as text for the quick panel's header
    /// (design D20): the model's name and, when the agent offers a reasoning
    /// option, the effort's name. `None` without a session.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn run_labels(&self, cx: &App) -> Option<RunLabels> {
        let store = self.store.read(cx);
        let view = self.view(cx)?;
        let model = store
            .session_model(view)
            .map(|m| m.label.clone())
            .or_else(|| view.model_label())
            .unwrap_or_else(|| "Model".into());
        let reasoning = view.reasoning_option().filter(|o| !o.choices.is_empty());
        let effort = reasoning
            .as_ref()
            .map(|o| (o.current_label(), view.pending_options.contains(&o.id)))
            .filter(|(label, _)| !label.is_empty());
        let efforts = reasoning
            .map(|o| o.choices.into_iter().map(|(_, label)| label).collect())
            .unwrap_or_default();
        Some(RunLabels {
            model,
            model_pending: view.pending_options.contains("model"),
            can_pick_model: !store.models(&view.agent_id).is_empty(),
            effort: effort.as_ref().map(|(label, _)| label.clone()),
            efforts,
            effort_pending: effort.is_some_and(|(_, pending)| pending),
        })
    }

    /// The agent mode of the compact composer, as its name in text: a box as
    /// wide as the widest of the agent's mode names (the other names are laid
    /// out at no height beside the shown one), so changing the mode moves
    /// nothing. Click moves to the next mode, Shift+click to the previous one,
    /// like Tab and Shift+Tab. Dimmed and inert while a change is pending.
    fn mode_name(&self, option: &SelectOption, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let key = if option.id.is_empty() {
            "mode"
        } else {
            option.id.as_str()
        };
        let pending = self
            .view(cx)
            .is_some_and(|v| v.pending_options.contains(key));
        let label = option.current_label();
        let tip = format!("Agent mode: {label}");
        let selector = format!("composer-mode:{label}");
        let (chip, text) = (p.chip, p.text);
        let others = option
            .choices
            .iter()
            .filter(|(v, _)| *v != option.current)
            .map(|(_, name)| div().h_0().overflow_hidden().child(name.clone()))
            .collect::<Vec<_>>();
        h_flex()
            .id("s-mode")
            .debug_selector(|| "composer-mode".into())
            .flex_none()
            .h(px(24.))
            .px_2()
            .items_center()
            .rounded(px(6.))
            .text_xs()
            .text_color(p.muted)
            .cursor_pointer()
            .when(pending, |el| el.opacity(0.6))
            .hover(move |s| s.bg(chip).text_color(text))
            .child(
                v_flex()
                    .child(
                        div()
                            .whitespace_nowrap()
                            .debug_selector(move || selector.clone())
                            .child(label),
                    )
                    .children(others.into_iter().map(|d| d.whitespace_nowrap())),
            )
            .tooltip(move |window, cx| kit::tip_with_stroke(tip.clone(), "tab").build(window, cx))
            .when(!pending, |el| {
                el.on_click(cx.listener(|this, _, window, cx| {
                    this.cycle_session_mode(window.modifiers().shift, cx)
                }))
            })
            .into_any_element()
    }

    // ---- header -------------------------------------------------------------

    /// What the open session contributes to the title bar: `(title, actions)`.
    /// Title is the centred group (back, status, title); the actions (details,
    /// menu, refresh) sit where the global connection status used to be.
    pub(super) fn session_chrome(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, Vec<AnyElement>)> {
        let (view, summary, plan) = {
            let store = self.store.read(cx);
            let session = self.view(cx)?;
            let view = session.shallow();
            let summary = store
                .session_summary(&view.agent_id, &view.session_id)
                .cloned();
            (view, summary, session.plan.clone())
        };
        let summary = summary.as_ref();
        let running = view.running();
        // A delegated session opened before the list knew it has no title
        // yet; take the list's once it refreshes.
        let title = if !view.title.trim().is_empty() {
            view.title.clone()
        } else {
            summary.map_or_else(|| "Untitled session".to_string(), |s| s.display_title())
        };
        let (status_label, status_color) = if view.loading {
            ("Loading", p.status_muted)
        } else if view.error.is_some() {
            ("Failed", p.danger)
        } else if running {
            ("Working", p.running)
        } else {
            ("Ready", p.success)
        };
        let usage = self.context_usage(cx);
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());

        let back = kit::title_icon_button(
            "session-back",
            IconName::ArrowLeft,
            if !self.back.is_empty() {
                "Back to parent session"
            } else {
                "Back to sessions"
            },
            p,
        )
        .on_click(cx.listener(|this, _, _, cx| this.go_back(cx)));
        // Copy feedback follows the desktop's copy chips: for 1.2s the chip shows
        // an accent check + "copied" over itself and the tooltip says "Copied".
        let copied = |key: &str| self.copied.as_deref() == Some(key);
        // Workspace prefix: dim text, click copies the full path (shown on hover).
        let workspace = {
            let (path, hover_bg, hover_fg) = (view.cwd.clone(), p.card, p.text);
            let done = copied(COPIED_TITLE_PATH);
            let tip = (!done).then(|| path.clone());
            let dim = p.dim;
            div()
                .id("title-workspace")
                .relative()
                // Shrinks with the title (down to a few letters) on narrow windows.
                .flex_shrink(1.)
                .min_w(px(28.))
                .max_w(px(180.))
                .px_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_sm()
                .text_color(p.dim)
                .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
                .child(div().truncate().child(workspace_name(&view.cwd)))
                .when(done, |el| el.child(copied_badge(p)))
                .tooltip(move |window, cx| {
                    copy_tooltip(tip.clone(), "Click to copy path", dim, false).build(window, cx)
                })
                .on_mouse_down(MouseButton::Left, kit::claim_press)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    this.flash_copied(COPIED_TITLE_PATH.into(), cx);
                }))
        };
        // Session ID: a small dim icon after the title; hover shows the ID, click
        // copies it and swaps the icon for an accent check.
        let id_button = {
            let sid = view.session_id.clone();
            let done = copied(COPIED_TITLE_ID);
            let tip = (!done).then(|| sid.clone());
            let dim = p.dim;
            kit::title_chip("title-session-id", p)
                .child(if done {
                    kit::icon(IconName::Check, kit::TITLE_ICON, p.accent)
                } else {
                    kit::icon(
                        IconName::FingerprintPattern,
                        kit::TITLE_ICON,
                        p.dim.opacity(0.8),
                    )
                })
                .tooltip(move |window, cx| {
                    copy_tooltip(tip.clone(), "Click to copy session ID", dim, true)
                        .build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sid.clone()));
                    this.flash_copied(COPIED_TITLE_ID.into(), cx);
                }))
        };
        let title = h_flex()
            .gap_1p5()
            .min_w_0()
            .child(back)
            .child(
                div()
                    .id("status-dot")
                    .flex_none()
                    .tooltip(move |window, cx| kit::wrap_tip(status_label).build(window, cx))
                    .child(if running {
                        kit::spinner("hdr-running", 12., status_color).into_any_element()
                    } else {
                        kit::dot(status_color).into_any_element()
                    }),
            )
            .when(!view.cwd.is_empty(), |el| {
                el.child(workspace).child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(p.dim.opacity(0.6))
                        .child("/"),
                )
            })
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(px(24.))
                    .text_sm()
                    .font_semibold()
                    .truncate()
                    .child(title),
            )
            .child(id_button)
            .into_any_element();

        let details_trigger = {
            kit::title_chip("session-details", p)
                .when(self.details_open, |el| el.bg(p.card))
                .when(!self.details_open, |el| {
                    el.tooltip(move |window, cx| {
                        let text = match usage {
                            Some((used, size)) => {
                                format!("Session details\n{}", kit::context_tip(used, size))
                            }
                            None => "Session details".to_string(),
                        };
                        kit::wrap_tip(text).build(window, cx)
                    })
                })
                .child(
                    usage
                        .and_then(|(used, size)| kit::context_ring(used, size, p))
                        .map_or_else(
                            || {
                                kit::icon(IconName::Info, kit::TITLE_ICON, p.muted)
                                    .into_any_element()
                            },
                            IntoElement::into_any_element,
                        ),
                )
                .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                    if this.dismissed.closed_by("session-details", ev) {
                        return;
                    }
                    this.details_open = !this.details_open;
                    this.menu_open = false;
                    this.plan_open = false;
                    cx.notify();
                }))
        };
        // Usage widget, ported from the desktop app's `SessionUsageBar`: context
        // meter, latest-request / session-total token rings, cost, active time.
        let details_panel = (self.details_open && self.density.has_panels()).then(|| {
            let mono = |el: Div| el.font_family(kit::MONO);
            let section_rule = || kit::divider(p).bg(p.border.opacity(0.6));

            // ---- worktree (desktop details list: branch, checkout, baseline, state)
            let worktree = summary.and_then(|s| s.worktree.clone()).map(|w| {
                let row = |id: &'static str, label: &'static str, value: AnyElement| {
                    h_flex()
                        .id(id)
                        .gap_3()
                        .py_0p5()
                        .text_xs()
                        .child(div().flex_none().text_color(p.muted).child(label))
                        .child(h_flex().flex_1().min_w_0().justify_end().child(value))
                };
                let text = |s: String, color: Hsla| {
                    mono(div()).min_w_0().truncate().text_color(color).child(s).into_any_element()
                };
                let usable = w.lifecycle.as_deref().is_none_or(|l| l == "ready");
                let checkout = w.checkout_path.clone().filter(|c| !c.is_empty()).map(|path| {
                    let done = self.copied.as_deref() == Some(COPIED_DETAILS_CHECKOUT);
                    let (hover, tip) = (p.card_hover, path.clone());
                    h_flex()
                        .id("details-checkout")
                        .debug_selector(|| "details-checkout".into())
                        .min_w_0()
                        .gap_1()
                        .px_1()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(text(path.clone(), p.text))
                        .child(kit::icon(
                            if done { IconName::Check } else { IconName::Copy },
                            12.,
                            if done { p.success } else { p.dim },
                        ))
                        .tooltip(move |window, cx| {
                            let tip = if done { "Copied".to_string() } else { format!("Copy checkout path\n{tip}") };
                            kit::wrap_tip(tip).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            this.flash_copied(COPIED_DETAILS_CHECKOUT.into(), cx);
                        }))
                        .into_any_element()
                });
                v_flex()
                    .gap_0p5()
                    .child(row(
                        "wt-branch",
                        "Worktree branch",
                        text(w.branch.clone().unwrap_or_else(|| "Unknown".into()), p.text),
                    ))
                    .children(checkout.map(|c| row("wt-checkout", "Checkout", c)))
                    .children(w.baseline_mode.clone().map(|b| {
                        row("wt-baseline", "Baseline", text(b.replace('_', " "), p.text))
                    }))
                    .children(w.lifecycle.clone().map(|l| {
                        row(
                            "wt-state",
                            "Worktree state",
                            text(l, if usable { p.text } else { p.warn }),
                        )
                    }))
                    .when(w.handed_off_to.is_some(), |el| {
                        el.child(row("wt-handed-off", "Handed off", text("Yes".into(), p.text)))
                    })
            });

            // ---- context
            let context = usage.map(|(used, size)| {
                let exact = format!("{} / {} context tokens", usage::format_exact(used), if size > 0 { usage::format_exact(size) } else { "unknown".into() });
                let row = h_flex().id("usage-context").gap_2().text_xs().text_color(p.muted).child(mono(div()).child("Context"));
                let row = if size > 0 {
                    let frac = (used as f32 / size as f32).clamp(0., 1.);
                    row.child(
                        div().flex_1().min_w(px(16.)).h(px(5.)).rounded_full().bg(p.card_hover).child(
                            div().h_full().rounded_full().w(relative(frac.max(0.01))).bg(p.accent),
                        ),
                    )
                    .child(mono(div()).font_semibold().text_color(p.text).child(format!("{:.0}%", frac * 100.)))
                    .child(mono(div()).child(format!("{} free", usage::format_tokens(size.saturating_sub(used)))))
                } else {
                    row.child(div().flex_1()).child(mono(div()).font_semibold().text_color(p.text).child(format!("{} tokens", usage::format_tokens(used))))
                };
                row.tooltip(move |window, cx| kit::wrap_tip(exact.clone()).build(window, cx))
            });

            // ---- token rings
            let breakdown = view.breakdown;
            let want_totals = self.usage_totals;
            let (has_latest, has_totals) = (breakdown.is_some_and(|b| b.latest.is_some()), breakdown.is_some_and(|b| b.totals.is_some()));
            let totals = (want_totals && has_totals) || !has_latest;
            let counts = breakdown.and_then(|b| if totals { b.totals } else { b.latest });
            let partial = totals && breakdown.is_some_and(|b| b.partial);
            let selected = self.usage_hovered.or(self.usage_pinned);
            let pinned = self.usage_pinned;
            let help = || {
                div()
                    .id("usage-help")
                    .flex_none()
                    .cursor_pointer()
                    .child(kit::icon(IconName::Info, 13., p.muted))
                    .tooltip(|window, cx| {
                        kit::wrap_tip(
                            "Request token usage is separate from current context occupancy. Cache reads reuse a prefix; cache writes create a cache entry. Reported zeros do not establish provider support. Select a category to inspect its exact count.",
                        )
                        .build(window, cx)
                    })
            };
            let breakdown_view = counts.map(|counts| {
                let scope_button = |id: &'static str, label: &'static str, on: bool, enabled: bool, to_totals: bool| {
                    let hover = p.card_hover;
                    div()
                        .id(id)
                        .px_2()
                        .py_1()
                        .rounded(px(6.))
                        .text_xs()
                        .when(on, |el| el.bg(p.card_hover).text_color(p.text).shadow_sm())
                        .when(!on, |el| el.text_color(p.muted))
                        .when(enabled && !on, |el| el.cursor_pointer().hover(move |s| s.bg(hover.opacity(0.5))))
                        .when(!enabled, |el| el.opacity(0.5))
                        .child(label)
                        .when(enabled, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                this.usage_totals = to_totals;
                                this.usage_pinned = None;
                                this.usage_hovered = None;
                                cx.notify();
                            }))
                        })
                };
                let heading = h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .p_0p5()
                            .gap_0p5()
                            .rounded(px(8.))
                            .bg(p.bg)
                            .border_1()
                            .border_color(p.border.opacity(0.6))
                            .child(scope_button("usage-latest", "Latest request", !totals, has_latest, false))
                            .child(scope_button("usage-totals", "Session totals", totals, has_totals, true)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when(partial, |el| {
                                el.child(
                                    div()
                                        .id("usage-partial")
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(p.warn)
                                        .child("Partial")
                                        .tooltip(|window, cx| kit::wrap_tip("Some requests did not report complete usage").build(window, cx)),
                                )
                            })
                            .child(help()),
                    );
                let [input, output] = usage::charts(&counts, partial);
                let columns = h_flex()
                    .items_start()
                    .child(usage_ring(&input, selected, pinned, p, cx).flex_1())
                    .child(div().w(px(1.)).self_stretch().bg(p.border.opacity(0.6)))
                    .child(usage_ring(&output, selected, pinned, p, cx).flex_1());
                v_flex().gap_2().child(heading).child(columns)
            });

            // ---- cost + activity
            let cost = usage::cost_lines(view.accounting.as_ref(), None);
            let line = |id: SharedString, label: SharedString, value: AnyElement, title: SharedString| {
                h_flex()
                    .id(id)
                    .justify_between()
                    .gap_3()
                    .py_1()
                    .text_xs()
                    .child(div().text_color(p.muted).child(label))
                    .child(value)
                    .tooltip(move |window, cx| kit::wrap_tip(title.clone()).build(window, cx))
            };
            let cost_rows: Vec<_> = cost
                .into_iter()
                .enumerate()
                .map(|(ix, c)| {
                    let color = match c.tone {
                        usage::Tone::Warn => p.warn,
                        usage::Tone::Muted => p.muted,
                        usage::Tone::Normal => p.text,
                    };
                    let value = mono(h_flex())
                        .font_semibold()
                        .text_color(color)
                        .children(c.prefix.map(|pre| div().text_color(p.dim).mr_0p5().child(pre)))
                        .child(c.value)
                        .into_any_element();
                    line(SharedString::from(format!("usage-cost-{ix}")), c.label.into(), value, c.title.into())
                })
                .collect();
            let live = view.active_started_at;
            let active_value = mono(h_flex())
                .gap_1p5()
                .font_semibold()
                .text_color(p.text)
                .when(live.is_some(), |el| el.child(kit::dot(p.running).size(px(6.))))
                .child(kit::live_duration("usage-active", view.active_work_ms, live))
                .when(counts.is_none(), |el| el.child(help()))
                .into_any_element();

            v_flex()
                .id("session-details-panel")
                .absolute()
                .top(px(32.))
                .right_0()
                .w(px(400.))
                .p_3()
                .gap_2p5()
                .rounded(px(12.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.dismissed.record("session-details", ev);
                    this.details_open = false;
                    this.usage_hovered = None;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_size(px(11.)).font_semibold().text_color(p.dim).child("SESSION"))
                        .child(h_flex().gap_1p5().text_xs().font_medium().text_color(p.text).child(kit::dot(status_color)).child(status_label)),
                )
                .when_some(worktree, |el, w| el.child(w).child(section_rule()))
                .children(context)
                .children(breakdown_view)
                .when(!cost_rows.is_empty(), |el| el.child(section_rule()).children(cost_rows))
                .child(section_rule())
                .child(line("usage-active-row".into(), "Active".into(), active_value, "Time spent actively processing prompts".into()))
        });

        let menu_trigger = kit::title_icon_button(
            "session-menu",
            IconName::EllipsisVertical,
            "Session actions",
            p,
        )
        .when(self.menu_open, |el| el.bg(p.card))
        .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
            if this.dismissed.closed_by("session-menu", ev) {
                return;
            }
            this.menu_open = !this.menu_open;
            this.details_open = false;
            this.plan_open = false;
            cx.notify();
        }));
        let menu_panel = self.menu_open.then(|| {
            let (can_create_worktree, can_handoff_worktree, developer) = {
                let store = self.store.read(cx);
                let skey = SessionKey::new(&agent_id, &session_id);
                let busy = store.worktree_pending(&skey);
                (
                    !busy && store.can_create_worktree(&skey, cx),
                    !busy && store.can_handoff_worktree(&skey, cx),
                    store.prefs.developer_mode,
                )
            };
            let (debug_agent, debug_session) = (agent_id.clone(), session_id.clone());
            let (pinned, resolved, archived) = summary
                .map(|s| (s.pinned, s.resolved, s.archived))
                .unwrap_or_default();
            let item = |id: &'static str, icon: IconName, label: String| {
                kit::menu_item(id, icon, label, p)
            };
            let (a1, s1) = (agent_id.clone(), session_id.clone());
            let (a2, s2) = (agent_id.clone(), session_id.clone());
            let (a3, s3) = (agent_id.clone(), session_id.clone());
            let (cwd, sid) = (view.cwd.clone(), session_id.clone());
            let checkout = summary
                .and_then(|s| s.worktree.as_ref())
                .and_then(|w| w.checkout_path.clone())
                .filter(|c| !c.is_empty());
            v_flex()
                .id("session-menu-panel")
                .absolute()
                .top(px(32.))
                .right_0()
                .w(px(220.))
                .p_1()
                .rounded(px(12.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    this.dismissed.record("session-menu", ev);
                    this.menu_open = false;
                    cx.notify();
                }))
                .child(
                    item(
                        "m-pin",
                        IconName::Pin,
                        if pinned { "Unpin" } else { "Pin" }.into(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu_open = false;
                        this.store
                            .update(cx, |s, cx| s.set_pinned(&a1, &s1, !pinned, cx));
                    })),
                )
                .child(
                    item(
                        "m-resolve",
                        IconName::CircleCheck,
                        if resolved { "Reopen" } else { "Mark resolved" }.into(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu_open = false;
                        this.store.update(cx, |s, cx| {
                            s.set_lifecycle(&a2, &s2, Some(!resolved), None, cx)
                        });
                    })),
                )
                .child(
                    item(
                        "m-archive",
                        IconName::Archive,
                        if archived { "Unarchive" } else { "Archive" }.into(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu_open = false;
                        this.store.update(cx, |s, cx| {
                            s.set_lifecycle(&a3, &s3, None, Some(!archived), cx)
                        });
                    })),
                )
                .when(can_create_worktree || can_handoff_worktree, |el| {
                    el.child(div().my_1().child(kit::divider(p)))
                })
                .when(can_create_worktree, |el| {
                    el.child(
                        item(
                            "m-worktree-new",
                            IconName::GitBranchPlus,
                            "New worktree session".into(),
                        )
                        .on_click(cx.listener(|_this, _, _window, cx| {
                            cx.emit(SessionPaneEvent::Worktree(WorktreeAction::Create))
                        })),
                    )
                })
                .when(can_handoff_worktree, |el| {
                    el.child(
                        item(
                            "m-worktree-handoff",
                            IconName::GitBranch,
                            "Hand off worktree".into(),
                        )
                        .on_click(cx.listener(|_this, _, _window, cx| {
                            cx.emit(SessionPaneEvent::Worktree(WorktreeAction::Handoff))
                        })),
                    )
                })
                .child(div().my_1().child(kit::divider(p)))
                // Like the desktop menu: copy items stay open and read "Copied" for a moment.
                .when(!cwd.is_empty(), |el| {
                    let done = self.copied.as_deref() == Some(COPIED_MENU_PATH);
                    el.child(
                        item(
                            "m-copy-path",
                            if done {
                                IconName::Check
                            } else {
                                IconName::FolderOpen
                            },
                            if done { "Copied" } else { "Copy project path" }.into(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(cwd.clone()));
                            this.flash_copied(COPIED_MENU_PATH.into(), cx);
                        })),
                    )
                })
                .when_some(checkout, |el, checkout| {
                    let done = self.copied.as_deref() == Some(COPIED_MENU_CHECKOUT);
                    el.child(
                        item(
                            "m-copy-checkout",
                            if done {
                                IconName::Check
                            } else {
                                IconName::GitBranch
                            },
                            if done { "Copied" } else { "Copy checkout path" }.into(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(checkout.clone()));
                            this.flash_copied(COPIED_MENU_CHECKOUT.into(), cx);
                        })),
                    )
                })
                .child({
                    let done = self.copied.as_deref() == Some(COPIED_MENU_ID);
                    item(
                        "m-copy-id",
                        if done {
                            IconName::Check
                        } else {
                            IconName::FingerprintPattern
                        },
                        if done { "Copied" } else { "Copy session ID" }.into(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(sid.clone()));
                        this.flash_copied(COPIED_MENU_ID.into(), cx);
                    }))
                })
                // Developer mode: the session's protocol traffic, full size.
                .when(developer, |el| {
                    el.child(div().my_1().child(kit::divider(p))).child(
                        item("m-debug-events", IconName::Bug, "Debug events…".into())
                            .debug_selector(|| "m-debug-events".into())
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SessionPaneEvent::DebugEvents {
                                    agent: debug_agent.clone(),
                                    session: debug_session.clone(),
                                })
                            })),
                    )
                })
        });

        // Plan: icon + `done/total`, items in a popover.
        let plan_el = crate::session::PlanDigest::from_entries(&plan).map(|digest| {
            let (done, total) = (digest.done, digest.total);
            let open = self.plan_open;
            let current = digest.current().map(|e| e.content.clone());
            let hover = p.card;
            let trigger = h_flex()
                .id("plan-toggle")
                .flex_none()
                .h(px(kit::TITLE_BTN))
                .px_1p5()
                .gap_1()
                .rounded(px(6.))
                .cursor_pointer()
                .text_xs()
                .text_color(p.muted)
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, kit::claim_press)
                .when(open, |el| el.bg(p.card))
                .child(kit::icon(
                    IconName::ListChecks,
                    16.,
                    if done == total { p.success } else { p.accent },
                ))
                .child(
                    div()
                        .font_family(kit::MONO)
                        .child(format!("{done}/{total}")),
                )
                .when(!open, |el| {
                    el.tooltip(move |window, cx| {
                        let text = match &current {
                            Some(c) => format!("Plan · {done}/{total}\n{c}"),
                            None => format!("Plan · {done}/{total}"),
                        };
                        kit::wrap_tip(text).build(window, cx)
                    })
                })
                .on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                    if this.dismissed.closed_by("plan", ev) {
                        return;
                    }
                    this.plan_open = !this.plan_open;
                    this.details_open = false;
                    this.menu_open = false;
                    cx.notify();
                }));
            let panel = open.then(|| {
                v_flex()
                    .id("plan-panel")
                    .absolute()
                    .top(px(32.))
                    .right_0()
                    .w(px(380.))
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .p_3()
                    .gap_1p5()
                    .rounded(px(12.))
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.dismissed.record("plan", ev);
                        this.plan_open = false;
                        cx.notify();
                    }))
                    .child(
                        h_flex()
                            .justify_between()
                            .pb_1()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_semibold()
                                    .text_color(p.dim)
                                    .child("PLAN"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(kit::MONO)
                                    .text_color(p.muted)
                                    .child(format!("{done}/{total}")),
                            ),
                    )
                    .children(plan.iter().map(|e| {
                        let (icon, color) = kit::plan_entry_mark(&e.status, p);
                        h_flex()
                            .items_start()
                            .gap_2()
                            .text_xs()
                            .child(div().pt(px(2.)).child(kit::icon(icon, 12., color)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .when(e.status == "completed", |el| {
                                        el.text_color(p.dim).line_through()
                                    })
                                    .when(e.status == "in_progress", |el| {
                                        el.text_color(p.text).font_medium()
                                    })
                                    .when(
                                        e.status != "completed" && e.status != "in_progress",
                                        |el| el.text_color(p.muted),
                                    )
                                    .child(e.content.clone()),
                            )
                    }))
                    .map(|el| self.smooth_scroll.region("plan-panel", el))
            });
            div()
                .relative()
                .flex_none()
                .child(trigger)
                .children(panel.map(|p| deferred(p).with_priority(1)))
                .into_any_element()
        });

        // The context ring and the details panel are the main window's.
        let details = self.density.has_panels().then(|| {
            div()
                .relative()
                .flex_none()
                .child(details_trigger)
                .children(details_panel.map(|p| deferred(p).with_priority(1)))
                .into_any_element()
        });
        let right = plan_el
            .into_iter()
            .chain(details)
            .chain([
                div()
                    .relative()
                    .flex_none()
                    .child(menu_trigger)
                    .children(menu_panel.map(|p| deferred(p).with_priority(1)))
                    .into_any_element(),
                kit::title_icon_button(
                    "session-refresh",
                    IconName::RefreshCw,
                    "Refresh session",
                    p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.reload(cx);
                    this.transcript.list.set_follow_mode(FollowMode::Tail);
                }))
                .into_any_element(),
            ])
            .collect();
        Some((title, right))
    }

    // ---- composer -----------------------------------------------------------

    fn session_options_panel(
        &self,
        mode: Option<&SelectOption>,
        control: bool,
        pending: &HashSet<String>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let section = |title: &'static str| {
            div()
                .text_xs()
                .font_semibold()
                .text_color(p.dim)
                .child(title)
        };
        let delivery = self.delivery;
        v_flex()
            .id("session-options-panel")
            .absolute()
            .bottom(px(40.))
            .left_0()
            .w(px(340.))
            .p_3()
            .gap_2()
            .rounded(px(12.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .occlude()
            .on_mouse_down_out(cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                this.dismissed.record("session-options", ev);
                this.options_open = false;
                this.restore_focus(window, cx);
                cx.notify();
            }))
            .child(div().text_sm().font_semibold().child("Session options"))
            .when_some(mode, |el, m| {
                let key = if m.id.is_empty() {
                    "mode".to_string()
                } else {
                    m.id.clone()
                };
                el.child(section("Mode")).child(segmented(
                    "s-mode",
                    m,
                    pending.contains(&key),
                    p,
                    cx,
                ))
            })
            .when(control, |el| {
                el.child(div().pt_1().child(kit::divider(p)))
                    .child(section("Message delivery"))
                    .child(
                        kit::segment_bar(p)
                            .child(
                                kit::segment("dl-steer", delivery == Delivery::Steer, p)
                                    .flex_1()
                                    .icon(IconName::Navigation)
                                    .label("Steer")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delivery = Delivery::Steer;
                                        this.restore_focus(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                kit::segment("dl-queue", delivery == Delivery::Queue, p)
                                    .flex_1()
                                    .icon(IconName::Plus)
                                    .label("Queue")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delivery = Delivery::Queue;
                                        this.restore_focus(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().text_xs().text_color(p.muted).child(
                        if delivery == Delivery::Queue {
                            "Queued messages are delivered after the current turn finishes."
                        } else {
                            "Steering interrupts the current turn with your message."
                        },
                    ))
            })
            .children(self.delegate_option_row(p, cx).map(|row| {
                v_flex()
                    .gap_2()
                    .child(div().pt_1().child(kit::divider(p)))
                    .child(row)
            }))
            .when(
                mode.is_none() && !control && self.delegate_option_row(p, cx).is_none(),
                |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(p.muted)
                            .child("This agent exposes no session options."),
                    )
                },
            )
    }

    fn session_composer(
        &mut self,
        view: &SessionView,
        control: bool,
        p: &Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let slash_list = self
            .slash_list(window, cx)
            .map(|list| self.render_slash_list(&list, p, cx));
        let slash_hint = self
            .slash_hint(cx)
            .map(|(name, hint)| self.render_slash_hint(name, hint, p));
        let running = view.running();
        let loading = view.loading;
        let has_text = !self.prompt.read(cx).value().trim().is_empty();
        let has_draft = has_text || !self.attachments.is_empty();
        let connected = self
            .store
            .read(cx)
            .runtime(&view.agent_id)
            .is_some_and(|rt| rt.is_connected());
        let blocked = loading || view.error.is_some() || !connected;
        let pending = view.pending_options.clone();
        let mode = view.mode_option();

        let plus = toolbar_button("s-attach", p)
            .debug_selector(|| "composer-attach".into())
            .w(px(32.))
            .px_0()
            .justify_center()
            .rounded_full()
            .tooltip(|window, cx| kit::wrap_tip("Attach files").build(window, cx))
            .child(kit::icon(IconName::Plus, 17., p.muted))
            .on_click(cx.listener(|this, _, _, cx| super::start::attach_files(this, cx)));

        // A compact composer is one line at rest: no options panel.
        let compact = self.density.is_compact();
        let model_button = (!compact).then(|| self.model_picker_button(p, cx));
        // Only an agent that offers modes has one to show: its name in the
        // composer, and its colour on the pill's outline (design D20).
        let compact_mode = mode
            .as_ref()
            .filter(|m| compact && !m.choices.is_empty() && !m.current_label().is_empty());
        let mode_name = compact_mode.map(|m| self.mode_name(m, p, cx));
        let mode_outline = compact_mode.map(|m| {
            mode_tone(m.choices.iter().map(|(v, _)| v.as_str()), &m.current, p)
                .opacity(MODE_OUTLINE_STRENGTH)
        });
        let options =
            (!compact).then(|| self.options_control(mode.as_ref(), control, &pending, p, cx));

        let attachments = super::preview::attachment_strip(self, p, cx).map(|s| s.pt_1p5());

        // Right side: stop / send, like the desktop composer.
        let actions = self.composer_actions(running, control, has_draft, blocked, cx);

        // While the command list shows, its keys come first (`slash_keys`).
        let input = self
            .slash_keys(div().relative(), cx)
            // The id lets a test or a film scene click it by name; a plain
            // id is not found by `find` unless the element is observed too
            // (inert without the kit's `test-support` feature).
            .id("composer-input")
            .test_support()
            .debug_selector(|| "composer-input".into())
            .children(slash_list.map(|list| deferred(list).with_priority(1)))
            .px_1()
            .when(!compact, |el| el.min_h(px(64.)))
            .when(compact, |el| el.flex_1().min_w_0())
            .capture_action(cx.listener(|this, a: &Enter, window, cx| {
                // A compact composer always sends on Enter and breaks the
                // line on Shift+Enter, whatever the main window's setting.
                let send = if this.density.is_compact() {
                    crate::store::SendShortcut::Enter
                } else {
                    this.store.read(cx).prefs.send_shortcut
                };
                if this.density.is_compact() && a.secondary {
                    // Cmd/Ctrl+Enter hands the session to the main window.
                    cx.stop_propagation();
                    cx.emit(SessionPaneEvent::OpenInMain);
                } else if send.submits(a.shift, a.secondary) {
                    cx.stop_propagation();
                    this.send_session_prompt(window, cx);
                }
            }))
            // Tab cycles modes instead of indenting (captured before the textarea).
            .capture_action(cx.listener(|this, _: &IndentInline, _, cx| {
                cx.stop_propagation();
                this.cycle_session_mode(false, cx);
            }))
            .capture_action(cx.listener(|this, _: &OutdentInline, _, cx| {
                cx.stop_propagation();
                this.cycle_session_mode(true, cx);
            }))
            // The compact composer is the quick panel's, in its palette:
            // placeholder, selection and caret too.
            .when(compact, |el| el.child(kit::input_colours(&self.prompt, p)))
            .child(
                Textarea::new(&self.prompt)
                    .appearance(false)
                    .on_paste(super::start::paste_handler(cx)),
            );

        // The HUD's composer floats: a pill of faint white over the glass
        // (the pane's own surface where there is none), lit along its top
        // edge by a 1 px highlight instead of a border.
        // The compact pill is one row: as much room above it as below, so
        // its buttons, input and mode share the pill's centre line. The main
        // window's composer keeps less above its input than below its toolbar.
        v_flex()
            .relative()
            .w_full()
            .when(compact, |el| el.py(px(6.)))
            .when(!compact, |el| el.pt_1().pb_2())
            .px_2()
            .gap_1p5()
            .rounded(px(if compact { 22. } else { 18. }))
            .bg(if compact && self.glass {
                p.chip
            } else {
                p.surface
            })
            .when(!compact, |el| el.border_1().border_color(p.border))
            // An agent with modes: a 1 px outline all round in the mode's
            // tone, and no highlight. The border is there whatever the mode,
            // so changing it moves nothing.
            .when_some(mode_outline, |el, tone| {
                el.debug_selector(|| "composer-outline".into())
                    .border_1()
                    .border_color(tone)
            })
            .when(compact && mode_outline.is_none(), |el| {
                el.child(
                    div()
                        .debug_selector(|| "composer-highlight".into())
                        .absolute()
                        .top_0()
                        .left(px(14.))
                        .right(px(14.))
                        .h(px(1.))
                        .bg(white().opacity(0.22)),
                )
            })
            // No shadow on a material: it would lie on the glass as a dark smudge.
            .when(!(compact && self.glass), |el| el.shadow_md())
            .children(slash_hint)
            .when_some(self.attach_error.clone(), |el, err| {
                el.child(
                    h_flex()
                        .gap_1p5()
                        .px_1()
                        .pt_1()
                        .text_xs()
                        .text_color(p.danger)
                        .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                        .child(div().flex_1().child(err))
                        .child(
                            kit::icon_button("s-dismiss-attach", IconName::X, "Dismiss")
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.attach_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .map(|el| {
                if compact {
                    el.child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(plus)
                            .child(input)
                            .children(mode_name)
                            .child(actions),
                    )
                    .children(attachments)
                } else {
                    el.child(input).children(attachments).child(
                        h_flex()
                            .gap_2()
                            .pt_2()
                            .border_t_1()
                            .border_color(p.border.opacity(0.6))
                            .child(plus)
                            .children(model_button)
                            .children(options)
                            .child(div().flex_1())
                            .child(actions),
                    )
                }
            })
    }

    /// The Options button with its panel, for the comfortable composer.
    fn options_control(
        &self,
        mode: Option<&SelectOption>,
        control: bool,
        pending: &HashSet<String>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let options_button = toolbar_button("s-options", p)
            .child(kit::icon(IconName::Settings2, 14., p.muted))
            .child("Options")
            .child(kit::icon(IconName::ChevronDown, 13., p.dim))
            .on_click(cx.listener(|this, ev: &ClickEvent, window, cx| {
                if this.dismissed.closed_by("session-options", ev) {
                    return;
                }
                this.options_open = !this.options_open;
                if !this.options_open {
                    this.restore_focus(window, cx);
                }
                cx.notify();
            }));
        let options_button: AnyElement =
            match mode.as_ref().filter(|m| !m.current_label().is_empty()) {
                Some(m) => {
                    let tone = mode_tone(m.choices.iter().map(|(v, _)| v.as_str()), &m.current, p);
                    badged_tone(options_button, &m.current_label(), tone, p).into_any_element()
                }
                None => options_button.into_any_element(),
            };
        div()
            .debug_selector(|| "composer-options".into())
            .relative()
            .child(options_button)
            .when(self.options_open, |el| {
                el.child(
                    deferred(self.session_options_panel(mode, control, pending, p, cx))
                        .with_priority(1),
                )
            })
    }

    /// Stop / send (steer / queue while a run is active) at the composer's end.
    fn composer_actions(
        &self,
        running: bool,
        control: bool,
        has_draft: bool,
        blocked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stop = kit::button("stop-turn")
            .ghost()
            .icon(IconName::Square)
            .tooltip("Stop agent")
            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)));
        // Whichever it is, it is one box at the composer's end.
        let actions = if running && (!control || !has_draft) {
            kit::button("stop-primary")
                .primary()
                .icon(IconName::Square)
                .tooltip("Stop agent")
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                .into_any_element()
        } else if running && control {
            let queue = self.delivery == Delivery::Queue;
            h_flex()
                .gap_1p5()
                .child(stop)
                .child(
                    kit::button("send-turn")
                        .primary()
                        .icon(if queue {
                            IconName::Plus
                        } else {
                            IconName::Navigation
                        })
                        .label(if queue { "Queue" } else { "Steer" })
                        .inactive(blocked)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.send_session_prompt(window, cx)),
                        ),
                )
                .into_any_element()
        } else {
            kit::button("send-turn")
                .primary()
                .icon(IconName::SendHorizontal)
                .tooltip("Send reply")
                .inactive(blocked || !has_draft)
                .on_click(cx.listener(|this, _, window, cx| this.send_session_prompt(window, cx)))
                .into_any_element()
        };
        div()
            .debug_selector(|| "composer-send".into())
            .child(actions)
            .into_any_element()
    }

    // ---- page ---------------------------------------------------------------

    pub(super) fn render_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = self.palette(cx);
        let find_bar = self.find_bar(&p, cx);
        let store = self.store.read(cx);
        let Some(session) = self.session.clone() else {
            return div().into_any_element();
        };
        let open = session.read(cx);
        let view = &open.view;

        let key = format!("{}/{}", view.agent_id, view.session_id);
        let (agent_id, session_id) = (view.agent_id.clone(), view.session_id.clone());
        let control = open.can_control_run();
        let running = view.running();
        let loading = view.loading;
        let error = view.error.clone();
        let error_details = view.error_details.clone();
        if !view.loading {
            crate::telemetry::session_rendered(&view.session_id, view.items.len());
        }
        let history_more = view.history_more;
        let history_loading = view.history_loading;
        let history_error = view.history_error.clone();
        let empty = view.items.is_empty();
        let requests: Vec<String> = self.cards.read(cx).keys(
            &super::inbox::RequestFilter::Session(SessionKey::new(&agent_id, &session_id)),
            cx,
        );
        let (act_title, act_preview) = activity(view);
        // Untruncated shell command of the running tool, for the bar's tooltip.
        let act_full = match view.items.last().map(|i| &i.kind) {
            Some(ItemKind::Work(entries)) => match entries.last() {
                Some(WorkEntry::Tool(call)) if call.running() => {
                    call.command().map(|c| workspace_relative(&c, &view.cwd))
                }
                _ => None,
            },
            _ => None,
        };

        let turn_end = view.turn_ends();
        let reverted_from = view.reverted_from();
        // Desktop: forks only from settled, live turns while nothing else runs.
        let skey = SessionKey::new(&agent_id, &session_id);
        let fork_ready = store.session_supports_fork(&skey, cx)
            && !store.fork_pending(&skey)
            && !running
            && !loading
            && !view.undo_pending;
        let fork_points: HashSet<usize> = if fork_ready {
            turn_end
                .keys()
                .copied()
                .filter(|ix| reverted_from.is_none_or(|r| *ix < r))
                .filter(|ix| view.fork_point(*ix).is_some())
                .collect()
        } else {
            HashSet::new()
        };
        let diff_open = self
            .turn_diff_range(cx)
            .and_then(|r| turn_end.keys().copied().filter(|ix| r.contains(ix)).max());
        let rc = Rc::new(RenderCtx {
            p,
            syntax: gpui_kit::component::ActiveTheme::theme(&**cx)
                .highlight_theme
                .clone(),
            weak: cx.entity().downgrade(),
            density: self.density,
            smooth_scroll: self.smooth_scroll.clone(),
            expanded: self.expanded.clone(),
            running,
            last_turn_start: view.last_turn_start(),
            last_ix: view.items.len().saturating_sub(1),
            turn_end,
            copied: self.copied.clone(),
            block_scrolls: self.transcript.block_scrolls.clone(),
            cwd: view.cwd.clone(),
            turn_user: view.turn_users(),
            undo_ready: open.supports_undo() && !running && !loading && !view.undo_pending,
            undoable: view.undoable(),
            redo_target: view.redo_target().map(str::to_owned),
            fork_points,
            diff_open,
            reverted_from,
            agent_id: view.agent_id.clone(),
            stream_fade: store.prefs.stream_fade.unwrap_or(true),
            md: self.transcript.md.clone(),
            tool_content: open.supports_tool_content(),
            developer: store.prefs.developer_mode,
        });
        self.transcript.sync(&key, &view.items, &rc);
        if !self.transcript.scroll_hooked {
            self.transcript.scroll_hooked = true;
            let weak = cx.entity().downgrade();
            self.transcript.list.set_scroll_handler(move |ev, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    // Near the top: fetch the preceding turns (no-op when none are left).
                    if ev.visible_range.start <= 2 {
                        this.load_older(cx);
                    }
                    cx.notify()
                });
            });
        }

        // A transcript that doesn't fill the viewport can't be scrolled, so the
        // scroll handler never fires: keep pulling older pages while the first
        // item is on screen (after layout; deferred out of render).
        if history_more
            && !history_loading
            && history_error.is_none()
            && self.transcript.list.item_is_above_viewport(0) == Some(false)
        {
            cx.on_next_frame(window, |this, _, cx| this.load_older(cx));
        }

        // Snapshot what the composer/header need before borrowing self mutably.
        let view_snapshot = view.shallow();

        // Title, status, and actions live in the title bar (`session_chrome`);
        // only the context strip stays on the page.

        // ---- transcript
        let items = self.transcript.items.clone();
        let at_bottom = self.transcript.at_bottom();
        let composer_h = self.transcript.composer_h.get();
        let transcript: AnyElement = if loading && empty {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(p.dim)
                .child(kit::spinner("loading", 20., p.accent))
                .child("Loading transcript…")
                .into_any_element()
        } else if let Some(err) = error.clone().filter(|_| empty) {
            let details_open = self.expanded.contains(ERROR_DETAILS_KEY);
            let copied = self.copied.as_deref() == Some(COPIED_ERROR_DETAILS);
            let details_panel = error_details.clone().filter(|_| details_open).map(|text| {
                div()
                    .id("load-error-details")
                    .w_full()
                    .max_w(px(self.density.column_max()))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .rounded(px(10.))
                    .bg(p.card)
                    .border_1()
                    .border_color(p.border)
                    .font_family(kit::MONO)
                    .text_xs()
                    .text_color(p.muted)
                    .whitespace_normal()
                    .children(
                        text.lines()
                            .map(|l| div().min_h(px(14.)).child(l.to_string()))
                            .collect::<Vec<_>>(),
                    )
                    .map(|el| self.smooth_scroll.region("load-error-details", el))
            });
            let actions = h_flex()
                .gap_2()
                .child(
                    kit::button("retry-load")
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .when_some(error_details.clone(), |el, text| {
                    el.child(
                        kit::button("toggle-load-error-details")
                            .label(if details_open {
                                "Hide details"
                            } else {
                                "Show details"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.expanded.remove(ERROR_DETAILS_KEY) {
                                    this.expanded.insert(ERROR_DETAILS_KEY.into());
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        kit::button("copy-load-error-details")
                            .icon(if copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            })
                            .label(if copied { "Copied" } else { "Copy details" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                this.flash_copied(COPIED_ERROR_DETAILS.into(), cx);
                            })),
                    )
                });
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .px_8()
                .text_sm()
                .text_color(p.danger)
                .child(kit::icon(IconName::CircleAlert, 18., p.danger))
                .child(div().text_center().child(err))
                .child(actions)
                .children(details_panel)
                .into_any_element()
        } else if empty {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .px_8()
                .text_color(p.dim)
                .child(div().text_sm().font_semibold().text_color(p.muted).child("No conversation yet"))
                .child(div().text_xs().text_center().child("Send a prompt below to start streaming messages, reasoning, and activities into this view."))
                .into_any_element()
        } else {
            let rc2 = rc.clone();
            let render_items = items.clone();
            // Telemetry: time each row built and the transcript's frame.
            let timed = crate::telemetry::enabled();
            let row_probe = self.transcript.probe.clone();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    v_flex()
                        .size_full()
                        .when(history_loading || history_error.is_some(), |el| {
                            el.child(
                                h_flex()
                                    .flex_none()
                                    .justify_center()
                                    .gap_2()
                                    .py_1p5()
                                    .text_xs()
                                    .text_color(p.dim)
                                    .map(|el| match &history_error {
                                        Some(err) if !history_loading => el
                                            .child(div().text_color(p.danger).truncate().child(
                                                format!("Could not load earlier turns: {err}"),
                                            ))
                                            .child(
                                                kit::button("history-retry")
                                                    .xsmall()
                                                    .ghost()
                                                    .label("Retry")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.retry_older(cx)
                                                    })),
                                            ),
                                        _ => el
                                            .child(kit::spinner("history-loading", 12., p.dim))
                                            .child("Loading earlier turns…"),
                                    }),
                            )
                        })
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .w_full()
                                .child(
                                    self.smooth_scroll.region(
                                        "transcript",
                                        list(self.transcript.list.clone(), move |ix, _, _| {
                                            let row = render_items.borrow().get(ix).cloned();
                                            match row {
                                                Some(row) => {
                                                    let started = timed.then(crate::util::now_ms);
                                                    rc2.md.begin_row(&row.key);
                                                    let el = render_row(&row, &rc2);
                                                    rc2.md.end_row();
                                                    if let Some(started) = started {
                                                        row_probe
                                                            .borrow_mut()
                                                            .row_built(&row, started);
                                                    }
                                                    el
                                                }
                                                None => div().into_any_element(),
                                            }
                                        })
                                        .size_full()
                                        // Top: start below the fade, so nothing is dimmed at rest.
                                        // Bottom: the composer floats over the list's end, so the
                                        // last message can scroll above it.
                                        .pt(px(TRANSCRIPT_TOP_FADE_H))
                                        .pb(px(composer_h)),
                                    ),
                                )
                                // Telemetry: sample the list after its layout (siblings
                                // prepaint in order). Absent unless export is on.
                                .when(crate::telemetry::enabled(), |el| {
                                    let list = self.transcript.list.clone();
                                    let probe = self.transcript.probe.clone();
                                    let smooth = self.smooth_scroll.clone();
                                    el.child(
                                        canvas(
                                            move |_, _, _| {
                                                sample_transcript(
                                                    &list, &probe, &smooth, composer_h,
                                                )
                                            },
                                            |_, _, _, _| {},
                                        )
                                        .absolute()
                                        .size_0(),
                                    )
                                })
                                // Same layered falloff as the composer halo: each copy
                                // fades from the background to transparent, and stacking
                                // them builds the dense edge. Plain divs have no hitbox,
                                // so the wheel still reaches the list.
                                // On a material there is no background colour to
                                // fade to, so there is no fade.
                                .children(
                                    (0..if self.glass { 0 } else { COMPOSER_HALO_LAYERS }).map(
                                        |_| {
                                            div()
                                                .absolute()
                                                .top_0()
                                                .left_0()
                                                .right_0()
                                                .h(px(TRANSCRIPT_TOP_FADE_H))
                                                .bg(linear_gradient(
                                                    180.,
                                                    linear_color_stop(p.bg, 0.),
                                                    linear_color_stop(p.bg.opacity(0.), 1.),
                                                ))
                                        },
                                    ),
                                )
                                // Over the fade, so nothing covers it.
                                .when_some(find_bar, |el, bar| el.child(bar)),
                        ),
                )
                .into_any_element()
        };

        // The plan lives in the title bar (`session_chrome`).

        // ---- activity dock (desktop `session-activity-bar-dock`): the
        // waiting-messages bubble, then the activity bar. Both use the halo
        // look: a page-coloured fill with a feathered page-coloured glow.
        let waiting = !requests.is_empty();
        let glass = self.glass;
        let halo_pill = |el: Stateful<Div>| {
            let el = el
                .flex_none()
                .h(px(32.))
                .rounded(px(8.))
                .text_color(p.muted);
            if glass {
                // No page colour to glow with: a plain raised pill.
                return el.bg(p.surface);
            }
            el.bg(p.bg).shadow(vec![
                BoxShadow {
                    color: p.bg,
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(4.),
                    inset: false,
                },
                BoxShadow {
                    color: p.bg.opacity(0.85),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(12.),
                    spread_radius: px(4.),
                    inset: false,
                },
                BoxShadow {
                    color: p.bg.opacity(0.5),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(28.),
                    spread_radius: px(9.),
                    inset: false,
                },
            ])
        };
        let activity_bar = (running || waiting).then(|| {
            let tip_cmd = if waiting {
                None
            } else {
                act_full.clone().or_else(|| act_preview.clone())
            };
            let can_cancel = running && !waiting;
            let (dim, mono) = (p.dim, kit::MONO);
            halo_pill(h_flex().id("activity-bar"))
                .min_w_0()
                .max_w_full()
                .gap_2()
                .px(px(11.))
                .child(if waiting {
                    kit::icon(IconName::MessageCircleQuestionMark, 14., p.accent).into_any_element()
                } else {
                    kit::spinner("activity-spin", 14., p.muted).into_any_element()
                })
                .child(
                    h_flex()
                        .min_w_0()
                        .text_size(px(12.5))
                        .font_semibold()
                        .text_color(p.text)
                        .child(div().flex_none().child(if waiting {
                            "Input needed".to_string()
                        } else {
                            act_title.clone()
                        }))
                        .when_some(act_preview.clone().filter(|_| !waiting), |el, pv| {
                            el.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(p.dim)
                                    .child(format!(" · {pv}")),
                            )
                        }),
                )
                .when(waiting, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(11.2))
                            .text_color(p.dim)
                            .child("Respond below to continue."),
                    )
                })
                .when(can_cancel || tip_cmd.is_some(), |el| {
                    el.tooltip(move |window, cx| {
                        let cmd = tip_cmd.clone();
                        Tooltip::element(move |_, _| {
                            v_flex()
                                .max_w(px(512.))
                                .gap_1()
                                .when_some(cmd.clone(), |el, c| {
                                    el.child(div().font_family(mono).text_xs().child(c))
                                })
                                .when(can_cancel, |el| {
                                    el.child(div().text_color(dim).child("Double Esc to cancel"))
                                })
                        })
                        .build(window, cx)
                    })
                })
        });

        let pending_inputs = view_snapshot.pending_inputs.clone();
        if pending_inputs.is_empty() {
            self.waiting_open = false;
        }
        let waiting_open = self.waiting_open;
        let count = pending_inputs.len();
        let waiting_bubble = (count > 0).then(|| {
            let (text, hover) = (p.text, p.card_hover);
            let hint = format!(
                "{count} waiting {}",
                if count == 1 { "message" } else { "messages" }
            );
            halo_pill(h_flex().id("waiting-bubble"))
                .gap(px(6.4))
                .px(px(9.6))
                .cursor_pointer()
                .when(waiting_open, |el| el.text_color(text).bg(p.bg.blend(hover)))
                .hover(move |s| s.text_color(text).bg(hover))
                .tooltip(move |window, cx| kit::wrap_tip(hint.clone()).build(window, cx))
                .child(kit::icon(
                    IconName::Clock,
                    14.,
                    if waiting_open { p.text } else { p.muted },
                ))
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_semibold()
                        .text_color(p.text)
                        .child(count.to_string()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.waiting_open = !this.waiting_open;
                    cx.notify();
                }))
        });
        let can_discard = control && open.can_discard_queued();
        let waiting_panel = (waiting_open && count > 0).then(|| {
            let rows = pending_inputs.iter().map(|input| {
                let queue =
                    Delivery::from_input_state_name(&input.delivery) == Some(Delivery::Queue);
                let mut state = input.state.clone();
                if let Some(first) = state.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                let removable = queue && input.state == "queued" && can_discard;
                let id = input.input_id.clone();
                let busy = input.discard_pending;
                h_flex()
                    .min_h(px(32.))
                    .items_start()
                    .gap_2()
                    .pl_2()
                    .pr(px(6.))
                    .py_1()
                    .rounded(px(8.))
                    .bg(p.card)
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .h(px(20.))
                            .text_size(px(10.2))
                            .font_semibold()
                            .text_color(p.warn)
                            .child(kit::icon(
                                if queue {
                                    IconName::Plus
                                } else {
                                    IconName::Navigation
                                },
                                12.,
                                p.warn,
                            ))
                            .child(state),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.8))
                            .line_height(px(20.))
                            .text_color(p.text)
                            .child(if input.text.is_empty() {
                                format!("{} attachment(s)", input.attachments)
                            } else {
                                input.text.clone()
                            }),
                    )
                    .when(removable, |el| {
                        el.child(
                            div()
                                .id(kit::keyed("waiting-rm", id.clone()))
                                .flex_none()
                                .size(px(24.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(8.))
                                .when(busy, |el| el.opacity(0.45))
                                .when(!busy, |el| {
                                    el.cursor_pointer().hover(move |s| s.bg(p.card_hover))
                                })
                                .tooltip(|window, cx| {
                                    kit::wrap_tip("Remove queued message").build(window, cx)
                                })
                                .child(kit::icon(IconName::X, 13., p.dim))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.discard_queued(&id, cx);
                                })),
                        )
                    })
            });
            v_flex()
                .id("waiting-panel")
                .occlude()
                .w_full()
                .p(px(5.))
                .rounded(px(12.))
                .border_1()
                .border_color(p.border)
                .bg(p.surface)
                .shadow_lg()
                .child(
                    v_flex()
                        .id("waiting-list")
                        .max_h(px(256.))
                        .overflow_y_scroll()
                        .gap(px(3.))
                        .children(rows)
                        .map(|el| self.smooth_scroll.region("waiting-list", el)),
                )
        });
        // Desktop `SessionScrollToBottomPill`: pinned to the dock's right end,
        // on the same row (and baseline) as the activity bar and bubbles.
        let scroll_pill = (!at_bottom).then(|| {
            let (hover, text) = (p.card, p.text);
            halo_pill(div().id("scroll-latest"))
                .ml_auto()
                .w(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(move |s| s.bg(hover).text_color(text))
                .child(gpui_kit::component::Icon::new(IconName::ChevronDown).size(px(16.)))
                .tooltip(|window, cx| {
                    kit::wrap_tip("Scroll to latest message and follow new content")
                        .build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.scroll_to_latest(cx)))
        });
        // The dock floats in a slot of constant height above the composer:
        // the measured floating area pads the transcript's end, so a dock that
        // took space when the activity bar or pill appeared moved the content
        // (40px at the pill, 29px at the bar). The slot keeps the last message
        // clear of the dock row; the waiting panel grows upward over the
        // transcript like a popover.
        let dock = div().relative().flex_none().h(px(DOCK_ROW_H)).child(
            v_flex()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .gap_2()
                .children(waiting_panel)
                .child(
                    h_flex()
                        .px_1()
                        .gap(px(6.4))
                        .min_w_0()
                        .children(waiting_bubble)
                        .children(activity_bar)
                        .children(scroll_pill),
                ),
        );

        // ---- requests + composer
        let mcp_notice = self.mcp_notice(&p, cx);
        let request_cards: Vec<AnyElement> = requests
            .iter()
            .filter_map(|k| {
                self.cards.update(cx, |cards, cx| {
                    cards.request_card(k, false, false, window, cx)
                })
            })
            .collect();
        // A read-only session (worktree handed off or archived) shows why
        // instead of a composer (desktop `describeSessionAccess`).
        let read_only = view_snapshot.access.as_ref().map(|a| a.describe());
        let composer = match read_only {
            Some(text) => h_flex()
                .w_full()
                .gap_2()
                .px_4()
                .py_3()
                .rounded(px(14.))
                .bg(p.surface)
                .border_1()
                .border_color(p.border)
                .text_sm()
                .text_color(p.muted)
                .child(kit::icon(IconName::Lock, 14., p.muted))
                .child(div().flex_1().min_w_0().child(text)),
            None => self.session_composer(&view_snapshot, control, &p, window, cx),
        };
        // A soft halo in the page background color around the composer: text
        // scrolling under it dissolves instead of meeting a hard edge.
        let halo = vec![
            BoxShadow {
                color: if self.glass { p.bg.opacity(0.) } else { p.bg },
                offset: point(px(0.), px(0.)),
                blur_radius: px(COMPOSER_HALO_BLUR),
                spread_radius: px(COMPOSER_HALO_SPREAD),
                inset: false
            };
            COMPOSER_HALO_LAYERS
        ];
        let composer = if read_only.is_some() {
            div().id("session-read-only").child(composer).shadow(halo)
        } else {
            super::start::drop_zone("session-drop", composer, &p, cx).shadow(halo)
        };
        // Measure the floating area; re-render once when its height changes.
        let measure = {
            let (cell, weak) = (self.transcript.composer_h.clone(), cx.entity().downgrade());
            canvas(
                move |bounds, window, _| {
                    let h = f32::from(bounds.size.height);
                    if (cell.get() - h).abs() > 0.5 {
                        cell.set(h);
                        window.on_next_frame(move |_, cx| {
                            let _ = weak.update(cx, |_, cx| cx.notify());
                        });
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        };
        // Floats over the transcript's end. The fade runs from transparent at
        // the top to the page background behind the composer; plain divs
        // have no hitbox, so the wheel over the fade still reaches the list.
        let bottom = div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .justify_center()
            .px(px(self.density.row_pad()))
            .pb_4()
            .pt(px(COMPOSER_FADE_H))
            // Fades in over the top strip, then stays a partial veil so text
            // that scrolled past is still faintly visible beside and below.
            .bg(linear_gradient(
                180.,
                linear_color_stop(p.bg.opacity(0.), 0.),
                linear_color_stop(
                    p.bg.opacity(if self.glass { 0. } else { COMPOSER_VEIL }),
                    0.35,
                ),
            ))
            .child(measure)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(self.density.column_max()))
                    .gap_2()
                    .children(mcp_notice)
                    .children(request_cards)
                    .child(dock)
                    .child(self.smooth_scroll.region("composer", composer)),
            );

        let chat = v_flex()
            .size_full()
            .when(self.density.is_compact(), |el| {
                el.font_family(kit::SYSTEM_UI)
            })
            .when_some(error.filter(|_| !empty), |el, err| {
                el.child(
                    h_flex()
                        .flex_none()
                        .mx(px(self.density.row_pad()))
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded(px(8.))
                        .bg(p.danger.opacity(0.1))
                        .text_xs()
                        .text_color(p.danger)
                        .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                        .child(div().flex_1().child(err)),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(transcript)
                    .child(bottom),
            )
            .into_any_element();
        self.with_turn_diff(chat, window, cx)
    }
}

impl Render for SessionPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_session(window, cx)
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{fenced, reasoning_title, work_blocks};
    use super::{
        humanize_tool_name, is_read, is_shell, link_ranges, tool_label, workspace_relative,
    };
    use crate::session::{Thought, WorkEntry};

    #[test]
    fn fences_outrun_backticks_in_the_code() {
        assert_eq!(fenced("yaml", "a: 1"), "```yaml\na: 1\n```");
        assert_eq!(
            fenced("yaml", "cmd: \"````x````\""),
            "`````yaml\ncmd: \"````x````\"\n`````"
        );
    }

    // Exercise the list operations used by expansion and transcript sync.
    #[gpui_kit::test]
    fn expansion_preserves_scroll_anchor_and_streaming_still_follows(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::{
            AppContext as _, Context, FollowMode, IntoElement as _, ListAlignment, ListOffset,
            ListState, Render, Styled as _, Window, div, list, point, px, size,
        };
        use std::{cell::Cell, rc::Rc};
        struct Fixture {
            state: ListState,
            expanded: Rc<Cell<bool>>,
        }
        impl Render for Fixture {
            fn render(
                &mut self,
                _: &mut Window,
                _: &mut Context<Self>,
            ) -> impl gpui_kit::IntoElement {
                let expanded = self.expanded.clone();
                list(self.state.clone(), move |ix, _, _| {
                    let height = if ix == 1 {
                        if expanded.get() { 2400. } else { 2000. }
                    } else {
                        100.
                    };
                    div().h(px(height)).w_full().into_any_element()
                })
                .size_full()
            }
        }
        let cx = cx.add_empty_window();
        let state = ListState::new(5, ListAlignment::Top, px(1200.));
        let expanded = Rc::new(Cell::new(false));
        let draw = |cx: &mut gpui_kit::VisualTestContext| {
            cx.draw(point(px(0.), px(0.)), size(px(600.), px(500.)), |_, cx| {
                cx.new(|_| Fixture {
                    state: state.clone(),
                    expanded: expanded.clone(),
                })
                .into_any_element()
            });
        };
        state.set_follow_mode(FollowMode::Tail);
        draw(cx);
        state.scroll_to(ListOffset {
            item_ix: 1,
            offset_in_item: px(900.),
        });
        draw(cx);
        let before = state.logical_scroll_top();
        state.pause_following_tail();
        expanded.set(true);
        state.remeasure_items(1..2);
        draw(cx);
        assert_eq!(before.offset_in_item, px(900.));
        assert_eq!(state.logical_scroll_top(), before);
        assert!(!state.is_following_tail());

        state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(25.),
        });
        draw(cx);
        let before = state.logical_scroll_top();
        state.pause_following_tail();
        expanded.set(false);
        state.remeasure_items(1..2);
        draw(cx);
        assert_eq!(state.logical_scroll_top(), before);

        state.set_follow_mode(FollowMode::Tail);
        state.scroll_to_end();
        draw(cx);
        let before = state.logical_scroll_top();
        state.pause_following_tail();
        expanded.set(true);
        state.remeasure_items(1..2);
        draw(cx);
        assert_eq!(state.logical_scroll_top(), before);
        assert!(!state.is_following_tail());

        // Re-enable follow, as the jump-to-bottom control does. Streaming
        // changes must still follow the end without a user disclosure click.
        expanded.set(false);
        state.remeasure_items(1..2);
        state.set_follow_mode(FollowMode::Tail);
        state.scroll_to_end();
        draw(cx);
        let before = state.logical_scroll_top();
        expanded.set(true);
        state.remeasure_items(1..2);
        draw(cx);
        assert!(state.is_following_tail());
        assert_eq!(
            state.logical_scroll_top().offset_in_item - before.offset_in_item,
            px(400.)
        );
    }

    /// Spec "Scrolling past a diagram": a diagram row's height is known when
    /// the list first builds it, so scrolling up past it leaves the anchor
    /// where it is and later frames do not move it.
    #[gpui_kit::test]
    fn scrolling_past_a_diagram_does_not_jump(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::{
            AppContext as _, Context, IntoElement as _, ListAlignment, ListOffset, ListState,
            ParentElement as _, Render, Styled as _, Window, div, list, point, px, size,
        };
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        // Wide enough to be scaled down and tall enough to matter.
        let mut diagram = String::from("```mermaid\nflowchart LR\n");
        for i in 0..12 {
            diagram.push_str(&format!(
                "  node{i}[Step number {i}] --> node{}[Step {}]\n",
                i + 1,
                i + 1
            ));
            diagram.push_str(&format!("  node{i} --> branch{i}[Branch {i}]\n"));
        }
        diagram.push_str("```");
        struct Fixture {
            state: ListState,
            kept: super::super::md::Kept,
            diagram: String,
        }
        impl Render for Fixture {
            fn render(
                &mut self,
                _: &mut Window,
                cx: &mut Context<Self>,
            ) -> impl gpui_kit::IntoElement {
                let p = *crate::theme::palette(cx);
                let (kept, diagram) = (self.kept.clone(), self.diagram.clone());
                list(self.state.clone(), move |ix, _, _| {
                    let key = format!("row-{ix}");
                    kept.begin_row(&key);
                    let row = match ix {
                        1 => div()
                            .w_full()
                            .child(kept.markdown(key.clone(), diagram.clone(), &p))
                            .into_any_element(),
                        _ => div().h(px(120.)).w_full().into_any_element(),
                    };
                    kept.end_row();
                    row
                })
                .size_full()
            }
        }
        let cx = cx.add_empty_window();
        let state = ListState::new(6, ListAlignment::Top, px(1200.));
        let kept = super::super::md::Kept::default();
        let draw = |cx: &mut gpui_kit::VisualTestContext| {
            cx.run_until_parked();
            cx.draw(point(px(0.), px(0.)), size(px(600.), px(500.)), |_, cx| {
                cx.new(|_| Fixture {
                    state: state.clone(),
                    kept: kept.clone(),
                    diagram: diagram.clone(),
                })
                .into_any_element()
            });
        };
        // Start below the diagram, then scroll up past it.
        state.scroll_to(ListOffset {
            item_ix: 3,
            offset_in_item: px(0.),
        });
        draw(cx);
        for _ in 0..6 {
            state.scroll_by(px(-60.));
            draw(cx);
        }
        let top = state.logical_scroll_top();
        let rows: Vec<_> = (0..6).map(|ix| state.bounds_for_item(ix)).collect();
        let height = rows[1].expect("diagram row was built").size.height;
        assert!(
            height > px(60.),
            "the diagram has its own height: {height:?}"
        );
        // Later frames and a remeasure of the diagram row change nothing.
        for _ in 0..3 {
            state.remeasure_items(1..2);
            draw(cx);
        }
        assert_eq!(state.logical_scroll_top(), top);
        let after: Vec<_> = (0..6).map(|ix| state.bounds_for_item(ix)).collect();
        assert_eq!(after, rows);
    }

    fn thought(id: &str, part: Option<&str>) -> WorkEntry {
        WorkEntry::Thought(Thought {
            id: id.into(),
            part: part.map(Into::into),
            text: format!("**{id}**"),
        })
    }

    #[test]
    fn summary_runs_group_into_one_block() {
        let entries = [
            thought("a", Some("rs:summary:0")),
            thought("b", Some("rs:summary:1")),
            thought("raw", None),
            thought("c", Some("rs:summary:2")),
        ];
        let sizes: Vec<usize> = work_blocks(&entries).iter().map(|b| b.len()).collect();
        // Raw reasoning breaks a run and is never joined.
        assert_eq!(sizes, [2, 1, 1]);
    }

    #[test]
    fn reasoning_title_is_plain_and_clipped() {
        assert_eq!(
            reasoning_title("**Checking scope filters**"),
            "Checking scope filters"
        );
        let long = "word ".repeat(60);
        let title = reasoning_title(&long);
        assert_eq!(title.chars().count(), 122);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn link_ranges_find_urls_without_markup() {
        let text = "see [llms.txt](https://a.com/llms.txt). Or <http://b.org/x?y=1>, \
                    \"https://c.io/p\" and https://d.dev/page.";
        let found: Vec<&str> = link_ranges(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(
            found,
            [
                "https://a.com/llms.txt",
                "http://b.org/x?y=1",
                "https://c.io/p",
                "https://d.dev/page"
            ]
        );
        assert!(link_ranges("https:// nothing, xhttps://glued.com").is_empty());
    }

    use crate::session::ToolCall;

    fn call(title: &str, kind: &str) -> ToolCall {
        let mut call = ToolCall::new("t");
        call.title = title.into();
        call.kind = kind.into();
        call.status = "completed".into();
        call
    }

    #[test]
    fn delegate_brief_keeps_objective_and_hides_the_rest() {
        let mut c = call("delegate", "other");
        c.input = Some(serde_json::json!({
            "target_agent_id": "cedar",
            "objective": " Cross-examine willow ",
            "context": "evidence",
            "constraints": "",
            "expected_output": "report",
        }));
        let brief = super::DelegateBrief::parse(&c).expect("delegate");
        assert_eq!(brief.target.as_deref(), Some("cedar"));
        assert_eq!(brief.objective, "Cross-examine willow");
        let labels: Vec<&str> = brief.details.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"Context") && labels.contains(&"Expected output"));
        assert!(!labels.contains(&"Constraints"), "empty fields are dropped");
        assert_eq!(tool_label(&c), "Delegate");
        assert_eq!(super::tool_preview_raw(&c), "Cross-examine willow");
        assert!(super::DelegateBrief::parse(&call("shell", "execute")).is_none());
    }

    #[test]
    fn paths_relative_to_workspace_only() {
        let ws = "/p/app";
        assert_eq!(workspace_relative("/p/app/src/a.rs", ws), "src/a.rs");
        assert_eq!(
            workspace_relative("cd /p/app && grep x /p/app/src", ws),
            "cd . && grep x src"
        );
        assert_eq!(
            workspace_relative("/p/app2/x /etc/hosts", ws),
            "/p/app2/x /etc/hosts"
        );
        assert_eq!(workspace_relative("/q/p/app/x", ws), "/q/p/app/x");
        assert_eq!(workspace_relative("/p/app/", ws), ".");
    }

    #[test]
    fn labels_match_desktop() {
        assert_eq!(tool_label(&call("read_tool", "other")), "Read file");
        assert_eq!(tool_label(&call("Run shell", "execute")), "Shell");
        assert_eq!(tool_label(&call("multiedit", "edit")), "Edit");
        assert_eq!(tool_label(&call("search_text", "search")), "Search");
        assert_eq!(tool_label(&call("get_function", "other")), "Read function");
        assert_eq!(tool_label(&call("mcp_Shell", "other")), "Shell");
        assert_eq!(humanize_tool_name("run my_tool.name"), "My tool name");
        assert_eq!(humanize_tool_name(""), "Tool call");
    }

    const SKILL_OUTPUT: &str = "<skill_content name=\"rust-skills\">\n\
        <description>Rust guidelines & examples</description>\n\
        <base_path>file:///skills/rust-skills</base_path>\n\
        <content>\n# Rust\n\n```xml\n<content>example</content>\n```\n\n    indented\n</content>\n\
        <sample_files>\n  - references (dir)\n  - SKILL.md (file)\n</sample_files>\n\
        </skill_content>";

    #[test]
    fn skill_result_separates_metadata_from_markdown() {
        let skill = super::SkillContent::parse(SKILL_OUTPUT).expect("valid skill output");
        assert!(!skill.incomplete);
        assert_eq!(skill.name, "rust-skills");
        assert_eq!(skill.description, "Rust guidelines & examples");
        assert_eq!(skill.base_path, "/skills/rust-skills");
        assert_eq!(
            skill.content,
            "# Rust\n\n```xml\n<content>example</content>\n```\n\n    indented"
        );
        assert_eq!(
            skill.sample_files,
            Some("  - references (dir)\n  - SKILL.md (file)")
        );
        // Sample files are optional; neither they nor the outer tags enter the markdown.
        let no_files = SKILL_OUTPUT.replace(
            "<sample_files>\n  - references (dir)\n  - SKILL.md (file)\n</sample_files>\n",
            "",
        );
        let parsed = super::SkillContent::parse(&no_files).expect("skill without files");
        assert_eq!(parsed.sample_files, None);
        assert_eq!(parsed.content, skill.content);
    }

    #[test]
    fn history_clipped_skill_body_still_uses_the_skill_card() {
        // The Rust skill exceeds history's 32 KiB result limit. Reproduce the
        // same byte-prefix clipping (including UTF-8 and markup in examples).
        let header = "<skill_content name=\"rust-skills\">\n\
            <description>Rust guidelines</description>\n\
            <base_path>file:///skills/rust-skills</base_path>\n<content>\n";
        let body = "# Rust\n\n```xml\n<content>example</content>\n```\n".to_string()
            + &"- Ownership and borrowing: 日本語\n".repeat(1400);
        let output = format!("{header}{body}\n</content>\n</skill_content>");
        let mut end = 32 * 1024;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        let clipped = &output[..end];
        let skill = super::SkillContent::parse(clipped).expect("metadata survives clipping");
        assert!(skill.incomplete);
        assert_eq!(skill.name, "rust-skills");
        assert_eq!(skill.description, "Rust guidelines");
        assert_eq!(skill.base_path, "/skills/rust-skills");
        assert_eq!(
            skill.content,
            clipped[header.len()..].trim_end_matches(['\r', '\n'])
        );
        assert!(skill.content.contains("<content>example</content>"));
        assert!(skill.sample_files.is_none());
        let mut tool = call("skill", "other");
        tool.set_output(clipped.into());
        assert!(super::attaches_to_header(&tool));
        assert_eq!(super::tool_preview_raw(&tool), "rust-skills");
        tool.status = "failed".into();
        assert!(!super::attaches_to_header(&tool));
    }

    #[test]
    fn skill_metadata_must_be_intact_before_rendering_a_partial_body() {
        for tag in ["<description>", "<base_path>", "<content>"] {
            let end = SKILL_OUTPUT.find(tag).expect("fixture tag") + tag.len() - 1;
            assert!(super::SkillContent::parse(&SKILL_OUTPUT[..end]).is_none());
        }
    }

    #[test]
    fn malformed_and_failed_skill_results_keep_the_raw_view() {
        for output in [
            "",
            "Permission denied",
            "<skill_content name=\"rust-skills\">",
            "<skill_content name=\"x\"><content>missing metadata</content></skill_content>",
        ] {
            assert!(super::SkillContent::parse(output).is_none());
        }
        let mut skill_call = call("skill", "other");
        skill_call.set_output(SKILL_OUTPUT.into());
        assert_eq!(super::tool_preview_raw(&skill_call), "rust-skills");
        assert!(super::attaches_to_header(&skill_call));
        skill_call.status = "failed".into();
        assert!(!super::attaches_to_header(&skill_call));
        skill_call.status = "completed".into();
        skill_call.set_output("legacy unwrapped skill instructions".into());
        assert!(!super::attaches_to_header(&skill_call));
        // A non-skill tool returning similar markup must retain its own renderer.
        let mut other = call("custom_tool", "other");
        other.set_output(SKILL_OUTPUT.into());
        assert!(!super::attaches_to_header(&other));
    }

    #[test]
    fn picks_attached_views() {
        assert!(is_read(&call("read_tool", "other")));
        assert!(is_read(&call("get_symbol", "other")));
        assert!(!is_read(&call("index", "other")));
        assert!(is_shell(&call("shell", "execute")));
        assert!(!is_shell(&call("read_tool", "other")));
    }
}

/// Frame cost of a transcript with one very large work group (run with
/// `cargo test --release -- --ignored --nocapture transcript_frame_cost`).
#[cfg(test)]
mod perf {
    use gpui_kit::test::TestWindowExt as _;
    use serde_json::json;

    use super::super::session_pane::open_pane_for_test;
    use crate::session::SessionView;

    /// A running turn of `calls` shell calls with realistic JSON results.
    pub(super) fn big_session(calls: usize) -> SessionView {
        let mut view = SessionView::new("test-agent", "s", "/w", "big");
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": { "type": "text", "text": "watch the repo" } }),
        );
        let stdout = format!(
            "## main...origin/main\n{}\n 12 files changed, 340 insertions(+), 120 deletions(-)",
            (0..40)
                .map(|i| format!(" M src/module_{i}/file_{i}.rs"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let output = json!({ "stdout": stdout, "stderr": "", "exit_code": 0 }).to_string();
        for i in 0..calls {
            view.apply(&json!({ "sessionUpdate": "tool_call", "toolCallId": format!("t{i}"),
                "title": "Run shell", "kind": "execute", "status": "completed",
                "rawInput": { "command": "git --no-optional-locks status --short --branch; git diff --name-status; git diff --cached --shortstat" },
                "content": [{ "type": "content", "content": { "type": "text", "text": output } }] }));
        }
        view.phase = "running".into();
        view
    }

    #[gpui_kit::test]
    #[ignore = "timing measurement, run by hand"]
    fn transcript_frame_cost(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, app) = open_pane_for_test(
            cx,
            crate::store::automations::schedule_ui_fixture(),
            big_session(1600),
        );
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, _| {
                // Expand the work group ("+N previous work entries").
                app.expanded.insert("tool-t0".into());
            });
            for _ in 0..3 {
                window.refresh();
                window.render_frame(cx);
            }
            let frames = 60;
            let started = std::time::Instant::now();
            for _ in 0..frames {
                app.update(cx, |app, _| {
                    app.transcript.list.scroll_by(gpui_kit::px(-40.))
                });
                window.refresh();
                window.render_frame(cx);
            }
            // The group really is one row per call, and the frames scrolled through them.
            assert!(app.read(cx).transcript.list.item_count() > 1600);
            let per_frame = started.elapsed().as_secs_f64() * 1000. / f64::from(frames);
            eprintln!("transcript_frame_cost: {per_frame:.1} ms/frame (1600 calls, expanded)");
        })
        .unwrap();
    }
}

#[cfg(test)]
mod compact_tests {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;

    use super::super::session_pane::{SessionPane, open_pane_with_density_for_test};
    use super::Density;
    use super::perf::big_session;

    fn pane(
        cx: &mut gpui_kit::TestAppContext,
        calls: usize,
        density: Density,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<SessionPane>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, pane) = open_pane_with_density_for_test(
            cx,
            crate::store::automations::schedule_ui_fixture(),
            big_session(calls),
            density,
        );
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        (handle, pane)
    }

    fn expand(
        cx: &mut gpui_kit::TestAppContext,
        handle: gpui_kit::AnyWindowHandle,
        pane: &gpui_kit::Entity<SessionPane>,
        key: &str,
    ) {
        cx.update_window(handle, |_, window, cx| {
            pane.update(cx, |pane, cx| {
                pane.expanded.insert(key.into());
                cx.notify();
            });
            window.render_frame(cx);
        })
        .unwrap();
    }

    // quick-panel / Tool call in the panel
    #[gpui_kit::test]
    fn compact_tool_call_is_one_row_until_expanded(cx: &mut gpui_kit::TestAppContext) {
        let (handle, pane) = pane(cx, 1, Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        let row = vcx.debug_bounds("tool-row-t0").expect("the tool row shows");
        assert!(
            f32::from(row.size.height) <= 32.,
            "one line, got {:?}",
            row.size.height
        );
        assert!(
            vcx.debug_bounds("tool-card-t0").is_none(),
            "no output card while collapsed"
        );
        drop(vcx);

        expand(cx, handle, &pane, "t0");
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(
            vcx.debug_bounds("tool-card-t0").is_some(),
            "expands inline when asked"
        );
    }

    #[gpui_kit::test]
    fn compact_running_group_folds_to_its_last_block(cx: &mut gpui_kit::TestAppContext) {
        // Comfortable shows up to three calls of a running turn whole.
        let (handle, _pane) = pane(cx, 3, Density::Comfortable);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        for id in ["tool-row-t0", "tool-row-t1", "tool-row-t2"] {
            assert!(vcx.debug_bounds(id).is_some(), "{id} in a comfortable pane");
        }
        drop(vcx);

        // Compact keeps only the last one behind "+N previous work entries".
        let (handle, _pane) = pane(cx, 3, Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("tool-row-t0").is_none());
        assert!(vcx.debug_bounds("tool-row-t1").is_none());
        assert!(vcx.debug_bounds("tool-row-t2").is_some());
    }

    #[gpui_kit::test]
    fn compact_composer_has_no_options_panel(cx: &mut gpui_kit::TestAppContext) {
        let (handle, _main) = pane(cx, 1, Density::Comfortable);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(
            vcx.debug_bounds("composer-options").is_some(),
            "main window options"
        );
        let comfortable_h = vcx
            .debug_bounds("composer-input")
            .expect("input")
            .size
            .height;
        drop(vcx);
        let (handle, _pane) = pane(cx, 1, Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("composer-options").is_none());
        let compact_h = vcx
            .debug_bounds("composer-input")
            .expect("input")
            .size
            .height;
        assert!(
            compact_h < comfortable_h,
            "{compact_h:?} vs {comfortable_h:?}"
        );
    }

    /// `big_session` where the agent offers Build, Plan and Review.
    fn with_modes(current: &str) -> crate::session::SessionView {
        let mut view = big_session(1);
        view.apply_modes(Some(&serde_json::json!({
            "currentModeId": current,
            "availableModes": [
                { "id": "build", "name": "Build" },
                { "id": "plan", "name": "Plan" },
                { "id": "review", "name": "Review" },
            ],
        })));
        view
    }

    fn pane_of(
        cx: &mut gpui_kit::TestAppContext,
        view: crate::session::SessionView,
        density: Density,
    ) -> gpui_kit::AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, _pane) = open_pane_with_density_for_test(
            cx,
            crate::store::automations::schedule_ui_fixture(),
            view,
            density,
        );
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        handle
    }

    // quick-panel / Mode in a conversation
    #[gpui_kit::test]
    fn compact_composer_shows_the_mode(cx: &mut gpui_kit::TestAppContext) {
        let handle = pane_of(cx, with_modes("plan"), Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        let chip = vcx
            .debug_bounds("composer-mode")
            .expect("a mode chip is shown");
        assert!(
            vcx.debug_bounds("composer-mode:Plan").is_some(),
            "named Plan"
        );
        // The send button follows it, with no model chip between (design D20).
        let send = vcx.debug_bounds("composer-send").expect("the send button");
        assert!(chip.right() <= send.left(), "{chip:?} vs {send:?}");
        drop(vcx);

        // The comfortable composer has the Options control instead.
        let handle = pane_of(cx, with_modes("plan"), Density::Comfortable);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("composer-mode:Plan").is_none());
    }

    #[gpui_kit::test]
    fn no_mode_chip_when_the_agent_offers_no_modes(cx: &mut gpui_kit::TestAppContext) {
        let handle = pane_of(cx, big_session(1), Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("composer-options").is_none());
        assert!(
            vcx.debug_bounds("composer-mode").is_none(),
            "no chip without modes"
        );
    }

    // quick-panel / Mode in a conversation: the chip is the mode's name, not
    // colour alone.
    #[gpui_kit::test]
    fn the_mode_chip_follows_the_session_mode(cx: &mut gpui_kit::TestAppContext) {
        for (mode, selector) in [
            ("build", "composer-mode:Build"),
            ("review", "composer-mode:Review"),
        ] {
            let handle = pane_of(cx, with_modes(mode), Density::Compact);
            let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
            assert!(vcx.debug_bounds(selector).is_some(), "{selector}");
        }
    }

    // quick-panel / Compact conversation: the HUD's composer floats and is lit
    // along its top edge, where the main window's has a border and no highlight.
    #[gpui_kit::test]
    fn compact_composer_is_a_pill_with_a_top_highlight(cx: &mut gpui_kit::TestAppContext) {
        let (handle, _main) = pane(cx, 1, Density::Comfortable);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        assert!(vcx.debug_bounds("composer-highlight").is_none());
        drop(vcx);
        let (handle, _pane) = pane(cx, 1, Density::Compact);
        let mut vcx = gpui_kit::VisualTestContext::from_window(handle, cx);
        let line = vcx
            .debug_bounds("composer-highlight")
            .expect("the highlight shows");
        assert_eq!(f32::from(line.size.height), 1.);
    }

    /// An assistant reply from a view, and a render context at `density`.
    fn agent_reply(
        cx: &mut gpui_kit::TestAppContext,
        density: Density,
    ) -> (crate::session::Item, super::RenderCtx) {
        use std::collections::{HashMap, HashSet};
        let (_, pane) = pane(cx, 1, density);
        let mut view = crate::session::SessionView::new("test-agent", "s", "/w", "t");
        view.apply(&serde_json::json!({ "sessionUpdate": "agent_message_chunk",
            "messageId": "a1", "content": { "type": "text", "text": "an answer" } }));
        let item = view.items.last().expect("the reply").clone();
        let rc = cx.update(|cx| super::RenderCtx {
            p: *crate::theme::palette(cx),
            syntax: gpui_kit::component::ActiveTheme::theme(&*cx)
                .highlight_theme
                .clone(),
            weak: pane.downgrade(),
            density,
            smooth_scroll: Default::default(),
            expanded: HashSet::new(),
            running: false,
            last_turn_start: 0,
            last_ix: 0,
            turn_end: HashMap::new(),
            copied: None,
            block_scrolls: Default::default(),
            cwd: String::new(),
            turn_user: HashMap::new(),
            undo_ready: false,
            undoable: HashSet::new(),
            redo_target: None,
            fork_points: HashSet::new(),
            diff_open: None,
            reverted_from: None,
            agent_id: "test-agent".into(),
            stream_fade: false,
            md: Default::default(),
            tool_content: false,
            developer: false,
        });
        (item, rc)
    }

    // quick-panel / Looks like a HUD: assistant text without a card.
    #[gpui_kit::test]
    fn compact_assistant_message_has_no_card(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::Styled as _;
        let (item, rc) = agent_reply(cx, Density::Compact);
        let super::ItemKind::Agent { text } = &item.kind else {
            panic!("an assistant message");
        };
        let mut message = super::agent_message(0, &item, text, None, &rc);
        let style = message.style();
        assert!(style.background.is_none(), "no fill");
        assert!(style.border_color.is_none(), "no border");
        assert!(style.corner_radii.top_left.is_none(), "no rounded card");
    }

    #[gpui_kit::test]
    fn compact_prose_uses_the_system_font_at_hud_size(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::Styled as _;
        for (density, size) in [(Density::Compact, Some(14.5)), (Density::Comfortable, None)] {
            let (item, rc) = agent_reply(cx, density);
            let super::ItemKind::Agent { text } = &item.kind else {
                panic!("an assistant message");
            };
            let mut message = super::agent_message(0, &item, text, None, &rc);
            let text_style = &message.style().text;
            let font = text_style.font_family.as_ref().map(|f| f.as_ref());
            match size {
                Some(px) => {
                    assert_eq!(font, Some(crate::ui::kit::SYSTEM_UI));
                    assert_eq!(
                        text_style.font_size,
                        Some(gpui_kit::AbsoluteLength::Pixels(gpui_kit::px(px)))
                    );
                }
                None => assert_eq!(font, None, "the main window keeps the theme's font"),
            }
        }
    }

    // quick-panel / Transcript uses the panel's colours
    #[cfg(not(target_family = "wasm"))]
    #[gpui_kit::test]
    fn a_pane_given_the_hud_palette_paints_its_transcript_with_it(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use crate::quick::hud::hud_palette;
        let (handle, pane) = pane(cx, 2, Density::Compact);
        let global = cx.update(|cx| *crate::theme::palette(cx));
        let hud = hud_palette(&crate::theme::Palette::light(), false);
        assert_ne!(hud, global);
        let render = |cx: &mut gpui_kit::TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.refresh();
                window.render_frame(cx)
            })
            .unwrap()
        };
        let unset = pane.read_with(cx, |p, _| p.transcript.sigs.clone());
        assert!(!unset.is_empty());

        // A pane without a palette uses the app's.
        assert_eq!(pane.read_with(cx, |p, cx| p.palette(cx)), global);

        // With one, the rows are built from it: their signatures change.
        pane.update(cx, |p, cx| p.set_palette(Some(hud), cx));
        render(cx);
        assert_eq!(pane.read_with(cx, |p, cx| p.palette(cx)), hud);
        let themed = pane.read_with(cx, |p, _| p.transcript.sigs.clone());
        assert_ne!(themed, unset, "rows were rebuilt with the HUD palette");

        // Back to none: the app's palette again, the same rows as before.
        pane.update(cx, |p, cx| p.set_palette(None, cx));
        render(cx);
        assert_eq!(pane.read_with(cx, |p, cx| p.palette(cx)), global);
        assert_eq!(pane.read_with(cx, |p, _| p.transcript.sigs.clone()), unset);
    }

    // quick-panel / Transcript uses the panel's colours: code and markdown take
    // the palette they are given instead of reading the app's.
    #[test]
    fn code_markdown_and_diffs_take_the_palette_they_are_given() {
        use gpui_kit::Styled as _;
        let mut p = crate::theme::Palette::dark();
        p.bg = gpui_kit::red();
        p.muted = gpui_kit::green();
        p.border = gpui_kit::blue();
        let mut block = super::code_block("let x = 1;".into(), &p);
        let style = block.style();
        assert_eq!(style.background, Some(p.bg.into()));
        assert_eq!(style.border_color, Some(p.border));
        assert_eq!(style.text.color, Some(p.muted));

        // The markdown view is handed the palette (no global read to override).
        let md = super::md::Kept::default().markdown("m", "`code`", &p);
        assert_eq!(md.palette(), &p);

        // Diff rows are built from the palette: tinted from its success and
        // danger colours, so they are as many rows as lines either way.
        let diff = crate::session::Diff {
            path: "a.rs".into(),
            old: Some("a\nb\n".into()),
            new: "a\nc\n".into(),
            old_start: None,
            new_start: None,
            snippet: false,
        };
        let (rows, adds, dels, _) = super::diff_rows(&diff, 50, &p);
        assert_eq!((adds, dels), (1, 1));
        assert!(!rows.is_empty());
    }

    #[test]
    fn compact_limits_are_below_comfortable() {
        let (c, k) = (Density::Comfortable, Density::Compact);
        assert!(k.output_lines() < c.output_lines());
        assert!(k.read_lines() < c.read_lines());
        assert!(k.diff_lines() < c.diff_lines());
        assert!(k.diff_block_h() < c.diff_block_h());
        assert!(k.column_max() < c.column_max());
        assert!(k.hover_actions() && !c.hover_actions());
        assert!(!k.has_panels() && c.has_panels());
        // One line at rest, growing to six; the main window's composer is
        // taller at rest.
        assert_eq!(k.composer_lines(), (1, 6));
        assert!(k.composer_lines().0 < c.composer_lines().0);
        assert!(k.composer_lines().1 < c.composer_lines().1);
    }

    #[gpui_kit::test]
    #[ignore = "timing measurement, run by hand"]
    fn compact_frame_cost_stays_within_comfortable(cx: &mut gpui_kit::TestAppContext) {
        fn measure(cx: &mut gpui_kit::TestAppContext, density: Density) -> f64 {
            let (handle, pane) = pane(cx, 1600, density);
            cx.update_window(handle, |_, window, cx| {
                pane.update(cx, |pane, _| {
                    pane.expanded.insert("tool-t0".into());
                });
                for _ in 0..3 {
                    window.refresh();
                    window.render_frame(cx);
                }
                let frames = 60;
                let started = std::time::Instant::now();
                for _ in 0..frames {
                    pane.update(cx, |pane, _| {
                        pane.transcript.list.scroll_by(gpui_kit::px(-40.))
                    });
                    window.refresh();
                    window.render_frame(cx);
                }
                started.elapsed().as_secs_f64() * 1000. / f64::from(frames)
            })
            .unwrap()
        }
        let comfortable = measure(cx, Density::Comfortable);
        let compact = measure(cx, Density::Compact);
        eprintln!(
            "compact_frame_cost: comfortable {comfortable:.1} ms/frame, compact {compact:.1} ms/frame"
        );
        assert!(
            compact <= comfortable * 1.1,
            "compact {compact:.1} ms vs comfortable {comfortable:.1} ms"
        );
    }
}

#[cfg(test)]
mod split_tests {
    use serde_json::{Value, json};

    use super::super::session_pane::{SessionPane, open_pane_for_test};
    use super::perf::big_session;

    struct Harness {
        handle: gpui_kit::AnyWindowHandle,
        app: gpui_kit::Entity<SessionPane>,
        rpc: std::rc::Rc<crate::rpc::RpcClient>,
        requests: async_channel::Receiver<String>,
    }

    fn app_with(
        cx: &mut gpui_kit::TestAppContext,
        view: crate::session::SessionView,
        methods: Value,
    ) -> Harness {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (rpc, requests) = store.rpc_for_test("test-agent");
        store.runtime_mut_for_test("test-agent").capabilities = Some(json!({ "methods": methods }));
        let (handle, app) = open_pane_for_test(cx, store, view);
        Harness {
            handle,
            app,
            rpc,
            requests,
        }
    }

    #[gpui_kit::test]
    fn an_expanded_large_group_becomes_one_row_per_block(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        use gpui_kit::test::TestWindowExt as _;
        let Harness { handle, app, .. } = app_with(cx, big_session(300), json!([]));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            // Running and collapsed: the user prompt, then one row with the
            // "+N previous" toggle and the last two calls.
            let rows = |app: &gpui_kit::Entity<SessionPane>, cx: &mut gpui_kit::App| {
                app.read(cx).transcript.list.item_count()
            };
            assert_eq!(rows(&app, cx), 2);
            app.update(cx, |app, cx| {
                app.expanded.insert("tool-t0".into());
                cx.notify();
            });
            window.render_frame(cx);
            // Expanded: the prompt, the header, and a row per call.
            assert_eq!(rows(&app, cx), 1 + 1 + 300);
            app.update(cx, |app, cx| {
                app.expanded.remove("tool-t0");
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(rows(&app, cx), 2);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn shortened_output_is_fetched_whole(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let mut view = big_session(3);
        // A page with no events still marks the shortened tool calls it names.
        view.history_page(&json!({
            "events": [],
            "history": { "complete": true },
            "truncated": [
                { "tool_call_id": "t1", "part": "result", "full_bytes": 50000 },
                { "tool_call_id": "t2", "part": "arguments", "path": "/command", "full_bytes": 9000 }
            ],
        }));
        let t1 = view.truncated_tool("t1").unwrap();
        assert_eq!(t1.truncated_result, Some(50000));
        assert_eq!(
            view.truncated_tool("t2").unwrap().truncated_args,
            Some(9000)
        );
        use gpui_kit::AppContext as _;
        let Harness {
            handle,
            app,
            rpc,
            requests,
        } = app_with(cx, view, json!(["querymt/session/toolContent"]));
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, _| {
                app.expanded.insert("t1".into());
            });
            window.render_frame(cx);
            app.update(cx, |app, cx| app.load_tool_content("t1", "result", cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        let request: Value = serde_json::from_str(&requests.try_recv().unwrap()).unwrap();
        assert_eq!(request["method"], "_querymt/session/toolContent");
        assert_eq!(request["params"]["toolCallId"], "t1");
        assert_eq!(request["params"]["part"], "result");
        let full =
            json!({ "stdout": "the whole output", "stderr": "", "exit_code": 0 }).to_string();
        rpc.handle(&json!({ "id": request["id"], "result": { "content": full } }).to_string());
        cx.run_until_parked();
        cx.update(|cx| {
            let call = app.read(cx).view(cx).unwrap().truncated_tool("t1").unwrap();
            assert_eq!(call.truncated_result, None);
            assert_eq!(call.pretty_output(), "the whole output");
            assert!(call.fetch.is_none());
        });
    }
}

/// Export the transcript metrics of a large session to a collector (run with
/// `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo test -- --ignored
/// --nocapture transcript_telemetry_export`).
#[cfg(test)]
mod perf_export {
    use gpui_kit::test::TestWindowExt as _;

    use super::super::session_pane::open_pane_for_test;
    use super::perf::big_session;

    #[gpui_kit::test]
    #[ignore = "needs a running OTLP collector"]
    fn transcript_telemetry_export(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        assert!(
            crate::telemetry::init().is_some(),
            "set OTEL_EXPORTER_OTLP_ENDPOINT"
        );
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, app) = open_pane_for_test(
            cx,
            crate::store::automations::schedule_ui_fixture(),
            big_session(1600),
        );
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, _| {
                app.expanded.insert("tool-t0".into());
            });
            for _ in 0..120 {
                app.update(cx, |app, _| {
                    app.transcript.list.scroll_by(gpui_kit::px(-40.))
                });
                window.refresh();
                window.render_frame(cx);
            }
        })
        .unwrap();
        crate::telemetry::shutdown();
    }
}

#[cfg(test)]
mod more_lines_tests {
    use super::clipped;

    #[test]
    fn clipped_keeps_the_first_lines() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(clipped(text, 2), "a\nb");
        assert_eq!(clipped(text, 4), "a\nb\nc\nd");
        assert_eq!(clipped(text, usize::MAX), "a\nb\nc\nd");
        assert_eq!(clipped("one", 1), "one");
    }
}

#[cfg(test)]
mod find_tests {
    use gpui_kit::AppContext as _;
    use gpui_kit::test::TestWindowExt as _;
    use serde_json::json;

    use super::super::find;
    use super::super::session_pane::{SessionPane, open_pane_for_test};
    use crate::session::SessionView;

    /// A user message with front matter, then a reply.
    fn conversation() -> SessionView {
        let mut view = SessionView::new("test-agent", "s", "/w", "find");
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": { "type": "text",
                "text": "---\nsource: cron\n---\n\n  check the **token**  \n" } }),
        );
        view.apply(
            &json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a1",
            "content": { "type": "text", "text": "the token is *fine*" } }),
        );
        view
    }

    fn open(
        cx: &mut gpui_kit::TestAppContext,
        view: SessionView,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<SessionPane>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let (handle, pane) =
            open_pane_for_test(cx, crate::store::automations::schedule_ui_fixture(), view);
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        (handle, pane)
    }

    // add-session-find: the index reads what the renderers build.
    #[gpui_kit::test]
    fn views_of_matches_what_the_rows_build(cx: &mut gpui_kit::TestAppContext) {
        let (_handle, pane) = open(cx, conversation());
        let (mut built, mut expected) = cx.update(|cx| {
            let pane = pane.read(cx);
            let items = &pane.view(cx).expect("a session").items;
            let expected: Vec<(String, String)> = items.iter().flat_map(find::views_of).collect();
            (pane.transcript.md.built(), expected)
        });
        built.sort();
        expected.sort();
        assert_eq!(expected.len(), 2, "a user message and a reply");
        assert_eq!(built, expected);
        let user = expected.iter().find(|(id, _)| id == "umd-user-u1").unwrap();
        assert_eq!(user.1, "check the **token**", "front matter is split off");
    }
}
