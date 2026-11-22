//! Root view: QueryMT shell (sidebar + page) and modal overlays.

mod agents;
mod automations;
mod chrome;
mod code;
mod connectors;
mod delegates;
mod fork;
mod inbox;
mod keys;
mod kit;
mod log_view;
mod md;
mod mesh;
mod preview;
mod profile_select;
mod session_view;
mod sessions;
mod settings;
mod setup;
mod sidebar;
mod smooth_scroll;
mod start;
mod turn_diff;
mod workspace_select;
mod workspaces;
mod worktree;

use gpui_kit::assets::IconName;
use std::collections::{HashMap, HashSet};

use gpui_kit::component::command::CommandState;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::store::{AgentsStore, Attachment, ConnState, SessionSummary};
use crate::theme::{self, Palette, palette};
use settings::SettingsTab;

gpui_kit::actions!(querymt, [ToggleSidebar]);
use crate::util::{short_time, workspace_name};
pub use keys::bind_keys;

/// Width of the expanded sidebar column, without its 1px hairline.
const SIDEBAR_W: f32 = 252.;
/// Width of the collapsed sidebar rail.
const RAIL_W: f32 = 56.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Start,
    Inbox,
    Sessions,
    Automations,
    Agents,
    Mesh,
    Settings,
    /// The open session's transcript.
    Session,
    /// First-run setup: full window, without the sidebar.
    Setup,
    /// Saved and recent workspace folders.
    Workspaces,
}

