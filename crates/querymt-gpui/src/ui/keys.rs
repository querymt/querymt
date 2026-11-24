//! Keyboard shortcuts (same set as the desktop app) and the command palette.
//!
//! Bindings have no key context: they reach [`QmtApp`]'s root element, which
//! tracks the app focus handle so they work even when no control is focused.
//! Text inputs keep their own bindings (their context is deeper), so Ctrl/Cmd+Z
//! edits text while typing and undoes the workspace elsewhere.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem};
use gpui_kit::component::input::Position;
use gpui_kit::component::{Icon, IndexPath, h_flex, v_flex};
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
    /// A slash command of the open session (`name` has no leading `/`).
    SlashCommand {
        name: String,
        takes_input: bool,
    },
}

/// One palette row: what it shows, what the search matches besides the
/// label, and what picking it does. Built together, so an entry's target can
/// never drift from its row (design D5).
struct PaletteEntry {
    label: SharedString,
    icon: IconName,
    /// A dimmed second line under the label (a slash command's help).
    description: Option<SharedString>,
    keywords: Vec<String>,
    target: PaletteTarget,
}

impl PaletteEntry {
    fn new(label: impl Into<SharedString>, icon: IconName, target: PaletteTarget) -> Self {
        Self {
            label: label.into(),
            icon,
            description: None,
            keywords: Vec::new(),
            target,
        }
    }

    /// Show `description` under the label; a blank one shows nothing.
    fn description(mut self, description: impl Into<SharedString>) -> Self {
        let description = description.into();
        self.description = (!description.trim().is_empty()).then_some(description);
        self
    }

    fn keywords<S: Into<String>>(mut self, keywords: impl IntoIterator<Item = S>) -> Self {
        self.keywords.extend(keywords.into_iter().map(Into::into));
        self
    }
}

struct PaletteGroup {
    label: &'static str,
    entries: Vec<PaletteEntry>,
}

