//! Find in session: the data half (design D1 to D3 of `add-session-find`).
//!
//! The transcript is virtualized, so rows outside the viewport have no
//! elements and no `TextViewState`. Counting and stepping therefore come from
//! data: [`FindIndex`] reads each item's Markdown source through
//! [`views_of`] and [`searchable_text`], and counts matches with
//! [`match_ranges`]. The painting half (`md::Kept`) searches the rendered text
//! of the views that are built with the same matcher, so both halves agree.
//!
//! Nothing here creates an element, and nothing runs while find is closed.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::{Disableable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use markdown::mdast::Node;

use super::kit;
use super::md::FindQuery;
use super::session_pane::SessionPane;
use crate::session::{Item, ItemKind};
use crate::theme::Palette;

// ---------------------------------------------------------------------------
// Matcher (D2)
// ---------------------------------------------------------------------------

/// One element of a folded query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    /// A run of whitespace in the query: matches one or more spaces or tabs.
    Space,
    /// A lower-cased character.
    Char(char),
}

/// A query folded once, to match many texts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matcher {
    tokens: Vec<Tok>,
}

impl Matcher {
    /// `None` for a query with nothing but whitespace.
    pub fn new(query: &str) -> Option<Self> {
        if query.trim().is_empty() {
            return None;
        }
        let mut tokens = Vec::new();
        for c in query.chars() {
            if c.is_whitespace() {
                if tokens.last() != Some(&Tok::Space) {
                    tokens.push(Tok::Space);
                }
            } else {
                tokens.extend(c.to_lowercase().map(Tok::Char));
            }
        }
        Some(Self { tokens })
    }

    /// Non-overlapping matches in `text`, as byte ranges on char boundaries.
    pub fn ranges(&self, text: &str) -> Vec<Range<usize>> {
        // Each lower-cased char with the byte range of the char it came from.
        let mut folded: Vec<(char, usize, usize)> = Vec::with_capacity(text.len());
        for (start, c) in text.char_indices() {
            let end = start + c.len_utf8();
            folded.extend(c.to_lowercase().map(|lc| (lc, start, end)));
        }
        let mut out = Vec::new();
        let mut last_end = 0;
        let mut i = 0;
        while i < folded.len() {
            match self.match_at(&folded, i) {
                Some(end) if end > i => {
                    let start = folded[i].1.max(last_end);
                    let stop = folded[end - 1].2;
                    if start < stop {
                        out.push(start..stop);
                        last_end = stop;
                    }
                    i = end;
                }
                _ => i += 1,
            }
        }
        out
    }

    /// How many matches `text` holds.
    pub fn count(&self, text: &str) -> usize {
        self.ranges(text).len()
    }

    /// The folded index one past the match starting at `at`, if there is one.
    fn match_at(&self, folded: &[(char, usize, usize)], at: usize) -> Option<usize> {
        let mut j = at;
        for token in &self.tokens {
            match token {
                Tok::Char(c) => {
                    if folded.get(j)?.0 != *c {
                        return None;
                    }
                    j += 1;
                }
                Tok::Space => {
                    let mut n = 0;
                    while folded.get(j).is_some_and(|f| f.0 == ' ' || f.0 == '\t') {
                        j += 1;
                        n += 1;
                    }
                    if n == 0 {
                        return None;
                    }
                }
            }
        }
        Some(j)
    }
}

/// Matches of `query` in `text`: case-insensitive, non-overlapping. A run of
/// whitespace in the query matches spaces and tabs, never a newline, so a
/// match does not cross a block boundary. An empty query matches nothing.
#[cfg(test)]
pub fn match_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    Matcher::new(query).map_or_else(Vec::new, |m| m.ranges(text))
}

// ---------------------------------------------------------------------------
// Searchable text (D2)
// ---------------------------------------------------------------------------

/// The text a `TextViewState` renders for Markdown `source`, rebuilt from the
/// Markdown tree: one line per leaf block, inline syntax removed. A corpus
/// test (`md.rs`) renders fixtures and requires the same match counts.
pub fn searchable_text(source: &str) -> String {
    let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
        return source.to_string();
    };
    let mut out = String::new();
    push_block(&root, source, &mut out);
    out
}

fn push_block(node: &Node, source: &str, out: &mut String) {
    match node {
        Node::Root(_)
        | Node::Blockquote(_)
        | Node::List(_)
        | Node::ListItem(_)
        | Node::FootnoteDefinition(_) => {
            for child in node.children().into_iter().flatten() {
                push_block(child, source, out);
            }
        }
        Node::Paragraph(_) | Node::Heading(_) => {
            let mut text = String::new();
            for child in node.children().into_iter().flatten() {
                push_inline(child, &mut text);
            }
            push_leaf(&text, out);
        }
        // Tables are custom blocks whose text is their source (`md::styled`).
        Node::Table(table) => {
            let slice = table
                .position
                .as_ref()
                .and_then(|p| source.get(p.start.offset..p.end.offset));
            push_leaf(slice.unwrap_or_default(), out);
        }
        Node::Code(code) => push_leaf(&code.value, out),
        _ => {}
    }
}

fn push_leaf(text: &str, out: &mut String) {
    if !text.is_empty() {
        out.push_str(text);
        out.push('\n');
    }
}

