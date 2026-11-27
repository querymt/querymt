//! `SessionPane`: one session shown in a surface (design D6). It owns what the
//! surface remembers about that session: the strong handle that keeps it
//! open, the back stack of delegated sessions, the transcript list and the
//! composer. Two panes that show the same session share its
//! [`OpenSession`](crate::session::open::OpenSession) and nothing else.
//!
//! What belongs to the app (dialogs, the image viewer, other pages) leaves as
//! a [`SessionPaneEvent`].

use std::collections::HashSet;

use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::inbox::RequestCards;
use super::inbox::RequestCardsEvent;
use super::preview::{PreviewImage, chat_gallery};
use super::session_view::{Density, Transcript};
use super::smooth_scroll::SmoothScroll;
use super::start::AttachmentHost;
use super::turn_diff::TurnDiff;
use super::{Page, QmtApp, kit};
use crate::session::SessionView;
use crate::session::open::{Delivery, OpenSession, SessionKey, SessionRef};
use crate::store::worktree::WorktreeAction;
use crate::store::{AgentsStore, Attachment, StoreEvent};
use crate::theme::Palette;
use crate::usage::TokenKey;

/// What a pane asks of the surface that hosts it.
#[derive(Clone)]
pub(crate) enum SessionPaneEvent {
    /// Back with nothing left on the back stack: show the session list.
    Leave,
    /// Ask before forking from the turn closing at item `end`.
    Fork { end: usize },
    /// Show images in the lightbox.
    OpenImage {
        images: Vec<PreviewImage>,
        index: usize,
    },
    /// Ask before rolling the workspace back to this user message.
    ConfirmUndo(String),
    /// Open the worktree dialog for the session.
    Worktree(WorktreeAction),
    /// Open the debug events of a session.
    DebugEvents { agent: String, session: String },
    /// Open the delegate routing dialog.
    Delegates,
    /// Open the model picker.
    ModelPicker,
    /// Start a session in `cwd` on `agent`.
    NewSessionIn { cwd: String, agent: String },
    /// Open the connector settings.
    ConnectorSettings,
    /// Continue the session in the main window (the quick panel's pane).
    OpenInMain,
}

pub(crate) struct SessionPane {
    pub(super) store: Entity<AgentsStore>,
    /// How much the pane shows: the main window is comfortable, the quick
    /// panel compact. Fixed for the pane's life.
    pub(super) density: Density,
    /// The pane sits on a translucent material (the quick panel on a desktop
    /// that has one): it paints no veil of its own over the transcript.
    pub(super) glass: bool,
    /// Colours of this pane in place of the app's, for a host with a look of
    /// its own (the quick panel's HUD). `None` uses the global palette.
    pub(super) palette: Option<Palette>,
    /// Lines the empty composer is tall (see `set_composer_bar`).
    rest_lines: usize,
    /// The session shown. The strong handle is what keeps it open.
    pub(super) session: Option<Entity<OpenSession>>,
    /// Delegated sessions left to return to, nearest last.
    pub(super) back: Vec<SessionRef>,
    pub(super) prompt: Entity<TextareaState>,
    pub(super) transcript: Transcript,
    /// The find bar over the transcript, while it is open.
    pub(super) find: Option<super::find::FindBar>,
    pub(super) attachments: Vec<Attachment>,
    pub(super) attach_error: Option<String>,
    pub(super) options_open: bool,
    pub(super) details_open: bool,
    pub(super) menu_open: bool,
    pub(super) plan_open: bool,
    pub(super) waiting_open: bool,
    pub(super) usage_totals: bool,
    pub(super) usage_pinned: Option<TokenKey>,
    pub(super) usage_hovered: Option<TokenKey>,
    pub(super) delivery: Delivery,
    pub(super) copied: Option<String>,
    pub(super) expanded: HashSet<String>,
    pub(super) smooth_scroll: SmoothScroll,
    pub(super) dismissed: kit::Dismissed,
    pub(super) turn_diff: TurnDiff,
    pub(super) cards: Entity<RequestCards>,
    /// Callback / code input of the connector sign-in shown above the composer.
    pub(super) oauth_code: Entity<InputState>,
    /// Width the host keeps beside the pane (the sidebar), for the turn diff.
    pub(super) reserved_w: Pixels,
    /// Highlighted row of the composer's slash command list.
    pub(super) slash_cursor: usize,
    /// Composer text at which Esc closed the slash command list; the list
    /// stays closed until the text changes.
    pub(super) slash_dismissed: Option<String>,
    pub(super) slash_scroll: ScrollHandle,
    _session: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<SessionPaneEvent> for SessionPane {}

impl SessionPane {
    pub(super) fn new(
        store: Entity<AgentsStore>,
        smooth_scroll: SmoothScroll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_density(store, smooth_scroll, Density::Comfortable, window, cx)
    }

