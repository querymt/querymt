//! Root view: QueryMT shell (sidebar + page) and modal overlays.

mod agents;
mod chrome;
mod code;
mod inbox;
mod kit;
mod mesh;
mod preview;
mod session_view;
mod sessions;
mod settings;
mod smooth_scroll;
mod start;

use gpui_kit::assets::IconName;
use std::collections::{HashMap, HashSet};

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::{StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::store::{AgentsStore, Attachment, ConnState, SessionSummary};
use crate::theme::{self, Palette, palette};
use settings::SettingsTab;

gpui_kit::actions!(querymt, [ToggleSidebar]);
use crate::util::{short_time, workspace_name};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Start,
    Inbox,
    Sessions,
    Agents,
    Mesh,
    Settings,
    /// The open session's transcript.
    Session,
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
    session_tab: SessionTab,
    collapsed: HashSet<String>,
    expanded: HashSet<String>,
    dialog: Option<AgentDialog>,
    details: Option<String>,
    confirm_delete: Option<String>,
    d_name: Entity<InputState>,
    d_command: Entity<InputState>,
    d_address: Entity<InputState>,
    header_drag: bool,
    invite_form: bool,
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
    form_inputs: HashMap<String, Entity<InputState>>,
    form_values: HashMap<String, serde_json::Value>,
    form_errors: HashMap<String, String>,
    /// Session id to open once its agent has listed it (deep link).
    pending_open: Option<String>,
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
                .submit_on_enter(true)
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
                .submit_on_enter(true)
                .placeholder("Reply to the agent…")
        });

        let mut subs = vec![
            cx.observe(&store, |this, _, cx| {
                this.open_pending_session(cx);
                cx.notify();
            }),
            // Follow the OS light/dark switch when the theme is set to System.
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.store.read(cx).prefs.dark.is_none() {
                    this.apply_theme(window, cx);
                }
            }),
            cx.subscribe_in(
                &prompt,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { shift: false, .. } => this.start_session(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
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
            cx.subscribe_in(
                &session_prompt,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.send_session_prompt(window, cx)
                    }
                    InputEvent::PressEnter { shift: true, .. } => {}
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
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

        let (page, details) = initial_view(&store, cx);
        Self {
            store,
            page,
            smooth_scroll: smooth_scroll::SmoothScroll::default(),
            cwd,
            prompt,
            model_open: false,
            model_query,
            options_open: false,
            attachments: Vec::new(),
            attach_error: None,
            session_query,
            session_tab: SessionTab::Open,
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            dialog: None,
            details,
            confirm_delete: None,
            d_name,
            d_command,
            d_address,
            header_drag: false,
            invite_form: false,
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
            confirm_reset: false,
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
            form_values: HashMap::new(),
            form_errors: HashMap::new(),
            pending_open: deep_link_session(),
            _subs: subs,
        }
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
        if self.preview.take().is_some() {
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
            || self.confirm_delete.take().is_some()
            || self.dialog.take().is_some()
            || self.details.take().is_some()
        {
            return true;
        }
        false
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
        if store.prefs.sidebar_collapsed {
            let badge = match page {
                Page::Inbox => Some((store.requests.len(), p.accent)),
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
            return kit::rail_button(label, active, p)
                .tooltip(move |window, cx| kit::wrap_tip(label).build(window, cx))
                .child(kit::icon(
                    icon,
                    18.,
                    if active { p.accent } else { p.muted },
                ))
                .children(badge)
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
        let store = self.store.read(cx);
        let mut open: Vec<SessionSummary> = store
            .all_sessions()
            .into_iter()
            .filter(|s| s.root && s.open)
            .cloned()
            .collect();
        open.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        open.truncate(10);
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
            .w(px(56.))
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
                        inbox_badge(self.store.read(cx).requests.len(), &p),
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
                    .child(separator())
                    .child(self.nav_item(Page::Agents, IconName::Bot, "Agents", None, &p, cx))
                    .child(self.nav_item(Page::Mesh, IconName::Network, "Mesh", None, &p, cx))
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
                            .children(open.into_iter().map(|s| {
                                let current = self.page == Page::Session
                                    && store.session.as_ref().is_some_and(|v| {
                                        v.agent_id == s.agent_id && v.session_id == s.id
                                    });
                                let title =
                                    format!("{}\n{}", s.display_title(), workspace_name(&s.cwd));
                                kit::rail_button(
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
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_summary(&s, cx)),
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.store.read(cx).prefs.sidebar_collapsed {
            return self.render_sidebar_rail(cx);
        }
        let p = *palette(cx);
        let show_pinned = self.store.read(cx).prefs.show_pinned_sidebar;
        let store = self.store.read(cx);
        let mut pinned: Vec<SessionSummary> = store
            .all_sessions()
            .into_iter()
            .filter(|s| s.root && s.pinned && !s.archived)
            .cloned()
            .collect();
        pinned.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let connected = store.connected_ids();
        let total = store.configs.len();
        let peers = connected
            .iter()
            .filter_map(|id| store.runtime(id)?.mesh.as_ref().map(|m| m.peers))
            .max();
        let mut open: Vec<SessionSummary> = store
            .all_sessions()
            .into_iter()
            .filter(|s| s.root && s.open && !(show_pinned && s.pinned))
            .cloned()
            .collect();
        open.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let open_total = open.len();
        open.truncate(16);
        let any_connecting = store.configs.iter().any(|c| {
            store.state(&c.id) == ConnState::Connecting
                || store
                    .runtime(&c.id)
                    .is_some_and(|r| r.is_connected() && !r.sessions_loaded)
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

        // "Open sessions" doubles as a link to the full list; the count on the
        // right tells how many there are beyond the rows that fit here.
        let open_header = || {
            h_flex()
                .id("sidebar-open-header")
                .cursor_pointer()
                .child(section("Open sessions").flex_1())
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
                .gap_px()
                .when(open.is_empty(), |el| {
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
                .when(show_pinned && !pinned.is_empty(), |el| {
                    el.child(section("Pinned"))
                        .children(pinned.iter().map(|s| {
                            let hover_bg = p.card_hover;
                            h_flex()
                                .id(kit::keyed("pinned-session", s.id.clone()))
                                .gap_2()
                                .px_2()
                                .h(px(28.))
                                .rounded(px(8.))
                                .text_color(p.muted)
                                .hover(move |el| el.bg(hover_bg))
                                .cursor_pointer()
                                .on_click({
                                    let s = s.clone();
                                    cx.listener(move |this, _, _, cx| this.open_summary(&s, cx))
                                })
                                .child(kit::identicon(&s.id, 14.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .truncate()
                                        .child(s.display_title()),
                                )
                                .when(s.running(), |el| {
                                    el.child(kit::spinner(
                                        kit::keyed("pin-spin", s.id.clone()),
                                        12.,
                                        p.success,
                                    ))
                                })
                        }))
                        .child(open_header())
                })
                .children(open.into_iter().map(|s| {
                    let hover_bg = p.card_hover;
                    let current = self.page == Page::Session
                        && store.session.as_ref().is_some_and(|v| v.session_id == s.id);
                    let s2 = s.clone();
                    v_flex()
                        .id(kit::keyed("open-session", s.id.clone()))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.open_summary(&s2, cx)))
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
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .text_color(if current { p.text } else { p.muted })
                                        .when(current, |el| el.font_medium())
                                        .truncate()
                                        .child(s.display_title()),
                                )
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
                        )
                }));

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
                    .w(px(252.))
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
                        inbox_badge(self.store.read(cx).requests.len(), &p),
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
                    .child(div().pt_3().child(hairline()))
                    .when(!(show_pinned && !pinned.is_empty()), |el| {
                        el.child(open_header())
                    })
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
        let body = match self.page {
            Page::Start => self.render_start(cx).into_any_element(),
            Page::Sessions => self.render_sessions(cx).into_any_element(),
            Page::Agents => self.render_agents(cx).into_any_element(),
            Page::Mesh => self.render_mesh(cx).into_any_element(),
            Page::Settings => self.render_settings(window, cx).into_any_element(),
            Page::Inbox => self.render_inbox(window, cx).into_any_element(),
            Page::Session => self.render_session(window, cx).into_any_element(),
        };
        let radii = gpui_kit::component::window_corner_radii(window, cx);
        // Full-window scrims must round themselves: GPUI's overflow mask is
        // rectangular, so the frame's `overflow_hidden` can't clip their corners.
        let overlay = self
            .render_preview(radii, window, cx)
            .or_else(|| self.render_model_modal(radii, cx))
            .or_else(|| self.render_overlay(radii, cx));
        let header = self.render_header(window, cx).into_any_element();

        v_flex()
            .relative()
            .size_full()
            .rounded_tl(radii.top_left)
            .rounded_tr(radii.top_right)
            .rounded_bl(radii.bottom_left)
            .rounded_br(radii.bottom_right)
            .bg(p.bg)
            .text_color(p.text)
            .font_family("IBM Plex Sans")
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
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key != "escape" {
                    return;
                }
                if this.close_topmost() {
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
                    .child(self.render_sidebar(cx))
                    .child(if self.page == Page::Session {
                        div().id("page").flex_1().min_w_0().h_full().child(body)
                    } else {
                        div()
                            .id("page")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .child(body)
                    }),
            )
            .children(overlay)
            .into_any_element()
    }
}
