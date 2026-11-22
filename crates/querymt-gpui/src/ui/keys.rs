//! Keyboard shortcuts (same set as the desktop app) and the command palette.
//!
//! Bindings have no key context: they reach [`QmtApp`]'s root element, which
//! tracks the app focus handle so they work even when no control is focused.
//! Text inputs keep their own bindings (their context is deeper), so Ctrl/Cmd+Z
//! edits text while typing and undoes the workspace elsewhere.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::IndexPath;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use super::{Page, QmtApp, ToggleSidebar};
use crate::store::{LAUNCH_REASONING, SessionSummary};
use crate::theme::palette;
use crate::util::workspace_name;

gpui_kit::actions!(
    querymt,
    [
        NewSession,
        ToggleCommandPalette,
        UndoWorkspace,
        RedoWorkspace,
        OpenModelPicker,
        CycleReasoning,
    ]
);

/// Open the n-th session of the sidebar (0-based; Ctrl/Cmd+0 is the tenth).
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = querymt, no_json)]
pub struct OpenSidebarSession {
    pub index: usize,
}

const DIGITS_CTRL: [&str; 10] = [
    "ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4", "ctrl-5", "ctrl-6", "ctrl-7", "ctrl-8", "ctrl-9",
    "ctrl-0",
];
const DIGITS_CMD: [&str; 10] = [
    "cmd-1", "cmd-2", "cmd-3", "cmd-4", "cmd-5", "cmd-6", "cmd-7", "cmd-8", "cmd-9", "cmd-0",
];

/// Register the app's key bindings. Ctrl on Linux and Windows, Cmd on macOS.
pub fn bind_keys(cx: &mut App) {
    let mac = cx.platform_info().is_mac();
    let k = |cmd: &'static str, ctrl: &'static str| if mac { cmd } else { ctrl };
    let mut bindings = vec![
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new(k("cmd-n", "ctrl-n"), NewSession, None),
        KeyBinding::new(k("cmd-p", "ctrl-p"), ToggleCommandPalette, None),
        KeyBinding::new(k("cmd-z", "ctrl-z"), UndoWorkspace, None),
        KeyBinding::new(k("cmd-shift-z", "ctrl-shift-z"), RedoWorkspace, None),
        KeyBinding::new(k("cmd-m", "ctrl-m"), OpenModelPicker, None),
        KeyBinding::new(k("cmd-t", "ctrl-t"), CycleReasoning, None),
    ];
    let digits = if mac { DIGITS_CMD } else { DIGITS_CTRL };
    bindings.extend(
        digits
            .into_iter()
            .enumerate()
            .map(|(index, keys)| KeyBinding::new(keys, OpenSidebarSession { index }, None)),
    );
    cx.bind_keys(bindings);
}

/// What a command palette entry does.
#[derive(Clone)]
enum PaletteTarget {
    NewSession,
    SchedulePrompt,
    ScheduleCurrentSession,
    ToggleSidebar,
    RunSetup,
    Page(Page),
    Session(Box<SessionSummary>),
}

const PAGES: [(Page, IconName, &str); 8] = [
    (Page::Start, IconName::LayoutDashboard, "Start"),
    (Page::Inbox, IconName::Inbox, "Inbox"),
    (Page::Sessions, IconName::MessagesSquare, "Sessions"),
    (Page::Workspaces, IconName::FolderOpen, "Workspaces"),
    (Page::Agents, IconName::Bot, "Agents"),
    (Page::Mesh, IconName::Network, "Mesh"),
    (Page::Automations, IconName::CalendarClock, "Automations"),
    (Page::Settings, IconName::Settings, "Settings"),
];