    /// A pane at `density` (the quick panel passes [`Density::Compact`]).
    pub(crate) fn with_density(
        store: Entity<AgentsStore>,
        smooth_scroll: SmoothScroll,
        density: Density,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (min_lines, max_lines) = density.composer_lines();
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(min_lines, max_lines)
                .placeholder("Reply to the agent…")
        });
        let cards = cx.new(|cx| RequestCards::new(store.clone(), cx));
        let oauth_code =
            cx.new(|cx| InputState::new(window, cx).placeholder("Callback URL or code"));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&cards, |_, _, cx| cx.notify()),
            cx.subscribe_in(&prompt, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    // A new query starts at the best match, and a
                    // dismissal (Esc) ends with the first edit.
                    this.slash_cursor = 0;
                    this.slash_dismissed = None;
                    cx.notify();
                }
            }),
            cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                this.on_store_event(event, cx)
            }),
        ];
        Self {
            store,
            density,
            glass: false,
            palette: None,
            rest_lines: min_lines,
            session: None,
            back: Vec::new(),
            prompt,
            transcript: Transcript::new(),
            find: None,
            attachments: Vec::new(),
            attach_error: None,
            options_open: false,
            details_open: false,
            menu_open: false,
            plan_open: false,
            waiting_open: false,
            usage_totals: false,
            usage_pinned: None,
            usage_hovered: None,
            delivery: Delivery::Steer,
            copied: None,
            expanded: HashSet::new(),
            smooth_scroll,
            dismissed: kit::Dismissed::default(),
            turn_diff: TurnDiff::default(),
            cards,
            oauth_code,
            reserved_w: px(0.),
            slash_cursor: 0,
            slash_dismissed: None,
            slash_scroll: ScrollHandle::new(),
            _session: None,
            _subs: subs,
        }
    }

    /// The session shown, if any.
    pub(crate) fn session(&self) -> Option<&Entity<OpenSession>> {
        self.session.as_ref()
    }

    /// There is a delegating session to return to.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// The composer's input, for hosts that draw it themselves.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn prompt_input(&self) -> &Entity<TextareaState> {
        &self.prompt
    }

    /// The files attached to the draft, which the caller now owns.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn take_attachments(&mut self) -> Vec<Attachment> {
        self.attach_error = None;
        std::mem::take(&mut self.attachments)
    }

    /// Text in the composer.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn draft(&self, cx: &App) -> String {
        self.prompt.read(cx).value().to_string()
    }

    /// Replace the composer's text.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn set_draft(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = SharedString::from(text.to_owned());
        self.prompt
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    /// Height of the transcript's viewport and how much taller than it the
    /// content is (0 when it fits), as of the last layout. The quick panel
    /// grows by the overflow (design D13).
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn content_metrics(&self) -> (f32, f32) {
        let list = &self.transcript.list;
        (
            f32::from(list.viewport_bounds().size.height),
            f32::from(list.max_offset_for_scrollbar().y).max(0.),
        )
    }

    /// Scroll the transcript to `item_ix`, with no following of the tail, so
    /// a test can tell that the position was kept.
    #[cfg(test)]
    pub(crate) fn scroll_to_item_for_test(&mut self, item_ix: usize) {
        let list = &self.transcript.list;
        list.set_follow_mode(gpui_kit::FollowMode::Normal);
        list.scroll_to(gpui_kit::ListOffset {
            item_ix,
            offset_in_item: gpui_kit::px(0.),
        });
    }

    /// The composer has the keyboard focus.
    #[cfg(test)]
    pub(crate) fn composer_focused_for_test(&self, window: &Window, cx: &App) -> bool {
        gpui_kit::Focusable::focus_handle(&self.prompt, cx).is_focused(window)
    }

    /// Where the transcript is scrolled to.
    #[cfg(test)]
    pub(crate) fn scroll_top_for_test(&self) -> gpui_kit::ListOffset {
        self.transcript.list.logical_scroll_top()
    }

    /// The pane sits on a translucent material.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn set_glass(&mut self, glass: bool, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.glass, glass) != glass {
            cx.notify();
        }
    }

    /// Paint the pane, its markdown and its diffs with `palette` instead of
    /// the app's; `None` goes back to the app's.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn set_palette(&mut self, palette: Option<Palette>, cx: &mut Context<Self>) {
        if self.palette != palette {
            self.palette = palette;
            // Rows are cached by signature; the colours are part of it.
            cx.notify();
        }
    }

    /// A host that shows the composer's input alone, as a bar, asks for one
    /// line at rest (`bar`); otherwise it rests at the density's lines. Either
    /// way it grows to the density's limit.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn set_composer_bar(&mut self, bar: bool, cx: &mut Context<Self>) {
        let lines = if bar {
            1
        } else {
            self.density.composer_lines().0
        };
        if std::mem::replace(&mut self.rest_lines, lines) != lines {
            let max = self.density.composer_lines().1;
            self.prompt
                .update(cx, |input, cx| input.set_auto_grow(lines, max, cx));
        }
    }

    /// The colours the pane paints with: its own, else the app's.
    pub(super) fn palette(&self, cx: &App) -> Palette {
        self.palette.unwrap_or_else(|| *crate::theme::palette(cx))
    }

    /// Put the keyboard focus in the composer.
    pub(crate) fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The composer is shown: a new chat, or a session that takes input. A
    /// read-only session (archived, handed off) has none to focus.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn takes_input(&self, cx: &App) -> bool {
        self.view(cx).is_none_or(|v| v.access.is_none())
    }

    /// Hand focus back to the composer after one of the pane's popovers
    /// closes, when the shown session takes input (not read-only).
    pub(super) fn restore_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view(cx).is_some_and(|v| v.access.is_none()) {
            self.focus_composer(window, cx);
        }
    }

    /// The transcript state of the session shown.
    pub(crate) fn view<'a>(&'a self, cx: &'a App) -> Option<&'a SessionView> {
        self.session.as_ref().map(|session| &session.read(cx).view)
    }

    /// Key of the session shown.
    pub(crate) fn key(&self, cx: &App) -> Option<SessionKey> {
        self.session
            .as_ref()
            .map(|session| session.read(cx).key.clone())
    }

    /// Show `session`, keeping the back stack.
    pub(crate) fn show(&mut self, session: Entity<OpenSession>, cx: &mut Context<Self>) {
        self.close_panels();
        self.close_find(cx);
        self._session = Some(cx.observe(&session, |this, _, cx| {
            this.sync_find(cx);
            cx.notify()
        }));
        self.session = Some(session);
        cx.notify();
    }

    /// Show nothing; the session closes when no other surface holds it.
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.close_panels();
        self.close_find(cx);
        self._session = None;
        self.session = None;
        self.back.clear();
        cx.notify();
    }

    /// Show the session `target` as a fresh start: the back stack is dropped.
    pub(crate) fn open(&mut self, target: SessionRef, cx: &mut Context<Self>) {
        let session = self.store.update(cx, |s, cx| s.open(target, cx));
        self.back.clear();
        self.show(session, cx);
    }

    /// Open delegated session `child` of the session shown; Back returns to
    /// the current one. Uses the child's own title and workspace when the
    /// session list knows them.
    pub(super) fn open_child(&mut self, agent: &str, child: &str, cx: &mut Context<Self>) {
        let Some(current) = self.current_ref(cx) else {
            return;
        };
        let (cwd, title) = match self.store.read(cx).session_summary(agent, child) {
            Some(s) if !s.cwd.is_empty() => (s.cwd.clone(), s.title.clone()),
            Some(s) => (current.cwd.clone(), s.title.clone()),
            None => (current.cwd.clone(), String::new()),
        };
        let target = SessionRef {
            key: SessionKey::new(agent, child),
            cwd,
            title,
        };
        let session = self.store.update(cx, |s, cx| s.open(target, cx));
        self.back.push(current);
        self.show(session, cx);
    }

    /// Back: to the session this one was opened from, else out of the pane
    /// (the host decides where).
    pub(crate) fn go_back(&mut self, cx: &mut Context<Self>) {
        let Some(mut parent) = self.back.pop() else {
            cx.emit(SessionPaneEvent::Leave);
            return;
        };
        // Prefer the list's current title; the stored one may be stale.
        if let Some(title) = self
            .store
            .read(cx)
            .session_summary(parent.key.agent.as_str(), parent.key.session.as_str())
            .map(|s| s.title.clone())
            .filter(|t| !t.trim().is_empty())
        {
            parent.title = title;
        }
        let session = self.store.update(cx, |s, cx| s.open(parent, cx));
        self.show(session, cx);
    }

    /// What it takes to open the session shown again.
    pub(crate) fn current_ref(&self, cx: &App) -> Option<SessionRef> {
        let session = self.session.as_ref()?.read(cx);
        Some(SessionRef {
            key: session.key.clone(),
            cwd: session.view.cwd.clone(),
            title: session.view.title.clone(),
        })
    }

    /// The pane shows a session another surface also holds: reload it.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.reload(cx));
        }
    }

    /// Close the popover of the pane that is open; true when one was.
    pub(super) fn close_popover(&mut self) -> bool {
        std::mem::take(&mut self.options_open)
            || std::mem::take(&mut self.waiting_open)
            || std::mem::take(&mut self.details_open)
            || std::mem::take(&mut self.menu_open)
    }

    /// Popovers belong to the session that opened them.
    pub(super) fn close_panels(&mut self) {
        self.options_open = false;
        self.details_open = false;
        self.menu_open = false;
    }

    fn on_store_event(&mut self, event: &StoreEvent, cx: &mut Context<Self>) {
        let here = self.key(cx);
        match event {
            StoreEvent::Spawned { from, child } if here.as_ref() == Some(from) => {
                self.open(child.clone(), cx)
            }
            StoreEvent::Deleted(key) => {
                self.back.retain(|r| &r.key != key);
                if here.as_ref() == Some(key) {
                    self.clear(cx);
                }
            }
            StoreEvent::Spawned { .. } => {}
        }
    }

    // ---- what the session's controls do -------------------------------------

    /// Fetch the turns preceding the loaded transcript.
    pub(super) fn load_older(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.load_older(cx));
        }
    }

    /// Clear a failed older-history fetch and try again.
    pub(super) fn retry_older(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.retry_older(cx));
        }
    }

    /// Fetch the complete `part` of tool call `id`, which a windowed load shortened.
    pub(super) fn load_tool_content(
        &mut self,
        id: &str,
        part: &'static str,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.load_tool_content(id, part, cx));
        }
    }

    /// `session/cancel` for the running turn.
    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.cancel(cx));
        }
    }

    /// Remove a queued message before it starts.
    pub(super) fn discard_queued(&mut self, input_id: &str, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |o, cx| o.discard_queued(input_id, cx));
        }
    }

    /// Revert the workspace to before the user turn `message_id`.
    pub(super) fn undo_to(&mut self, message_id: String, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            self.store
                .update(cx, |s, cx| s.undo_workspace_to(&session, message_id, cx));
        }
    }

    /// Re-apply the most recently undone turn.
    pub(super) fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            self.store
                .update(cx, |s, cx| s.redo_workspace(&session, cx));
        }
    }

    /// Change a config option of the session.
    pub(super) fn set_option(&mut self, config_id: &str, value: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            self.store.update(cx, |s, cx| {
                s.set_session_option(&session, config_id, value, cx)
            });
        }
    }

    /// Switch the session shown to its next reasoning effort (Cmd/Ctrl+T in
    /// the quick panel). Nothing happens while the agent offers none or a
    /// change is still pending.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn cycle_reasoning(&mut self, cx: &mut Context<Self>) {
        let Some(option) = self.view(cx).and_then(|v| {
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
        self.set_option(&option.id, &value, cx);
    }

    /// Open attachment `ix` of transcript item `key` in the lightbox.
    pub(super) fn open_chat_image(&mut self, key: &str, ix: usize, cx: &mut Context<Self>) {
        let Some(view) = self.view(cx) else {
            return;
        };
        let (images, index) = chat_gallery(view, key, ix);
        if !images.is_empty() {
            cx.emit(SessionPaneEvent::OpenImage { images, index });
        }
    }
}

