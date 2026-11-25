//! Sessions page: search, lifecycle tabs, pinned group, sessions grouped by
//! workspace, nested forks and worktree children, and per-session actions.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::{Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{
    ArchiveFilter, LocationFilter, Page, QmtApp, ResolutionFilter, RuntimeFilter, SessionFilters,
    SessionTab, kit,
};
use crate::store::{AgentsStore, SessionSummary, Worktree};
use crate::theme::{Palette, palette};
use crate::util::{absolute_time, short_time, workspace_name};

const GROUP_ROWS: usize = 8;
/// Sessions added to a group per "Show more".
const GROUP_PAGE: usize = 10;
/// Side of a session row's actions button.
const SESSION_MENU_SIZE: f32 = 28.;
const PINNED_KEY: &str = "\u{0}pinned";
const RECENT_KEY: &str = "\u{0}recent";
/// Indent of one level of nested child sessions.
const CHILD_INDENT: f32 = 18.;
/// Deepest nesting rendered; deeper chains open from their own row.
const MAX_DEPTH: usize = 6;

/// Lifecycle tabs only list sessions whose state the agent reported; the rest
/// appear in All (the desktop app's "lifecycle unavailable").
fn in_tab(s: &SessionSummary, tab: SessionTab) -> bool {
    let (resolution, archive) = tab.lifecycle();
    matches_lifecycle(s, resolution, archive)
}

/// Desktop `sessionMatchesLifecycle`: with no lifecycle state a session only
/// matches the unfiltered view.
fn matches_lifecycle(
    s: &SessionSummary,
    resolution: ResolutionFilter,
    archive: ArchiveFilter,
) -> bool {
    if (resolution, archive) == (ResolutionFilter::Any, ArchiveFilter::Include) {
        return true;
    }
    if !s.lifecycle_known {
        return false;
    }
    let resolved = match resolution {
        ResolutionFilter::Any => true,
        ResolutionFilter::Open => !s.resolved,
        ResolutionFilter::Resolved => s.resolved,
    };
    let archived = match archive {
        ArchiveFilter::Include => true,
        ArchiveFilter::Hide => !s.archived,
        ArchiveFilter::Only => s.archived,
    };
    resolved && archived
}

/// Runtime and location refinements (desktop `sessionMatchesFilter`).
fn matches_refinements(s: &SessionSummary, f: &SessionFilters, needs_input: bool) -> bool {
    let runtime = match f.runtime {
        RuntimeFilter::Any => true,
        RuntimeFilter::Running => s.running() && !needs_input,
        RuntimeFilter::NeedsInput => needs_input,
    };
    let location = match f.location {
        LocationFilter::Any => true,
        LocationFilter::Local => s.remote.is_none(),
        LocationFilter::Remote => s.remote.is_some(),
    };
    runtime && location
}

pub(crate) fn matches_query(s: &SessionSummary, agent_name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let remote = s.remote.as_ref().map(|r| r.name()).unwrap_or_default();
    let branch = s
        .worktree
        .as_ref()
        .and_then(|w| w.branch.as_deref())
        .unwrap_or_default();
    [
        s.title.as_str(),
        s.cwd.as_str(),
        s.group_cwd(),
        s.id.as_str(),
        agent_name,
        branch,
        remote.as_str(),
    ]
    .iter()
    .any(|field| field.to_lowercase().contains(query))
}

fn worktree_tip(w: &Worktree) -> String {
    [
        w.branch.as_ref().map(|b| format!("Branch {b}")),
        w.checkout_path.as_ref().map(|c| format!("Checkout {c}")),
        w.baseline_mode
            .as_ref()
            .map(|b| format!("Baseline {}", b.replace('_', " "))),
        (!w.usable()).then(|| {
            format!(
                "Worktree {}",
                w.lifecycle.as_deref().unwrap_or("unavailable")
            )
        }),
        w.handed_off_to
            .as_ref()
            .map(|_| "Handed off to another session".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// What an agent lets the user do with its sessions.
#[derive(Clone, Default)]
struct AgentInfo {
    name: String,
    connected: bool,
    can_pin: bool,
    can_lifecycle: bool,
    can_delete: bool,
}

/// One entry of the virtualized session list.
pub(super) enum Entry {
    Head(GroupHead),
    Row {
        s: Box<SessionSummary>,
        scope: String,
        in_pinned: bool,
        depth: usize,
        /// Row state kept outside the summary (expanded, pending, error).
        state_sig: u64,
    },
    /// Paging footer of a group: `shown` of `total` sessions listed.
    More {
        key: String,
        shown: usize,
        total: usize,
    },
    ChildError {
        agent: String,
        id: String,
        row_key: SharedString,
        depth: usize,
        error: String,
    },
    Note {
        parent: SharedString,
        depth: usize,
        text: &'static str,
    },
}

#[derive(Hash)]
pub(super) struct GroupHead {
    key: String,
    name: String,
    kind: GroupKind,
    collapsed: bool,
    total: usize,
    latest: String,
    running: usize,
    machines: Vec<String>,
    has_local: bool,
    owner: Option<String>,
    /// First entry of the list: no gap above.
    first: bool,
    /// The project path was just copied (the copy button shows a check).
    copied: bool,
}

fn hash_of(f: impl FnOnce(&mut std::collections::hash_map::DefaultHasher)) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    f(&mut h);
    h.finish()
}

#[derive(Clone, Copy)]
pub(super) enum GroupLimit {
    First(usize),
    All,
}

impl GroupLimit {
    fn shown(self, total: usize) -> usize {
        match self {
            Self::First(limit) => limit.min(total),
            Self::All => total,
        }
    }
}

fn show_rows(
    key: &str,
    limit: GroupLimit,
    cx: &Context<QmtApp>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let key = key.to_owned();
    cx.listener(move |this, _, _, cx| {
        if matches!(limit, GroupLimit::First(GROUP_ROWS)) {
            this.group_shown.remove(&key);
        } else {
            this.group_shown.insert(key.clone(), limit);
        }
        cx.notify();
    })
}

/// `copied` key of a group's project path.
fn workspace_copy_key(group: &str) -> String {
    format!("workspace:{group}")
}

/// Indent of the notes under the children of a row at `depth`.
fn child_indent(depth: usize) -> Pixels {
    px((depth + 1) as f32 * CHILD_INDENT + 30.)
}

/// Everything about a summary that shows in its row.
fn hash_summary(s: &SessionSummary, h: &mut impl Hasher) {
    (
        &s.agent_id,
        &s.id,
        &s.cwd,
        &s.title,
        &s.updated_at,
        &s.phase,
    )
        .hash(h);
    (
        s.root,
        s.open,
        s.resolved,
        s.archived,
        s.lifecycle_known,
        s.pinned,
    )
        .hash(h);
    (s.messages, s.has_errors, s.fork_count, s.has_children).hash(h);
    (&s.parent, &s.fork_origin).hash(h);
    s.worktree.as_ref().map(|w| format!("{w:?}")).hash(h);
    s.remote
        .as_ref()
        .map(|r| (&r.node_id, &r.label, r.connected))
        .hash(h);
}

impl Entry {
    /// Changes whenever the entry's rendered height may change.
    fn sig(&self) -> u64 {
        hash_of(|h| match self {
            Entry::Head(head) => (0u8, head).hash(h),
            Entry::Row {
                s,
                scope,
                in_pinned,
                depth,
                state_sig,
            } => {
                (1u8, scope, in_pinned, depth, state_sig).hash(h);
                hash_summary(s, h);
            }
            Entry::More { key, shown, total } => (2u8, key, shown, total).hash(h),
            Entry::ChildError {
                row_key,
                depth,
                error,
                ..
            } => (3u8, row_key.as_ref(), depth, error).hash(h),
            Entry::Note {
                parent,
                depth,
                text,
            } => (4u8, parent.as_ref(), depth, text).hash(h),
        })
    }
}

/// Virtualized Sessions page list: only the entries in view are built and
/// laid out. Laying out every expanded row each frame made scrolling a few
/// hundred sessions drop to 2-3 frames per second.
pub(super) struct SessionList {
    pub(super) state: ListState,
    entries: Rc<Vec<Entry>>,
    sigs: Vec<u64>,
    rc: Option<Rc<RowCtx>>,
    /// Inputs affecting every row (tab, agent column): a change starts over.
    global: Option<u64>,
}

impl Default for SessionList {
    fn default() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(600.)),
            entries: Rc::default(),
            sigs: Vec::new(),
            rc: None,
            global: None,
        }
    }
}

