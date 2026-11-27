//! Keyboard shortcuts (same set as the desktop app) and the command palette.
//!
//! Bindings have no key context: they reach [`QmtApp`]'s root element, which
//! tracks the app focus handle so they work even when no control is focused.
//! Text inputs keep their own bindings (their context is deeper), so Ctrl/Cmd+Z
//! edits text while typing and undoes the workspace elsewhere.
//!
//! Shortcuts (Cmd on macOS, Ctrl elsewhere): N new session, P command
//! palette, B sidebar, Z / Shift+Z undo / redo, M model picker, T reasoning,
//! 1 to 0 sidebar sessions, F find in the open session. O and K belong to the
//! quick panel.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem};
use gpui_kit::component::input::IndentInline;
use gpui_kit::component::{Icon, IndexPath, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use super::session_pane::Blocked;
use super::{Page, QmtApp, ToggleSidebar};
use crate::session::commands::CommandTraits;
use crate::store::{LAUNCH_REASONING, SessionSummary};
use crate::theme::{self, Appearance, ThemeId, palette};
use crate::util::workspace_name;
use querymt_agent_session_core::view::{CommandInput, SlashCommandSource};

gpui_kit::actions!(
    querymt,
    [
        NewSession,
        ToggleCommandPalette,
        UndoWorkspace,
        RedoWorkspace,
        OpenModelPicker,
        CycleReasoning,
        ChooseFolder,
        ToggleSessionList,
        FindInSession,
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
        // Handled by the quick panel only.
        KeyBinding::new(k("cmd-o", "ctrl-o"), ChooseFolder, None),
        // Handled by the quick panel only; the main window ignores it. A
        // future main-window use of this key needs a key context on one side.
        KeyBinding::new(k("cmd-k", "ctrl-k"), ToggleSessionList, None),
        // Find in the open session (main window only). A composer input is
        // not searchable, so its own `cmd-f` binding propagates to this one.
        KeyBinding::new(k("cmd-f", "ctrl-f"), FindInSession, None),
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
    /// `blocked` is why the session cannot take it now; such a row stays in
    /// its place and picking it does nothing.
    SlashCommand {
        name: String,
        input: CommandInput,
        blocked: Option<Blocked>,
    },
    /// A subcommand of a slash command of the open session, such as
    /// `worktree list` (design D4). Picking it only puts
    /// `/<command> <subcommand> ` in the composer; nothing is sent.
    /// `blocked` is the parent's: such a row does nothing.
    SlashSubcommand {
        command: String,
        subcommand: String,
        blocked: Option<Blocked>,
    },
    /// "Select theme…": the palette lists the themes instead (design D12).
    SelectTheme,
    /// A row of the theme list.
    Theme(ThemeId),
}

/// What the command palette lists (design D12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaletteMode {
    Commands,
    /// The theme list. `restore` is what the window showed when it opened:
    /// closing without confirming puts it back.
    Themes {
        restore: Appearance,
    },
}

/// One palette row: what it shows, what the search matches besides the
/// label, and what picking it does. Built together, so an entry's target can
/// never drift from its row (design D5).
struct PaletteEntry {
    label: SharedString,
    icon: IconName,
    /// A dimmed second line under the label (a slash command's help, or the
    /// reason it is unavailable).
    description: Option<SharedString>,
    /// A small word at the end of the first line (`Prompt`).
    mark: Option<SharedString>,
    /// Drawn dimmed: the row does nothing now. Not a kit-disabled item, which
    /// would move the highlight to another row (design D4).
    dim: bool,
    keywords: Vec<String>,
    /// Marked with a check (the theme in use).
    checked: bool,
    target: PaletteTarget,
}

impl PaletteEntry {
    fn new(label: impl Into<SharedString>, icon: IconName, target: PaletteTarget) -> Self {
        Self {
            label: label.into(),
            icon,
            description: None,
            mark: None,
            dim: false,
            keywords: Vec::new(),
            checked: false,
            target,
        }
    }

    fn mark(mut self, mark: impl Into<SharedString>) -> Self {
        self.mark = Some(mark.into());
        self
    }

    fn dim(mut self, dim: bool) -> Self {
        self.dim = dim;
        self
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
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

/// The word the palette search matches a command's source by.
fn source_word(source: SlashCommandSource) -> &'static str {
    match source {
        SlashCommandSource::Builtin => "builtin",
        SlashCommandSource::Global => "global",
        SlashCommandSource::Project => "project",
        SlashCommandSource::Configured => "configured",
    }
}

/// The theme list: the dark themes, then the light ones, each in catalogue
/// order, `shown` (the theme in use) marked.
fn theme_groups(shown: ThemeId) -> Vec<PaletteGroup> {
    [
        (true, "Dark themes", IconName::Moon),
        (false, "Light themes", IconName::Sun),
    ]
    .into_iter()
    .map(|(dark, label, icon)| PaletteGroup {
        label,
        entries: ThemeId::of_appearance(dark)
            .into_iter()
            .map(|id| {
                PaletteEntry::new(id.label(), icon, PaletteTarget::Theme(id)).checked(id == shown)
            })
            .collect(),
    })
    .collect()
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
        .on_action(cx.listener(|this, _: &FindInSession, window, cx| {
            if this.page == Page::Session && !this.palette_open {
                this.pane.update(cx, |pane, cx| pane.open_find(window, cx));
            }
        }))
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
                let Some(option) = self.main_view(cx).and_then(|v| {
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
                self.pane
                    .update(cx, |pane, cx| pane.set_option(&option.id, &value, cx));
            }
            _ => {}
        }
    }

    /// Start page with the prompt focused.
    pub(super) fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(cx);
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
        self.close_palette(cx);
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
            let target = self.main_view(cx).and_then(|v| v.undo_target());
            if let Some(message_id) = target {
                self.pane
                    .update(cx, |pane, cx| pane.request_undo(message_id, cx));
            }
        } else {
            self.pane.update(cx, |pane, cx| pane.redo(cx));
        }
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_palette(cx) {
            self.restore_focus(window, cx);
        } else {
            self.palette_open = true;
            self.model_picker.update(cx, |picker, cx| {
                picker.close(cx);
            });
            self.options_open = false;
            self.palette.update(cx, |s, cx| {
                s.set_query("", window, cx);
                s.focus(window, cx);
            });
        }
        cx.notify();
    }

    /// Close the palette. A theme list that was not confirmed puts the
    /// theme it opened over back (design D12); every way of closing comes
    /// through here, so none can forget it. Returns whether it was open.
    pub(super) fn close_palette(&mut self, cx: &mut Context<Self>) -> bool {
        self.theme_highlight = None;
        let was_open = std::mem::take(&mut self.palette_open);
        if let PaletteMode::Themes { restore } =
            std::mem::replace(&mut self.palette_mode, PaletteMode::Commands)
        {
            theme::apply(restore, cx);
            cx.notify();
        }
        was_open
    }

    /// What the window showed, theme and accent, as the preferences say.
    fn shown_appearance(&self, cx: &App) -> Appearance {
        Appearance {
            theme: palette(cx).theme,
            accent: self.store.read(cx).prefs.accent_choice(),
        }
    }

    /// "Select theme…": the palette lists the themes, the one shown marked
    /// and highlighted once the list is drawn.
    fn open_theme_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let restore = self.shown_appearance(cx);
        self.palette_mode = PaletteMode::Themes { restore };
        // The rows are the dark group, then the light one, as `theme_groups`
        // builds them.
        let dark = restore.theme.dark();
        let row = ThemeId::of_appearance(dark)
            .iter()
            .position(|id| *id == restore.theme);
        self.theme_highlight = row.map(|row| (usize::from(!dark), row));
        self.palette.update(cx, |s, cx| {
            s.set_query("", window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// Show `id` without saving it, with the accent the window had.
    fn preview_theme(&mut self, id: ThemeId, cx: &mut Context<Self>) {
        let PaletteMode::Themes { restore } = self.palette_mode else {
            return;
        };
        if palette(cx).theme != id {
            theme::apply(
                Appearance {
                    theme: id,
                    accent: restore.accent,
                },
                cx,
            );
            cx.notify();
        }
    }

    /// Show the highlighted row's theme: the highlight moves by itself when
    /// the search changes the rows.
    fn sync_theme_preview(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.palette_mode, PaletteMode::Themes { .. }) {
            return;
        }
        let query = self.palette.read(cx).query(cx).to_string();
        let groups = filter_palette(self.palette_groups(cx), &query);
        let target = self.palette.read(cx).selected_index().and_then(|ix| {
            groups
                .get(ix.section)?
                .entries
                .get(ix.row)
                .map(|e| e.target.clone())
        });
        if let Some(PaletteTarget::Theme(id)) = target {
            self.preview_theme(id, cx);
        }
    }

    /// Enter on a theme: save it as the theme of its appearance. The mode
    /// follows only when that is not the appearance shown, so System stays
    /// System for a theme of the desktop's appearance.
    fn confirm_theme(&mut self, id: ThemeId, window: &mut Window, cx: &mut Context<Self>) {
        let PaletteMode::Themes { restore } = self.palette_mode else {
            return;
        };
        // Confirmed: nothing to put back.
        self.palette_mode = PaletteMode::Commands;
        self.close_palette(cx);
        let dark = id.dark();
        let key = id.key().to_string();
        self.store.update(cx, |s, cx| {
            s.update_prefs(
                |p| {
                    if dark {
                        p.theme_dark = key;
                    } else {
                        p.theme_light = key;
                    }
                    if dark != restore.theme.dark() {
                        p.dark = Some(dark);
                    }
                },
                cx,
            )
        });
        self.apply_theme(window, cx);
        self.restore_focus(window, cx);
    }

    fn run_palette(&mut self, target: PaletteTarget, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            PaletteTarget::SelectTheme => return self.open_theme_list(window, cx),
            PaletteTarget::Theme(id) => return self.confirm_theme(id, window, cx),
            // Unavailable: nothing is sent and the palette stays open.
            PaletteTarget::SlashCommand {
                blocked: Some(_), ..
            }
            | PaletteTarget::SlashSubcommand {
                blocked: Some(_), ..
            } => return,
            _ => {}
        }
        self.close_palette(cx);
        match target {
            // Handled above.
            PaletteTarget::SelectTheme | PaletteTarget::Theme(_) => return,
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
            PaletteTarget::SlashCommand { name, input, .. } => {
                return self.run_slash_command(&name, input, window, cx);
            }
            PaletteTarget::SlashSubcommand {
                command,
                subcommand,
                ..
            } => return self.prefill_slash_subcommand(&command, &subcommand, window, cx),
        }
        self.restore_focus(window, cx);
        cx.notify();
    }

    /// Picking a slash command in the palette. With no input or an optional
    /// one it is sent as a prompt (`/name`), under the same rules as a typed
    /// one: while a turn runs `send_prompt` queues it. With a required input
    /// the composer gets `/name ` (keeping a draft as the arguments), the
    /// cursor at the end and the focus, so the arguments can be typed.
    pub(super) fn run_slash_command(
        &mut self,
        name: &str,
        input: CommandInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The main window's session pane owns the composer.
        self.pane.update(cx, |pane, cx| {
            pane.run_slash_command(name, input, window, cx)
        });
        cx.notify();
    }

    /// Picking a subcommand: the composer gets `/<command> <subcommand> `
    /// and the focus. A draft is kept as the arguments, and a subcommand
    /// already typed after the command is replaced. The cursor ends after the
    /// subcommand when a typed word was replaced and at the end otherwise.
    /// Nothing is sent, whatever the subcommand does.
    fn prefill_slash_subcommand(
        &mut self,
        command: &str,
        subcommand: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane.update(cx, |pane, cx| {
            pane.prefill_slash_subcommand(command, subcommand, window, cx)
        });
        cx.notify();
    }

    /// The target of the highlighted palette row, if any.
    fn highlighted_target(&self, cx: &App) -> Option<PaletteTarget> {
        let ix = self.palette.read(cx).selected_index()?;
        let query = self.palette.read(cx).query(cx).to_string();
        let groups = filter_palette(self.palette_groups(cx), &query);
        groups
            .into_iter()
            .nth(ix.section)?
            .entries
            .into_iter()
            .nth(ix.row)
            .map(|e| e.target)
    }

    /// Tab in the palette: put the highlighted slash command in the composer
    /// instead of running it (design D6). Only an available command that
    /// takes input does; on any other row Tab does nothing, and the key is
    /// spent either way so focus cannot leave the search field.
    fn palette_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.highlighted_target(cx) {
            Some(PaletteTarget::SlashCommand {
                name,
                input: CommandInput::Optional | CommandInput::Required,
                blocked: None,
            }) => {
                self.close_palette(cx);
                self.pane
                    .update(cx, |pane, cx| pane.prefill_slash(&name, window, cx));
                cx.notify();
            }
            // Tab on an available subcommand does what picking it does.
            Some(PaletteTarget::SlashSubcommand {
                command,
                subcommand,
                blocked: None,
            }) => {
                self.close_palette(cx);
                self.prefill_slash_subcommand(&command, &subcommand, window, cx);
            }
            _ => {}
        }
    }

    /// The command palette overlay, when open.
    pub(super) fn render_palette(
        &mut self,
        radii: Corners<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.palette_open {
            return None;
        }
        if matches!(self.palette_mode, PaletteMode::Themes { .. }) {
            // After this frame the list's rows exist: highlight the theme in
            // use, then show the highlighted one.
            let this = cx.entity().downgrade();
            let highlight = self.theme_highlight.take();
            let state = self.palette.clone();
            window.defer(cx, move |window, cx| {
                if let Some((section, row)) = highlight {
                    state.update(cx, |s, cx| {
                        s.set_selected_index(
                            Some(IndexPath::default().section(section).row(row)),
                            window,
                            cx,
                        )
                    });
                }
                let _ = this.update(cx, |this, cx| this.sync_theme_preview(cx));
            });
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
            // Tab is bound to an action of the search field, which runs
            // before key-down listeners: catch the action first. The key-down
            // listener covers a keymap that does not bind Tab.
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.palette_tab(window, cx);
                cx.stop_propagation();
            }))
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let m = ev.keystroke.modifiers;
                if ev.keystroke.key != "tab" || m.modified() {
                    return;
                }
                this.palette_tab(window, cx);
                cx.stop_propagation();
            }))
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
                    this.close_palette(cx);
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
        let selected = this.clone();
        let on_select_targets = targets.clone();
        let mut command = Command::new(&self.palette)
            .placeholder("Search actions, commands, pages and sessions…")
            .max_h(px(380.))
            .bordered(false)
            .filterable(false)
            // A new query rebuilds the groups on the next render.
            .on_query(move |_, _, cx| {
                let _ = requery.update(cx, |_, cx| cx.notify());
            })
            // Moving the highlight over the theme list shows that theme.
            .on_select(move |ix: IndexPath, _, cx| {
                let Some(PaletteTarget::Theme(id)) = on_select_targets
                    .get(ix.section)
                    .and_then(|g| g.get(ix.row))
                    .cloned()
                else {
                    return;
                };
                let _ = selected.update(cx, |this, cx| this.preview_theme(id, cx));
            });
        for group in groups {
            command = command.group(CommandGroup::new().label(group.label).items(
                group.entries.into_iter().map(move |e| {
                    let item = CommandItem::new()
                        .label(e.label.clone())
                        .icon(e.icon)
                        .checked(e.checked)
                        .keywords(e.keywords);
                    // The kit draws icon and label only; a description, a
                    // mark or a dimmed row needs a row of our own (the
                    // label and keywords still search).
                    if e.description.is_none() && e.mark.is_none() && !e.dim {
                        return item;
                    }
                    let (label, icon, description, mark, dim) =
                        (e.label, e.icon, e.description, e.mark, e.dim);
                    let (name_color, icon_color) = if dim {
                        (p.dim, p.dim)
                    } else {
                        (p.text, p.muted)
                    };
                    item.child(move |_, _| {
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(icon).size_4().text_color(icon_color))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_sm()
                                                    .text_color(name_color)
                                                    .whitespace_nowrap()
                                                    .truncate()
                                                    .child(label.clone()),
                                            )
                                            .children(mark.clone().map(|mark| {
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(p.muted)
                                                    .child(mark)
                                            })),
                                    )
                                    .children(description.clone().map(|description| {
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .whitespace_nowrap()
                                            .truncate()
                                            .child(description)
                                    })),
                            )
                    })
                }),
            ));
        }
        // While an available command with optional input is highlighted:
        // Enter runs it, Tab adds arguments. The footer is built with the
        // state at render, so it follows the highlight.
        let footer_targets = targets.clone();
        command = command.footer(move |state, _, _| {
            let optional = state
                .selected_index()
                .and_then(|ix| footer_targets.get(ix.section)?.get(ix.row))
                .is_some_and(|t| {
                    matches!(
                        t,
                        PaletteTarget::SlashCommand {
                            input: CommandInput::Optional,
                            blocked: None,
                            ..
                        }
                    )
                });
            let key = |k: &'static str| {
                Keystroke::parse(k)
                    .ok()
                    .map(gpui_kit::component::kbd::Kbd::new)
            };
            div().when(optional, |el| {
                el.debug_selector(|| "palette-key-hint".into())
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_3()
                    .py_1p5()
                    .text_xs()
                    .text_color(p.muted)
                    .children(key("enter"))
                    .child("to run")
                    .child("·")
                    .children(key("tab"))
                    .child("to add arguments")
            })
        });
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
                    this.close_palette(cx);
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
        if let PaletteMode::Themes { restore } = self.palette_mode {
            return theme_groups(restore.theme);
        }
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
            PaletteEntry::new(
                "Select theme…",
                IconName::Palette,
                PaletteTarget::SelectTheme,
            )
            .keywords(["colour", "color", "appearance", "dark", "light"]),
        );
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

        let pane = self.pane.read(cx);
        let commands = pane.slash_commands(cx);
        if self.page == Page::Session && !commands.is_empty() {
            groups.push(PaletteGroup {
                label: "Slash commands",
                entries: commands
                    .iter()
                    .flat_map(|c| {
                        let traits = CommandTraits::of(c);
                        let blocked = pane.readiness(Some(c), cx).err();
                        // The bare name: the palette lists commands, it is
                        // not the composer. A required input ends it with
                        // `…`. The second line is the agent's own help text,
                        // or why the session cannot take the command now.
                        let label = if traits.input == CommandInput::Required {
                            format!("{}…", c.name)
                        } else {
                            c.name.clone()
                        };
                        let second: String = blocked.map_or_else(
                            || c.description.clone(),
                            |reason| reason.reason().to_owned(),
                        );
                        let mut entry = PaletteEntry::new(
                            label,
                            IconName::SquareSlash,
                            PaletteTarget::SlashCommand {
                                name: c.name.clone(),
                                input: traits.input,
                                blocked,
                            },
                        )
                        .description(second)
                        .dim(blocked.is_some())
                        .keywords(
                            std::iter::once(c.description.clone())
                                .chain(c.input_hint.clone().filter(|h| !h.is_empty()))
                                .chain(traits.source.map(source_word).map(str::to_owned))
                                .chain(traits.is_prompt().then(|| "prompt".to_owned())),
                        );
                        if traits.is_prompt() {
                            entry = entry.mark("Prompt");
                        }
                        // Each subcommand follows its command, in the
                        // agent's order, with the parent's readiness and
                        // source (design D4).
                        let subcommands = c.subcommands.iter().map(move |sub| {
                            PaletteEntry::new(
                                format!("{} {}…", c.name, sub.name),
                                IconName::SquareSlash,
                                PaletteTarget::SlashSubcommand {
                                    command: c.name.clone(),
                                    subcommand: sub.name.clone(),
                                    blocked,
                                },
                            )
                            .description(blocked.map_or_else(
                                || sub.description.clone(),
                                |reason| reason.reason().to_owned(),
                            ))
                            .dim(blocked.is_some())
                            .keywords(
                                std::iter::once(sub.description.clone())
                                    .chain(traits.source.map(source_word).map(str::to_owned))
                                    .chain(traits.is_prompt().then(|| "prompt".to_owned())),
                            )
                        });
                        std::iter::once(entry).chain(subcommands)
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
    use crate::ui::session_pane::Blocked;
    use crate::ui::{Page, QmtApp};
    use gpui_kit::assets::IconName;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use querymt_agent_session_core::view::CommandInput;
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

    /// `/compact` takes optional input (its descriptor says so), `/worktree`
    /// has no input and no descriptor.
    fn two_commands() -> Value {
        json!([
            { "name": "compact", "description": COMPACT,
              "input": { "hint": "[optional instructions]" },
              "_meta": { "querymt": { "source": "builtin", "effect": "action",
                  "input": "optional", "accepts": ["prompt", "queue"] } } },
            { "name": "worktree", "description": "Work in a git worktree" },
        ])
    }

    /// `compact` (optional input, accepts queueing), `explain` (a project
    /// prompt command with required input) and `worktree` (required input,
    /// accepts only the prompt submission).
    fn described_commands() -> Value {
        json!([
            { "name": "compact", "description": COMPACT,
              "input": { "hint": "[optional instructions]" },
              "_meta": { "querymt": { "source": "builtin", "effect": "action",
                  "input": "optional", "accepts": ["prompt", "queue"] } } },
            { "name": "explain", "description": "Explain a topic",
              "input": { "hint": "[topic]" },
              "_meta": { "querymt": { "source": "project", "effect": "prompt",
                  "input": "required", "accepts": ["prompt", "queue"] } } },
            { "name": "worktree", "description": "Work in a git worktree",
              "input": { "hint": "[subcommand]" },
              "_meta": { "querymt": { "source": "builtin", "effect": "action",
                  "input": "required", "accepts": ["prompt"] } } },
        ])
    }

    /// A running turn the agent lets the app queue into.
    fn running_session(commands: Value) -> SessionView {
        let mut view = session_with(commands);
        view.phase = "model".into();
        view
    }

    fn with_turn_control(store: &mut AgentsStore) {
        store.runtime_mut_for_test("test-agent").capabilities = Some(json!({
            "features": { "steering": true },
            "methods": ["querymt/session/steer", "querymt/session/queue",
                        "querymt/session/runtimeState"],
        }));
    }

    fn slash_rows(groups: &[PaletteGroup]) -> Vec<&PaletteEntry> {
        groups
            .iter()
            .filter(|g| g.label == "Slash commands")
            .flat_map(|g| &g.entries)
            .collect()
    }

    fn rig(
        cx: &mut TestAppContext,
        page: Page,
        view: Option<SessionView>,
        setup: impl FnOnce(&mut AgentsStore),
    ) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(crate::theme::Appearance::builtin(true), cx);
        });
        let mut store = crate::store::launch::peer_fixture();
        let (_rpc, requests) = store.rpc_for_test("test-agent");
        // Before the view: its session is bound to the agent as set up here.
        setup(&mut store);
        let (handle, app) = cx.update(|cx| {
            let store = cx.new(|cx| {
                let mut store = store;
                if let Some(view) = view {
                    store.install_main_for_test(view, cx);
                }
                store
            });
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
            PaletteTarget::SlashCommand {
                ref name,
                input: CommandInput::Optional,
                blocked: None
            } if name == "compact"
        ));
        assert!(matches!(
            target_of(&all, "worktree"),
            PaletteTarget::SlashCommand {
                ref name,
                input: CommandInput::None,
                blocked: None
            } if name == "worktree"
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
                        input: CommandInput::Optional,
                        blocked: None,
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
                    (PaletteTarget::SelectTheme, "Actions") => {
                        assert_eq!(entry.label.as_ref(), "Select theme…");
                    }
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
            assert_eq!(app.pane.read(cx).prompt.read(cx).value().as_ref(), "");
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
        let all = groups(cx, &rig);
        let target = target_of(&all, "worktree");
        assert!(matches!(
            target,
            PaletteTarget::SlashCommand {
                blocked: Some(Blocked::Loading),
                ..
            }
        ));
        let row = slash_rows(&all)[1];
        assert_eq!(row.description.as_deref(), Some("Session is loading"));
        assert!(row.dim);
        pick(cx, &rig, target);
        assert!(
            sent(&rig).is_empty(),
            "same readiness rule as a typed prompt"
        );
        // Picking it did nothing: the palette stays open.
        rig.app.read_with(cx, |app, _| assert!(app.palette_open));
    }

    #[gpui_kit::test]
    fn palette_disables_commands_while_disconnected(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |store| {
                store.runtime_mut_for_test("test-agent").state =
                    crate::store::ConnState::Failed("gone".into())
            },
        );
        let all = groups(cx, &rig);
        let rows = slash_rows(&all);
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert!(row.dim);
            assert_eq!(row.description.as_deref(), Some("Agent not connected"));
        }
    }

    #[gpui_kit::test]
    fn palette_runs_an_optional_input_command(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        let target = target_of(&groups(cx, &rig), "compact");
        pick(cx, &rig, target);
        assert_eq!(prompts(&sent(&rig)), ["/compact"]);
        rig.app.read_with(cx, |app, cx| {
            assert!(!app.palette_open, "the palette closes");
            assert_eq!(app.pane.read(cx).prompt.read(cx).value().as_ref(), "");
        });
    }

    #[gpui_kit::test]
    fn prompt_only_command_is_unavailable_during_a_turn(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(running_session(described_commands())),
            with_turn_control,
        );
        let all = groups(cx, &rig);
        let worktree = slash_rows(&all)
            .into_iter()
            .find(|e| e.label.as_ref() == "worktree…")
            .expect("worktree…");
        assert!(worktree.dim);
        assert_eq!(
            worktree.description.as_deref(),
            Some("Available when the turn ends")
        );
        let compact = slash_rows(&all)
            .into_iter()
            .find(|e| e.label.as_ref() == "compact")
            .expect("compact");
        assert!(!compact.dim);
        assert_eq!(compact.description.as_deref(), Some(COMPACT));
    }

    #[gpui_kit::test]
    fn picking_an_unavailable_command_keeps_the_palette_open(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(running_session(described_commands())),
            with_turn_control,
        );
        open_palette(cx, &rig);
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette
                    .update(cx, |s, cx| s.set_query("worktree", window, cx))
            });
        })
        .unwrap();
        cx.run_until_parked();
        render(cx, &rig);
        let query = rig
            .app
            .read_with(cx, |app, cx| app.palette.read(cx).query(cx).to_string());
        let shown = filter_palette(groups(cx, &rig), &query);
        assert_eq!(shown[0].entries[0].label.as_ref(), "worktree…");
        let target = shown[0].entries[0].target.clone();
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        rig.app.read_with(cx, |app, cx| {
            assert!(app.palette_open, "the palette stays open");
            assert_eq!(app.pane.read(cx).prompt.read(cx).value().as_ref(), "");
        });
    }

    #[gpui_kit::test]
    fn required_input_rows_end_with_an_ellipsis(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        assert_eq!(
            entry_labels(&groups(cx, &rig), "Slash commands"),
            ["compact", "explain…", "worktree…"]
        );
    }

    #[gpui_kit::test]
    fn prompt_commands_are_marked(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        let all = groups(cx, &rig);
        let marks: Vec<Option<&str>> = slash_rows(&all).iter().map(|e| e.mark.as_deref()).collect();
        assert_eq!(marks, [None, Some("Prompt"), None]);
        // A command without a descriptor shows no mark and no source word.
        let rig = rig_plain(cx);
        let all = groups(cx, &rig);
        assert!(slash_rows(&all).iter().all(|e| e.mark.is_none()));
    }

    fn rig_plain(cx: &mut TestAppContext) -> Rig {
        rig(
            cx,
            Page::Session,
            Some(session_with(json!([
                { "name": "review", "description": "Review", "input": { "hint": "[scope]" } },
            ]))),
            |_| {},
        )
    }

    #[gpui_kit::test]
    fn palette_searches_the_source(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        let found = filter_palette(groups(cx, &rig), "project");
        assert_eq!(entry_labels(&found, "Slash commands"), ["explain…"]);
        let found = filter_palette(groups(cx, &rig), "prompt");
        assert!(entry_labels(&found, "Slash commands").contains(&"explain…".to_string()));
        assert!(!entry_labels(&found, "Slash commands").contains(&"compact".to_string()));
    }

    #[gpui_kit::test]
    fn composer_draft_becomes_the_arguments(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        cx.update_window(rig.handle, |_, window, cx| {
            let prompt = rig.app.read(cx).pane.read(cx).prompt.clone();
            prompt.update(cx, |s, cx| s.set_value("closures in Rust", window, cx));
        })
        .unwrap();
        let target = target_of(&groups(cx, &rig), "explain…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty());
        cx.update_window(rig.handle, |_, _, cx| {
            let state = rig.app.read(cx).pane.read(cx).prompt.clone();
            let state = state.read(cx);
            assert_eq!(state.value().as_ref(), "/explain closures in Rust");
            let c = state.cursor_position();
            assert_eq!((c.line, c.character), (0, 25), "cursor at the end");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_path_draft_becomes_the_arguments(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        cx.update_window(rig.handle, |_, window, cx| {
            let prompt = rig.app.read(cx).pane.read(cx).prompt.clone();
            prompt.update(cx, |s, cx| {
                s.set_value("/tmp/build.log is empty", window, cx)
            });
        })
        .unwrap();
        let target = target_of(&groups(cx, &rig), "explain…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty());
        cx.update_window(rig.handle, |_, _, cx| {
            let state = rig.app.read(cx).pane.read(cx).prompt.clone();
            let state = state.read(cx);
            // The path is part of the message, not a command name to replace.
            assert_eq!(state.value().as_ref(), "/explain /tmp/build.log is empty");
            let c = state.cursor_position();
            assert_eq!((c.line, c.character), (0, 32), "cursor at the end");
        })
        .unwrap();
    }

    fn press_tab(cx: &mut TestAppContext, rig: &Rig) {
        cx.update_window(rig.handle, |_, window, cx| window.press("tab", cx))
            .unwrap();
        cx.run_until_parked();
        render(cx, rig);
    }

    /// Open the palette and highlight the row labelled `label`.
    fn highlight_row(cx: &mut TestAppContext, rig: &Rig, label: &str) {
        if !rig.app.read_with(cx, |app, _| app.palette_open) {
            open_palette(cx, rig);
        }
        let shown = filter_palette(groups(cx, rig), "");
        let (section, row) = shown
            .iter()
            .enumerate()
            .find_map(|(s, g)| {
                g.entries
                    .iter()
                    .position(|e| e.label.as_ref() == label)
                    .map(|r| (s, r))
            })
            .unwrap_or_else(|| panic!("no row {label}"));
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette.update(cx, |s, cx| {
                    s.set_selected_index(
                        Some(IndexPath::default().section(section).row(row)),
                        window,
                        cx,
                    )
                })
            });
        })
        .unwrap();
        cx.run_until_parked();
        render(cx, rig);
    }

    fn key_hint_shown(cx: &mut TestAppContext, rig: &Rig) -> bool {
        gpui_kit::VisualTestContext::from_window(rig.handle, cx)
            .debug_bounds("palette-key-hint")
            .is_some()
    }

    #[gpui_kit::test]
    fn tab_adds_arguments_to_an_optional_command(cx: &mut TestAppContext) {
        cx.update(super::bind_keys);
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        highlight_row(cx, &rig, "compact");
        press_tab(cx, &rig);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        rig.app.read_with(cx, |app, cx| {
            assert!(!app.palette_open, "the palette closes");
            assert_eq!(
                app.pane.read(cx).prompt.read(cx).value().as_ref(),
                "/compact "
            );
        });
        assert!(prompt_focused(cx, &rig));
    }

    #[gpui_kit::test]
    fn tab_on_a_command_without_input_does_nothing(cx: &mut TestAppContext) {
        cx.update(super::bind_keys);
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        highlight_row(cx, &rig, "worktree");
        press_tab(cx, &rig);
        assert!(sent(&rig).is_empty());
        rig.app.read_with(cx, |app, cx| {
            assert!(app.palette_open, "the palette stays open");
            assert_eq!(app.pane.read(cx).prompt.read(cx).value().as_ref(), "");
        });
        // An Actions row: Tab changes nothing either.
        highlight_row(cx, &rig, "New session");
        press_tab(cx, &rig);
        assert!(sent(&rig).is_empty());
        rig.app.read_with(cx, |app, cx| {
            assert!(app.palette_open);
            assert_eq!(app.pane.read(cx).prompt.read(cx).value().as_ref(), "");
        });
    }

    #[gpui_kit::test]
    fn key_hint_shows_for_an_optional_input_command(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(two_commands())),
            |_| {},
        );
        highlight_row(cx, &rig, "compact");
        assert!(key_hint_shown(cx, &rig));
        highlight_row(cx, &rig, "New session");
        assert!(!key_hint_shown(cx, &rig), "not for a row of another group");
        highlight_row(cx, &rig, "worktree");
        assert!(!key_hint_shown(cx, &rig), "not for a command without input");
    }

    const WORKTREE_SUBS: [(&str, &str); 10] = [
        ("create", "Start an isolated session in a new worktree"),
        (
            "handoff",
            "Hand this session's work to a new isolated session",
        ),
        ("list", "List the worktrees of this checkout"),
        ("status", "Show a worktree's state"),
        ("result", "Show a worktree's result"),
        (
            "publish",
            "Pin a worktree's working state under a result ref",
        ),
        ("merge", "Merge a worktree into its source checkout"),
        ("archive", "Archive a worktree session"),
        ("remove", "Remove a worktree; refuses to drop unsaved work"),
        ("recover", "Finish an interrupted create or remove"),
    ];

    /// `compact`, and `worktree` advertising its ten subcommands.
    fn worktree_commands() -> Value {
        let subcommands: Vec<Value> = WORKTREE_SUBS
            .iter()
            .map(|(name, description)| json!({ "name": name, "description": description }))
            .collect();
        json!([
            { "name": "compact", "description": COMPACT,
              "input": { "hint": "[optional instructions]" },
              "_meta": { "querymt": { "source": "builtin", "effect": "action",
                  "input": "optional", "accepts": ["prompt", "queue"] } } },
            { "name": "worktree", "description": "Work in a git worktree",
              "input": { "hint": "[subcommand]" },
              "_meta": { "querymt": { "source": "builtin", "effect": "action",
                  "input": "required", "accepts": ["prompt"],
                  "subcommands": subcommands } } },
        ])
    }

    fn prompt_value(cx: &mut TestAppContext, rig: &Rig) -> (String, (u32, u32)) {
        rig.app.read_with(cx, |app, cx| {
            let state = app.pane.read(cx).prompt.clone();
            let state = state.read(cx);
            let cursor = state.cursor_position();
            (state.value().to_string(), (cursor.line, cursor.character))
        })
    }

    #[gpui_kit::test]
    fn palette_lists_worktree_subcommands(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        let all = groups(cx, &rig);
        let mut expected = vec!["compact".to_owned(), "worktree…".to_owned()];
        expected.extend(
            WORKTREE_SUBS
                .iter()
                .map(|(name, _)| format!("worktree {name}…")),
        );
        assert_eq!(entry_labels(&all, "Slash commands"), expected);
        // Each subcommand shows its description under it, not dimmed.
        let rows = slash_rows(&all);
        for (row, (_, description)) in rows[2..].iter().zip(WORKTREE_SUBS) {
            assert_eq!(row.description.as_deref(), Some(description));
            assert!(!row.dim);
        }
    }

    #[gpui_kit::test]
    fn searching_for_a_subcommand(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        let found = filter_palette(groups(cx, &rig), "worktree status");
        assert_eq!(found[0].label, "Slash commands");
        assert_eq!(found[0].entries[0].label.as_ref(), "worktree status…");
    }

    #[gpui_kit::test]
    fn searching_a_subcommands_description(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        let found = filter_palette(groups(cx, &rig), "interrupted create");
        assert!(
            entry_labels(&found, "Slash commands").contains(&"worktree recover…".to_owned()),
            "{:?}",
            entry_labels(&found, "Slash commands")
        );
    }

    #[gpui_kit::test]
    fn agent_without_subcommands(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        // `worktree` advertises none: only its own row.
        assert_eq!(
            entry_labels(&groups(cx, &rig), "Slash commands"),
            ["compact", "explain…", "worktree…"]
        );
    }

    #[gpui_kit::test]
    fn picking_a_subcommand_prefills_it(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        let target = target_of(&groups(cx, &rig), "worktree list…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        rig.app.read_with(cx, |app, _| {
            assert!(!app.palette_open, "the palette closes")
        });
        assert_eq!(prompt_value(cx, &rig), ("/worktree list ".into(), (0, 15)));
        assert!(prompt_focused(cx, &rig));
    }

    #[gpui_kit::test]
    fn tab_on_a_subcommand_prefills_it(cx: &mut TestAppContext) {
        cx.update(super::bind_keys);
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        highlight_row(cx, &rig, "worktree status…");
        press_tab(cx, &rig);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        rig.app.read_with(cx, |app, _| {
            assert!(!app.palette_open, "the palette closes")
        });
        assert_eq!(prompt_value(cx, &rig).0, "/worktree status ");
        assert!(prompt_focused(cx, &rig));
    }

    #[gpui_kit::test]
    fn subcommand_keeps_the_composer_draft(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        cx.update_window(rig.handle, |_, window, cx| {
            let prompt = rig.app.read(cx).pane.read(cx).prompt.clone();
            prompt.update(cx, |s, cx| s.set_value("feature-x", window, cx));
        })
        .unwrap();
        let target = target_of(&groups(cx, &rig), "worktree create…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty());
        assert_eq!(
            prompt_value(cx, &rig),
            ("/worktree create feature-x".into(), (0, 26))
        );
    }

    /// A subcommand already typed is replaced, not stacked.
    #[gpui_kit::test]
    fn subcommand_replaces_a_typed_subcommand(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(worktree_commands())),
            |_| {},
        );
        cx.update_window(rig.handle, |_, window, cx| {
            let prompt = rig.app.read(cx).pane.read(cx).prompt.clone();
            prompt.update(cx, |s, cx| s.set_value("/worktree status ws-1", window, cx));
        })
        .unwrap();
        let target = target_of(&groups(cx, &rig), "worktree list…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty());
        assert_eq!(
            prompt_value(cx, &rig),
            ("/worktree list ws-1".into(), (0, 15))
        );
    }

    #[gpui_kit::test]
    fn subcommand_during_a_turn(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(running_session(worktree_commands())),
            with_turn_control,
        );
        let all = groups(cx, &rig);
        let rows: Vec<&PaletteEntry> = slash_rows(&all)
            .into_iter()
            .filter(|e| e.label.starts_with("worktree "))
            .collect();
        assert_eq!(rows.len(), 10);
        for row in &rows {
            assert!(row.dim, "{}", row.label);
            assert_eq!(
                row.description.as_deref(),
                Some("Available when the turn ends")
            );
        }
        let target = target_of(&all, "worktree list…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        rig.app.read_with(cx, |app, _| {
            assert!(app.palette_open, "the palette stays open")
        });
        assert_eq!(prompt_value(cx, &rig).0, "", "the composer is unchanged");
    }

    /// A read-only session (archived, handed off) has no composer: it lists
    /// no commands and sends nothing, whatever reaches `run_slash_command`.
    #[gpui_kit::test]
    fn a_read_only_session_offers_and_sends_no_commands(cx: &mut TestAppContext) {
        let mut view = session_with(two_commands());
        view.access = Some(crate::session::SessionAccess {
            reason: None,
            owner_session_id: None,
        });
        let rig = rig(cx, Page::Session, Some(view), |_| {});
        assert!(!group_labels(&groups(cx, &rig)).contains(&"Slash commands"));
        pick(
            cx,
            &rig,
            PaletteTarget::SlashCommand {
                name: "worktree".into(),
                input: CommandInput::None,
                blocked: None,
            },
        );
        assert!(sent(&rig).is_empty(), "no prompt for a read-only session");
    }

    #[gpui_kit::test]
    fn palette_fills_the_composer_for_a_command_with_input(cx: &mut TestAppContext) {
        let rig = rig(
            cx,
            Page::Session,
            Some(session_with(described_commands())),
            |_| {},
        );
        let target = target_of(&groups(cx, &rig), "explain…");
        pick(cx, &rig, target);
        assert!(sent(&rig).is_empty(), "nothing is sent");
        cx.update_window(rig.handle, |_, window, cx| {
            let entity = rig.app.read(cx).pane.read(cx).prompt.clone();
            let state = entity.read(cx);
            assert_eq!(state.value().as_ref(), "/explain ");
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
                app.pane.read(cx).prompt.clone()
            } else {
                app.prompt.clone()
            };
            gpui_kit::Focusable::focus_handle(&prompt, cx).is_focused(window)
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

    /// The session list's key belongs to the quick panel: the main window
    /// shows the same page, with the palette as it was.
    #[gpui_kit::test]
    fn toggle_session_list_changes_nothing_in_the_main_window(cx: &mut TestAppContext) {
        cx.update(super::bind_keys);
        for page in [Page::Start, Page::Session] {
            let rig = rig(cx, page, Some(session_with(two_commands())), |_| {});
            let mac = cx.update(|cx| cx.platform_info().is_mac());
            let key = if mac { "cmd-k" } else { "ctrl-k" };
            cx.update_window(rig.handle, |_, window, cx| window.press(key, cx))
                .unwrap();
            cx.run_until_parked();
            rig.app.read_with(cx, |app, _| {
                assert_eq!(app.page, page);
                assert!(!app.palette_open);
            });
        }
    }

    /// A shortcut's hint is read from the keymap in the platform's notation.
    #[gpui_kit::test]
    fn shortcut_label_shows_the_bound_key_in_the_platforms_notation(cx: &mut TestAppContext) {
        cx.update(super::bind_keys);
        let rig = rig(cx, Page::Start, None, |_| {});
        let mac = cx.update(|cx| cx.platform_info().is_mac());
        let (model, list) = cx
            .update_window(rig.handle, |_, window, _| {
                (
                    crate::ui::kit::shortcut_label(&super::OpenModelPicker, window),
                    crate::ui::kit::shortcut_label(&super::ToggleSessionList, window),
                )
            })
            .unwrap();
        // The notation follows the OS the app is built for, the key the
        // platform it believes it runs on.
        let main = match (cfg!(target_os = "macos"), mac) {
            (true, true) => "⌘",
            (true, false) => "⌃",
            (false, true) => "Win+",
            (false, false) => "Ctrl+",
        };
        assert_eq!(model.as_deref(), Some(format!("{main}M").as_str()));
        assert_eq!(list.as_deref(), Some(format!("{main}K").as_str()));
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
            let view = app.main_view(cx).unwrap();
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
                input: CommandInput::None,
                blocked: None,
            },
        );
        assert_eq!(prompts(&sent(&rig)), ["/web"]);
    }

    // ---- the theme list (design D12)

    use super::PaletteMode;
    use crate::theme::{ThemeId, palette};
    use gpui_kit::{Modifiers, Rgba, VisualTestContext, point, px};

    /// A theme of the catalogue by key.
    fn id(key: &str) -> ThemeId {
        [true, false]
            .into_iter()
            .map(|dark| ThemeId::resolve(key, dark))
            .find(|id| *id.key() == *key)
            .unwrap_or_else(|| panic!("no theme {key}"))
    }

    /// Where the theme list shows `theme`: the dark group first.
    fn row_of(theme: ThemeId) -> (usize, usize) {
        let dark = theme.dark();
        let row = ThemeId::of_appearance(dark)
            .iter()
            .position(|t| *t == theme)
            .expect("listed");
        (usize::from(!dark), row)
    }

    fn render(cx: &mut TestAppContext, rig: &Rig) {
        cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Change preferences and apply them, as the settings page does.
    fn set_prefs(
        cx: &mut TestAppContext,
        rig: &Rig,
        change: impl FnOnce(&mut crate::store::Prefs),
    ) {
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.store.update(cx, |s, cx| s.update_prefs(change, cx));
                app.apply_theme(window, cx);
            });
            window.render_frame(cx);
        })
        .unwrap();
    }

    /// Open the palette and pick "Select theme…".
    fn open_themes(cx: &mut TestAppContext, rig: &Rig) {
        open_palette(cx, rig);
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.run_palette(PaletteTarget::SelectTheme, window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        // The highlight is set after the list's first frame.
        render(cx, rig);
        render(cx, rig);
    }

    /// Move the highlight to `theme`, as the arrow keys do.
    fn highlight(cx: &mut TestAppContext, rig: &Rig, theme: ThemeId) {
        let (section, row) = row_of(theme);
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette.update(cx, |s, cx| {
                    s.set_selected_index(
                        Some(IndexPath::default().section(section).row(row)),
                        window,
                        cx,
                    )
                });
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        render(cx, rig);
    }

    fn shown(cx: &mut TestAppContext) -> ThemeId {
        cx.update(|cx| palette(cx).theme)
    }

    fn pref(cx: &mut TestAppContext, rig: &Rig) -> (Option<bool>, String, String, String) {
        rig.app.read_with(cx, |app, cx| {
            let p = &app.store.read(cx).prefs;
            (
                p.dark,
                p.theme_dark.clone(),
                p.theme_light.clone(),
                p.accent.clone(),
            )
        })
    }

    fn confirm(cx: &mut TestAppContext, rig: &Rig) {
        VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes("enter");
        cx.run_until_parked();
        render(cx, rig);
    }

    use gpui_kit::component::IndexPath;

    #[gpui_kit::test]
    fn select_theme_lists_both_appearances_and_marks_the_shown_one(cx: &mut TestAppContext) {
        let rig = rig(cx, Page::Start, None, |_| {});
        assert!(
            entry_labels(&groups(cx, &rig), "Actions")
                .iter()
                .any(|l| l == "Select theme…")
        );
        set_prefs(cx, &rig, |p| {
            p.dark = Some(true);
            p.theme_dark = "tokyo-night".into();
        });
        open_themes(cx, &rig);
        let all = groups(cx, &rig);
        assert_eq!(group_labels(&all), ["Dark themes", "Light themes"]);
        assert_eq!(entry_labels(&all, "Dark themes").len(), 29);
        assert_eq!(entry_labels(&all, "Light themes").len(), 15);
        let checked: Vec<String> = all
            .iter()
            .flat_map(|g| &g.entries)
            .filter(|e| e.checked)
            .map(|e| e.label.to_string())
            .collect();
        assert_eq!(checked, ["Tokyo Night"]);
        let selected = rig
            .app
            .read_with(cx, |app, cx| app.palette.read(cx).selected_index())
            .expect("a row is highlighted");
        assert_eq!(
            (selected.section, selected.row),
            row_of(id("tokyo-night")),
            "Tokyo Night is highlighted"
        );
    }

    #[gpui_kit::test]
    fn moving_the_highlight_previews_without_saving(cx: &mut TestAppContext) {
        let rig = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig, |p| {
            p.dark = Some(true);
            p.theme_dark = "tokyo-night".into();
        });
        let before = pref(cx, &rig);
        open_themes(cx, &rig);
        assert_eq!(shown(cx), id("tokyo-night"));
        highlight(cx, &rig, id("catppuccin-mocha"));
        assert_eq!(shown(cx), id("catppuccin-mocha"), "previewed");
        assert_eq!(pref(cx, &rig), before, "nothing saved");
    }

    #[gpui_kit::test]
    fn confirming_a_theme_of_the_shown_appearance_keeps_the_mode(cx: &mut TestAppContext) {
        // Dark mode, a dark theme.
        let rig_dark = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig_dark, |p| p.dark = Some(true));
        open_themes(cx, &rig_dark);
        highlight(cx, &rig_dark, id("one-dark"));
        confirm(cx, &rig_dark);
        let (mode, dark, light, _) = pref(cx, &rig_dark);
        assert_eq!(
            (mode, dark.as_str(), light.as_str()),
            (Some(true), "one-dark", "")
        );
        assert_eq!(shown(cx), id("one-dark"));
        rig_dark.app.read_with(cx, |app, _| {
            assert!(!app.palette_open);
            assert_eq!(app.palette_mode, PaletteMode::Commands);
        });

        // System mode: the theme of the desktop's appearance. The test
        // platform's desktop appearance is whatever the window reports.
        let rig_system = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig_system, |p| p.dark = None);
        let desktop_dark = cx
            .update_window(rig_system.handle, |_, window, _| {
                matches!(
                    window.appearance(),
                    gpui_kit::WindowAppearance::Dark | gpui_kit::WindowAppearance::VibrantDark
                )
            })
            .unwrap();
        let key = if desktop_dark {
            "one-dark"
        } else {
            "one-light"
        };
        open_themes(cx, &rig_system);
        highlight(cx, &rig_system, id(key));
        confirm(cx, &rig_system);
        let (mode, dark, light, _) = pref(cx, &rig_system);
        assert_eq!(mode, None, "System stays System");
        // Saved as the theme of its appearance, and only that one.
        let (saved, other) = if desktop_dark {
            (dark, light)
        } else {
            (light, dark)
        };
        assert_eq!((saved.as_str(), other.as_str()), (key, ""));
        assert_eq!(shown(cx), id(key));
    }

    #[gpui_kit::test]
    fn confirming_a_theme_of_the_other_appearance_switches_the_mode(cx: &mut TestAppContext) {
        let rig = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig, |p| {
            p.dark = Some(true);
            p.theme_dark = "tokyo-night".into();
        });
        open_themes(cx, &rig);
        highlight(cx, &rig, id("solarized-light"));
        confirm(cx, &rig);
        let (mode, dark, light, _) = pref(cx, &rig);
        assert_eq!(mode, Some(false), "the mode follows the theme");
        assert_eq!(light, "solarized-light");
        assert_eq!(dark, "tokyo-night", "the dark theme is untouched");
        assert_eq!(shown(cx), id("solarized-light"));
    }

    #[gpui_kit::test]
    fn closing_without_confirming_restores_the_theme(cx: &mut TestAppContext) {
        for closer in ["escape", "toggle", "scrim"] {
            let rig = rig(cx, Page::Start, None, |_| {});
            set_prefs(cx, &rig, |p| {
                p.dark = Some(true);
                p.theme_dark = "tokyo-night".into();
            });
            let before = pref(cx, &rig);
            open_themes(cx, &rig);
            highlight(cx, &rig, id("ayu-mirage"));
            assert_eq!(shown(cx), id("ayu-mirage"), "{closer}: previewed");
            match closer {
                "escape" => {
                    VisualTestContext::from_window(rig.handle, cx).simulate_keystrokes("escape")
                }
                "toggle" => cx
                    .update_window(rig.handle, |_, window, cx| {
                        rig.app.update(cx, |app, cx| app.toggle_palette(window, cx))
                    })
                    .unwrap(),
                _ => VisualTestContext::from_window(rig.handle, cx)
                    .simulate_click(point(px(8.), px(300.)), Modifiers::none()),
            }
            cx.run_until_parked();
            render(cx, &rig);
            assert_eq!(shown(cx), id("tokyo-night"), "{closer}: restored");
            assert_eq!(pref(cx, &rig), before, "{closer}: nothing saved");
            rig.app.read_with(cx, |app, _| {
                assert!(!app.palette_open, "{closer}: closed");
                assert_eq!(app.palette_mode, PaletteMode::Commands, "{closer}");
            });
        }
    }

    #[gpui_kit::test]
    fn typing_filters_the_theme_list(cx: &mut TestAppContext) {
        let rig = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig, |p| {
            p.dark = Some(true);
            p.theme_dark = "tokyo-night".into();
        });
        open_themes(cx, &rig);
        cx.update_window(rig.handle, |_, window, cx| {
            rig.app.update(cx, |app, cx| {
                app.palette
                    .update(cx, |s, cx| s.set_query("gruv", window, cx))
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        render(cx, &rig);
        let found = rig.app.read_with(cx, |app, cx| {
            let query = app.palette.read(cx).query(cx).to_string();
            filter_palette(app.palette_groups(cx), &query)
        });
        let labels: Vec<String> = found
            .iter()
            .flat_map(|g| &g.entries)
            .map(|e| e.label.to_string())
            .collect();
        assert_eq!(labels.len(), 5, "{labels:?}");
        assert!(
            labels.iter().all(|l| l.starts_with("Gruvbox")),
            "{labels:?}"
        );
        // The highlight moved to a Gruvbox row, and that theme is shown.
        assert!(
            shown(cx).label().starts_with("Gruvbox"),
            "{}",
            shown(cx).label()
        );
    }

    #[gpui_kit::test]
    fn the_accent_choice_is_kept(cx: &mut TestAppContext) {
        let rig = rig(cx, Page::Start, None, |_| {});
        set_prefs(cx, &rig, |p| {
            p.dark = Some(true);
            p.accent = "blue".into();
        });
        let blue: gpui_kit::Hsla = gpui_kit::rgb(0x60a5fa).into();
        let is_blue = |cx: &mut TestAppContext| {
            let (a, b) = (
                Rgba::from(cx.update(|cx| palette(cx).accent)),
                Rgba::from(blue),
            );
            (a.r - b.r).abs() < 2e-3 && (a.g - b.g).abs() < 2e-3 && (a.b - b.b).abs() < 2e-3
        };
        assert!(is_blue(cx), "blue before");
        open_themes(cx, &rig);
        highlight(cx, &rig, id("gruvbox-dark"));
        assert_eq!(shown(cx), id("gruvbox-dark"));
        assert!(is_blue(cx), "blue while previewing");
        confirm(cx, &rig);
        assert!(is_blue(cx), "blue after confirming");
        assert_eq!(pref(cx, &rig).3, "blue");
    }
}