/// The groups' entries that match `query`, best first within each group
/// (the app's one matcher, `search::rank_tiered`: every word of the query
/// must match the label or, counting for half, a keyword). Empty groups are
/// dropped, and the groups are ordered by their best entry, so the first row
/// is the best match of the whole palette (Enter runs it); groups that tie
/// keep their order. An empty query keeps everything as it is.
fn filter_palette(groups: Vec<PaletteGroup>, query: &str) -> Vec<PaletteGroup> {
    if query.trim().is_empty() {
        return groups;
    }
    let mut found: Vec<(u32, PaletteGroup)> = groups
        .into_iter()
        .filter_map(|group| {
            let ranked = crate::search::rank_tiered(
                query,
                &group.entries,
                |entry| vec![entry.label.to_string()],
                |entry| entry.keywords.clone(),
            );
            // Best first: the head of the ranking is the group's score.
            let best = ranked.first()?.1;
            let mut slots: Vec<Option<PaletteEntry>> =
                group.entries.into_iter().map(Some).collect();
            let entries: Vec<PaletteEntry> = ranked
                .into_iter()
                .filter_map(|(ix, _)| slots[ix].take())
                .collect();
            Some((
                best,
                PaletteGroup {
                    label: group.label,
                    entries,
                },
            ))
        })
        .collect();
    // Stable: equal scores keep the groups' built order.
    found.sort_by_key(|(best, _)| std::cmp::Reverse(*best));
    found.into_iter().map(|(_, group)| group).collect()
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
        self.restore_focus(window, cx);
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
            self.restore_focus(window, cx);
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
            PaletteTarget::SlashCommand { name, takes_input } => {
                return self.run_slash_command(&name, takes_input, window, cx);
            }
        }
        self.restore_focus(window, cx);
        cx.notify();
    }

    /// Picking a slash command in the palette. Without an input it is sent
    /// as a prompt (`/name`), under the same rules as a typed one: nothing is
    /// sent while the session is not ready, and while a turn runs
    /// `send_prompt` queues it. With an input the composer gets `/name `, the
    /// cursor at the end and the focus, so the arguments can be typed.
    pub(super) fn run_slash_command(
        &mut self,
        name: &str,
        takes_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Names are bare (the core trims the slash); a prefix would send `//`.
        let name = name.trim_start_matches('/');
        if takes_input {
            let text = format!("/{name} ");
            let end = Position::new(0, text.chars().count() as u32);
            self.session_prompt.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                // Moves the caret and focuses the composer.
                state.set_cursor_position(end, window, cx);
            });
            cx.notify();
            return;
        }
        if !self.session_ready(cx) {
            return;
        }
        let delivery = self.delivery;
        self.store.update(cx, |s, cx| {
            s.send_prompt(format!("/{name}"), Vec::new(), delivery, cx)
        });
        self.transcript.list.set_follow_mode(FollowMode::Tail);
        self.transcript.list.scroll_to_end();
        self.restore_focus(window, cx);
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
                    this.restore_focus(window, cx);
                    cx.notify();
                }))
                .child(card)
                .into_any_element(),
        )
    }

    /// The command palette (Ctrl/Cmd+P): actions, pages and sidebar sessions.
    fn palette_view(&self, cx: &mut Context<Self>) -> AnyElement {
        // The kit does not filter (`filterable(false)`): the groups are
        // built for the current query, and `on_confirm`'s index path counts
        // into exactly these groups, so targets cannot drift from rows.
        let query = self.palette.read(cx).query(cx).to_string();
        let groups = filter_palette(self.palette_groups(cx), &query);
        let targets: Rc<Vec<Vec<PaletteTarget>>> = Rc::new(
            groups
                .iter()
                .map(|g| g.entries.iter().map(|e| e.target.clone()).collect())
                .collect(),
        );
        let p = *palette(cx);
        let this = cx.entity().downgrade();
        let cancel = this.clone();
        let requery = this.clone();
        let mut command = Command::new(&self.palette)
            .placeholder("Search actions, commands, pages and sessions…")
            .max_h(px(380.))
            .bordered(false)
            .filterable(false)
            // A new query rebuilds the groups on the next render.
            .on_query(move |_, _, cx| {
                let _ = requery.update(cx, |_, cx| cx.notify());
            });
        for group in groups {
            command = command.group(CommandGroup::new().label(group.label).items(
                group.entries.into_iter().map(move |e| {
                    let item = CommandItem::new()
                        .label(e.label.clone())
                        .icon(e.icon)
                        .keywords(e.keywords);
                    // The kit draws icon and label only; a description needs
                    // a row of our own (the label and keywords still search).
                    match e.description {
                        Some(description) => {
                            let (label, icon) = (e.label, e.icon);
                            item.child(move |_, _| {
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(icon).size_4().text_color(p.muted))
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(p.text)
                                                    .child(label.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(p.muted)
                                                    .whitespace_nowrap()
                                                    .truncate()
                                                    .child(description.clone()),
                                            ),
                                    )
                            })
                        }
                        None => item,
                    }
                }),
            ));
        }
        command
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
                    this.restore_focus(window, cx);
                    cx.notify();
                });
            })
            .into_any_element()
    }

    /// Every palette group for the current state, unfiltered: Actions, the
    /// open session's slash commands (only on the session page, and only when
    /// it advertises some), Go to, and the sidebar's sessions.
    fn palette_groups(&self, cx: &App) -> Vec<PaletteGroup> {
        let mut actions = vec![
            PaletteEntry::new("New session", IconName::Plus, PaletteTarget::NewSession),
            PaletteEntry::new(
                "Toggle sidebar",
                IconName::PanelLeftClose,
                PaletteTarget::ToggleSidebar,
            ),
        ];
        for (current, target, label) in [
            (false, PaletteTarget::SchedulePrompt, "Schedule a prompt…"),
            (
                true,
                PaletteTarget::ScheduleCurrentSession,
                "Schedule current session…",
            ),
        ] {
            if self.has_automation_target(current, cx) {
                actions.push(
                    PaletteEntry::new(label, IconName::CalendarClock, target)
                        .keywords(["automation"]),
                );
            }
        }
        actions.push(
            PaletteEntry::new("Run setup", IconName::Wand, PaletteTarget::RunSetup).keywords([
                "onboarding",
                "first run",
                "providers",
            ]),
        );
        let mut groups = vec![PaletteGroup {
            label: "Actions",
            entries: actions,
        }];

        let commands = self.store.read(cx).session_slash_commands();
        if self.page == Page::Session && !commands.is_empty() {
            groups.push(PaletteGroup {
                label: "Slash commands",
                entries: commands
                    .iter()
                    .map(|c| {
                        // The bare name: the palette lists commands, it is
                        // not the composer. The description is the agent's
                        // own help text for the command.
                        PaletteEntry::new(
                            c.name.clone(),
                            IconName::SquareSlash,
                            PaletteTarget::SlashCommand {
                                name: c.name.clone(),
                                takes_input: c.input_hint.is_some(),
                            },
                        )
                        .description(c.description.clone())
                        .keywords(
                            std::iter::once(c.description.clone())
                                .chain(c.input_hint.clone().filter(|h| !h.is_empty())),
                        )
                    })
                    .collect(),
            });
        }

        groups.push(PaletteGroup {
            label: "Go to",
            entries: PAGES
                .iter()
                .map(|(page, icon, label)| {
                    PaletteEntry::new(*label, *icon, PaletteTarget::Page(*page))
                })
                .collect(),
        });
        let sessions = self.sidebar_sessions(cx);
        if !sessions.is_empty() {
            groups.push(PaletteGroup {
                label: "Sessions",
                entries: sessions
                    .iter()
                    .map(|s| {
                        PaletteEntry::new(
                            s.display_title(),
                            IconName::MessageSquare,
                            PaletteTarget::Session(Box::new(s.clone())),
                        )
                        .keywords([workspace_name(&s.cwd)])
                    })
                    .collect(),
            });
        }
        groups
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the gpui glob brings a `test` macro that shadows `#[test]`.
    use super::{PaletteEntry, PaletteGroup, PaletteTarget, filter_palette};
    use crate::session::SessionView;
    use crate::store::AgentsStore;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::assets::IconName;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use serde_json::{Value, json};

    const COMPACT: &str = "Summarize the conversation now to free up context";

    struct Rig {
        handle: AnyWindowHandle,
        app: Entity<QmtApp>,
        requests: async_channel::Receiver<String>,
    }

    /// A session view of "test-agent" that advertises `commands`.
    fn session_with(commands: Value) -> SessionView {
        let mut view = SessionView::new("test-agent", "s1", "/w", "t");
        view.session_update(&json!({ "sessionId": "s1", "update": {
            "sessionUpdate": "available_commands_update", "availableCommands": commands } }));
        view
    }

    /// `/compact` takes input, `/worktree` does not.
    fn two_commands() -> Value {
        json!([
            { "name": "compact", "description": COMPACT,
              "input": { "hint": "[optional instructions]" } },
            { "name": "worktree", "description": "Work in a git worktree" },
        ])
    }

    fn rig(
        cx: &mut TestAppContext,
        page: Page,
        view: Option<SessionView>,
        setup: impl FnOnce(&mut AgentsStore),
    ) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::launch::peer_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        store.session = view;
        setup(&mut store);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|_| store);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| QmtApp::new(store, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| app.go(page, cx));
            window.render_frame(cx);
        })
        .unwrap();
        Rig {
            handle,
            app,
            requests,
        }
    }

    fn groups(cx: &mut TestAppContext, rig: &Rig) -> Vec<PaletteGroup> {
        rig.app.read_with(cx, |app, cx| app.palette_groups(cx))
    }

    fn group_labels(groups: &[PaletteGroup]) -> Vec<&'static str> {
        groups.iter().map(|g| g.label).collect()
    }

    fn entry_labels(groups: &[PaletteGroup], group: &str) -> Vec<String> {
        groups
            .iter()
            .find(|g| g.label == group)
            .map(|g| g.entries.iter().map(|e| e.label.to_string()).collect())
            .unwrap_or_default()
    }

    /// The target of the entry labelled `label`.
    fn target_of(groups: &[PaletteGroup], label: &str) -> PaletteTarget {
        groups
            .iter()
            .flat_map(|g| &g.entries)
            .find(|e| e.label.as_ref() == label)
            .unwrap_or_else(|| panic!("no entry {label}"))
            .target
            .clone()
    }

    fn pick(cx: &mut TestAppContext, rig: &Rig, target: PaletteTarget) {
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette_open = true;
                app.run_palette(target, window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
        // A request goes out when its future is first polled.
        cx.run_until_parked();
    }

    /// The JSON requests sent to the agent so far.
    fn sent(rig: &Rig) -> Vec<Value> {
        std::iter::from_fn(|| rig.requests.try_recv().ok())
            .map(|line| serde_json::from_str(&line).expect("a JSON-RPC line"))
            .collect()
    }

    /// Text of the `session/prompt` requests in `requests`.
    fn prompts(requests: &[Value]) -> Vec<String> {
        requests
            .iter()
            .filter(|r| r["method"] == "session/prompt")
            .map(|r| {
                r["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[gpui_kit::test]
    fn palette_lists_session_commands(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let all = groups(cx, &rig);
        // After Actions, before Go to.
        let labels = group_labels(&all);
        let at = labels.iter().position(|l| *l == "Slash commands").unwrap();
        assert_eq!(labels[at - 1], "Actions");
        assert_eq!(labels[at + 1], "Go to");
        assert_eq!(
            entry_labels(&all, "Slash commands"),
            ["compact", "worktree"]
        );
        // The row shows the agent's help under the name.
        let descriptions: Vec<Option<&str>> = all
            .iter()
            .find(|g| g.label == "Slash commands")
            .unwrap()
            .entries
            .iter()
            .map(|e| e.description.as_deref())
            .collect();
        assert_eq!(descriptions[0], Some(COMPACT));
        assert!(descriptions[1].is_some(), "every command shows its help");
        assert!(matches!(
            target_of(&all, "compact"),
            PaletteTarget::SlashCommand { ref name, takes_input: true } if name == "compact"
        ));
        assert!(matches!(
            target_of(&all, "worktree"),
            PaletteTarget::SlashCommand { ref name, takes_input: false } if name == "worktree"
        ));
        // The search matches the description: `free context` finds /compact
        // (words in order or not), and the other command drops out.
        for query in ["free context", "context free", "instructions"] {
            let found = filter_palette(groups(cx, &rig), query);
            assert_eq!(group_labels(&found), ["Slash commands"], "{query}");
            assert_eq!(
                entry_labels(&found, "Slash commands"),
                ["compact"],
                "{query}"
            );
        }
        // The name matches too and ranks first. (Typo tolerance lets the long
        // description of /compact match `worktree` loosely, below it; the
        // composer list, which matches names only, does not have this.)
        let found = filter_palette(groups(cx, &rig), "worktree");
        assert_eq!(entry_labels(&found, "Slash commands")[0], "worktree");
        // Other groups still filter by label.
        let found = filter_palette(groups(cx, &rig), "sidebar");
        assert_eq!(entry_labels(&found, "Actions"), ["Toggle sidebar"]);
        assert!(entry_labels(&found, "Slash commands").is_empty());
    }

    /// Regression: with the groups in their built order, `comp` listed the
    /// action `Schedule a prompt…` (its letters are scattered over the label)
    /// above the command `compact`, and Enter ran the weaker match.
    #[gpui_kit::test]
    fn groups_are_ordered_by_their_best_match(_cx: &mut TestAppContext) {
        let actions = PaletteGroup {
            label: "Actions",
            entries: vec![
                PaletteEntry::new("New session", IconName::Plus, PaletteTarget::NewSession),
                PaletteEntry::new(
                    "Schedule a prompt…",
                    IconName::CalendarClock,
                    PaletteTarget::SchedulePrompt,
                )
                .keywords(["automation"]),
            ],
        };
        let slash = PaletteGroup {
            label: "Slash commands",
            entries: vec![
                PaletteEntry::new(
                    "compact",
                    IconName::SquareSlash,
                    PaletteTarget::SlashCommand {
                        name: "compact".into(),
                        takes_input: true,
                    },
                )
                .description(COMPACT)
                .keywords([COMPACT]),
            ],
        };
        let found = filter_palette(vec![actions, slash], "comp");
        // Both still match: the action stays listed, below the command.
        assert_eq!(group_labels(&found), ["Slash commands", "Actions"]);
        assert_eq!(entry_labels(&found, "Actions"), ["Schedule a prompt…"]);
        assert!(matches!(
            found[0].entries[0].target,
            PaletteTarget::SlashCommand { ref name, .. } if name == "compact"
        ));
        // A query only the action matches puts it first again, and the
        // built order is the tie-breaker.
        let actions = PaletteGroup {
            label: "Actions",
            entries: vec![PaletteEntry::new(
                "Toggle sidebar",
                IconName::PanelLeftClose,
                PaletteTarget::ToggleSidebar,
            )],
        };
        let other = PaletteGroup {
            label: "Go to",
            entries: vec![PaletteEntry::new(
                "Toggle sidebar",
                IconName::LayoutDashboard,
                PaletteTarget::Page(Page::Start),
            )],
        };
        let found = filter_palette(vec![actions, other], "toggle");
        assert_eq!(
            group_labels(&found),
            ["Actions", "Go to"],
            "ties keep order"
        );
        // An empty query keeps the built order untouched.
        let slash_last = vec![
            PaletteGroup {
                label: "Actions",
                entries: vec![PaletteEntry::new(
                    "New session",
                    IconName::Plus,
                    PaletteTarget::NewSession,
                )],
            },
            PaletteGroup {
                label: "Slash commands",
                entries: vec![PaletteEntry::new(
                    "compact",
                    IconName::SquareSlash,
                    PaletteTarget::NewSession,
                )],
            },
        ];
        assert_eq!(
            group_labels(&filter_palette(slash_last, "")),
            ["Actions", "Slash commands"]
        );
    }

    /// The first row of the filtered palette is what Enter runs.
    #[gpui_kit::test]
    fn the_best_match_is_the_first_row(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let found = filter_palette(groups(cx, &rig), "comp");
        assert_eq!(group_labels(&found)[0], "Slash commands");
        assert!(matches!(
            found[0].entries[0].target,
            PaletteTarget::SlashCommand { ref name, .. } if name == "compact"
        ));
        // `work` is the other command's name; its group leads too.
        let found = filter_palette(groups(cx, &rig), "worktree");
        assert!(matches!(
            found[0].entries[0].target,
            PaletteTarget::SlashCommand { ref name, .. } if name == "worktree"
        ));
    }

    #[gpui_kit::test]
    fn no_slash_group_without_a_session(cx: &mut TestAppContext) {
        // The Start page: no session, no group.
        let rig_start = rig(cx, Page::Start, None, |_| {});
        assert!(!group_labels(&groups(cx, &rig_start)).contains(&"Slash commands"));
        // An open session that advertises nothing.
        let rig_empty = rig(cx, Page::Session, Some(session_with(json!([]))), |_| {});
        assert!(!group_labels(&groups(cx, &rig_empty)).contains(&"Slash commands"));
        // A session that is open but not on screen: the commands belong to
        // the session page.
        let rig_away = rig(cx, Page::Start, Some(session_with(two_commands())), |_| {});
        assert!(!group_labels(&groups(cx, &rig_away)).contains(&"Slash commands"));
    }

    #[gpui_kit::test]
    fn palette_targets_stay_aligned_without_sessions(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let all = groups(cx, &rig);
        // The Sessions group is absent, and the Slash group sits between two
        // others: every row must still carry its own target.
        assert!(!group_labels(&all).contains(&"Sessions"));
        for group in &all {
            for entry in &group.entries {
                match (&entry.target, group.label) {
                    (PaletteTarget::SlashCommand { name, .. }, "Slash commands") => {
                        assert_eq!(entry.label.as_ref(), name.as_str());
                    }
                    (PaletteTarget::Page(page), "Go to") => {
                        let label = super::PAGES.iter().find(|(p, ..)| p == page).unwrap().2;
                        assert_eq!(entry.label.as_ref(), label);
                    }
                    (
                        PaletteTarget::NewSession
                        | PaletteTarget::ToggleSidebar
                        | PaletteTarget::SchedulePrompt
                        | PaletteTarget::ScheduleCurrentSession
                        | PaletteTarget::RunSetup,
                        "Actions",
                    ) => {}
                    _ => panic!(
                        "{} carries the wrong target in {}",
                        entry.label, group.label
                    ),
                }
            }
        }
        // As `on_confirm` sees it: an index path counts into the filtered
        // groups. Filtering down to one command makes (0, 0) that command.
        let found = filter_palette(all, "worktree");
        assert_eq!(group_labels(&found), ["Slash commands"]);
        let targets: Vec<Vec<PaletteTarget>> = found
            .iter()
            .map(|g| g.entries.iter().map(|e| e.target.clone()).collect())
            .collect();
        assert!(matches!(
            targets[0][0],
            PaletteTarget::SlashCommand { ref name, .. } if name == "worktree"
        ));
        // Picking it runs the command rather than another target.
        pick(cx, &rig, targets[0][0].clone());
        assert_eq!(prompts(&sent(&rig)), ["/worktree"]);
    }

    #[gpui_kit::test]
    fn palette_sends_a_command_without_input(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let target = target_of(&groups(cx, &rig), "worktree");
        pick(cx, &rig, target);
        assert_eq!(prompts(&sent(&rig)), ["/worktree"]);
        rig.app.read_with(cx, |app, _| {
            assert!(!app.palette_open, "the palette closes")
        });
        // The composer was not touched.
        rig.app.read_with(cx, |app, cx| {
            assert_eq!(app.session_prompt.read(cx).value().as_ref(), "");
        });
    }

    #[gpui_kit::test]
    fn typing_in_the_palette_filters_the_rendered_rows(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let total: usize = groups(cx, &rig).iter().map(|g| g.entries.len()).sum();
        let matched = |cx: &mut TestAppContext| {
            rig.app
                .read_with(cx, |app, cx| app.palette.read(cx).matched_count())
        };
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| app.toggle_palette(window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(matched(cx), total, "an empty query lists everything");
        // As typed: the kit reports the query, the app rebuilds its groups.
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette
                    .update(cx, |s, cx| s.set_query("free context", window, cx))
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        assert_eq!(matched(cx), 1, "only /compact matches `free context`");
        // Picking the shown row runs the filtered entry, not an unfiltered one.
        let query = rig
            .app
            .read_with(cx, |app, cx| app.palette.read(cx).query(cx).to_string());
        let shown = filter_palette(groups(cx, &rig), &query);
        assert!(matches!(
            shown[0].entries[0].target,
            PaletteTarget::SlashCommand { ref name, .. } if name == "compact"
        ));
    }

    #[gpui_kit::test]
    fn palette_does_not_send_while_the_session_is_not_ready(cx: &mut TestAppContext) {
        let mut view = session_with(two_commands());
        view.loading = true;
        let rig = rig(cx, Page::Session, Some(view), |_| {});
        let target = target_of(&groups(cx, &rig), "worktree");
        pick(cx, &rig, target);
        assert!(
            sent(&rig).is_empty(),
            "same readiness rule as a typed prompt"
        );
    }

    #[gpui_kit::test]
    fn palette_fills_the_composer_for_a_command_with_input(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let target = target_of(&groups(cx, &rig), "compact");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        cx.update_window(rig.handle, |_, window, cx| {
            let entity = rig.app.read(cx).session_prompt.clone();
            let state = entity.read(cx);
            assert_eq!(state.value().as_ref(), "/compact ");
            let cursor = state.cursor_position();
            assert_eq!((cursor.line, cursor.character), (0, 9), "cursor at the end");
            let focus = gpui_kit::Focusable::focus_handle(&entity, cx);
            assert!(focus.is_focused(window), "the composer has focus");
        })
        .unwrap();
    }

    fn prompt_focused(cx: &mut TestAppContext, rig: &Rig) -> bool {
        cx.update_window(rig.handle, |_, window, cx| {
            let app = rig.app.read(cx);
            let prompt = if app.page == Page::Session {
                &app.session_prompt
            } else {
                &app.prompt
            };
            gpui_kit::Focusable::focus_handle(prompt, cx).is_focused(window)
        })
        .unwrap()
    }

    fn open_palette(cx: &mut TestAppContext, rig: &Rig) {
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| app.toggle_palette(window, cx));
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn escape_closes_the_palette_and_focuses_the_prompt(cx: &mut TestAppContext) {
        for page in [Page::Start, Page::Session] {
            let rig = rig(cx, page, Some(session_with(two_commands())), |_| {});
            open_palette(cx, &rig);
            assert!(!prompt_focused(cx, &rig), "the palette's search has focus");
            gpui_kit::VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes("escape");
            rig.app.read_with(cx, |app, _| assert!(!app.palette_open));
            assert!(prompt_focused(cx, &rig), "{page:?}: prompt after Esc");
        }
    }

    #[gpui_kit::test]
    fn toggling_the_palette_off_focuses_the_prompt(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        open_palette(cx, &rig);
        open_palette(cx, &rig);
        assert!(prompt_focused(cx, &rig));
    }

    #[gpui_kit::test]
    fn picking_a_palette_page_entry_focuses_the_prompt(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        pick(cx, &rig, PaletteTarget::Page(Page::Start));
        assert!(prompt_focused(cx, &rig), "Start's prompt");
        pick(cx, &rig, PaletteTarget::ToggleSidebar);
        assert!(prompt_focused(cx, &rig), "still the prompt");
    }

    #[gpui_kit::test]
    fn palette_command_is_queued_while_running(cx: &mut TestAppContext) {
        let mut view = session_with(two_commands());
        view.phase = "model".into();
        let rig = rig(cx, Page::Session, Some(view), |store| {
            store.runtime_mut_for_test("test-agent").capabilities = Some(json!({
                "features": { "steering": true },
                "methods": ["querymt/session/steer", "querymt/session/queue",
                            "querymt/session/runtimeState"],
            }));
        });
        let target = target_of(&groups(cx, &rig), "worktree");
        pick(cx, &rig, target);
        let requests = sent(&rig);
        assert!(prompts(&requests).is_empty(), "not a new prompt");
        let queued: Vec<&Value> = requests
            .iter()
            .filter(|r| r["method"] == "_querymt/session/queue")
            .collect();
        assert_eq!(queued.len(), 1, "{requests:?}");
        assert_eq!(queued[0]["params"]["prompt"][0]["text"], "/worktree");
        assert!(
            requests
                .iter()
                .all(|r| r["method"] != "_querymt/session/steer")
        );
        rig.app.read_with(cx, |app, cx| {
            let view = app.store.read(cx).session.as_ref().unwrap();
            assert_eq!(view.pending_inputs.len(), 1);
            assert_eq!(view.pending_inputs[0].delivery, "queue");
            assert_eq!(view.pending_inputs[0].text, "/worktree");
        });
    }

    #[gpui_kit::test]
    fn spec_and_prefixed_names_are_sent_with_one_slash(cx: &mut TestAppContext) {
        // The agent advertises `web` (the spec's form) and `/compact`
        // (QueryMT's old form, and other agents').
        let commands = json!([
            { "name": "web", "description": "Search the web" },
            { "name": "/compact", "description": COMPACT },
        ]);
        let first = rig(
            cx,
            Page::Session,
            Some(session_with(commands.clone())),
            |_| {},
        );
        assert_eq!(
            entry_labels(&groups(cx, &first), "Slash commands"),
            ["web", "compact"]
        );
        // One session per send: a sent prompt starts a turn, and the next
        // pick would (rightly) wait for it.
        for (label, expected) in [("web", "/web"), ("compact", "/compact")] {
            let rig = rig(
                cx,
                Page::Session,
                Some(session_with(commands.clone())),
                |_| {},
            );
            let target = target_of(&groups(cx, &rig), label);
            pick(cx, &rig, target);
            assert_eq!(prompts(&sent(&rig)), [expected]);
        }
        // A target built with a prefixed name is not doubled either.
        let rig = rig(cx, Page::Session, Some(session_with(commands)), |_| {});
        pick(
            cx,
            &rig,
            PaletteTarget::SlashCommand {
                name: "/web".into(),
                takes_input: false,
            },
        );
        assert_eq!(prompts(&sent(&rig)), ["/web"]);
    }
}