impl SessionList {
    /// Replace the entries, invalidating only the ones whose height may have
    /// changed so the list keeps its measurements and scroll position.
    /// Replace the entries, invalidating only the ones whose height may have
    /// changed so the list keeps its measurements and scroll position.
    fn sync(&mut self, entries: Vec<Entry>, rc: RowCtx, global: u64) {
        let sigs: Vec<u64> = entries.iter().map(Entry::sig).collect();
        if self.global != Some(global) {
            self.state.reset(sigs.len());
        } else {
            kit::sync_list_state(&self.state, &self.sigs, &sigs);
        }
        self.global = Some(global);
        self.sigs = sigs;
        self.entries = Rc::new(entries);
        self.rc = Some(Rc::new(rc));
    }
}

/// Values every row of one render needs.
pub(super) struct RowCtx {
    p: Palette,
    agents: HashMap<String, AgentInfo>,
    show_agent: bool,
    all_tab: bool,
    searching: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum GroupKind {
    Pinned,
    Recent,
    Workspace,
}

struct Group {
    key: String,
    name: String,
    kind: GroupKind,
    sessions: Vec<SessionSummary>,
}

/// Which groups start open (desktop `DesktopSessionList`): on first show only
/// the first group; Pinned opens once it exists, Recent only when there is no
/// Pinned group above it (it mostly repeats rows listed elsewhere). After that
/// the user's choices stand.
#[derive(Default)]
pub(super) struct GroupOpenState {
    open: std::collections::HashSet<String>,
    groups_initialized: bool,
    pinned_initialized: bool,
    recent_initialized: bool,
}

impl GroupOpenState {
    fn initialize(&mut self, groups: &[Group]) {
        if !self.groups_initialized && !groups.is_empty() {
            self.open.insert(groups[0].key.clone());
            self.groups_initialized = true;
        }
        let has = |kind| groups.iter().any(|g| g.kind == kind);
        let has_pinned = has(GroupKind::Pinned);
        if !self.pinned_initialized && has_pinned {
            self.open.insert(PINNED_KEY.into());
            self.pinned_initialized = true;
        }
        if !self.recent_initialized && has(GroupKind::Recent) {
            if !has_pinned {
                self.open.insert(RECENT_KEY.into());
            }
            self.recent_initialized = true;
        }
    }

    fn is_open(&self, key: &str) -> bool {
        self.open.contains(key)
    }

    fn toggle(&mut self, key: &str) {
        if !self.open.remove(key) {
            self.open.insert(key.to_string());
        }
    }
}

/// Wrap `el` so it shows `text` as a tooltip.
fn with_tip(id: ElementId, el: impl IntoElement, text: impl Into<SharedString>) -> AnyElement {
    let text: SharedString = text.into();
    div()
        .id(id)
        .flex_none()
        .child(el)
        .tooltip(move |window, cx| kit::wrap_tip(text.clone()).build(window, cx))
        .into_any_element()
}

/// Icon, title, explanation and an optional action, centred in the page.
fn state_panel(
    icon: IconName,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    action: Option<AnyElement>,
    p: &Palette,
) -> AnyElement {
    v_flex()
        .items_center()
        .gap_3()
        .py_12()
        .px_6()
        .child(
            div()
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(p.card)
                .child(kit::icon(icon, 17., p.muted)),
        )
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(div().text_sm().font_semibold().child(title.into()))
                .child(
                    div()
                        .max_w(px(440.))
                        .text_center()
                        .text_sm()
                        .text_color(p.dim)
                        .child(body.into()),
                ),
        )
        .children(action)
        .into_any_element()
}

fn skeleton_rows() -> AnyElement {
    v_flex()
        .gap_1()
        .children(["a", "b", "c", "d"].map(|_| {
            h_flex()
                .gap_3()
                .px_2()
                .py_2()
                .child(Skeleton::new().size(px(18.)).rounded_full())
                .child(
                    v_flex()
                        .flex_1()
                        .gap_1p5()
                        .child(Skeleton::new().h(px(12.)).w(relative(0.45)).rounded(px(4.)))
                        .child(Skeleton::new().h(px(10.)).w(relative(0.25)).rounded(px(4.))),
                )
        }))
        .into_any_element()
}

impl QmtApp {
    pub(super) fn render_sessions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let query = self.session_query.read(cx).value().trim().to_lowercase();
        let store = self.store.read(cx);
        let connected = store.connected_ids();
        let refreshing = store
            .configs
            .iter()
            .any(|c| store.runtime(&c.id).is_some_and(|r| r.busy));
        let loading = connected
            .iter()
            .any(|id| store.runtime(id).is_some_and(|r| !r.sessions_loaded));
        // First page shown, older pages still arriving: counts and search
        // are incomplete until they finish.
        let catalog_loading = connected
            .iter()
            .any(|id| store.runtime(id).is_some_and(|r| r.catalog_loading));
        let errors: Vec<String> = connected
            .iter()
            .filter_map(|id| {
                let err = store.runtime(id)?.sessions_error.clone()?;
                let name = store.config(id).map(|c| c.name.clone()).unwrap_or_default();
                Some(format!("{name}: {err}"))
            })
            .collect();
        let agents: HashMap<String, AgentInfo> = store
            .configs
            .iter()
            .map(|c| {
                let rt = store.runtime(&c.id);
                let info = AgentInfo {
                    name: c.name.clone(),
                    connected: rt.is_some_and(|r| r.is_connected()),
                    can_pin: rt.is_some_and(|r| r.can_pin()),
                    can_lifecycle: rt.is_some_and(|r| r.can_set_lifecycle()),
                    can_delete: rt.is_some_and(|r| r.is_connected() && r.can_delete_sessions),
                };
                (c.id.clone(), info)
            })
            .collect();
        let roots: Vec<SessionSummary> = store
            .all_sessions()
            .into_iter()
            .filter(|s| s.root)
            .cloned()
            .collect();
        let agent_name = |id: &str| agents.get(id).map(|a| a.name.as_str()).unwrap_or_default();