impl AttachmentHost for SessionPane {
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn attachments_mut(&mut self) -> &mut Vec<Attachment> {
        &mut self.attachments
    }

    fn set_attach_error(&mut self, error: Option<String>) {
        self.attach_error = error;
    }

    fn open_attachment_image(&mut self, id: &str, _: &mut Window, cx: &mut Context<Self>) {
        let (images, index) = super::preview::composer_gallery(&self.attachments, id);
        if !images.is_empty() {
            cx.emit(SessionPaneEvent::OpenImage { images, index });
        }
    }
}

impl QmtApp {
    /// The session the main window shows, read.
    pub(crate) fn main_view<'a>(&'a self, cx: &'a App) -> Option<&'a SessionView> {
        self.pane.read(cx).view(cx)
    }

    /// The session entity the main window shows.
    #[cfg(test)]
    pub(crate) fn main_session_for_test(
        &self,
        cx: &App,
    ) -> Option<Entity<crate::session::open::OpenSession>> {
        self.pane.read(cx).session().cloned()
    }

    /// Key of the session the main window shows.
    pub(crate) fn main_key(&self, cx: &App) -> Option<SessionKey> {
        self.pane.read(cx).key(cx)
    }

    /// Show `target` in the main window's pane.
    pub(super) fn open_ref(&mut self, target: SessionRef, cx: &mut Context<Self>) {
        self.pane.update(cx, |pane, cx| pane.open(target, cx));
        self.page = Page::Session;
        cx.notify();
    }