pub(crate) struct AgentDialog {
    pub editing: Option<String>,
    pub websocket: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionTab {
    Open,
    Resolved,
    Archived,
    All,
}

/// Sessions-page refinements (desktop `SessionBrowserState`). The lifecycle
/// tabs are presets of `resolution` + `archive`; other combinations select
/// no tab.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionFilters {
    pub runtime: RuntimeFilter,
    pub location: LocationFilter,
    pub resolution: ResolutionFilter,
    pub archive: ArchiveFilter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFilter {
    Any,
    Running,
    NeedsInput,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LocationFilter {
    Any,
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResolutionFilter {
    Any,
    Open,
    Resolved,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFilter {
    Hide,
    Only,
    Include,
}

impl SessionTab {
    pub const ALL: [SessionTab; 4] = [
        SessionTab::Open,
        SessionTab::Resolved,
        SessionTab::Archived,
        SessionTab::All,
    ];

    /// Lifecycle filter the tab stands for.
    pub fn lifecycle(self) -> (ResolutionFilter, ArchiveFilter) {
        match self {
            SessionTab::Open => (ResolutionFilter::Open, ArchiveFilter::Hide),
            SessionTab::Resolved => (ResolutionFilter::Resolved, ArchiveFilter::Hide),
            SessionTab::Archived => (ResolutionFilter::Any, ArchiveFilter::Only),
            SessionTab::All => (ResolutionFilter::Any, ArchiveFilter::Include),
        }
    }
}

impl Default for SessionFilters {
    fn default() -> Self {
        let (resolution, archive) = SessionTab::Open.lifecycle();
        Self {
            runtime: RuntimeFilter::Any,
            location: LocationFilter::Any,
            resolution,
            archive,
        }
    }
}

impl SessionFilters {
    /// The tab matching the lifecycle filter, if any.
    pub fn tab(&self) -> Option<SessionTab> {
        SessionTab::ALL
            .into_iter()
            .find(|t| t.lifecycle() == (self.resolution, self.archive))
    }

    pub fn set_tab(&mut self, tab: SessionTab) {
        (self.resolution, self.archive) = tab.lifecycle();
    }

    /// Lifecycle state narrows the list (anything but All).
    pub fn filters_lifecycle(&self) -> bool {
        (self.resolution, self.archive) != SessionTab::All.lifecycle()
    }

    /// Refinements beyond the selected tab, for the Filters badge and the
    /// summary line (desktop `refinementLabels`).
    pub fn refinements(&self, show_location: bool) -> Vec<&'static str> {
        let mut out = Vec::new();
        match self.runtime {
            RuntimeFilter::Any => {}
            RuntimeFilter::Running => out.push("Running"),
            RuntimeFilter::NeedsInput => out.push("Needs input"),
        }
        if show_location {
            match self.location {
                LocationFilter::Any => {}
                LocationFilter::Local => out.push("Local"),
                LocationFilter::Remote => out.push("Remote"),
            }
        }
        if self.tab().is_none() {
            out.push(match self.resolution {
                ResolutionFilter::Any => "Any resolution",
                ResolutionFilter::Open => "Open",
                ResolutionFilter::Resolved => "Resolved",
            });
            out.push(match self.archive {
                ArchiveFilter::Hide => "Not archived",
                ArchiveFilter::Only => "Archived only",
                ArchiveFilter::Include => "Including archived",
            });
        }
        out
    }

    /// Clear runtime and location, and return to Open if no tab matches.
    pub fn reset_refinements(&mut self) {
        self.runtime = RuntimeFilter::Any;
        self.location = LocationFilter::Any;
        if self.tab().is_none() {
            self.set_tab(SessionTab::Open);
        }
    }
}

pub struct QmtApp {
    store: Entity<AgentsStore>,
    page: Page,
    /// Eases mouse-wheel notches into animated scrolls for every container.
    smooth_scroll: smooth_scroll::SmoothScroll,
    cwd: Entity<InputState>,
    prompt: Entity<TextareaState>,
    model_open: bool,
    model_query: Entity<InputState>,
    options_open: bool,
    attachments: Vec<Attachment>,
    attach_error: Option<String>,
    session_query: Entity<InputState>,
    session_filters: SessionFilters,
    automations: automations::AutomationPage,
    delegates: delegates::DelegateDialog,
    setup: setup::SetupState,
    workspaces: workspaces::WorkspacesPage,
    /// Which Sessions-page groups are open.
    session_groups: sessions::GroupOpenState,
    /// Virtualized Sessions page list.
    session_list: sessions::SessionList,
    /// Display limits for expanded Sessions-page groups, keyed by group.
    group_shown: HashMap<String, sessions::GroupLimit>,
    /// `agent:session` keys of Sessions-page rows showing their child sessions.
    session_children: HashSet<String>,
    /// Session awaiting delete confirmation.
    confirm_session_delete: Option<SessionSummary>,
    dialog: Option<AgentDialog>,
    details: Option<String>,
    confirm_delete: Option<String>,
    d_name: Entity<InputState>,
    d_command: Entity<InputState>,
    d_address: Entity<InputState>,
    header_drag: bool,
    /// The last header press landed on a control rather than the bare bar.
    header_press_claimed: bool,
    invite_form: bool,
    /// The created invite shows its QR code.
    invite_qr: bool,
    invite_ttl: &'static str,
    invite_uses: Entity<InputState>,
    settings_tab: SettingsTab,
    key_editing: Option<String>,
    /// Providers settings: expanded provider rows, the open auth-method menu,
    /// and the Advanced maintenance / failed-plugin disclosures.
    provider_open: HashSet<String>,
    method_menu: Option<String>,
    maintenance_open: bool,
    failed_plugins_open: bool,
    /// General settings: Advanced / Danger zone disclosures and the reset confirmation.
    general_advanced_open: bool,
    general_danger_open: bool,
    confirm_reset: bool,
    /// Profiles settings: curated templates (or why they could not be read),
    /// per-row feedback `(message, is_error)`, open Details rows and the
    /// "How profiles work" disclosure.
    templates: Result<Vec<crate::profile_templates::TemplateInfo>, String>,
    template_feedback: HashMap<String, (String, bool)>,
    template_details: HashSet<String>,
    templates_help_open: bool,
    /// Workspace undo awaiting confirmation: user turn to go back to.
    confirm_undo: Option<String>,
    /// Fork from a reply awaiting confirmation, or being created.
    fork: Option<fork::ForkDialog>,
    /// New worktree session / worktree handoff dialog.
    worktree: Option<worktree::WorktreeDialog>,
    /// Turn diff panel beside the chat.
    turn_diff: turn_diff::TurnDiff,
    /// Appearance settings: the mouse-wheel scrolling disclosure.
    appearance_scroll_open: bool,
    key_input: Entity<InputState>,
    oauth_code: Entity<InputState>,
    session_prompt: Entity<TextareaState>,
    transcript: session_view::Transcript,
    session_attachments: Vec<Attachment>,
    session_attach_error: Option<String>,
    /// Open image lightbox (composer or chat attachments).
    preview: Option<preview::Preview>,
    preview_focus: FocusHandle,
    /// Options popover of the session composer.
    session_options_open: bool,
    /// The activity dock's waiting-messages panel is open.
    waiting_open: bool,
    /// When Escape was last pressed during a running turn (double Esc cancels).
    last_escape: Option<std::time::Instant>,
    /// Header details popover (context, token usage, cost, active time).
    session_details_open: bool,
    /// Details popover shows session totals instead of the latest request.
    usage_totals: bool,
    /// Token category pinned (clicked) / hovered in the usage rings.
    usage_pinned: Option<crate::usage::TokenKey>,
    usage_hovered: Option<crate::usage::TokenKey>,
    /// Session actions (resolve / archive / pin) menu.
    session_menu_open: bool,
    /// `steer` or `queue`: how a message is delivered while a turn runs.
    delivery: &'static str,
    /// Item key whose text was just copied (shows a check for a moment).
    copied: Option<String>,
    /// Expanded tool calls / thoughts (by id).
    expanded_items: HashSet<String>,
    plan_open: bool,
    /// Text inputs of request forms, by (request key, field, custom answer).
    form_inputs: HashMap<(String, usize, bool), (Entity<InputState>, Subscription)>,
    /// Inbox cards the user expanded / collapsed (others use the default).
    request_collapsed: HashMap<String, bool>,
    /// Session id to open once its agent has listed it (deep link).
    pending_open: Option<String>,
    /// Focus of the app shell: keeps global shortcuts working when no
    /// control is focused.
    app_focus: FocusHandle,
    /// Command palette (Ctrl/Cmd+P).
    palette: Entity<CommandState>,
    palette_open: bool,
    /// Selects of the Start Options (Profile, Target): the open one, its
    /// search and highlighted row (one menu is open at a time).
    option_menu: Option<profile_select::OptionMenu>,
    profile_query: Entity<InputState>,
    profile_cursor: usize,
    profile_scroll: ScrollHandle,
    /// Last painted trigger bounds per select and window height, to place the menu.
    option_triggers: [Bounds<Pixels>; 2],
    profile_viewport_h: Pixels,
    /// Where the open select menu was placed (it overhangs the Options panel).
    profile_menu_bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// Popovers just closed by an outside mouse down, so their triggers toggle.
    dismissed: kit::Dismissed,
    /// Start composer's workspace field, editing `cwd`.
    workspace: Entity<workspace_select::WorkspaceField>,
    /// Full-size log viewer (agent logs).
    log_view: log_view::LogView,
    /// Sidebar session tabs: paging, dragging, the right-click menu.
    side: sidebar::SideUi,
    _subs: Vec<Subscription>,
}

impl QmtApp {
    pub fn new(store: Entity<AgentsStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd_value = store.read(cx).prefs.cwd.clone();
        let cwd = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/on/the/agent/machine")
                .default_value(cwd_value)
        });
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .placeholder("Ask QueryMT to inspect, change, debug, explain, or plan something.")
        });
        let model_query = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search models, providers, nodes…"));
        let session_query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search sessions, workspaces, agents…")
        });
        let d_name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Build server"));
        let d_command = cx.new(|cx| InputState::new(window, cx).placeholder("qmtcode --acp"));
        let d_address = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1:42069"));
        let invite_uses = cx.new(|cx| InputState::new(window, cx).placeholder("Unlimited"));
        let key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Paste API key")
                .masked(true)
        });
        let oauth_code =
            cx.new(|cx| InputState::new(window, cx).placeholder("Callback URL or code"));
        let session_prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .placeholder("Reply to the agent…")
        });

        let mut subs = vec![
            cx.observe(&store, |this, store, cx| {
                // Drop inputs of requests that left the inbox.
                let s = store.read(cx);
                this.form_inputs
                    .retain(|(k, _, _), _| s.requests.iter().any(|r| &r.key == k));
                this.request_collapsed
                    .retain(|k, _| s.requests.iter().any(|r| &r.key == k));
                this.open_pending_session(cx);
                if this.page == Page::Automations {
                    this.load_automations(false, cx);
                }
                cx.notify();
            }),
            // Follow the OS light/dark switch when the theme is set to System.
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.store.read(cx).prefs.dark.is_none() {
                    this.apply_theme(window, cx);
                }
            }),
            cx.subscribe_in(&prompt, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&cwd, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &model_query,
                window,
                |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.pick_first_model(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(&session_prompt, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&session_query, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        for input in [&d_name, &d_command, &d_address] {
            subs.push(
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, _, cx| match ev {
                    InputEvent::PressEnter { .. } => this.save_dialog(cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }),
            );
        }

        let app_focus = cx.focus_handle();
        app_focus.focus(window, cx);
        // Removing a focused control (closing a popover, the palette, …)
        // returns focus to the shell so shortcuts keep working.
        subs.push(cx.on_focus_lost(window, |this, window, cx| this.app_focus.focus(window, cx)));
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let profile_query = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search by name, ID or description"));
        subs.push(cx.subscribe_in(
            &profile_query,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.pick_option_at_cursor(window, cx),
                InputEvent::Change => {
                    this.profile_cursor = 0;
                    this.profile_scroll.scroll_to_item(0);
                    cx.notify();
                }
                _ => {}
            },
        ));

        let (page, details) = initial_view(&store, cx);
        let smooth_scroll = smooth_scroll::SmoothScroll::default();
        let workspace = {
            let (store, cwd, scroll) = (store.clone(), cwd.clone(), smooth_scroll.clone());
            cx.new(|cx| workspace_select::WorkspaceField::new(store, cwd, false, scroll, cx))
        };
        let workspaces =
            workspaces::WorkspacesPage::new(store.clone(), smooth_scroll.clone(), window, cx);
        subs.push(workspaces.subscribe(window, cx));
        let (log_view, log_sub) = log_view::LogView::new(window, cx);
        subs.push(log_sub);
        let mut this = Self {
            store,
            page,
            smooth_scroll,
            cwd,
            prompt,
            model_open: false,
            model_query,
            options_open: false,
            attachments: Vec::new(),
            attach_error: None,
            session_query,
            session_filters: SessionFilters::default(),
            automations: Default::default(),
            delegates: Default::default(),
            setup: Default::default(),
            workspaces,
            session_groups: Default::default(),
            session_list: Default::default(),
            group_shown: HashMap::new(),
            session_children: HashSet::new(),
            confirm_session_delete: None,
            dialog: None,
            details,
            confirm_delete: None,
            d_name,
            d_command,
            d_address,
            header_drag: false,
            header_press_claimed: false,
            invite_form: false,
            invite_qr: false,
            invite_ttl: "24h",
            invite_uses,
            settings_tab: SettingsTab::General,
            key_editing: None,
            provider_open: HashSet::new(),
            method_menu: None,
            maintenance_open: false,
            failed_plugins_open: false,
            general_advanced_open: false,
            general_danger_open: false,
            templates: crate::profile_templates::list(),
            template_feedback: HashMap::new(),
            template_details: HashSet::new(),
            templates_help_open: false,
            confirm_reset: false,
            confirm_undo: None,
            fork: None,
            worktree: None,
            turn_diff: Default::default(),
            appearance_scroll_open: false,
            key_input,
            oauth_code,
            session_prompt,
            transcript: session_view::Transcript::new(),
            session_attachments: Vec::new(),
            session_attach_error: None,
            preview: None,
            preview_focus: cx.focus_handle(),
            session_options_open: false,
            waiting_open: false,
            last_escape: None,
            session_details_open: false,
            usage_totals: false,
            usage_pinned: None,
            usage_hovered: None,
            session_menu_open: false,
            delivery: "steer",
            copied: None,
            expanded_items: HashSet::new(),
            plan_open: false,
            form_inputs: HashMap::new(),
            request_collapsed: HashMap::new(),
            pending_open: deep_link_session(),
            app_focus,
            palette,
            palette_open: false,
            option_menu: None,
            profile_query,
            profile_cursor: 0,
            profile_scroll: ScrollHandle::new(),
            option_triggers: Default::default(),
            profile_viewport_h: px(0.),
            profile_menu_bounds: Default::default(),
            dismissed: kit::Dismissed::default(),
            workspace,
            log_view,
            side: Default::default(),
            _subs: subs,
        };
        this.watch_first_run(cx);
        this
    }

    /// Open the deep-linked session (`?session=` / `QMT_SESSION`) once the
    /// store knows about it. Driven by store notifications, not by `render`.
    fn open_pending_session(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.pending_open.as_deref() else {
            return;
        };
        let found = self
            .store
            .read(cx)
            .all_sessions()
            .into_iter()
            .find(|s| s.id == sid)
            .cloned();
        if let Some(summary) = found {
            self.pending_open = None;
            self.open_summary(&summary, cx);
        }
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.session_options_open = false;
        self.session_details_open = false;
        self.session_menu_open = false;
        if page == Page::Automations {
            self.load_automations(true, cx);
        }
        // Mesh and Settings show live server data: refresh it on entry.
        if matches!(page, Page::Mesh | Page::Settings)
            && let Some(agent) = self.store.read(cx).primary_agent()
        {
            self.store.update(cx, |s, cx| s.refresh_extras(&agent, cx));
        }
        cx.notify();
    }

    /// Re-apply theme mode + accent from preferences.
    pub(crate) fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.store.read(cx).prefs.clone();
        let dark = prefs.dark.unwrap_or_else(|| {
            matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            )
        });
        theme::apply(dark, &prefs.accent, cx);
        cx.notify();
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.update_prefs(|p| p.sidebar_collapsed = !p.sidebar_collapsed, cx)
        });
    }

    /// Close the top-most overlay. Returns whether one was open.
    fn close_topmost(&mut self) -> bool {
        if self.preview.take().is_some() || std::mem::take(&mut self.palette_open) {
            return true;
        }
        if self.side.menu.take().is_some() {
            return true;
        }
        if self.close_log_view() {
            return true;
        }
        if std::mem::take(&mut self.options_open) || std::mem::take(&mut self.model_open) {
            return true;
        }
        if std::mem::take(&mut self.session_options_open)
            || std::mem::take(&mut self.waiting_open)
            || std::mem::take(&mut self.session_details_open)
            || std::mem::take(&mut self.session_menu_open)
        {
            return true;
        }
        if std::mem::take(&mut self.confirm_reset)
            || self.confirm_undo.take().is_some()
            || self.close_fork_dialog()
            || self.close_worktree_dialog()
            || self.confirm_delete.take().is_some()
            || self.confirm_session_delete.take().is_some()
            || self.dialog.take().is_some()
            || std::mem::take(&mut self.invite_form)
            || self.details.take().is_some()
            || self.close_workspace_overlay()
        {
            return true;
        }
        // Below every popover and dialog: Escape closes those first.
        self.page == Page::Session && self.close_turn_diff()
    }

    fn start_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.prompt.read(cx).value().trim().to_string();
        let cwd = self.cwd.read(cx).value().trim().to_string();
        let Some(agent) = self.store.read(cx).primary_agent() else {
            return;
        };
        if (prompt.is_empty() && self.attachments.is_empty()) || self.store.read(cx).launch.busy {
            return;
        }
        if cwd.is_empty() {
            self.store.update(cx, |s, cx| {
                s.launch.error =
                    Some("Choose a workspace: an absolute path on the agent's machine.".into());
                cx.notify();
            });
            return;
        }
        let attachments = std::mem::take(&mut self.attachments);
        self.attach_error = None;
        self.store
            .update(cx, |s, cx| s.launch(&agent, cwd, prompt, attachments, cx));
        self.page = Page::Session;
        self.prompt
            .update(cx, |s, cx| s.set_value(SharedString::default(), window, cx));
    }

    // ---- shell ---------------------------------------------------------------

    fn nav_item(
        &self,
        page: Page,
        icon: IconName,
        label: &'static str,
        trailing: Option<AnyElement>,
        p: &Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let active = self.page == page;
        let store = self.store.read(cx);
        // Agents in trouble (desktop `agentsNeedingAttention`): a dot that
        // says which and why.
        let trouble: Option<SharedString> = (page == Page::Agents)
            .then(|| store.agents_needing_attention())
            .filter(|list| !list.is_empty())
            .map(|list| {
                list.into_iter()
                    .map(|(name, why)| format!("{name} {why}"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into()
            });
        let trouble_dot = |t: SharedString| {
            div()
                .id("agents-attention")
                .debug_selector(|| "agents-attention".into())
                .flex_none()
                .size(px(7.))
                .rounded_full()
                .bg(p.warn)
                .tooltip(move |window, cx| kit::wrap_tip(t.clone()).build(window, cx))
        };
        if store.prefs.sidebar_collapsed {
            let badge = match page {
                Page::Inbox => Some((store.pending_request_count(), p.accent)),
                Page::Agents => Some((store.connected_ids().len(), p.success)),
                _ => None,
            }
            .filter(|(count, _)| *count > 0)
            .map(|(count, color)| {
                div()
                    .absolute()
                    .top(px(-3.))
                    .right(px(-3.))
                    .min_w(px(18.))
                    .h(px(18.))
                    .px(px(3.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_2()
                    .border_color(p.bg)
                    .bg(color)
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(p.on_accent)
                    .child(if count > 99 {
                        "99+".to_string()
                    } else {
                        count.to_string()
                    })
            });
            let tip: SharedString = match &trouble {
                Some(t) => format!("{label}\n{t}").into(),
                None => label.into(),
            };
            return kit::rail_button(label, active, p)
                .tooltip(move |window, cx| kit::wrap_tip(tip.clone()).build(window, cx))
                .child(kit::icon(
                    icon,
                    18.,
                    if active { p.accent } else { p.muted },
                ))
                .children(badge)
                .when_some(trouble, |el, t| {
                    el.child(
                        div()
                            .absolute()
                            .bottom(px(1.))
                            .right(px(1.))
                            .p(px(1.5))
                            .rounded_full()
                            .bg(p.bg)
                            .child(trouble_dot(t)),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.go(page, cx)));
        }
        // Flat list row on the shell background: selection is an accent tint
        // (same as `list_active`), hover is the quieter neutral fill.
        let hover_bg = p.card_hover;
        h_flex()
            .id(label)
            .gap_2p5()
            .px_2()
            .h(px(32.))
            .rounded(px(8.))
            .cursor_pointer()
            .text_sm()
            .text_color(if active { p.text } else { p.muted })
            .when(active, |el| el.bg(p.accent_dim).font_medium())
            .when(!active, |el| el.hover(move |el| el.bg(hover_bg)))
            .child(kit::icon(
                icon,
                16.,
                if active { p.accent } else { p.muted },
            ))
            .child(div().flex_1().child(label))
            .children(trouble.map(trouble_dot))
            .children(trailing)
            .on_click(cx.listener(move |this, _, _, cx| this.go(page, cx)))
    }

    fn collapse_button(&self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        if collapsed {
            let p = palette(cx);
            return kit::rail_button("sidebar-toggle", false, p)
                .tooltip(|window, cx| kit::wrap_tip("Expand sidebar (Ctrl+B)").build(window, cx))
                .child(kit::icon(IconName::PanelLeftOpen, 18., p.muted))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                .into_any_element();
        }
        kit::icon_button(
            "sidebar-toggle",
            IconName::PanelLeftClose,
            "Collapse sidebar (Ctrl+B)",
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
        .into_any_element()
    }

    fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *palette(cx);
        // Pinned then open, in their tab order (see `sidebar_shown`).
        let open = self.sidebar_shown(cx).open;
        let waiting = self.store.read(cx).sessions_waiting();
        let current_tab = self
            .store
            .read(cx)
            .session
            .as_ref()
            .map(|v| format!("{}/{}", v.agent_id, v.session_id));
        let separator = || {
            div()
                .flex_none()
                .w(px(28.))
                .py_0p5()
                .child(kit::divider(&p))
        };
        // Flat, transparent rail like the desktop app's collapsed sidebar: no
        // card, no borders; tiles are ghost buttons that only fill on hover.
        div()
            .flex_none()
            .w(px(RAIL_W))
            .h_full()
            .pl_2()
            .pb_2()
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .pt_1()
                    .gap_1p5()
                    .child(self.nav_item(
                        Page::Start,
                        IconName::LayoutDashboard,
                        "Start",
                        None,
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Inbox,
                        IconName::Inbox,
                        "Inbox",
                        inbox_badge(self.store.read(cx).pending_request_count(), &p),
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Sessions,
                        IconName::MessagesSquare,
                        "Sessions",
                        None,
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Workspaces,
                        IconName::FolderOpen,
                        "Workspaces",
                        None,
                        &p,
                        cx,
                    ))
                    .child(separator())
                    .child(self.nav_item(Page::Agents, IconName::Bot, "Agents", None, &p, cx))
                    .child(self.nav_item(Page::Mesh, IconName::Network, "Mesh", None, &p, cx))
                    .child(self.nav_item(
                        Page::Automations,
                        IconName::CalendarClock,
                        "Automations",
                        None,
                        &p,
                        cx,
                    ))
                    .child(separator())
                    .child(
                        v_flex()
                            .id("rail-sessions")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .w_full()
                            .items_center()
                            .gap_1p5()
                            .children(open.into_iter().enumerate().map(|(ix, s)| {
                                let key = crate::store::sidebar::tab_key(&s);
                                let current = self.page == Page::Session
                                    && current_tab.as_deref() == Some(key.as_str());
                                let is_waiting = waiting.contains(&key);
                                let title = format!(
                                    "{}\n{}{}",
                                    s.display_title(),
                                    workspace_name(&s.cwd),
                                    if is_waiting { "\nWaiting for you" } else { "" }
                                );
                                let list =
                                    if s.pinned && self.store.read(cx).prefs.show_pinned_sidebar {
                                        crate::store::sidebar::SideList::Pinned
                                    } else {
                                        crate::store::sidebar::SideList::Open
                                    };
                                let tile = kit::rail_button(
                                    SharedString::from(format!(
                                        "rail-session-{}-{}",
                                        s.agent_id, s.id
                                    )),
                                    current,
                                    &p,
                                )
                                .tooltip(move |window, cx| {
                                    kit::wrap_tip(title.clone()).build(window, cx)
                                })
                                .child(kit::identicon(&s.id, 30.))
                                .when(s.running(), |el| {
                                    el.child(
                                        div()
                                            .absolute()
                                            .top(px(-1.))
                                            .right(px(-1.))
                                            .p_0p5()
                                            .rounded_full()
                                            .bg(p.bg)
                                            .child(kit::spinner(
                                                kit::keyed("rail-spin", s.id.clone()),
                                                10.,
                                                p.success,
                                            )),
                                    )
                                })
                                .when(is_waiting, |el| {
                                    el.child(
                                        div()
                                            .absolute()
                                            .bottom(px(1.))
                                            .right(px(1.))
                                            .p(px(1.5))
                                            .rounded_full()
                                            .bg(p.bg)
                                            .child(sidebar::waiting_dot(&s.id, &p)),
                                    )
                                });
                                let s2 = s.clone();
                                self.tab_row(tile, list, ix, &s, sidebar::TabLook::Tile, 6., &p, cx)
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.open_summary(&s2, cx)
                                        }),
                                    )
                            })),
                    )
                    .child(separator())
                    .child(self.nav_item(
                        Page::Settings,
                        IconName::Settings,
                        "Settings",
                        None,
                        &p,
                        cx,
                    ))
                    .child(self.collapse_button(true, cx)),
            )
            .into_any_element()
    }

    /// Width the sidebar (or the collapsed rail) takes from the window's left
    /// edge, including the hairline after the full sidebar.
    pub(super) fn sidebar_width(&self, cx: &App) -> Pixels {
        if self.store.read(cx).prefs.sidebar_collapsed {
            px(RAIL_W)
        } else {
            px(SIDEBAR_W + 1.)
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        // The right-click menu floats over everything; it lives here so it
        // shows on every page.
        let menu = self.render_side_menu(cx);
        let bar = if self.store.read(cx).prefs.sidebar_collapsed {
            self.render_sidebar_rail(cx)
        } else {
            self.render_sidebar_full(cx)
        };
        h_flex()
            .flex_none()
            .h_full()
            .child(bar)
            .children(menu)
            .into_any_element()
    }

    fn render_sidebar_full(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *palette(cx);
        let show_pinned = self.store.read(cx).prefs.show_pinned_sidebar;
        // Tab order, within the shown limits (see `sidebar_shown`).
        let sidebar::Shown {
            pinned,
            pinned_total,
            open,
            open_total,
        } = self.sidebar_shown(cx);
        let waiting = self.store.read(cx).sessions_waiting();
        let current_tab = (self.page == Page::Session)
            .then(|| self.store.read(cx).session.as_ref())
            .flatten()
            .map(|v| format!("{}/{}", v.agent_id, v.session_id));
        let store = self.store.read(cx);
        let connected = store.connected_ids();
        let total = store.configs.len();
        let peers = connected
            .iter()
            .filter_map(|id| store.runtime(id)?.mesh.as_ref().map(|m| m.peers))
            .max();
        let any_connecting = store.configs.iter().any(|c| {
            store.state(&c.id) == ConnState::Connecting
                || store
                    .runtime(&c.id)
                    .is_some_and(|r| r.is_connected() && !r.sessions_loaded)
        });
        // Older catalog pages still arriving after the first was shown.
        let catalog_loading = store.configs.iter().any(|c| {
            store
                .runtime(&c.id)
                .is_some_and(|r| r.is_connected() && r.catalog_loading)
        });

        // Quiet group labels: small caps in the dim tone, no widget-heading weight.
        let section = |text: &'static str| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_size(px(11.))
                .font_medium()
                .text_color(p.dim)
                .child(text.to_uppercase())
        };
        let hairline = || div().mx_2().child(kit::divider(&p));
        let agents_count = h_flex()
            .text_xs()
            .child(
                div()
                    .font_semibold()
                    .text_color(if connected.is_empty() {
                        p.dim
                    } else {
                        p.success
                    })
                    .child(connected.len().to_string()),
            )
            .child(div().text_color(p.dim).child(format!(" / {total}")))
            .into_any_element();

        // Rows are built first: they borrow `cx` for their listeners.
        let pinned_shown = show_pinned && !pinned.is_empty();
        let pinned_more = pinned_total.saturating_sub(pinned.len());
        let open_empty = open.is_empty();
        let pinned_rows: Vec<AnyElement> = pinned
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let hover_bg = p.card_hover;
                let key = crate::store::sidebar::tab_key(s);
                let current = current_tab.as_deref() == Some(key.as_str());
                let group = SharedString::from(format!("pinned-session-title-{}", s.id));
                let background = if current {
                    p.bg.blend(p.accent_dim)
                } else {
                    p.bg
                };
                let row = h_flex()
                    .id(kit::keyed("pinned-session", s.id.clone()))
                    .group(group.clone())
                    .gap_2()
                    .px_2()
                    .h(px(28.))
                    .rounded(px(8.))
                    .text_color(if current { p.text } else { p.muted })
                    .when(current, |el| el.bg(p.accent_dim))
                    .when(!current, |el| el.hover(move |el| el.bg(hover_bg)))
                    .cursor_pointer()
                    .child(kit::identicon(&s.id, 14.))
                    .child(kit::fading_title(
                        s.display_title(),
                        group,
                        background,
                        if current {
                            background
                        } else {
                            p.bg.blend(hover_bg)
                        },
                    ))
                    .when(waiting.contains(&key), |el| {
                        el.child(sidebar::waiting_dot(&s.id, &p))
                    })
                    .when(s.running(), |el| {
                        el.child(kit::spinner(
                            kit::keyed("pin-spin", s.id.clone()),
                            12.,
                            p.success,
                        ))
                    });
                let s2 = s.clone();
                self.tab_row(
                    row,
                    crate::store::sidebar::SideList::Pinned,
                    ix,
                    s,
                    sidebar::TabLook::Pinned,
                    1.,
                    &p,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_summary(&s2, cx)))
                .into_any_element()
            })
            .collect();
        let open_rows: Vec<AnyElement> = open
            .iter()
            .enumerate()
            .map(|(ix, s)| {
                let hover_bg = p.card_hover;
                let key = crate::store::sidebar::tab_key(s);
                let current = current_tab.as_deref() == Some(key.as_str());
                let s2 = s.clone();
                let group = SharedString::from(format!("open-session-title-{}", s.id));
                let background = if current {
                    p.bg.blend(p.accent_dim)
                } else {
                    p.bg
                };
                let row = v_flex()
                    .id(kit::keyed("open-session", s.id.clone()))
                    .group(group.clone())
                    .cursor_pointer()
                    .px_2()
                    .py_1p5()
                    .gap_0p5()
                    .rounded(px(8.))
                    .when(current, |el| el.bg(p.accent_dim))
                    .when(!current, |el| el.hover(move |el| el.bg(hover_bg)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                kit::fading_title(
                                    s.display_title(),
                                    group,
                                    background,
                                    if current {
                                        background
                                    } else {
                                        p.bg.blend(hover_bg)
                                    },
                                )
                                .text_color(if current { p.text } else { p.muted })
                                .when(current, |el| el.font_medium()),
                            )
                            .when(waiting.contains(&key), |el| {
                                el.child(sidebar::waiting_dot(&s.id, &p))
                            })
                            .when(s.running(), |el| {
                                el.child(kit::spinner(
                                    kit::keyed("sb-spin", s.id.clone()),
                                    13.,
                                    p.success,
                                ))
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(p.dim)
                            .child(kit::identicon(&s.id, 14.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(workspace_name(&s.cwd)),
                            )
                            .child(div().flex_none().child(short_time(&s.updated_at))),
                    );
                self.tab_row(
                    row,
                    crate::store::sidebar::SideList::Open,
                    ix,
                    s,
                    sidebar::TabLook::Open {
                        workspace: workspace_name(&s.cwd).into(),
                        time: short_time(&s.updated_at).into(),
                    },
                    1.,
                    &p,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_summary(&s2, cx)))
                .into_any_element()
            })
            .collect();
        let open_more = open_total > open.len();

        // "Open sessions" doubles as a link to the full list; the count on the
        // right tells how many there are beyond the rows that fit here.
        let open_header = || {
            h_flex()
                .id("sidebar-open-header")
                .cursor_pointer()
                .child(section("Open sessions").flex_1())
                .when(catalog_loading, |el| {
                    el.child(
                        div()
                            .id("sidebar-catalog-loading")
                            .pt_3()
                            .pb_1()
                            .pr_1()
                            .child(kit::spinner("sidebar-catalog-spinner", 11., p.dim))
                            .tooltip(|window, cx| {
                                kit::wrap_tip("Loading older sessions…").build(window, cx)
                            }),
                    )
                })
                .when(open_total > 0, |el| {
                    el.child(
                        div()
                            .pt_3()
                            .pb_1()
                            .pr_2()
                            .text_size(px(11.))
                            .text_color(p.dim)
                            .child(open_total.to_string()),
                    )
                })
                .on_click(cx.listener(|this, _, _, cx| this.go(Page::Sessions, cx)))
        };

        let sessions_list =
            v_flex()
                .id("sidebar-sessions")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.side.scroll)
                // The next page of open sessions loads near the end (desktop rail).
                .on_scroll(cx.listener(|this, _: &ScrollEvent, _, cx| this.sidebar_near_bottom(cx)))
                .gap_px()
                .when(open_empty, |el| {
                    el.child(div().px_2().py_1().text_xs().text_color(p.dim).child(
                        if any_connecting {
                            "Loading sessions…"
                        } else if connected.is_empty() {
                            "Connect an agent to see sessions."
                        } else {
                            "No open sessions."
                        },
                    ))
                })
                .when(pinned_shown, |el| {
                    el.child(section("Pinned"))
                        .children(pinned_rows)
                        .when(pinned_more > 0, |el| {
                            el.child(
                                div()
                                    .id("sidebar-more-pinned")
                                    .px_2()
                                    .py_1()
                                    .rounded(px(8.))
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(p.dim)
                                    .hover(move |s| s.bg(p.card_hover).text_color(p.muted))
                                    .child(format!("Show more pinned ({pinned_more})"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.side.pinned_limit += sidebar::PINNED_PAGE;
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(open_header())
                })
                .children(open_rows)
                .when(open_more, |el| {
                    el.child(h_flex().justify_center().py_2().child(kit::spinner(
                        "sidebar-more-open",
                        12.,
                        p.dim,
                    )))
                });

        // No card: the sidebar is a column of the window chrome, on the same
        // `p.bg` plane as the title bar and the collapsed rail. The 9px gutter
        // puts the 16px nav icons on the title bar's brand-mark axis, and the
        // only edge is one hairline on the right, inset from top and bottom.
        h_flex()
            .flex_none()
            .h_full()
            .items_stretch()
            .child(
                v_flex()
                    .w(px(SIDEBAR_W))
                    .h_full()
                    .pl(px(9.))
                    .pr_2()
                    .pb_2()
                    .gap_0p5()
                    .child(section("Work"))
                    .child(self.nav_item(
                        Page::Start,
                        IconName::LayoutDashboard,
                        "Start",
                        None,
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Inbox,
                        IconName::Inbox,
                        "Inbox",
                        inbox_badge(self.store.read(cx).pending_request_count(), &p),
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Sessions,
                        IconName::MessagesSquare,
                        "Sessions",
                        None,
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Workspaces,
                        IconName::FolderOpen,
                        "Workspaces",
                        None,
                        &p,
                        cx,
                    ))
                    .child(section("Manage"))
                    .child(self.nav_item(
                        Page::Agents,
                        IconName::Bot,
                        "Agents",
                        Some(agents_count),
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Mesh,
                        IconName::Network,
                        "Mesh",
                        peers.map(|n| {
                            div()
                                .text_xs()
                                .text_color(p.dim)
                                .child(n.to_string())
                                .into_any_element()
                        }),
                        &p,
                        cx,
                    ))
                    .child(self.nav_item(
                        Page::Automations,
                        IconName::CalendarClock,
                        "Automations",
                        None,
                        &p,
                        cx,
                    ))
                    .child(div().pt_3().child(hairline()))
                    .when(!pinned_shown, |el| el.child(open_header()))
                    .child(sessions_list)
                    .child(div().pt_1().pb_1().child(hairline()))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                self.nav_item(
                                    Page::Settings,
                                    IconName::Settings,
                                    "Settings",
                                    None,
                                    &p,
                                    cx,
                                )
                                .flex_1(),
                            )
                            .child(self.collapse_button(false, cx)),
                    ),
            )
            .child(div().flex_none().w(px(1.)).my_3().bg(p.border))
            .into_any_element()
    }
}

/// `?session=<id>` on the web, `QMT_SESSION=<id>` natively.
fn deep_link_session() -> Option<String> {
    #[cfg(target_family = "wasm")]
    {
        let search = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix("session="))
            .map(str::to_owned)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        std::env::var("QMT_SESSION").ok()
    }
}

fn inbox_badge(n: usize, p: &Palette) -> Option<AnyElement> {
    (n > 0).then(|| kit::tone_chip(n.to_string(), p.accent).into_any_element())
}

/// Deep link for the first view: `?page=agents&details=1` on the web,
/// `--page agents` / `QMT_PAGE=agents` natively. Handy for testing too.
fn initial_view(store: &Entity<AgentsStore>, cx: &App) -> (Page, Option<String>) {
    #[cfg(target_family = "wasm")]
    let (page, details) = {
        let search = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        let mut page = None;
        let mut details = false;
        for kv in search.trim_start_matches('?').split('&') {
            match kv.split_once('=') {
                Some(("page", v)) => page = Some(v.to_string()),
                Some(("details", _)) => details = true,
                _ => {}
            }
        }
        (page, details)
    };
    #[cfg(not(target_family = "wasm"))]
    let (page, details) = {
        let args: Vec<String> = std::env::args().collect();
        let page = args
            .iter()
            .position(|a| a == "--page")
            .and_then(|i| args.get(i + 1).cloned())
            .or_else(|| std::env::var("QMT_PAGE").ok());
        (page, std::env::var("QMT_DETAILS").is_ok())
    };
    let page = match page.as_deref() {
        Some("agents") => Page::Agents,
        Some("sessions") => Page::Sessions,
        Some("inbox") => Page::Inbox,
        Some("mesh") => Page::Mesh,
        Some("automations") => Page::Automations,
        Some("settings") => Page::Settings,

        _ => Page::Start,
    };
    let details = details
        .then(|| store.read(cx).configs.first().map(|c| c.id.clone()))
        .flatten();
    (page, details)
}

impl Render for QmtApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *palette(cx);
        let render_started = crate::util::now_ms();
        // Sessions that came and went since the last frame take or leave
        // their place in the sidebar.
        self.store.update(cx, |s, _| s.sync_sidebar_order());
        if self.side.drop_target.is_some() && !cx.has_active_drag() {
            self.side.drop_target = None;
        }
        let body = match self.page {
            Page::Start => self.render_start(cx).into_any_element(),
            Page::Sessions => self.render_sessions(cx).into_any_element(),
            Page::Agents => self.render_agents(cx).into_any_element(),
            Page::Mesh => self.render_mesh(cx).into_any_element(),
            Page::Automations => self.render_automations(cx).into_any_element(),
            Page::Settings => self.render_settings(cx).into_any_element(),
            Page::Inbox => self.render_inbox(window, cx).into_any_element(),
            Page::Session => self.render_session(window, cx).into_any_element(),
            Page::Setup => self.render_setup(cx).into_any_element(),
            Page::Workspaces => self.render_workspaces(cx).into_any_element(),
        };
        let setup = self.page == Page::Setup;
        let radii = gpui_kit::component::window_corner_radii(window, cx);
        // Full-window scrims must round themselves: GPUI's overflow mask is
        // rectangular, so the frame's `overflow_hidden` can't clip their corners.
        let overlay = self
            .render_preview(radii, window, cx)
            .or_else(|| self.render_palette(radii, cx))
            .or_else(|| self.render_log_view(radii, window, cx))
            .or_else(|| {
                (self.page == Page::Session)
                    .then(|| self.render_delegate_dialog(radii, cx))
                    .flatten()
            })
            .or_else(|| {
                (self.page == Page::Session)
                    .then(|| self.render_fork_dialog(radii, cx))
                    .flatten()
            })
            .or_else(|| {
                (self.page == Page::Session)
                    .then(|| self.render_worktree_dialog(radii, window, cx))
                    .flatten()
            })
            .or_else(|| self.render_model_modal(radii, cx))
            .or_else(|| self.render_automation_overlay(radii, cx))
            .or_else(|| self.render_workspace_overlay(radii, cx))
            .or_else(|| self.render_overlay(radii, cx));
        let header = self.render_header(window, cx).into_any_element();
        let render_ms = crate::util::now_ms() - render_started;
        if render_ms > 16. {
            tracing::warn!(render.ms = render_ms, "slow app render");
        }

        let root = v_flex()
            .track_focus(&self.app_focus)
            .relative()
            .size_full()
            .rounded_tl(radii.top_left)
            .rounded_tr(radii.top_right)
            .rounded_bl(radii.bottom_left)
            .rounded_br(radii.bottom_right)
            .bg(p.bg)
            .text_color(p.text)
            .font_family("IBM Plex Sans");
        self.on_shortcuts(root, cx)
            // First child: its capture listener must precede every scroll container's.
            .child({
                let prefs = &self.store.read(cx).prefs;
                let or = |v: &str, default: &'static str| {
                    if v.is_empty() {
                        default.to_string()
                    } else {
                        v.to_string()
                    }
                };
                let preset = or(&prefs.scroll_smoothing, smooth_scroll::DEFAULT_PRESET);
                let curve = or(&prefs.scroll_curve, smooth_scroll::DEFAULT_CURVE);
                let inertia = or(&prefs.scroll_inertia, smooth_scroll::DEFAULT_INERTIA);
                self.smooth_scroll.set_preset(&preset, &curve, &inertia);
                self.smooth_scroll.element()
            })
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key != "escape" {
                    return;
                }
                if this.close_delegate_dialog(cx)
                    || this.close_topmost()
                    || this.close_automation_overlay(cx)
                {
                    this.last_escape = None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                // Desktop: a second Escape within 700ms stops the running turn.
                let running = this.page == Page::Session
                    && this
                        .store
                        .read(cx)
                        .session
                        .as_ref()
                        .is_some_and(|v| v.running());
                if !running {
                    this.last_escape = None;
                    return;
                }
                let now = std::time::Instant::now();
                if this
                    .last_escape
                    .is_some_and(|t| now.duration_since(t) <= std::time::Duration::from_millis(700))
                {
                    this.last_escape = None;
                    this.store.update(cx, |s, cx| s.cancel_turn(cx));
                    cx.stop_propagation();
                } else {
                    this.last_escape = Some(now);
                }
            }))
            .child(header)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    // Setup is left only by finishing or skipping it.
                    .when(!setup, |el| {
                        let sidebar = self.render_sidebar(cx);
                        el.child(self.smooth_scroll.region("sidebar", sidebar))
                    })
                    .child(self.smooth_scroll.region(
                        "page",
                        // These pages scroll their own (virtualized) lists.
                        if matches!(self.page, Page::Session | Page::Sessions) {
                            div().id("page").flex_1().min_w_0().h_full().child(body)
                        } else {
                            div()
                                .id("page")
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .overflow_y_scroll()
                                .child(body)
                        },
                    )),
            )
            .children(overlay.map(|el| self.smooth_scroll.region("overlay", el)))
            .into_any_element()
    }
}