        let count = |tab| roots.iter().filter(|s| in_tab(s, tab)).count();
        let counts = [
            (SessionTab::Open, "Open", count(SessionTab::Open)),
            (
                SessionTab::Resolved,
                "Resolved",
                count(SessionTab::Resolved),
            ),
            (
                SessionTab::Archived,
                "Archived",
                count(SessionTab::Archived),
            ),
            (SessionTab::All, "All", roots.len()),
        ];
        // Location only applies while remote sessions are listed.
        let show_location = store.prefs.show_remote_sessions;
        let mut filters = self.session_filters;
        if !show_location {
            filters.location = LocationFilter::Any;
        }
        // The runtime can wait on work that does not require the user.
        let needs_input: HashSet<(&str, &str)> = store
            .requests
            .iter()
            .filter(|r| r.is_pending() && !r.offline)
            .filter_map(|r| Some((r.agent_id.as_str(), r.session_id.as_deref()?)))
            .collect();
        let tab = filters.tab();
        let refinements = filters.refinements(show_location);
        let unknown_lifecycle = roots.iter().filter(|s| !s.lifecycle_known).count();
        let mut visible: Vec<SessionSummary> = roots
            .iter()
            .filter(|s| {
                matches_lifecycle(s, filters.resolution, filters.archive)
                    && matches_refinements(
                        s,
                        &filters,
                        needs_input.contains(&(s.agent_id.as_str(), s.id.as_str())),
                    )
                    && matches_query(s, agent_name(&s.agent_id), &query)
            })
            .cloned()
            .collect();
        visible.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        // Pinned, then recently opened, then workspaces by latest activity.
        // Pinned and recent rows also stay in their workspace group.
        let mut groups: Vec<Group> = Vec::new();
        let pinned: Vec<SessionSummary> = visible.iter().filter(|s| s.pinned).cloned().collect();
        if !pinned.is_empty() {
            groups.push(Group {
                key: PINNED_KEY.into(),
                name: "Pinned sessions".into(),
                kind: GroupKind::Pinned,
                sessions: pinned,
            });
        }
        let recent: Vec<SessionSummary> = store
            .prefs
            .recent_sessions
            .iter()
            .filter_map(|(agent, id)| {
                visible
                    .iter()
                    .find(|s| &s.agent_id == agent && &s.id == id && !s.pinned)
            })
            .cloned()
            .collect();
        if !recent.is_empty() {
            groups.push(Group {
                key: RECENT_KEY.into(),
                name: "Recent sessions".into(),
                kind: GroupKind::Recent,
                sessions: recent,
            });
        }
        for s in visible {
            let cwd = s.group_cwd().to_string();
            match groups
                .iter_mut()
                .find(|g| g.kind == GroupKind::Workspace && g.key == cwd)
            {
                Some(g) => g.sessions.push(s),
                None => groups.push(Group {
                    name: if cwd.is_empty() {
                        "No workspace".into()
                    } else {
                        workspace_name(&cwd)
                    },
                    key: cwd,
                    kind: GroupKind::Workspace,
                    sessions: vec![s],
                }),
            }
        }
        let rc = RowCtx {
            p,
            show_agent: connected.len() > 1,
            all_tab: !filters.filters_lifecycle(),
            searching: !query.is_empty(),
            agents,
        };
        if !rc.searching {
            self.session_groups.initialize(&groups);
        }
        let has_groups = !groups.is_empty();
        let mut entries = Vec::new();
        for (i, g) in groups.into_iter().enumerate() {
            self.push_group_entries(&mut entries, g, i == 0, &rc, store);
        }
        // Inputs every row depends on: a change rebuilds the whole list.
        let global = hash_of(|h| {
            (rc.show_agent, rc.all_tab, rc.searching).hash(h);
            // A filter change starts the list over at the top.
            (
                filters.runtime as u8,
                filters.location as u8,
                filters.resolution as u8,
                filters.archive as u8,
            )
                .hash(h);
            let mut agents: Vec<_> = rc.agents.iter().collect();
            agents.sort_by(|a, b| a.0.cmp(b.0));
            for (id, a) in agents {
                (
                    id,
                    &a.name,
                    a.connected,
                    a.can_pin,
                    a.can_lifecycle,
                    a.can_delete,
                )
                    .hash(h);
            }
        });