    /// Show `target` in the main window's pane, as the quick panel's "open in
    /// main window" does. The session is the one the panel holds, so it is
    /// not loaded again.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn show_session(&mut self, target: SessionRef, cx: &mut Context<Self>) {
        self.open_ref(target, cx);
    }

    /// Do what the main window does when its pane asks for `event`; the quick
    /// panel hands such requests over after showing its session here.
    #[cfg_attr(target_family = "wasm", allow(dead_code))] // used by the desktop-only quick panel
    pub(crate) fn apply_pane_event(
        &mut self,
        event: SessionPaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.pane.clone();
        self.on_pane_event(&pane, &event, window, cx);
    }

    pub(super) fn open_session(
        &mut self,
        agent: &str,
        session_id: &str,
        cwd: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        self.open_ref(
            SessionRef {
                key: SessionKey::new(agent, session_id),
                cwd: cwd.to_owned(),
                title: title.to_owned(),
            },
            cx,
        );
    }

    pub(super) fn open_summary(
        &mut self,
        s: &crate::store::SessionSummary,
        cx: &mut Context<Self>,
    ) {
        let title = s.display_title();
        self.open_session(&s.agent_id, &s.id, &s.cwd, &title, cx);
    }

    pub(super) fn on_pane_event(
        &mut self,
        _: &Entity<SessionPane>,
        event: &SessionPaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.clone() {
            SessionPaneEvent::Leave => self.go(Page::Sessions, cx),
            SessionPaneEvent::Fork { end } => self.request_fork(end, cx),
            SessionPaneEvent::OpenImage { images, index } => {
                self.open_preview(images, index, window, cx)
            }
            SessionPaneEvent::ConfirmUndo(message_id) => {
                self.confirm_undo = Some(message_id);
                cx.notify();
            }
            SessionPaneEvent::Worktree(action) => self.open_worktree_dialog(action, window, cx),
            SessionPaneEvent::DebugEvents { agent, session } => {
                self.open_debug_events(&agent, Some(session), window, cx)
            }
            SessionPaneEvent::Delegates => self.open_delegate_dialog(window, cx),
            SessionPaneEvent::ModelPicker => self.open_model_picker(window, cx),
            SessionPaneEvent::NewSessionIn { cwd, agent } => {
                self.new_session_in(cwd, agent, window, cx)
            }
            SessionPaneEvent::ConnectorSettings => {
                self.settings_tab = super::settings::SettingsTab::Connectors;
                self.go(Page::Settings, cx);
            }
            // Only the quick panel's pane asks for it.
            SessionPaneEvent::OpenInMain => {}
        }
    }

    pub(super) fn on_cards_event(
        &mut self,
        _: &Entity<super::inbox::RequestCards>,
        event: &RequestCardsEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RequestCardsEvent::OpenSession(target) => self.open_ref(target.clone(), cx),
        }
    }

    /// The Session page: the pane, or what the window is waiting for.
    pub(super) fn render_session_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.pane.read(cx).session().is_some() {
            let reserved = self.sidebar_width(cx);
            self.pane.update(cx, |pane, _| pane.reserved_w = reserved);
            return self.pane.clone().into_any_element();
        }
        let p = *crate::theme::palette(cx);
        let busy = self.launch.busy;
        let err = self.launch.error.clone();
        let progress = self.launch.label();
        gpui_kit::component::v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .text_color(p.muted)
            .when(busy, |el| {
                el.child(kit::spinner("starting", 22., p.accent))
                    .child(progress)
            })
            .when(!busy, |el| {
                el.child(err.unwrap_or_else(|| "No session open.".into()))
                    .child(
                        kit::button("back-start")
                            .label("Back to Start")
                            .on_click(cx.listener(|this, _, _, cx| this.go(Page::Start, cx))),
                    )
            })
            .into_any_element()
    }
}