fn push_inline(node: &Node, out: &mut String) {
    match node {
        // A soft break renders as a space.
        Node::Text(t) => out.push_str(&t.value.replace('\n', " ")),
        Node::InlineCode(t) => out.push_str(&t.value),
        Node::Break(_) => out.push('\n'),
        // Inline tags render as nothing; the text between them is Text.
        Node::Html(_) => {}
        _ => {
            for child in node.children().into_iter().flatten() {
                push_inline(child, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Views (D2): the one table both the renderers and the index read
// ---------------------------------------------------------------------------

/// Id of the Markdown view of a user message.
pub fn user_view_id(item_key: &str) -> String {
    format!("umd-{item_key}")
}

/// Id of the Markdown view of an agent reply.
pub fn reply_view_id(item_key: &str) -> String {
    format!("md-{item_key}")
}

/// The part of a user message shown in its bubble: front matter split off,
/// trimmed.
pub fn user_body(text: &str) -> &str {
    crate::util::split_front_matter(text)
        .map_or(text, |(_, body)| body)
        .trim()
}

/// The Markdown views `item` shows, as `(view id, source)` in display order.
pub fn views_of(item: &Item) -> Vec<(String, String)> {
    match &item.kind {
        ItemKind::User { text, command, .. } => {
            // A command bubble draws `/name` apart and the arguments as
            // Markdown; the arguments are what find can search.
            let body = match command {
                Some(command) => command.arguments.as_str(),
                None => user_body(text),
            };
            if body.is_empty() {
                Vec::new()
            } else {
                vec![(user_view_id(&item.key), body.to_string())]
            }
        }
        ItemKind::Agent { text } => vec![(reply_view_id(&item.key), text.clone())],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Index (D1, D3)
// ---------------------------------------------------------------------------

/// One match, addressed by what survives a recompute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub item_key: Arc<str>,
    pub view_id: Arc<str>,
    /// The k-th match in the view's text.
    pub occurrence: usize,
}

/// Where a match sits in the index, in transcript order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
    item: usize,
    view: usize,
    occurrence: usize,
}

struct ViewEntry {
    id: Arc<str>,
    text: Arc<str>,
    count: usize,
}

struct ItemEntry {
    key: Arc<str>,
    rev: u64,
    views: Vec<ViewEntry>,
    /// Views were counted for the current query.
    counted: bool,
}

impl ItemEntry {
    fn read(item: &Item) -> Self {
        Self {
            key: item.key.as_str().into(),
            rev: item.rev,
            views: views_of(item)
                .into_iter()
                .map(|(id, source)| ViewEntry {
                    id: id.into(),
                    text: searchable_text(&source).into(),
                    count: 0,
                })
                .collect(),
            counted: false,
        }
    }
}

/// Matches of a query over a transcript's items, in transcript order, with a
/// current match that keeps its identity while the transcript changes.
#[derive(Default)]
pub struct FindIndex {
    query: String,
    matcher: Option<Matcher>,
    entries: Vec<ItemEntry>,
    slots: Vec<Slot>,
    current: Option<FindMatch>,
    current_slot: Option<usize>,
    #[cfg(test)]
    reads: usize,
}

impl FindIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// The query is not blank, so matches are counted and painted.
    pub fn active(&self) -> bool {
        self.matcher.is_some()
    }

    pub fn total(&self) -> usize {
        self.slots.len()
    }

    /// 1-based position of the current match.
    pub fn position(&self) -> Option<usize> {
        self.current_slot.map(|ix| ix + 1)
    }

    pub fn current(&self) -> Option<&FindMatch> {
        self.current.as_ref()
    }

    /// `n of m`, `No results`, or nothing for an empty query.
    pub fn label(&self) -> Option<String> {
        self.matcher.as_ref()?;
        Some(match self.position() {
            Some(n) => format!("{n} of {}", self.total()),
            None => "No results".to_string(),
        })
    }

    /// Items read since the last call (tests).
    #[cfg(test)]
    pub fn take_reads(&mut self) -> usize {
        std::mem::take(&mut self.reads)
    }

    /// Change the query. The current match becomes the first one in an item at
    /// or after `from_item` (the item at the top of the viewport), else the
    /// first overall.
    pub fn set_query(&mut self, query: &str, items: &[Item], from_item: usize) {
        self.query = query.to_string();
        self.matcher = Matcher::new(query);
        self.current = None;
        self.current_slot = None;
        for entry in &mut self.entries {
            entry.counted = false;
        }
        self.refresh_entries(items);
        self.recount();
        self.rebuild();
        let first = self
            .slots
            .iter()
            .position(|s| s.item >= from_item)
            .or((!self.slots.is_empty()).then_some(0));
        self.select(first);
    }

    /// The items changed (a notify of the session): re-read only the items
    /// whose revision changed, keeping the current match where it still is.
    pub fn sync(&mut self, items: &[Item]) {
        self.refresh_entries(items);
        self.recount();
        self.rebuild();
    }

    /// Move to the next match, wrapping at the end.
    pub fn next(&mut self) {
        let n = self.slots.len();
        if n == 0 {
            return;
        }
        let ix = self.current_slot.map_or(0, |ix| (ix + 1) % n);
        self.select(Some(ix));
    }

    /// Move to the previous match, wrapping at the start.
    pub fn prev(&mut self) {
        let n = self.slots.len();
        if n == 0 {
            return;
        }
        let ix = self.current_slot.map_or(n - 1, |ix| (ix + n - 1) % n);
        self.select(Some(ix));
    }

    fn select(&mut self, slot: Option<usize>) {
        self.current_slot = slot;
        self.current = slot.map(|ix| self.match_at(self.slots[ix]));
    }

    fn match_at(&self, slot: Slot) -> FindMatch {
        let entry = &self.entries[slot.item];
        FindMatch {
            item_key: entry.key.clone(),
            view_id: entry.views[slot.view].id.clone(),
            occurrence: slot.occurrence,
        }
    }

    /// Make `entries` line up with `items`, reading only what is new or whose
    /// revision changed.
    fn refresh_entries(&mut self, items: &[Item]) {
        let same_keys = self.entries.len() == items.len()
            && self
                .entries
                .iter()
                .zip(items)
                .all(|(e, i)| *e.key == *i.key);
        if same_keys {
            for (entry, item) in self.entries.iter_mut().zip(items) {
                if entry.rev != item.rev {
                    *entry = ItemEntry::read(item);
                    #[cfg(test)]
                    {
                        self.reads += 1;
                    }
                }
            }
            return;
        }
        // Items were added or prepended: reuse entries by key.
        let mut old: HashMap<Arc<str>, ItemEntry> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(|e| (e.key.clone(), e))
            .collect();
        for item in items {
            let entry = match old.remove(item.key.as_str()) {
                Some(e) if e.rev == item.rev => e,
                _ => {
                    #[cfg(test)]
                    {
                        self.reads += 1;
                    }
                    ItemEntry::read(item)
                }
            };
            self.entries.push(entry);
        }
    }

    /// Count the views whose count is not current for the query.
    fn recount(&mut self) {
        for entry in &mut self.entries {
            if entry.counted {
                continue;
            }
            for view in &mut entry.views {
                view.count = self.matcher.as_ref().map_or(0, |m| m.count(&view.text));
            }
            entry.counted = true;
        }
    }

    /// Rebuild the ordered slot list and locate the current match again.
    fn rebuild(&mut self) {
        let old_slot = self.current_slot;
        self.slots.clear();
        for (item, entry) in self.entries.iter().enumerate() {
            for (view, v) in entry.views.iter().enumerate() {
                for occurrence in 0..v.count {
                    self.slots.push(Slot {
                        item,
                        view,
                        occurrence,
                    });
                }
            }
        }
        let Some(current) = self.current.clone() else {
            self.current_slot = None;
            return;
        };
        let found = self.slots.iter().position(|s| self.match_at(*s) == current);
        let slot = found.or_else(|| self.successor_of(&current, old_slot));
        self.select(slot);
    }

    /// The first match after where `gone` was; the last match if none is.
    fn successor_of(&self, gone: &FindMatch, old_slot: Option<usize>) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let last = self.slots.len() - 1;
        let Some(item) = self.entries.iter().position(|e| e.key == gone.item_key) else {
            return Some(old_slot.unwrap_or(0).min(last));
        };
        let view = self.entries[item]
            .views
            .iter()
            .position(|v| v.id == gone.view_id)
            .unwrap_or(0);
        let at = Slot {
            item,
            view,
            occurrence: gone.occurrence,
        };
        self.slots.iter().position(|s| *s >= at).or(Some(last))
    }
}

// ---------------------------------------------------------------------------
// Find bar (D4, D5)
// ---------------------------------------------------------------------------

/// The open find bar of a [`SessionPane`]: the query field and the index it
/// drives. The pane owns it; closing the bar drops it.
pub(super) struct FindBar {
    index: FindIndex,
    input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl FindBar {
    /// `n of m`, `No results` or nothing, as the bar shows it.
    #[cfg(test)]
    pub(super) fn label(&self) -> Option<String> {
        self.index.label()
    }

    #[cfg(test)]
    pub(super) fn current(&self) -> Option<FindMatch> {
        self.index.current().cloned()
    }

    #[cfg(test)]
    pub(super) fn input(&self) -> &Entity<InputState> {
        &self.input
    }
}

impl SessionPane {
    /// Cmd/Ctrl+F: open the find bar over the transcript, or focus its query
    /// and select it when it is open. Does nothing without a transcript.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view(cx).is_none_or(|v| v.items.is_empty()) {
            return;
        }
        if let Some(bar) = &self.find {
            bar.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in session"));
        let subs =
            vec![
                cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::Change => this.find_query_changed(cx),
                    InputEvent::PressEnter { shift, .. } => this.find_step(!*shift, cx),
                    _ => {}
                }),
            ];
        input.update(cx, |input, cx| input.focus(window, cx));
        self.find = Some(FindBar {
            index: FindIndex::new(),
            input,
            _subs: subs,
        });
        cx.notify();
    }

    /// Close the bar and remove every highlight. Scroll and expansion stay.
    pub(crate) fn close_find(&mut self, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            self.transcript.md.set_find(None, cx);
            cx.notify();
        }
    }

    /// Close the bar and hand the focus back: to the composer, or to the
    /// window when the session takes no input.
    pub(crate) fn close_find_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_find(cx);
        if self.view(cx).is_some_and(|v| v.access.is_none()) {
            self.focus_composer(window, cx);
        } else {
            window.blur(cx);
        }
    }

    /// The find bar, when it is open.
    #[cfg(test)]
    pub(super) fn find_bar_state(&self) -> Option<&FindBar> {
        self.find.as_ref()
    }

    fn find_query_changed(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let from = self.transcript.top_item();
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        let query = bar.input.read(cx).value().to_string();
        // Enter also reports a change of an unchanged text; it is a step,
        // not a new query.
        if query == bar.index.query() {
            return;
        }
        bar.index
            .set_query(&query, &session.read(cx).view.items, from);
        self.find_apply(true, cx);
    }

    /// Enter / Shift+Enter and the bar's arrows.
    pub(crate) fn find_step(&mut self, next: bool, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if next {
            bar.index.next();
        } else {
            bar.index.prev();
        }
        self.find_apply(true, cx);
    }

    /// The session changed (streaming): update the matches while the bar is
    /// open, without moving the transcript.
    pub(super) fn sync_find(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        bar.index.sync(&session.read(cx).view.items);
        self.find_apply(false, cx);
    }

    /// Hand the query and the current match to the kept views and, when
    /// `scroll`, bring the current match's row into view (D4).
    fn find_apply(&mut self, scroll: bool, cx: &mut Context<Self>) {
        let Some(bar) = &self.find else {
            return;
        };
        let current = bar.index.current().cloned();
        let query = bar.index.active().then(|| {
            FindQuery::new(
                bar.index.query(),
                current
                    .as_ref()
                    .map(|m| (m.view_id.to_string(), m.occurrence)),
                &self.palette(cx),
            )
        });
        self.transcript.md.set_find(query, cx);
        if scroll
            && let Some(m) = current
            && let Some(row) = self.transcript.row_of_item(&m.item_key)
        {
            self.transcript.list.scroll_to_reveal_item(row);
            self.transcript.list.pause_following_tail();
        }
        cx.notify();
    }

    /// The bar floating over the transcript's top right corner. It is a
    /// popover-class surface like the app's pickers: `popover` fill, a
    /// hairline, the overlay shadow, and it occludes the list under it.
    pub(super) fn find_bar(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bar = self.find.as_ref()?;
        let label = bar.index.label().unwrap_or_default();
        let none = bar.index.total() == 0;
        Some(
            h_flex()
                .id("find-bar")
                .absolute()
                .top_2()
                .right_4()
                .w(px(340.))
                .h(px(40.))
                .pl_3()
                .pr_1()
                .gap_1()
                .items_center()
                .rounded(px(12.))
                .bg(p.popover)
                .border_1()
                .border_color(p.border)
                .when(p.overlay_shadow(), |el| el.shadow_lg())
                .occlude()
                // Escape closes the bar before the app's double-Escape
                // tracker sees the key (as `workspace_select` does).
                .capture_action(cx.listener(|this, _: &Escape, window, cx| {
                    this.close_find_and_focus(window, cx);
                    cx.stop_propagation();
                }))
                .child(
                    Input::new(&bar.input)
                        .appearance(false)
                        .text_color(p.text)
                        .aria_label("Find in session")
                        .flex_1()
                        .min_w_0(),
                )
                .child(
                    div()
                        .id("find-label")
                        .flex_none()
                        .min_w(px(64.))
                        .text_xs()
                        .text_right()
                        .text_color(p.dim)
                        .child(label),
                )
                .child(
                    kit::icon_button(
                        "find-prev",
                        IconName::ChevronUp,
                        "Previous match (Shift+Enter)",
                    )
                    .disabled(none)
                    .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx))),
                )
                .child(
                    kit::icon_button("find-next", IconName::ChevronDown, "Next match (Enter)")
                        .disabled(none)
                        .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx))),
                )
                .child(
                    kit::icon_button("find-close", IconName::X, "Close (Esc)").on_click(
                        cx.listener(|this, _, window, cx| this.close_find_and_focus(window, cx)),
                    ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{FindIndex, Item, ItemKind, match_ranges, searchable_text, views_of};
    use crate::session::UserCommand;

    fn text_of(text: &str, query: &str) -> Vec<String> {
        match_ranges(text, query)
            .into_iter()
            .map(|r| text[r].to_string())
            .collect()
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(text_of("Fix the Parser", "parser"), ["Parser"]);
        assert_eq!(text_of("Fix the Parser", "PARSER"), ["Parser"]);
    }

    #[test]
    fn matches_repeat_and_do_not_overlap() {
        assert_eq!(match_ranges("abab ab", "ab"), [0..2, 2..4, 5..7]);
        assert_eq!(match_ranges("aaa", "aa").len(), 1);
        assert_eq!(match_ranges("aaa", "aa")[0], 0..2);
        assert_eq!(match_ranges("aaaa", "aa"), [0..2, 2..4]);
    }

    #[test]
    fn empty_and_blank_queries_match_nothing() {
        assert!(match_ranges("abc", "").is_empty());
        assert!(match_ranges("a b", "  ").is_empty());
    }

    #[test]
    fn a_whitespace_run_matches_spaces_and_tabs() {
        assert_eq!(text_of("retry  \t loop", "retry loop").len(), 1);
        assert_eq!(text_of("retry loop", "retry   loop").len(), 1);
        assert!(text_of("retryloop", "retry loop").is_empty());
    }

    #[test]
    fn a_whitespace_run_does_not_match_a_newline() {
        assert!(text_of("retry\nloop", "retry loop").is_empty());
        assert_eq!(text_of("retry\nloop", "loop").len(), 1);
    }

    #[test]
    fn offsets_stay_on_char_boundaries() {
        let text = "Straße and STRAßE, 中文字 and 中文";
        let ranges = match_ranges(text, "straße");
        assert_eq!(ranges.len(), 2);
        for r in &ranges {
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        }
        assert_eq!(&text[ranges[0].clone()], "Straße");
        assert_eq!(text_of(text, "中文"), ["中文", "中文"]);
        // A fold that grows: `İ` lower-cases to two chars.
        let dotted = "xİy";
        for r in match_ranges(dotted, "y") {
            assert!(dotted.is_char_boundary(r.start) && dotted.is_char_boundary(r.end));
        }
    }

    #[test]
    fn searchable_text_drops_markdown_syntax() {
        assert_eq!(searchable_text("the **retry** loop"), "the retry loop\n");
        assert!(match_ranges(&searchable_text("the **retry** loop"), "**").is_empty());
        assert_eq!(
            match_ranges(&searchable_text("the **retry** loop"), "retry loop").len(),
            1
        );
    }

    fn agent(key: &str, rev: u64, text: &str) -> Item {
        Item {
            key: key.into(),
            rev,
            kind: ItemKind::Agent { text: text.into() },
        }
    }

    fn user(key: &str, text: &str) -> Item {
        Item {
            key: key.into(),
            rev: 1,
            kind: ItemKind::User {
                text: text.into(),
                attachments: Vec::new(),
                local: false,
                command: None,
            },
        }
    }

    fn command(key: &str, name: &str, arguments: &str) -> Item {
        let typed = if arguments.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arguments}")
        };
        Item {
            key: key.into(),
            rev: 1,
            kind: ItemKind::User {
                text: typed,
                attachments: Vec::new(),
                local: false,
                command: Some(UserCommand {
                    name: name.into(),
                    arguments: arguments.into(),
                    expansion: "The expanded template the model received.".into(),
                }),
            },
        }
    }

    // slash-command-transcript: a command bubble draws `/name` apart and the
    // arguments as Markdown, so the arguments are what find indexes.
    #[test]
    fn a_command_row_indexes_its_arguments() {
        assert_eq!(
            views_of(&command("u1", "explain", "closures")),
            [("umd-u1".to_string(), "closures".to_string())]
        );
        // Neither the name nor the expansion is searchable.
        let items = [command("u1", "explain", "closures")];
        assert_eq!(index("explain", &items).total(), 0);
        assert_eq!(index("template", &items).total(), 0);
        assert!(index("closures", &items).total() > 0);
    }

    #[test]
    fn a_command_without_arguments_has_no_view() {
        assert!(views_of(&command("u1", "thing", "")).is_empty());
    }

    fn index(query: &str, items: &[Item]) -> FindIndex {
        let mut index = FindIndex::new();
        index.set_query(query, items, 0);
        index
    }

    #[test]
    fn user_views_drop_front_matter_and_trim() {
        let item = user("u1", "---\nsource: cron\n---\n\n  check the token  \n");
        assert_eq!(
            views_of(&item),
            [("umd-u1".to_string(), "check the token".to_string())]
        );
        assert!(views_of(&user("u2", "   ")).is_empty());
        assert_eq!(
            views_of(&agent("a1", 1, "reply")),
            [("md-a1".to_string(), "reply".to_string())]
        );
    }

    #[test]
    fn the_count_is_shown_as_n_of_m() {
        let items = [user("u1", "a token"), agent("a1", 1, "token and Token")];
        let index = index("token", &items);
        assert_eq!(index.label().as_deref(), Some("1 of 3"));
    }

    #[test]
    fn nothing_found_and_an_empty_query() {
        let items = [agent("a1", 1, "hello")];
        assert_eq!(index("zzz", &items).label().as_deref(), Some("No results"));
        assert_eq!(index("", &items).label(), None);
        assert!(index("zzz", &items).current().is_none());
    }

    #[test]
    fn next_wraps_around_and_previous_steps_back() {
        let items = [agent("a1", 1, "x x x")];
        let mut index = index("x", &items);
        index.next();
        index.next();
        assert_eq!(index.label().as_deref(), Some("3 of 3"));
        index.next();
        assert_eq!(index.label().as_deref(), Some("1 of 3"));
        index.prev();
        assert_eq!(index.label().as_deref(), Some("3 of 3"));
        index.prev();
        assert_eq!(index.label().as_deref(), Some("2 of 3"));
        index.prev();
        assert_eq!(index.label().as_deref(), Some("1 of 3"));
    }

    #[test]
    fn matches_are_in_transcript_order() {
        let items = [agent("a1", 1, "x"), user("u1", "x x"), agent("a2", 1, "x")];
        let mut index = index("x", &items);
        let mut seen = Vec::new();
        for _ in 0..4 {
            let m = index.current().unwrap().clone();
            seen.push((m.item_key.to_string(), m.occurrence));
            index.next();
        }
        assert_eq!(
            seen,
            [
                ("a1".to_string(), 0),
                ("u1".to_string(), 0),
                ("u1".to_string(), 1),
                ("a2".to_string(), 0)
            ]
        );
    }

    #[test]
    fn the_first_match_follows_the_reading_position() {
        let items = [
            agent("a1", 1, "error"),
            agent("a2", 1, "error"),
            agent("a3", 1, "error"),
        ];
        let mut index = FindIndex::new();
        index.set_query("error", &items, 1);
        assert_eq!(&*index.current().unwrap().item_key, "a2");
        index.set_query("error", &items, 9);
        assert_eq!(&*index.current().unwrap().item_key, "a1");
    }

    #[test]
    fn a_streaming_reply_adds_matches_and_keeps_the_current_one() {
        let mut items = vec![user("u1", "test"), agent("a1", 1, "a test")];
        let mut index = index("test", &items);
        assert_eq!(index.label().as_deref(), Some("1 of 2"));
        let before = index.current().unwrap().clone();
        items[1] = agent("a1", 2, "a test. Another test.");
        index.sync(&items);
        assert_eq!(index.label().as_deref(), Some("1 of 3"));
        assert_eq!(index.current().unwrap(), &before);
        // Appending to the current item after the current match keeps it too.
        index.next();
        let second = index.current().unwrap().clone();
        items[1] = agent("a1", 3, "a test. Another test. And a test.");
        index.sync(&items);
        assert_eq!(index.label().as_deref(), Some("2 of 4"));
        assert_eq!(index.current().unwrap(), &second);
    }

    #[test]
    fn a_recompute_reads_only_the_item_that_changed() {
        let mut items = vec![
            agent("a1", 1, "x"),
            agent("a2", 1, "x"),
            agent("a3", 1, "x"),
        ];
        let mut index = index("x", &items);
        assert_eq!(index.take_reads(), 3);
        index.sync(&items);
        assert_eq!(index.take_reads(), 0);
        items[1] = agent("a2", 2, "x x");
        index.sync(&items);
        assert_eq!(index.take_reads(), 1);
        assert_eq!(index.total(), 4);
        items.push(agent("a4", 1, "x"));
        index.sync(&items);
        assert_eq!(index.take_reads(), 1);
        items.insert(0, agent("a0", 1, "x"));
        index.sync(&items);
        assert_eq!(index.take_reads(), 1);
        assert_eq!(index.total(), 6);
    }

    #[test]
    fn a_vanished_current_match_moves_to_the_next() {
        let mut items = vec![
            agent("a1", 1, "x"),
            agent("a2", 1, "x x"),
            agent("a3", 1, "x"),
        ];
        let mut index = index("x", &items);
        index.next();
        index.next();
        assert_eq!(index.label().as_deref(), Some("3 of 4"));
        // The current match is a2's second occurrence; it goes away.
        items[1] = agent("a2", 2, "x");
        index.sync(&items);
        assert_eq!(&*index.current().unwrap().item_key, "a3");
        // With nothing after it, the last match is current.
        items.truncate(2);
        index.sync(&items);
        assert_eq!(index.label().as_deref(), Some("2 of 2"));
    }
}