impl QmtApp {
    /// Attach the shortcut handlers to the root element.
    pub(super) fn on_shortcuts(&self, root: Div, cx: &mut Context<Self>) -> Div {
        // Setup is left only by finishing or skipping it: app shortcuts wait.
        root.on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
            if this.page != Page::Setup {
                this.toggle_sidebar(cx)
            }
        }))
        .on_action(cx.listener(|this, _: &NewSession, window, cx| {
            if this.page != Page::Setup {
                this.new_session(window, cx)
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleCommandPalette, window, cx| {
            if this.page != Page::Setup {
                this.toggle_palette(window, cx)
            }
        }))
        .on_action(cx.listener(|this, a: &OpenSidebarSession, window, cx| {
            if this.page != Page::Setup {
                this.open_sidebar_session(a.index, window, cx)
            }
        }))
        .on_action(cx.listener(|this, _: &UndoWorkspace, window, cx| {
            this.workspace_history(true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &RedoWorkspace, window, cx| {
            this.workspace_history(false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &OpenModelPicker, window, cx| {
            if matches!(this.page, Page::Start | Page::Session) && this.picker_agent(cx).is_some() {
                this.open_model_picker(window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &CycleReasoning, _, cx| this.cycle_reasoning(cx)))
    }

    /// Ctrl/Cmd+T: next reasoning effort, for the open session or new sessions.
    fn cycle_reasoning(&mut self, cx: &mut Context<Self>) {
        match self.page {
            Page::Start => {
                let current = self.store.read(cx).prefs.reasoning().to_string();
                let n = LAUNCH_REASONING.len();
                let at = LAUNCH_REASONING
                    .iter()
                    .position(|(id, ..)| *id == current)
                    .unwrap_or(0);
                let next = LAUNCH_REASONING[(at + 1) % n].0;
                self.store
                    .update(cx, |s, cx| s.set_launch_reasoning(next, cx));
            }
            Page::Session => {
                let Some(option) = self.store.read(cx).session.as_ref().and_then(|v| {
                    v.reasoning_option()
                        .filter(|o| !o.choices.is_empty() && !v.pending_options.contains(&o.id))
                }) else {
                    return;
                };
                let at = option
                    .choices
                    .iter()
                    .position(|(v, _)| *v == option.current)
                    .unwrap_or(0);
                let value = option.choices[(at + 1) % option.choices.len()].0.clone();
                self.store
                    .update(cx, |s, cx| s.set_session_option(&option.id, &value, cx));
            }
            _ => {}
        }
    }

    /// Start page with the prompt focused.
    pub(super) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.go(Page::Start, cx);
        self.prompt.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Sessions as the sidebar shows them, top to bottom (Alt+1…9, palette).
    fn sidebar_sessions(&self, cx: &App) -> Vec<SessionSummary> {
        let shown = self.sidebar_shown(cx);
        shown.pinned.into_iter().chain(shown.open).collect()
    }

    fn open_sidebar_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.sidebar_sessions(cx).into_iter().nth(index) else {
            return;
        };
        self.palette_open = false;
        self.open_summary(&s, cx);
        self.app_focus.focus(window, cx);
    }

    /// Ctrl/Cmd+Z and Ctrl/Cmd+Shift+Z in the session view. Inside a text
    /// field the shortcut belongs to the field.
    fn workspace_history(&mut self, undo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Session || window.context_stack().iter().any(|c| c.contains("Input"))
        {
            cx.propagate();
            return;
        }
        if undo {
            let target = self
                .store
                .read(cx)
                .session
                .as_ref()
                .and_then(|v| v.undo_target());
            if let Some(message_id) = target {
                self.request_undo(message_id, cx);
            }
        } else {
            self.store.update(cx, |s, cx| s.redo_workspace(cx));
        }
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.palette_open) {
            self.app_focus.focus(window, cx);
        } else {
            self.palette_open = true;
            self.model_open = false;
            self.options_open = false;
            self.palette.update(cx, |s, cx| {
                s.set_query("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn run_palette(&mut self, target: PaletteTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        match target {
            PaletteTarget::NewSession => return self.new_session(window, cx),
            PaletteTarget::SchedulePrompt | PaletteTarget::ScheduleCurrentSession => {
                self.open_automation_dialog(
                    matches!(target, PaletteTarget::ScheduleCurrentSession),
                    window,
                    cx,
                );
                cx.notify();
                return;
            }
            PaletteTarget::ToggleSidebar => self.toggle_sidebar(cx),
            PaletteTarget::RunSetup => return self.open_setup(None, cx),
            PaletteTarget::Page(page) => self.go(page, cx),
            PaletteTarget::Session(s) => self.open_summary(&s, cx),
        }
        self.app_focus.focus(window, cx);
        cx.notify();
    }

    /// The command palette overlay, when open.
    pub(super) fn render_palette(
        &mut self,
        radii: Corners<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.palette_open {
            return None;
        }
        let p = *palette(cx);
        let card = div()
            .id("palette-card")
            .w(px(560.))
            .p_1()
            .rounded(px(16.))
            .bg(p.surface)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(self.palette_view(cx));
        Some(
            div()
                .id("scrim-palette")
                .absolute()
                .inset_0()
                .rounded_tl(radii.top_left)
                .rounded_tr(radii.top_right)
                .rounded_bl(radii.bottom_left)
                .rounded_br(radii.bottom_right)
                .bg(black().opacity(if p.dark { 0.55 } else { 0.28 }))
                .occlude()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(96.))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.palette_open = false;
                    this.app_focus.focus(window, cx);
                    cx.notify();
                }))
                .child(card)
                .into_any_element(),
        )
    }

    /// The command palette (Ctrl/Cmd+P): actions, pages and sidebar sessions.
    fn palette_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let sessions = self.sidebar_sessions(cx);
        let mut action_targets = vec![PaletteTarget::NewSession, PaletteTarget::ToggleSidebar];
        let mut actions = CommandGroup::new()
            .label("Actions")
            .item(CommandItem::new().label("New session").icon(IconName::Plus))
            .item(
                CommandItem::new()
                    .label("Toggle sidebar")
                    .icon(IconName::PanelLeftClose),
            );
        for (current, target, label) in [
            (false, PaletteTarget::SchedulePrompt, "Schedule a prompt…"),
            (
                true,
                PaletteTarget::ScheduleCurrentSession,
                "Schedule current session…",
            ),
        ] {
            if self.has_automation_target(current, cx) {
                action_targets.push(target);
                actions = actions.item(
                    CommandItem::new()
                        .label(label)
                        .icon(IconName::CalendarClock)
                        .keywords(["automation"]),
                );
            }
        }
        action_targets.push(PaletteTarget::RunSetup);
        actions = actions.item(
            CommandItem::new()
                .label("Run setup")
                .icon(IconName::Wand)
                .keywords(["onboarding", "first run", "providers"]),
        );
        let targets: Rc<Vec<Vec<PaletteTarget>>> = Rc::new(vec![
            action_targets,
            PAGES
                .iter()
                .map(|(page, ..)| PaletteTarget::Page(*page))
                .collect(),
            sessions
                .iter()
                .map(|s| PaletteTarget::Session(Box::new(s.clone())))
                .collect(),
        ]);
        let pages = CommandGroup::new().label("Go to").items(
            PAGES
                .iter()
                .map(|(_, icon, label)| CommandItem::new().label(*label).icon(*icon)),
        );
        let open = CommandGroup::new()
            .label("Sessions")
            .items(sessions.iter().map(|s| {
                CommandItem::new()
                    .label(s.display_title())
                    .icon(IconName::MessageSquare)
                    .keywords([workspace_name(&s.cwd)])
            }));
        let this = cx.entity().downgrade();
        let cancel = this.clone();
        Command::new(&self.palette)
            .placeholder("Search actions, pages and sessions…")
            .max_h(px(380.))
            .bordered(false)
            .group(actions)
            .group(pages)
            .when(!sessions.is_empty(), |c| c.group(open))
            .on_confirm(move |ix: IndexPath, window, cx| {
                let Some(target) = targets.get(ix.section).and_then(|g| g.get(ix.row)).cloned()
                else {
                    return;
                };
                let _ = this.update(cx, |this, cx| this.run_palette(target, window, cx));
            })
            .on_cancel(move |window, cx| {
                let _ = cancel.update(cx, |this, cx| {
                    this.palette_open = false;
                    this.app_focus.focus(window, cx);
                    cx.notify();
                });
            })
            .into_any_element()
    }
}