        // ---- header
        let header = kit::page_header(
            "Sessions",
            None,
            None,
            vec![
                kit::loading_icon_button(
                    "sessions-refresh",
                    IconName::RefreshCw,
                    "Refresh sessions",
                    refreshing,
                    24.,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.refresh_all(cx))),
                )
                .into_any_element(),
            ],
            &p,
        );
        let search = h_flex()
            .h(px(40.))
            .px_3()
            .gap_2()
            .rounded(px(10.))
            .bg(p.card)
            .child(kit::icon(IconName::Search, 15., p.dim))
            .child(Input::new(&self.session_query).appearance(false).flex_1());
        let catalog_note = catalog_loading.then(|| {
            h_flex()
                .ml_auto()
                .pb_2()
                .gap_1p5()
                .text_xs()
                .text_color(p.dim)
                .child(kit::spinner("sessions-catalog-spinner", 12., p.dim))
                .child("Loading older sessions…")
        });
        let tabs = h_flex()
            .gap_5()
            .border_b_1()
            .border_color(p.border)
            .children(counts.into_iter().map(|(t, label, n)| {
                let active = Some(t) == tab;
                h_flex()
                    .id(label)
                    .gap_1p5()
                    .pb_2()
                    .mb(px(-1.))
                    .cursor_pointer()
                    .text_sm()
                    .border_b_2()
                    .border_color(if active { p.text } else { transparent_black() })
                    .text_color(if active { p.text } else { p.muted })
                    .when(active, |el| el.font_medium())
                    .child(label)
                    .child(div().text_xs().text_color(p.dim).child(n.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.session_filters.set_tab(t);
                        cx.notify();
                    }))
            }))
            .children(catalog_note);
        // Refinements beyond the tab, with a way back to just the tab.
        let refinement_summary = (!refinements.is_empty()).then(|| {
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(p.muted)
                .child(kit::icon(IconName::SlidersHorizontal, 13., p.dim))
                .child(refinements.join(" · "))
                .child(
                    kit::flat_btn("sessions-clear-refinements", true, true, &p)
                        .h(px(24.))
                        .px(px(8.))
                        .text_xs()
                        .child("Clear")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.session_filters.reset_refinements();
                            cx.notify();
                        })),
                )
        });
        // Agents without lifecycle support only appear in All; say so.
        let coverage = (filters.filters_lifecycle() && unknown_lifecycle > 0).then(|| {
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(p.dim)
                .child(kit::icon(IconName::Info, 13., p.dim))
                .child(format!(
                    "{unknown_lifecycle} {} without lifecycle state {} only in All.",
                    if unknown_lifecycle == 1 {
                        "session"
                    } else {
                        "sessions"
                    },
                    if unknown_lifecycle == 1 {
                        "appears"
                    } else {
                        "appear"
                    },
                ))
                .child(
                    kit::button("sessions-view-all")
                        .ghost()
                        .xsmall()
                        .label("View all")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.session_filters.set_tab(SessionTab::All);
                            cx.notify();
                        })),
                )
        });
        let retry = |id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            kit::button(id)
                .small()
                .label(label)
                .on_click(
                    cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.refresh_all(cx))),
                )
                .into_any_element()
        };
        // Refresh failed but older results are still listed.
        let inline_error = (!errors.is_empty() && !roots.is_empty()).then(|| {
            h_flex()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .bg(p.danger.opacity(0.1))
                .text_sm()
                .child(kit::icon(IconName::TriangleAlert, 15., p.danger))
                .child(div().flex_1().min_w_0().child(format!(
                    "Sessions could not be refreshed. {}",
                    errors.join(" · ")
                )))
                .child(retry("sessions-retry-inline", "Retry", cx))
        });

        let mut list_body = false;
        let body = if connected.is_empty() && roots.is_empty() {
            state_panel(
                IconName::PlugZap,
                "No agents connected",
                "Connect an agent before browsing or creating sessions.",
                Some(
                    kit::button("sessions-open-agents")
                        .primary()
                        .small()
                        .label("Open agents")
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx)))
                        .into_any_element(),
                ),
                &p,
            )
        } else if !errors.is_empty() && roots.is_empty() {
            state_panel(
                IconName::TriangleAlert,
                "Sessions could not be loaded",
                errors.join(" · "),
                Some(retry("sessions-retry", "Try again", cx)),
                &p,
            )
        } else if loading && roots.is_empty() {
            skeleton_rows()
        } else if roots.is_empty() {
            state_panel(
                IconName::MessageSquarePlus,
                "No sessions yet",
                "Start a task and its conversation will appear here.",
                Some(
                    kit::button("sessions-new")
                        .primary()
                        .small()
                        .label("New session")
                        .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx)))
                        .into_any_element(),
                ),
                &p,
            )
        } else if !has_groups && (!query.is_empty() || !refinements.is_empty()) {
            state_panel(
                IconName::SearchX,
                "No matching sessions",
                "Try another search or clear the filters.",
                Some(
                    kit::button("sessions-clear-search")
                        .small()
                        .label("Clear filters")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.session_query.update(cx, |s, cx| {
                                s.set_value(SharedString::default(), window, cx)
                            });
                            this.session_filters.reset_refinements();
                            cx.notify();
                        }))
                        .into_any_element(),
                ),
                &p,
            )
        } else if !has_groups {
            let (icon, title, text) = match tab {
                Some(SessionTab::Resolved) => (
                    IconName::CircleCheck,
                    "No resolved sessions",
                    "Sessions you mark as resolved appear here.",
                ),
                Some(SessionTab::Archived) => (
                    IconName::Archive,
                    "No archived sessions",
                    "Sessions you archive appear here.",
                ),
                _ => (
                    IconName::MessageSquarePlus,
                    "No open sessions",
                    "Browse All to find resolved and archived sessions.",
                ),
            };
            let (label, target) = if tab == Some(SessionTab::Resolved) {
                ("View open sessions", SessionTab::Open)
            } else {
                ("View all sessions", SessionTab::All)
            };
            state_panel(
                icon,
                title,
                text,
                Some(
                    kit::button("sessions-switch-tab")
                        .small()
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.session_filters.set_tab(target);
                            cx.notify();
                        }))
                        .into_any_element(),
                ),
                &p,
            )
        } else {
            self.session_list.sync(entries, rc, global);
            list_body = true;
            let render =
                cx.processor(|this: &mut Self, ix: usize, _, cx| this.render_session_entry(ix, cx));
            list(self.session_list.state.clone(), move |ix, window, cx| {
                render(ix, window, cx)
            })
            .size_full()
            .pb_6()
            .into_any_element()
        };

        let top =
            div().w_full().flex().justify_center().px_8().pt_6().child(
                v_flex()
                    .w_full()
                    .max_w(px(1040.))
                    .gap_4()
                    .pb_4()
                    .child(header)
                    .child(h_flex().gap_2().child(search.flex_1()).child(
                        self.session_filters_button(
                            filters,
                            refinements.len(),
                            show_location,
                            &p,
                            cx,
                        ),
                    ))
                    .child(tabs)
                    .children(refinement_summary)
                    .children(coverage)
                    .children(inline_error),
            );
        // The list scrolls itself below the fixed header; the state panels
        // scroll with the page column.
        v_flex().size_full().child(top).child(if list_body {
            div().flex_1().min_h_0().child(body).into_any_element()
        } else {
            div()
                .id("sessions-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .justify_center()
                .px_8()
                .pb_6()
                .child(div().w_full().max_w(px(1040.)).child(body))
                .into_any_element()
        })
    }

    /// Prefill the Start page with this workspace and agent, then focus the prompt.
    pub(super) fn new_session_in(
        &mut self,
        cwd: String,
        agent_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cwd
            .update(cx, |s, cx| s.set_value(cwd.clone(), window, cx));
        self.store.update(cx, |s, cx| {
            s.set_cwd(cwd);
            s.set_primary_agent(&agent_id, cx);
        });
        self.new_session(window, cx);
    }

    /// Flat list entries of `group`: its head, then (when open) its rows.
    fn push_group_entries(
        &self,
        out: &mut Vec<Entry>,
        group: Group,
        first: bool,
        rc: &RowCtx,
        store: &AgentsStore,
    ) {
        let Group {
            key,
            name,
            kind,
            sessions,
        } = group;
        let special = kind != GroupKind::Workspace;
        // While searching every group is open, as in the desktop app.
        let collapsed = !rc.searching && !self.session_groups.is_open(&key);
        let total = sessions.len();
        let latest = sessions
            .first()
            .map(|s| s.updated_at.clone())
            .unwrap_or_default();
        let running = sessions.iter().filter(|s| s.running()).count();
        // Grows by GROUP_PAGE per "Show more", or straight to all.
        let shown = self
            .group_shown
            .get(&key)
            .copied()
            .unwrap_or(GroupLimit::First(GROUP_ROWS))
            .shown(total);
        // Machines of remote sessions in this workspace.
        let machines: Vec<String> = sessions
            .iter()
            .filter_map(|s| s.remote.as_ref().map(|r| r.name()))
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        let has_local = sessions.iter().any(|s| s.remote.is_none());
        // A local session's connected agent can start another one here.
        let owner = (!special && !key.is_empty())
            .then(|| {
                sessions.iter().find(|s| {
                    s.remote.is_none() && rc.agents.get(&s.agent_id).is_some_and(|a| a.connected)
                })
            })
            .flatten()
            .map(|s| s.agent_id.clone());
        out.push(Entry::Head(GroupHead {
            key: key.clone(),
            name,
            kind,
            collapsed,
            total,
            latest,
            running,
            machines,
            has_local,
            owner,
            first,
            copied: self.copied.as_deref() == Some(workspace_copy_key(&key).as_str()),
        }));
        if collapsed {
            return;
        }
        for s in sessions.iter().take(shown) {
            self.push_session_entries(out, s, &key, special, 0, rc, store);
        }
        if total > GROUP_ROWS {
            out.push(Entry::More { key, shown, total });
        }
    }

    /// Filters button beside the search, with the number of refinements
    /// beyond the tab, and its popover (desktop `session-filter-popover`).
    fn session_filters_button(
        &self,
        filters: SessionFilters,
        active: usize,
        show_location: bool,
        p: &Palette,
        cx: &Context<Self>,
    ) -> AnyElement {
        let trigger = kit::button("sessions-filters")
            .ghost()
            .h_10()
            .bg(p.card)
            .icon(IconName::SlidersHorizontal)
            .label("Filters")
            .when(active > 0, |button| {
                button
                    .accessibility_label(format!("Filters, {active} active"))
                    .child(kit::chip(active.to_string(), p))
            });

        fn row<T: Copy + PartialEq + 'static>(
            id: &'static str,
            label: &'static str,
            current: T,
            options: &[(T, &'static str)],
            set: fn(&mut SessionFilters, T),
            p: &Palette,
            owner: &WeakEntity<QmtApp>,
        ) -> Div {
            h_flex()
                .gap_4()
                .justify_between()
                .child(div().text_sm().text_color(p.muted).child(label))
                .child(kit::segment_bar(p).children(options.iter().enumerate().map(
                    |(ix, (value, text))| {
                        let (value, owner) = (*value, owner.clone());
                        kit::segment((id, ix), value == current, p)
                            .label(*text)
                            .on_click(move |_, _, cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    set(&mut this.session_filters, value);
                                    cx.notify();
                                });
                            })
                    },
                )))
        }

        let owner = cx.entity().downgrade();
        let p = *p;
        gpui_kit::component::popover::Popover::new("sessions-filters-popover")
            .anchor(Anchor::TopRight)
            .trigger(trigger)
            .content(move |_, _, cx| {
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(div().font_semibold().child("Filters"))
                            .child(
                                kit::row_icon_button(
                                    "sessions-filters-close",
                                    IconName::X,
                                    "Close filters",
                                    &p,
                                    cx,
                                )
                                .xsmall()
                                .on_click(
                                    cx.listener(|state, _, window, cx| state.dismiss(window, cx)),
                                ),
                            ),
                    )
                    .child(row(
                        "runtime",
                        "Runtime",
                        filters.runtime,
                        &[
                            (RuntimeFilter::Any, "Any"),
                            (RuntimeFilter::Running, "Running"),
                            (RuntimeFilter::NeedsInput, "Needs input"),
                        ],
                        |f, v| f.runtime = v,
                        &p,
                        &owner,
                    ))
                    .when(show_location, |el| {
                        el.child(row(
                            "location",
                            "Location",
                            filters.location,
                            &[
                                (LocationFilter::Any, "Any"),
                                (LocationFilter::Local, "Local"),
                                (LocationFilter::Remote, "Remote"),
                            ],
                            |f, v| f.location = v,
                            &p,
                            &owner,
                        ))
                    })
                    .child(div().border_t_1().border_color(p.border))
                    .child(row(
                        "resolution",
                        "Resolution",
                        filters.resolution,
                        &[
                            (ResolutionFilter::Any, "Any"),
                            (ResolutionFilter::Open, "Open"),
                            (ResolutionFilter::Resolved, "Resolved"),
                        ],
                        |f, v| f.resolution = v,
                        &p,
                        &owner,
                    ))
                    .child(row(
                        "archive",
                        "Archived",
                        filters.archive,
                        &[
                            (ArchiveFilter::Hide, "Hide"),
                            (ArchiveFilter::Only, "Only"),
                            (ArchiveFilter::Include, "Include"),
                        ],
                        |f, v| f.archive = v,
                        &p,
                        &owner,
                    ))
            })
            .into_any_element()
    }

    /// Paging footer of a group: Show 10 more and Show all while sessions
    /// remain, Show less once more than the first rows are listed, and how
    /// many of the group's sessions are listed.
    fn render_group_more(
        &self,
        key: &str,
        shown: usize,
        total: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remaining = total - shown;
        let step = remaining.min(GROUP_PAGE);
        let button = |id: &'static str, icon: IconName, label: String| {
            kit::flat_btn(kit::keyed(id, key.to_string()), true, true, p)
                .h(px(28.))
                .px(px(10.))
                .child(kit::icon(icon, 13., p.muted))
                .child(label)
        };
        h_flex()
            // Under the session titles: expander, gap and identicon.
            .pl(px(30.))
            .pt_1()
            .pb_2()
            .gap_2()
            .when(remaining > 0, |el| {
                el.child(
                    button(
                        "group-more",
                        IconName::ChevronDown,
                        format!("Show {step} more"),
                    )
                    .on_click(show_rows(
                        key,
                        GroupLimit::First(shown + step),
                        cx,
                    )),
                )
                .when(remaining > step, |el| {
                    el.child(
                        button(
                            "group-all",
                            IconName::ChevronsDown,
                            format!("Show all ({remaining} left)"),
                        )
                        .on_click(show_rows(key, GroupLimit::All, cx)),
                    )
                })
            })
            .when(shown > GROUP_ROWS, |el| {
                el.child(
                    button("group-less", IconName::ChevronsUp, "Show less".into())
                        .on_click(show_rows(key, GroupLimit::First(GROUP_ROWS), cx)),
                )
            })
            .child(
                div()
                    .ml_auto()
                    .pr_2()
                    .text_xs()
                    .text_color(p.dim)
                    .child(format!("{shown} of {total}")),
            )
            .into_any_element()
    }

    /// One entry of the virtualized list, centered like the page header.
    fn render_session_entry(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let entries = self.session_list.entries.clone();
        let (Some(entry), Some(rc)) = (entries.get(ix), self.session_list.rc.clone()) else {
            return div().into_any_element();
        };
        let p = &rc.p;
        let el = match entry {
            Entry::Head(head) => self.render_group_head(head, &rc, cx),
            Entry::Row {
                s,
                scope,
                in_pinned,
                depth,
                ..
            } => self.render_session_row(s, scope, *in_pinned, *depth, &rc, cx),
            Entry::More { key, shown, total } => self.render_group_more(key, *shown, *total, p, cx),
            Entry::ChildError {
                agent,
                id,
                row_key,
                depth,
                error,
            } => {
                let (agent, id) = (agent.clone(), id.clone());
                h_flex()
                    .pl(child_indent(*depth))
                    .py_1()
                    .gap_2()
                    .text_xs()
                    .text_color(p.danger)
                    .child(error.clone())
                    .child(
                        kit::button(kit::keyed("children-retry", row_key.clone()))
                            .ghost()
                            .xsmall()
                            .label("Retry")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store
                                    .update(cx, |st, cx| st.load_children(&agent, &id, cx))
                            })),
                    )
                    .into_any_element()
            }
            Entry::Note { depth, text, .. } => div()
                .pl(child_indent(*depth))
                .py_1()
                .text_xs()
                .text_color(p.dim)
                .child(*text)
                .into_any_element(),
        };
        div()
            .w_full()
            .flex()
            .justify_center()
            .px_8()
            .child(div().w_full().max_w(px(1040.)).child(el))
            .into_any_element()
    }

    fn render_group_head(&self, h: &GroupHead, rc: &RowCtx, cx: &mut Context<Self>) -> AnyElement {
        let p = &rc.p;
        let key = h.key.clone();
        let special = h.kind != GroupKind::Workspace;
        let collapsed = h.collapsed;
        let hover = p.card;
        let k1 = key.clone();
        let group = SharedString::from(format!("group-head-{key}"));
        let trigger = h_flex()
            .id(kit::keyed("group-head", key.clone()))
            .group(group.clone())
            .flex_1()
            .min_w_0()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(px(8.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(kit::icon(
                if h.kind == GroupKind::Pinned {
                    IconName::Pin
                } else if h.kind == GroupKind::Recent {
                    IconName::Clock
                } else if collapsed {
                    IconName::Folder
                } else {
                    IconName::FolderOpen
                },
                14.,
                p.dim,
            ))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_semibold()
                    .child(h.name.clone()),
            )
            .when(!special && !key.is_empty(), |el| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(p.dim)
                        .font_family(kit::MONO)
                        .child(key.clone()),
                )
                .child({
                    // Shown on hover (or while confirming a copy), like the
                    // transcript's copy buttons, with the same check feedback.
                    let (path, copy_key) = (key.clone(), workspace_copy_key(&key));
                    let copied = h.copied;
                    div()
                        .flex_none()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .when(copied, |el| el.visible())
                        .child(
                            kit::row_icon_button(
                                kit::keyed("copy-workspace", key.clone()),
                                if copied {
                                    IconName::Check
                                } else {
                                    IconName::Copy
                                },
                                if copied {
                                    "Copied"
                                } else {
                                    "Copy project path"
                                },
                                p,
                                cx,
                            )
                            .size(px(24.))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    // The group header toggles on click; copying must not.
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                                    this.flash_copied(copy_key.clone(), cx);
                                },
                            )),
                        )
                })
            })
            .when(!special && !h.machines.is_empty(), |el| {
                let names = &h.machines;
                let text = match (h.has_local, names.len()) {
                    (true, 1) => format!("local + {}", names[0]),
                    (true, n) => format!("local + {n} machines"),
                    (false, 1) => names[0].clone(),
                    (false, n) => format!("{n} machines"),
                };
                let tip = format!(
                    "{}remote workspace on {}",
                    if h.has_local { "Local and " } else { "" },
                    names.join(", ")
                );
                el.child(with_tip(
                    kit::keyed("group-machines", key.clone()),
                    kit::icon_chip(IconName::Network, text, p),
                    tip,
                ))
            })
            .child(div().flex_1())
            .when(h.running > 0, |el| {
                el.child(kit::tone_chip(format!("{} running", h.running), p.success))
            })
            .when(!special, |el| {
                let short = div()
                    .text_xs()
                    .text_color(p.dim)
                    .child(short_time(&h.latest));
                el.child(match absolute_time(&h.latest) {
                    Some(full) => with_tip(kit::keyed("group-time", key.clone()), short, full),
                    None => short.into_any_element(),
                })
            })
            .child(div().text_xs().text_color(p.dim).child(h.total.to_string()))
            .child(kit::icon(
                if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                },
                14.,
                p.dim,
            ))
            .when(!rc.searching, |el| {
                el.on_click(cx.listener(move |this, _, _, cx| {
                    this.session_groups.toggle(&k1);
                    cx.notify();
                }))
            });
        h_flex()
            .gap_1()
            // The groups' former column gap.
            .when(!h.first, |el| el.pt_2())
            .child(trigger)
            .when_some(h.owner.clone(), |el, agent| {
                let cwd = key.clone();
                el.child(
                    kit::icon_button(
                        kit::keyed("group-new", key.clone()),
                        IconName::Plus,
                        format!("New session in {}", h.name),
                    )
                    .xsmall()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.new_session_in(cwd.clone(), agent.clone(), window, cx)
                    })),
                )
            })
            .into_any_element()
    }

    /// The entry of `s`, followed by its child sessions when expanded.
    #[allow(clippy::too_many_arguments)]
    fn push_session_entries(
        &self,
        out: &mut Vec<Entry>,
        s: &SessionSummary,
        scope: &str,
        in_pinned: bool,
        depth: usize,
        rc: &RowCtx,
        store: &AgentsStore,
    ) {
        let connected = rc.agents.get(&s.agent_id).is_some_and(|a| a.connected);
        let open_key = format!("{}:{}", s.agent_id, s.id);
        let expandable = (s.fork_count > 0 || s.has_children) && connected;
        let expanded = expandable && self.session_children.contains(&open_key);
        let rt = store.runtime(&s.agent_id);
        let state_sig = hash_of(|h| {
            expanded.hash(h);
            rt.is_some_and(|r| r.session_pending.contains(&s.id))
                .hash(h);
            rt.and_then(|r| r.session_errors.get(&s.id)).hash(h);
        });
        out.push(Entry::Row {
            s: Box::new(s.clone()),
            scope: scope.to_string(),
            in_pinned,
            depth,
            state_sig,
        });
        if !expanded {
            return;
        }
        let row_key: SharedString = format!("{scope}/{}:{}", s.agent_id, s.id).into();
        let note = |text: &'static str| Entry::Note {
            parent: row_key.clone(),
            depth,
            text,
        };
        match rt.and_then(|r| r.children.get(&s.id)) {
            Some(list) if list.error.is_some() && list.sessions.is_empty() => {
                out.push(Entry::ChildError {
                    agent: s.agent_id.clone(),
                    id: s.id.clone(),
                    row_key: row_key.clone(),
                    depth,
                    error: list.error.clone().unwrap_or_default(),
                });
            }
            Some(list) if !list.sessions.is_empty() => {
                if depth + 1 > MAX_DEPTH {
                    out.push(note("Open this session to see deeper children."));
                    return;
                }
                for child in &list.sessions {
                    self.push_session_entries(out, child, scope, in_pinned, depth + 1, rc, store);
                }
            }
            Some(list) if list.loaded && !list.loading => out.push(note("No child sessions.")),
            _ => out.push(note("Loading child sessions…")),
        }
    }

    /// The row of one session.
    fn render_session_row(
        &self,
        s: &SessionSummary,
        scope: &str,
        in_pinned: bool,
        depth: usize,
        rc: &RowCtx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = &rc.p;
        let info = rc.agents.get(&s.agent_id).cloned().unwrap_or_default();
        // A session can appear in several groups; keep each row's ids distinct.
        let row_key: SharedString = format!("{scope}/{}:{}", s.agent_id, s.id).into();
        let open_key = format!("{}:{}", s.agent_id, s.id);
        let expandable = (s.fork_count > 0 || s.has_children) && info.connected;
        let expanded = expandable && self.session_children.contains(&open_key);
        let (pending, action_error) = {
            let rt = self.store.read(cx).runtime(&s.agent_id);
            (
                rt.is_some_and(|r| r.session_pending.contains(&s.id)),
                rt.and_then(|r| r.session_errors.get(&s.id).cloned()),
            )
        };

        let expander = if expandable {
            let (agent, id, key) = (s.agent_id.clone(), s.id.clone(), open_key.clone());
            kit::icon_button(
                kit::keyed("session-expand", row_key.clone()),
                if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                if expanded {
                    "Hide child sessions"
                } else {
                    "Show child sessions"
                },
            )
            .xsmall()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.session_children.remove(&key) {
                    this.session_children.insert(key.clone());
                    this.store
                        .update(cx, |st, cx| st.load_children(&agent, &id, cx));
                }
                cx.notify();
            }))
            .into_any_element()
        } else {
            div().flex_none().w(px(22.)).into_any_element()
        };

        let status: Option<AnyElement> = if s.running() {
            Some(
                kit::spinner(kit::keyed("row-spin", row_key.clone()), 13., p.success)
                    .into_any_element(),
            )
        } else if s.status_unknown() {
            Some(with_tip(
                kit::keyed("row-unknown", row_key.clone()),
                kit::icon(IconName::CircleQuestionMark, 13., p.dim),
                "Status unavailable",
            ))
        } else {
            None
        };
        let time = {
            let short = div()
                .w(px(48.))
                .text_right()
                .text_xs()
                .text_color(p.dim)
                .child(short_time(&s.updated_at));
            match absolute_time(&s.updated_at) {
                Some(full) => with_tip(kit::keyed("row-time", row_key.clone()), short, full),
                None => short.into_any_element(),
            }
        };
        let meta = h_flex()
            .flex_wrap()
            .gap_1p5()
            .text_xs()
            .text_color(p.dim)
            .when(in_pinned, |el| {
                el.child(workspace_name(s.group_cwd())).child("·")
            })
            .when(rc.show_agent, |el| el.child(info.name.clone()).child("·"))
            .child(div().font_family(kit::MONO).child(s.short_id()))
            .when(s.messages > 0, |el| {
                el.child("·").child(format!("{} msgs", s.messages))
            })
            .children(self.session_badges(s, rc, in_pinned, &row_key, action_error));

        let main = h_flex()
            .id(kit::keyed("session-open", row_key.clone()))
            .flex_1()
            .min_w_0()
            .gap_3()
            .cursor_pointer()
            .on_click({
                let s = s.clone();
                cx.listener(move |this, _, _, cx| this.open_summary(&s, cx))
            })
            .child(kit::identicon(&s.id, 18.))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_semibold()
                            .child(s.display_title()),
                    )
                    .child(meta),
            )
            .children(status)
            .child(time);

        let hover = p.card;
        h_flex()
            .id(kit::keyed("session-row", row_key.clone()))
            .gap_1()
            .pl(px(depth as f32 * CHILD_INDENT + 2.))
            .pr_1()
            .py_1p5()
            .rounded(px(10.))
            .hover(move |el| el.bg(hover))
            .child(expander)
            .child(main)
            // A little apart from the time beside it.
            .child(
                div()
                    .flex_none()
                    .ml_2()
                    .child(self.session_menu(s, &info, pending, &row_key, cx)),
            )
            .into_any_element()
    }

    /// Relationship, worktree, machine and lifecycle badges of one row.
    fn session_badges(
        &self,
        s: &SessionSummary,
        rc: &RowCtx,
        in_pinned: bool,
        row_key: &SharedString,
        action_error: Option<String>,
    ) -> Vec<AnyElement> {
        let p = &rc.p;
        let id = |name: &'static str| kit::keyed(name, row_key.clone());
        let mut out = Vec::new();
        if s.pinned && !in_pinned {
            out.push(with_tip(
                id("badge-pin"),
                kit::icon(IconName::Pin, 12., p.dim),
                "Pinned",
            ));
        }
        if s.parent.is_some() {
            let (icon, label) = match s.fork_origin.as_deref() {
                Some("user") => (IconName::GitFork, "Fork"),
                Some("worktree") => (IconName::GitBranch, "Worktree"),
                Some("delegation") => (IconName::Bot, "Delegate"),
                _ => (IconName::GitFork, "Child"),
            };
            out.push(kit::icon_chip(icon, label, p).into_any_element());
        }
        if let Some(w) = &s.worktree {
            let branch = w.branch.clone().unwrap_or_else(|| "worktree".into());
            let chip = if w.usable() {
                kit::icon_chip(IconName::GitBranch, branch, p)
            } else {
                let state = w.lifecycle.as_deref().unwrap_or("unavailable");
                kit::icon_tone_chip(
                    IconName::TriangleAlert,
                    format!("{branch} · {state}"),
                    p.warn,
                )
            };
            out.push(with_tip(id("badge-worktree"), chip, worktree_tip(w)));
            if w.handed_off_to.is_some() {
                out.push(with_tip(
                    id("badge-handoff"),
                    kit::chip("Handed off", p),
                    "This worktree was handed off to another session",
                ));
            }
        }
        if s.fork_count > 0 {
            let n = s.fork_count;
            out.push(
                kit::icon_chip(
                    IconName::GitFork,
                    format!("{n} {}", if n == 1 { "fork" } else { "forks" }),
                    p,
                )
                .into_any_element(),
            );
        }
        if let Some(r) = &s.remote {
            let chip = if r.connected {
                kit::icon_chip(IconName::Network, r.name(), p)
            } else {
                kit::icon_tone_chip(
                    IconName::Network,
                    format!("{} · disconnected", r.name()),
                    p.warn,
                )
            };
            out.push(with_tip(
                id("badge-remote"),
                chip,
                format!("Remote session on {}", r.name()),
            ));
        }
        if s.lifecycle_known {
            if s.resolved {
                out.push(with_tip(
                    id("badge-resolved"),
                    kit::icon_tone_chip(IconName::CircleCheck, "Resolved", p.success),
                    "This session's purpose is resolved. This does not indicate execution success.",
                ));
            }
            if s.archived {
                out.push(with_tip(
                    id("badge-archived"),
                    kit::icon_tone_chip(IconName::Archive, "Archived", p.status_muted),
                    "Hidden from the working list. Execution and schedules are unchanged.",
                ));
            }
        } else if rc.all_tab {
            out.push(kit::chip("Lifecycle unavailable", p).into_any_element());
        }
        if s.has_errors {
            out.push(with_tip(
                id("badge-errors"),
                kit::icon(IconName::TriangleAlert, 12., p.running),
                "The last turn ended with an error",
            ));
        }
        if let Some(error) = action_error {
            out.push(with_tip(
                id("badge-action-error"),
                kit::icon_tone_chip(IconName::CircleAlert, "Action failed", p.danger),
                error,
            ));
        }
        out
    }

    /// Row actions (desktop `SessionActionsMenu`): pin, resolve, archive,
    /// copy id and delete, each only when the owning agent supports it.
    fn session_menu(
        &self,
        s: &SessionSummary,
        info: &AgentInfo,
        pending: bool,
        row_key: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *palette(cx);
        let trigger = kit::row_icon_button(
            kit::keyed("session-menu", row_key.clone()),
            IconName::Ellipsis,
            "Session actions",
            &p,
            cx,
        )
        .size(px(SESSION_MENU_SIZE));
        if pending {
            return trigger.loading(true).into_any_element();
        }
        let local = s.remote.is_none();
        let can_pin = info.can_pin && s.root && local;
        let can_lifecycle = info.can_lifecycle
            && s.lifecycle_known
            && s.remote.as_ref().is_none_or(|r| r.connected);
        let can_delete = info.can_delete && local;
        let (store, app, s) = (self.store.clone(), cx.entity().downgrade(), s.clone());
        trigger
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let (agent, sid) = (s.agent_id.clone(), s.id.clone());
                let (pinned, resolved, archived) = (s.pinned, s.resolved, s.archived);
                menu.when(can_pin, |m| {
                    let (store, agent, sid) = (store.clone(), agent.clone(), sid.clone());
                    m.item(
                        PopupMenuItem::new(if pinned {
                            "Unpin session"
                        } else {
                            "Pin session"
                        })
                        .icon(Icon::new(if pinned {
                            IconName::PinOff
                        } else {
                            IconName::Pin
                        }))
                        .on_click(move |_, _, cx| {
                            store.update(cx, |st, cx| st.set_pinned(&agent, &sid, !pinned, cx))
                        }),
                    )
                })
                .map(|m| {
                    if !can_lifecycle {
                        return m.label(if s.lifecycle_known {
                            "Lifecycle controls unavailable"
                        } else {
                            "Lifecycle state is unknown"
                        });
                    }
                    let (st1, a1, s1) = (store.clone(), agent.clone(), sid.clone());
                    let (st2, a2, s2) = (store.clone(), agent.clone(), sid.clone());
                    m.item(
                        PopupMenuItem::new(if resolved { "Reopen" } else { "Mark resolved" })
                            .icon(Icon::new(if resolved {
                                IconName::RotateCcw
                            } else {
                                IconName::CircleCheck
                            }))
                            .on_click(move |_, _, cx| {
                                st1.update(cx, |st, cx| {
                                    st.set_lifecycle(&a1, &s1, Some(!resolved), None, cx)
                                })
                            }),
                    )
                    .item(
                        PopupMenuItem::new(if archived { "Restore" } else { "Archive" })
                            .icon(Icon::new(if archived {
                                IconName::ArchiveRestore
                            } else {
                                IconName::Archive
                            }))
                            .on_click(move |_, _, cx| {
                                st2.update(cx, |st, cx| {
                                    st.set_lifecycle(&a2, &s2, None, Some(!archived), cx)
                                })
                            }),
                    )
                })
                .separator()
                .item(
                    PopupMenuItem::new("Copy session ID")
                        .icon(Icon::new(IconName::Copy))
                        .on_click({
                            let sid = sid.clone();
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(sid.clone()))
                            }
                        }),
                )
                .when(can_delete, |m| {
                    let (app, s) = (app.clone(), s.clone());
                    m.separator().item(
                        PopupMenuItem::new("Delete session…")
                            .icon(Icon::new(IconName::Trash))
                            .on_click(move |_, _, cx| {
                                let s = s.clone();
                                let _ = app.update(cx, |this, cx| {
                                    this.confirm_session_delete = Some(s);
                                    cx.notify();
                                });
                            }),
                    )
                })
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{
        ArchiveFilter, GROUP_PAGE, GROUP_ROWS, Group, GroupKind, GroupLimit, GroupOpenState,
        LocationFilter, PINNED_KEY, RECENT_KEY, ResolutionFilter, RuntimeFilter, SessionFilters,
        SessionSummary, SessionTab, Worktree, in_tab, matches_lifecycle, matches_query,
        matches_refinements,
    };

    fn group(key: &str, kind: GroupKind) -> Group {
        Group {
            key: key.into(),
            name: key.into(),
            kind,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn only_pinned_or_recent_start_open() {
        // Pinned present: only Pinned opens; Recent and workspaces stay closed.
        let mut state = GroupOpenState::default();
        state.initialize(&[
            group(PINNED_KEY, GroupKind::Pinned),
            group(RECENT_KEY, GroupKind::Recent),
            group("/w", GroupKind::Workspace),
        ]);
        assert!(state.is_open(PINNED_KEY));
        assert!(!state.is_open(RECENT_KEY) && !state.is_open("/w"));

        // No pinned: Recent opens, workspaces stay closed.
        let mut state = GroupOpenState::default();
        state.initialize(&[
            group(RECENT_KEY, GroupKind::Recent),
            group("/w", GroupKind::Workspace),
        ]);
        assert!(state.is_open(RECENT_KEY) && !state.is_open("/w"));

        // Neither: the first workspace opens. Later renders keep user choices.
        let mut state = GroupOpenState::default();
        state.initialize(&[
            group("/w", GroupKind::Workspace),
            group("/x", GroupKind::Workspace),
        ]);
        assert!(state.is_open("/w") && !state.is_open("/x"));
        state.toggle("/w");
        state.initialize(&[group("/w", GroupKind::Workspace)]);
        assert!(!state.is_open("/w"));
    }

    fn session(lifecycle: Option<(bool, bool)>) -> SessionSummary {
        let (resolved, archived) = lifecycle.unwrap_or_default();
        SessionSummary {
            id: "s".into(),
            root: true,
            resolved,
            archived,
            open: !resolved && !archived,
            lifecycle_known: lifecycle.is_some(),
            ..Default::default()
        }
    }

    #[test]
    fn unknown_lifecycle_is_only_listed_in_all() {
        let unknown = session(None);
        assert!(!in_tab(&unknown, SessionTab::Open));
        assert!(!in_tab(&unknown, SessionTab::Archived));
        assert!(in_tab(&unknown, SessionTab::All));
        let resolved = session(Some((true, false)));
        assert!(in_tab(&resolved, SessionTab::Resolved));
        assert!(!in_tab(&resolved, SessionTab::Open));
        let archived = session(Some((true, true)));
        assert!(in_tab(&archived, SessionTab::Archived));
        assert!(!in_tab(&archived, SessionTab::Resolved));
    }

    #[test]
    fn custom_lifecycle_filter_can_show_only_resolved_archived_sessions() {
        let filters = SessionFilters {
            resolution: ResolutionFilter::Resolved,
            archive: ArchiveFilter::Only,
            ..Default::default()
        };
        assert!(filters.tab().is_none());
        assert_eq!(filters.refinements(false), ["Resolved", "Archived only"]);
        let matches =
            |s: &SessionSummary| matches_lifecycle(s, filters.resolution, filters.archive);
        assert!(matches(&session(Some((true, true)))));
        assert!(!matches(&session(Some((false, true)))));
        assert!(!matches(&session(Some((true, false)))));
        assert!(!matches(&session(None)));
    }

    #[test]
    fn runtime_waiting_is_running_unless_input_is_pending() {
        let mut s = session(Some((false, false)));
        s.phase = "waiting".into();
        let mut filters = SessionFilters {
            runtime: RuntimeFilter::Running,
            ..Default::default()
        };
        assert!(matches_refinements(&s, &filters, false));
        assert!(!matches_refinements(&s, &filters, true));
        filters.runtime = RuntimeFilter::NeedsInput;
        assert!(matches_refinements(&s, &filters, true));
        assert!(!matches_refinements(&s, &filters, false));
    }

    #[test]
    fn location_combines_with_runtime_and_clear_preserves_tab() {
        let mut s = session(Some((true, true)));
        let mut filters = SessionFilters {
            runtime: RuntimeFilter::NeedsInput,
            location: LocationFilter::Remote,
            ..Default::default()
        };
        filters.set_tab(SessionTab::Archived);
        assert!(!matches_refinements(&s, &filters, true));
        s.remote = Some(crate::store::RemoteOwner::default());
        assert!(!matches_refinements(&s, &filters, false));
        assert!(matches_refinements(&s, &filters, true));
        assert_eq!(filters.refinements(true), ["Needs input", "Remote"]);
        filters.reset_refinements();
        assert!(filters.tab() == Some(SessionTab::Archived));
        assert!(filters.refinements(true).is_empty());
    }

    #[test]
    fn show_all_includes_sessions_arriving_after_expansion() {
        let one_page = GroupLimit::First(GROUP_ROWS + GROUP_PAGE);
        assert_eq!(one_page.shown(5), 5);
        assert_eq!(one_page.shown(120), 18);
        assert_eq!(GroupLimit::All.shown(120), 120);
        assert_eq!(GroupLimit::All.shown(135), 135);
    }

    #[test]
    fn search_covers_branch_and_machine() {
        let mut s = session(Some((false, false)));
        s.worktree = Some(Worktree {
            branch: Some("feat/list".into()),
            ..Default::default()
        });
        s.remote = Some(crate::store::RemoteOwner {
            node_id: "node".into(),
            label: Some("Workstation".into()),
            connected: true,
        });
        assert!(matches_query(&s, "agent", "feat/list"));
        assert!(matches_query(&s, "agent", "workstation"));
        assert!(!matches_query(&s, "agent", "missing"));
    }
}