#[cfg(test)]
mod app_tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use crate::session::SessionView;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Entity, Focusable as _, SharedString, TestAppContext,
        VisualTestContext,
    };
    use serde_json::{Value, json};

    struct Rig {
        handle: AnyWindowHandle,
        app: Entity<QmtApp>,
        requests: async_channel::Receiver<String>,
    }

    /// `turns` of a question and an answer; the answers named in `needles`
    /// contain "needle". A running session is mid-turn.
    fn conversation(turns: usize, needles: &[usize], running: bool) -> SessionView {
        let mut view = SessionView::new("test-agent", "s1", "/w", "find");
        for i in 0..turns {
            view.apply(
                &json!({ "sessionUpdate": "user_message_chunk", "messageId": format!("u{i}"),
                "content": { "type": "text", "text": format!("question {i}") } }),
            );
            let extra = if needles.contains(&i) { " needle" } else { "" };
            view.apply(
                &json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("a{i}"),
                "content": { "type": "text", "text": format!("answer {i}{extra}") } }),
            );
        }
        if running {
            view.phase = "model".into();
        }
        view
    }

    fn rig(cx: &mut TestAppContext, view: SessionView, page: Page, control: bool) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
            crate::ui::keys::bind_keys(cx);
        });
        let mut store = crate::store::launch::peer_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        if control {
            store.runtime_mut_for_test("test-agent").capabilities = Some(json!({
                "features": { "steering": true },
                "methods": ["querymt/session/steer", "querymt/session/queue",
                            "querymt/session/runtimeState"],
            }));
        }
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|cx| {
                let mut store = store;
                store.install_main_for_test(view, cx);
                store
            });
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.go(page, cx);
                if page == Page::Session {
                    app.pane
                        .update(cx, |pane, cx| pane.focus_composer(window, cx));
                }
            });
            window.render_frame(cx);
        })
        .unwrap();
        Rig {
            handle,
            app,
            requests,
        }
    }

    fn render(cx: &mut TestAppContext, rig: &Rig) {
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
    }

    fn keys(cx: &mut TestAppContext, rig: &Rig, keys: &str) {
        VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes(keys);
        render(cx, rig);
    }

    fn type_text(cx: &mut TestAppContext, rig: &Rig, text: &str) {
        VisualTestContext::from_window(rig.handle, cx).simulate_input(text);
        render(cx, rig);
    }

    fn click(cx: &mut TestAppContext, rig: &Rig, id: &'static str) {
        cx.update_window(rig.handle, |_, window, cx| {
            window.click(SharedString::from(id), cx)
        })
        .unwrap();
        render(cx, rig);
    }

    fn find_key(cx: &mut TestAppContext, rig: &Rig) {
        let mac = cx.update(|cx| cx.platform_info().is_mac());
        keys(cx, rig, if mac { "cmd-f" } else { "ctrl-f" });
    }

    fn pane(cx: &mut TestAppContext, rig: &Rig) -> Entity<crate::ui::session_pane::SessionPane> {
        rig.app.read_with(cx, |app, _| app.pane.clone())
    }

    fn is_open(cx: &mut TestAppContext, rig: &Rig) -> bool {
        let pane = pane(cx, rig);
        pane.read_with(cx, |p, _| p.find_bar_state().is_some())
    }

    fn label(cx: &mut TestAppContext, rig: &Rig) -> Option<String> {
        let pane = pane(cx, rig);
        pane.read_with(cx, |p, _| p.find_bar_state().and_then(|b| b.label()))
    }

    fn query_focused(cx: &mut TestAppContext, rig: &Rig) -> bool {
        let pane = pane(cx, rig);
        let input = pane.read_with(cx, |p, _| p.find_bar_state().map(|b| b.input().clone()));
        let Some(input) = input else {
            return false;
        };
        cx.update_window(rig.handle, |_, window, cx| {
            input.focus_handle(cx).is_focused(window)
        })
        .unwrap()
    }

    fn composer_focused(cx: &mut TestAppContext, rig: &Rig) -> bool {
        let pane = pane(cx, rig);
        cx.update_window(rig.handle, |_, window, cx| {
            pane.read(cx).composer_focused_for_test(window, cx)
        })
        .unwrap()
    }

    fn draft(cx: &mut TestAppContext, rig: &Rig) -> String {
        let pane = pane(cx, rig);
        pane.read_with(cx, |p, cx| p.draft(cx))
    }

    fn focus_composer(cx: &mut TestAppContext, rig: &Rig) {
        let pane = pane(cx, rig);
        cx.update_window(rig.handle, |_, window, cx| {
            pane.update(cx, |p, cx| p.focus_composer(window, cx))
        })
        .unwrap();
        render(cx, rig);
    }

    fn scroll_top(cx: &mut TestAppContext, rig: &Rig) -> gpui_kit::ListOffset {
        let pane = pane(cx, rig);
        pane.read_with(cx, |p, _| p.scroll_top_for_test())
    }

    /// Scroll to the first row, so every match is at or below the viewport top.
    fn at_top(cx: &mut TestAppContext, rig: &Rig) {
        let pane = pane(cx, rig);
        pane.update(cx, |p, _| p.scroll_to_item_for_test(0));
        render(cx, rig);
    }

    fn sent(rig: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| rig.requests.try_recv().ok())
            .map(|line| serde_json::from_str(&line).expect("a JSON-RPC line"))
            .collect()
    }

    // add-session-find / Open find with the shortcut
    #[gpui_kit::test]
    fn the_shortcut_opens_the_bar_from_the_composer(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[], false), Page::Session, false);
        type_text(cx, &rig, "my draft");
        assert!(composer_focused(cx, &rig));
        find_key(cx, &rig);
        assert!(is_open(cx, &rig), "the bar opens");
        assert!(query_focused(cx, &rig), "with the query field focused");
        assert_eq!(draft(cx, &rig), "my draft", "the composer is untouched");
        assert_eq!(label(cx, &rig), None, "an empty query shows no count");
    }

    #[gpui_kit::test]
    fn the_shortcut_again_selects_the_query(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[], false), Page::Session, false);
        find_key(cx, &rig);
        type_text(cx, &rig, "cache");
        focus_composer(cx, &rig);
        assert!(!query_focused(cx, &rig));
        find_key(cx, &rig);
        assert!(query_focused(cx, &rig));
        // The query is selected: typing replaces it.
        type_text(cx, &rig, "z");
        let pane = pane(cx, &rig);
        let value = pane.read_with(cx, |p, cx| {
            p.find_bar_state()
                .unwrap()
                .input()
                .read(cx)
                .value()
                .to_string()
        });
        assert_eq!(value, "z");
        assert_eq!(draft(cx, &rig), "", "nothing typed into the composer");
    }

    #[gpui_kit::test]
    fn the_shortcut_does_nothing_without_a_session(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[], false), Page::Start, false);
        find_key(cx, &rig);
        assert!(!is_open(cx, &rig));
    }

    // add-session-find / Match count and Step through matches
    #[gpui_kit::test]
    fn typing_shows_the_count_and_enter_steps_through(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            conversation(12, &[2, 5, 9], false),
            Page::Session,
            false,
        );
        at_top(cx, &rig);
        find_key(cx, &rig);
        type_text(cx, &rig, "needle");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"));
        keys(cx, &rig, "enter");
        assert_eq!(label(cx, &rig).as_deref(), Some("2 of 3"));
        keys(cx, &rig, "shift-enter");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"));
        keys(cx, &rig, "shift-enter");
        assert_eq!(label(cx, &rig).as_deref(), Some("3 of 3"), "previous wraps");
        keys(cx, &rig, "enter");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"), "next wraps");
        type_text(cx, &rig, "zzz");
        assert_eq!(label(cx, &rig).as_deref(), Some("No results"));
    }

    #[gpui_kit::test]
    fn the_arrow_buttons_step_through_the_matches(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            conversation(12, &[2, 5, 9], false),
            Page::Session,
            false,
        );
        at_top(cx, &rig);
        find_key(cx, &rig);
        type_text(cx, &rig, "needle");
        click(cx, &rig, "find-next");
        assert_eq!(label(cx, &rig).as_deref(), Some("2 of 3"));
        click(cx, &rig, "find-prev");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"));
        click(cx, &rig, "find-prev");
        assert_eq!(label(cx, &rig).as_deref(), Some("3 of 3"));
    }

    // add-session-find / Match far off screen
    #[gpui_kit::test]
    fn a_far_match_scrolls_into_view_and_pauses_following(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            conversation(400, &[3, 390], false),
            Page::Session,
            false,
        );
        let pane = pane(cx, &rig);
        assert!(pane.read_with(cx, |p, _| p.transcript.list.is_following_tail()));
        find_key(cx, &rig);
        // At the tail nothing is at or below the viewport: the first match
        // overall is the current one, about 780 rows up.
        type_text(cx, &rig, "needle");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 2"));
        let (top, following, row) = pane.read_with(cx, |p, _| {
            (
                p.scroll_top_for_test().item_ix,
                p.transcript.list.is_following_tail(),
                p.transcript
                    .row_of_item("assistant-a3")
                    .expect("the row exists"),
            )
        });
        assert!(!following, "following the tail is paused");
        assert!(
            (top..top + 12).contains(&row),
            "the match's row {row} is at the top of the viewport (top {top})"
        );
        keys(cx, &rig, "enter");
        let (top, row) = pane.read_with(cx, |p, _| {
            (
                p.scroll_top_for_test().item_ix,
                p.transcript
                    .row_of_item("assistant-a390")
                    .expect("the row exists"),
            )
        });
        assert!(top <= row && row < top + 40, "the next match is revealed");
    }

    // add-session-find / First match follows the reading position
    #[gpui_kit::test]
    fn typing_picks_the_match_at_or_below_the_viewport_top(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(40, &[5, 30], false), Page::Session, false);
        let pane = pane(cx, &rig);
        let target = pane.read_with(cx, |p, _| p.transcript.row_of_item("user-u20").unwrap());
        pane.update(cx, |p, _| p.scroll_to_item_for_test(target));
        render(cx, &rig);
        find_key(cx, &rig);
        type_text(cx, &rig, "needle");
        assert_eq!(label(cx, &rig).as_deref(), Some("2 of 2"));
        let current = pane.read_with(cx, |p, _| p.find_bar_state().unwrap().current());
        assert_eq!(
            current.map(|m| m.item_key.to_string()).as_deref(),
            Some("assistant-a30")
        );
    }

    // add-session-find / Highlight does not move content
    #[gpui_kit::test]
    fn highlights_do_not_move_the_content(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[0, 1, 2], false), Page::Session, false);
        let pane = pane(cx, &rig);
        let before = (
            pane.read_with(cx, |p, _| p.content_metrics()),
            scroll_top(cx, &rig),
            pane.read_with(cx, |p, _| {
                (0..p.transcript.list.item_count())
                    .map(|ix| p.transcript.list.bounds_for_item(ix))
                    .collect::<Vec<_>>()
            }),
        );
        find_key(cx, &rig);
        type_text(cx, &rig, "answer");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"));
        let after = (
            pane.read_with(cx, |p, _| p.content_metrics()),
            scroll_top(cx, &rig),
            pane.read_with(cx, |p, _| {
                (0..p.transcript.list.item_count())
                    .map(|ix| p.transcript.list.bounds_for_item(ix))
                    .collect::<Vec<_>>()
            }),
        );
        assert_eq!(before.0, after.0, "content size");
        assert_eq!(before.1.item_ix, after.1.item_ix, "scroll position");
        assert_eq!(before.2, after.2, "row bounds");
        // And the text is painted: all three matches are highlighted.
        let painted = pane.read_with(cx, |p, _| p.transcript.md.painted("md-assistant-a0"));
        assert_eq!(painted.len(), 1);
    }

    // add-session-find / Close find
    #[gpui_kit::test]
    fn escape_closes_the_bar_and_restores_the_composer(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(12, &[2, 5], false), Page::Session, false);
        find_key(cx, &rig);
        type_text(cx, &rig, "needle");
        let top = scroll_top(cx, &rig).item_ix;
        let pane = pane(cx, &rig);
        keys(cx, &rig, "escape");
        assert!(!is_open(cx, &rig));
        assert!(composer_focused(cx, &rig));
        assert_eq!(scroll_top(cx, &rig).item_ix, top, "the scroll position");
        let (left, query) = pane.read_with(cx, |p, _| {
            (
                p.transcript.md.painted("md-assistant-a2"),
                p.transcript.md.find(),
            )
        });
        assert!(left.is_empty(), "no highlight remains");
        assert!(query.is_none());
    }

    #[gpui_kit::test]
    fn the_close_button_closes_the_bar(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(12, &[2, 5], false), Page::Session, false);
        find_key(cx, &rig);
        type_text(cx, &rig, "needle");
        click(cx, &rig, "find-close");
        assert!(!is_open(cx, &rig));
        assert!(composer_focused(cx, &rig));
    }

    #[gpui_kit::test]
    fn escape_closes_the_bar_before_it_counts_toward_cancelling(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[], true), Page::Session, true);
        find_key(cx, &rig);
        assert!(is_open(cx, &rig));
        keys(cx, &rig, "escape");
        assert!(!is_open(cx, &rig), "the first Escape closes the bar");
        assert!(
            rig.app.read_with(cx, |a, _| a.last_escape.is_none()),
            "and is not counted"
        );
        // A second Escape right after is the first of a double Escape.
        keys(cx, &rig, "escape");
        cx.run_until_parked();
        let cancelled = sent(&rig).iter().any(|r| r["method"] == "session/cancel");
        assert!(!cancelled, "the turn keeps running");
    }

    #[gpui_kit::test]
    fn opening_another_session_closes_the_bar(cx: &mut TestAppContext) {
        let rig = rig(cx, conversation(3, &[], false), Page::Session, false);
        find_key(cx, &rig);
        assert!(is_open(cx, &rig));
        let pane = pane(cx, &rig);
        let other = rig.app.update(cx, |app, cx| {
            let store = app.store.clone();
            store.update(cx, |s, cx| {
                s.adopt_for_test(SessionView::new("test-agent", "s2", "/w", "other"), cx)
            })
        });
        pane.update(cx, |p, cx| p.show(other, cx));
        render(cx, &rig);
        assert!(!is_open(cx, &rig));
    }

    // add-session-find / Find stays live while the session streams
    #[gpui_kit::test]
    fn a_streaming_reply_updates_the_count_without_moving(cx: &mut TestAppContext) {
        let mut view = conversation(2, &[], false);
        view.apply(
            &json!({ "sessionUpdate": "agent_message_chunk", "messageId": "last",
            "content": { "type": "text", "text": "first test" } }),
        );
        let rig = rig(cx, view, Page::Session, false);
        find_key(cx, &rig);
        type_text(cx, &rig, "test");
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 1"));
        let pane = pane(cx, &rig);
        let top = scroll_top(cx, &rig).item_ix;
        let before = pane.read_with(cx, |p, _| p.find_bar_state().unwrap().current());
        let session = pane.read_with(cx, |p, _| p.session().cloned().unwrap());
        session.update(cx, |open, cx| {
            open.view.apply(
                &json!({ "sessionUpdate": "agent_message_chunk", "messageId": "last",
                "content": { "type": "text", "text": ", then another test and a test" } }),
            );
            cx.notify();
        });
        render(cx, &rig);
        assert_eq!(label(cx, &rig).as_deref(), Some("1 of 3"));
        let after = pane.read_with(cx, |p, _| p.find_bar_state().unwrap().current());
        assert_eq!(before, after, "the current match is the same match");
        assert_eq!(
            scroll_top(cx, &rig).item_ix,
            top,
            "the transcript did not move"
        );
    }
}