impl QmtApp {
    /// Show a check on the control that copied `key` for a moment.
    pub(super) fn flash_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied = Some(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1200))
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
}

#[cfg(test)]
impl QmtApp {
    /// Change the transcript state of the main pane's session.
    pub(super) fn edit_main_for_test<R>(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut SessionView) -> R,
    ) -> R {
        let session = self
            .pane
            .read(cx)
            .session()
            .cloned()
            .expect("a session is open");
        session.update(cx, |o, _| edit(&mut o.view))
    }
}

/// A window whose root is a pane showing `view` of `store`.
#[cfg(test)]
pub(super) fn open_pane_for_test(
    cx: &mut TestAppContext,
    store: AgentsStore,
    view: SessionView,
) -> (AnyWindowHandle, Entity<SessionPane>) {
    open_pane_with_density_for_test(cx, store, view, Density::Comfortable)
}

/// [`open_pane_for_test`] at `density`.
#[cfg(test)]
pub(super) fn open_pane_with_density_for_test(
    cx: &mut TestAppContext,
    store: AgentsStore,
    view: SessionView,
    density: Density,
) -> (AnyWindowHandle, Entity<SessionPane>) {
    cx.update(|cx| {
        let store = cx.new(|_| store);
        let session = store.update(cx, |s, cx| s.adopt_for_test(view, cx));
        let (handle, pane) = gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| {
                SessionPane::with_density(store, SmoothScroll::default(), density, window, cx)
            })
        })
        .unwrap();
        pane.update(cx, |pane, cx| pane.show(session, cx));
        (handle, pane)
    })
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, EntityId, TestAppContext};
    use serde_json::json;

    use super::{
        AgentsStore, SessionKey, SessionPane, SessionRef, SessionView, SmoothScroll,
        open_pane_for_test,
    };
    use crate::ui::inbox::{RequestCards, RequestFilter};

    struct Rig {
        window: AnyWindowHandle,
        main: Entity<SessionPane>,
        store: Entity<AgentsStore>,
    }

    fn rig(cx: &mut TestAppContext) -> Rig {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(true, "orange", cx);
        });
        let mut store = crate::store::automations::schedule_ui_fixture();
        let (_, requests) = store.rpc_for_test("test-agent");
        // Nothing answers the loads: the sessions stay loading, which is
        // all these tests need.
        std::mem::forget(requests);
        let (window, main) = open_pane_for_test(
            cx,
            store,
            SessionView::new("test-agent", "p", "/w", "Parent"),
        );
        let store = main.read_with(cx, |pane, _| pane.store.clone());
        Rig {
            window,
            main,
            store,
        }
    }

    /// A second pane, as the quick panel has one, on the same store.
    fn second_pane(cx: &mut TestAppContext, rig: &Rig) -> Entity<SessionPane> {
        let store = rig.store.clone();
        cx.update_window(rig.window, |_, window, cx| {
            cx.new(|cx| SessionPane::new(store, SmoothScroll::default(), window, cx))
        })
        .unwrap()
    }

    fn shown(cx: &mut TestAppContext, pane: &Entity<SessionPane>) -> Option<String> {
        pane.read_with(cx, |pane, cx| pane.key(cx).map(|k| k.session.to_string()))
    }

    fn back(cx: &mut TestAppContext, pane: &Entity<SessionPane>) -> Vec<String> {
        pane.read_with(cx, |pane, _| {
            pane.back
                .iter()
                .map(|r| r.key.session.to_string())
                .collect()
        })
    }

    fn entity_of(cx: &mut TestAppContext, pane: &Entity<SessionPane>) -> Option<EntityId> {
        pane.read_with(cx, |p, _| p.session().map(|s| s.entity_id()))
    }

    fn child(cx: &mut TestAppContext, pane: &Entity<SessionPane>, id: &str) {
        cx.update(|cx| pane.update(cx, |p, cx| p.open_child("test-agent", id, cx)));
    }

    fn go_back(cx: &mut TestAppContext, pane: &Entity<SessionPane>) {
        cx.update(|cx| pane.update(cx, |p, cx| p.go_back(cx)));
    }

    fn open_parent(cx: &mut TestAppContext, pane: &Entity<SessionPane>) {
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.open(
                    SessionRef {
                        key: SessionKey::new("test-agent", "p"),
                        cwd: "/w".into(),
                        title: "Parent".into(),
                    },
                    cx,
                )
            })
        });
    }

    #[gpui_kit::test]
    fn child_and_back_in_one_pane(cx: &mut TestAppContext) {
        let rig = rig(cx);
        child(cx, &rig.main, "c");
        // A grandchild: Back walks up the chain one step at a time.
        child(cx, &rig.main, "g");
        let at = (shown(cx, &rig.main), back(cx, &rig.main));
        assert_eq!(at, (Some("g".into()), vec!["p".to_string(), "c".into()]));
        go_back(cx, &rig.main);
        assert_eq!(shown(cx, &rig.main).as_deref(), Some("c"));
        go_back(cx, &rig.main);
        let at = (shown(cx, &rig.main), back(cx, &rig.main));
        assert_eq!(at, (Some("p".into()), Vec::new()));
    }

    #[gpui_kit::test]
    fn child_in_main_does_not_move_quick(cx: &mut TestAppContext) {
        let rig = rig(cx);
        let quick = second_pane(cx, &rig);
        open_parent(cx, &quick);
        // The same open session in both panes.
        assert_eq!(entity_of(cx, &quick), entity_of(cx, &rig.main));
        let parent = entity_of(cx, &rig.main);

        child(cx, &rig.main, "c");
        assert_eq!(shown(cx, &rig.main).as_deref(), Some("c"));
        let quick_at = (shown(cx, &quick), back(cx, &quick));
        assert_eq!(quick_at, (Some("p".into()), Vec::new()));

        // Going back in the main pane finds the parent still open: no new load.
        go_back(cx, &rig.main);
        assert_eq!(shown(cx, &rig.main).as_deref(), Some("p"));
        assert_eq!(entity_of(cx, &rig.main), parent);
    }

    #[gpui_kit::test]
    fn reload_keeps_back_stack(cx: &mut TestAppContext) {
        let rig = rig(cx);
        child(cx, &rig.main, "c");
        cx.update(|cx| rig.main.update(cx, |p, cx| p.reload(cx)));
        let at = (shown(cx, &rig.main), back(cx, &rig.main));
        assert_eq!(at, (Some("c".into()), vec!["p".to_string()]));
        go_back(cx, &rig.main);
        assert_eq!(shown(cx, &rig.main).as_deref(), Some("p"));
    }

    fn ask(cx: &mut TestAppContext, store: &Entity<AgentsStore>, id: u64, session: &str) {
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.request_for_test(
                    "test-agent",
                    id,
                    "session/request_permission",
                    json!({
                        "sessionId": session,
                        "toolCall": { "toolCallId": format!("t{id}"), "title": "Run ls", "kind": "execute" },
                        "options": [
                            { "optionId": "o", "name": "Allow", "kind": "allow_once" },
                            { "optionId": "r", "name": "Reject", "kind": "reject_once" }
                        ],
                    }),
                    cx,
                )
            })
        });
    }

    fn listed(
        cx: &mut TestAppContext,
        cards: &Entity<RequestCards>,
        filter: RequestFilter,
    ) -> usize {
        cards.read_with(cx, |cards, cx| cards.keys(&filter, cx).len())
    }

    #[gpui_kit::test]
    fn pane_shows_only_its_sessions_requests(cx: &mut TestAppContext) {
        let rig = rig(cx);
        ask(cx, &rig.store, 1, "p");
        ask(cx, &rig.store, 2, "other");
        ask(cx, &rig.store, 3, "p");
        let cards = rig.main.read_with(cx, |p, _| p.cards.clone());
        let mine = RequestFilter::Session(SessionKey::new("test-agent", "p"));
        let theirs = RequestFilter::Session(SessionKey::new("test-agent", "other"));
        assert_eq!(listed(cx, &cards, mine), 2);
        assert_eq!(listed(cx, &cards, theirs), 1);
        // The Inbox lists every one of them.
        assert_eq!(listed(cx, &cards, RequestFilter::All), 3);
    }

    #[gpui_kit::test]
    fn answer_in_pane_resolves_inbox(cx: &mut TestAppContext) {
        let rig = rig(cx);
        ask(cx, &rig.store, 1, "p");
        assert_eq!(rig.store.read_with(cx, |s, _| s.pending_request_count()), 1);
        let cards = rig.main.read_with(cx, |p, _| p.cards.clone());
        let mine = RequestFilter::Session(SessionKey::new("test-agent", "p"));
        let key = cards.read_with(cx, |c, cx| c.keys(&mine, cx).remove(0));
        // The pane's own card answers it.
        cx.update(|cx| cards.update(cx, |c, cx| c.request_action(&key, "o", cx)));
        cx.run_until_parked();
        // It leaves the Inbox's pending count and the pane's list.
        assert_eq!(rig.store.read_with(cx, |s, _| s.pending_request_count()), 0);
        assert_eq!(listed(cx, &cards, mine), 0);
    }
}
