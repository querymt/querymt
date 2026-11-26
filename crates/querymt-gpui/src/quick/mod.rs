//! The quick panel: a floating conversation window summoned from anywhere
//! (design D7). Desktop only; none of this is compiled for the web.
//!
//! [`QuickHost`] outlives every window. It owns the store handle, what the
//! panel showed when it was last dismissed ([`QuickState`]) and the window
//! while it is open. Each opening builds a new window and pane from that
//! state; closing writes the session and the draft back.

pub(crate) mod activity;
pub(crate) mod chrome;
pub(crate) mod edge;
// Pure; only the macOS renderer uses it outside its tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod edge_plan;
pub(crate) mod hotkey;
pub(crate) mod hud;
pub(crate) mod instance;
mod panel;
mod tray;

use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use gpui_kit::*;

use crate::session::open::{OpenSession, SessionRef};
use crate::store::{AgentsStore, QuickPrefs};
use crate::ui::{QmtApp, SessionPaneEvent};
use hotkey::Hotkey;
pub(crate) use hotkey::HotkeyStatus;
pub(crate) use panel::{QuickPanel, QuickPanelEvent};
use tray::Tray;

/// Requests from outside the UI: the shortcut, the tray menu and a second
/// instance of the app. They share one bounded channel that a foreground
/// loop awaits.
#[derive(Debug)]
pub(crate) enum Signal {
    /// Show the panel, or close it when it is open.
    TogglePanel,
    /// Bring the main window forward, reopening it when closed.
    OpenMain,
    /// End the app.
    Quit,
    /// The portal answered a request to bind `shortcut`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    PortalBound {
        shortcut: String,
        result: Result<(), String>,
    },
}

/// Capacity of the signal channel; a press that finds it full is dropped.
const SIGNAL_QUEUE: usize = 8;

/// How long the panel stays closed before its next opening starts on a new,
/// empty quick chat and its session is released.
pub(crate) const RELEASE_AFTER: Duration = Duration::from_secs(15 * 60);

/// What the user chose in the panel for its next quick chat. These are the
/// panel's own: the start page's launch defaults are not changed by them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Picks {
    /// Model, by selection key.
    pub(crate) model: Option<String>,
    /// Working folder.
    pub(crate) cwd: Option<String>,
    /// Reasoning effort.
    pub(crate) reasoning: Option<String>,
    /// Agent mode, by id.
    pub(crate) mode: Option<String>,
}

/// What the panel remembers while it is closed.
#[derive(Default)]
pub(crate) struct QuickState {
    /// The conversation the panel showed. The strong handle keeps it open
    /// while no window shows it.
    pub(crate) session: Option<Entity<OpenSession>>,
    /// Unsent text of the composer.
    pub(crate) draft: String,
    /// Model, folder and reasoning effort picked for the next quick chat.
    pub(crate) picks: Picks,
    /// When the panel was dismissed; `None` while it is open.
    pub(crate) hidden_at: Option<Instant>,
}

impl QuickState {
    /// The panel closed at `now` showing `session` with `draft` in its composer.
    pub(crate) fn hide(
        &mut self,
        session: Option<Entity<OpenSession>>,
        draft: String,
        now: Instant,
    ) {
        self.session = session;
        self.draft = draft;
        self.hidden_at = Some(now);
    }

    /// Drop the session and the draft when the panel has been closed for
    /// [`RELEASE_AFTER`]. True when it did.
    pub(crate) fn release_if_due(&mut self, now: Instant) -> bool {
        match self.hidden_at {
            Some(at) if now.saturating_duration_since(at) >= RELEASE_AFTER => {
                self.session = None;
                self.draft.clear();
                self.picks = Picks::default();
                self.hidden_at = None;
                true
            }
            _ => false,
        }
    }

    /// What a window opened at `now` starts with. The window takes over the
    /// session, so the state holds nothing until the window closes.
    pub(crate) fn resume(&mut self, now: Instant) -> Resume {
        self.release_if_due(now);
        self.hidden_at = None;
        Resume {
            session: self.session.take(),
            draft: std::mem::take(&mut self.draft),
            picks: std::mem::take(&mut self.picks),
        }
    }
}

/// What a window opens with.
#[derive(Clone)]
pub(crate) struct Resume {
    pub(crate) session: Option<Entity<OpenSession>>,
    pub(crate) draft: String,
    pub(crate) picks: Picks,
}

/// The open panel window.
struct OpenPanel {
    window: AnyWindowHandle,
    view: Entity<QuickPanel>,
    _events: Subscription,
}

pub(crate) struct QuickHost {
    store: Entity<AgentsStore>,
    state: QuickState,
    panel: Option<OpenPanel>,
    /// The main window, while it is known to be open.
    main: Option<(AnyWindowHandle, Entity<QmtApp>)>,
    /// Fires [`RELEASE_AFTER`] after the panel was dismissed.
    release: Option<Task<()>>,
    /// Where the shortcut, tray and socket send their requests.
    signals: Sender<Signal>,
    /// The global shortcut, while the panel is enabled.
    hotkey: Option<Hotkey>,
    /// Why the last attempt to register the configured shortcut failed. The
    /// previous shortcut, if any, is still active.
    hotkey_error: Option<String>,
    /// The tray icon, while the panel is enabled.
    tray: Option<Tray>,
    /// The socket later instances forward `--quick` over, while enabled.
    listening: Option<instance::Listening>,
    /// The settings the app state follows.
    applied: Option<QuickPrefs>,
    _store: Subscription,
}

/// The host as a global, so the shortcut, tray and socket handlers reach it.
struct QuickGlobal(Entity<QuickHost>);

impl Global for QuickGlobal {}

impl QuickHost {
    /// Create the host and make it the app's. `main` is the window the app
    /// opened at startup, if any.
    pub(crate) fn install(
        store: Entity<AgentsStore>,
        main: Option<(AnyWindowHandle, Entity<QmtApp>)>,
        cx: &mut App,
    ) -> Entity<Self> {
        let (signals, requests) = async_channel::bounded(SIGNAL_QUEUE);
        let host = cx.new(|cx: &mut Context<Self>| {
            let observed = store.clone();
            Self {
                store,
                state: QuickState::default(),
                panel: None,
                main,
                release: None,
                signals,
                hotkey: None,
                hotkey_error: None,
                tray: None,
                listening: None,
                applied: None,
                _store: cx.observe(&observed, |host, _, cx| host.sync(cx)),
            }
        });
        cx.set_global(QuickGlobal(host.clone()));
        host.update(cx, |host, cx| {
            host.listen(requests, cx);
            host.sync(cx);
        });
        host
    }

    /// Handle what arrives on the signal channel, one at a time.
    fn listen(&mut self, requests: Receiver<Signal>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Ok(signal) = requests.recv().await {
                if this
                    .update(cx, |host, cx| host.on_signal(signal, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        match signal {
            Signal::TogglePanel => self.toggle(cx),
            Signal::OpenMain => self.show_main(cx),
            Signal::Quit => cx.quit(),
            #[cfg(target_os = "linux")]
            Signal::PortalBound { shortcut, result } => {
                if let Some(hotkey) = &mut self.hotkey {
                    self.hotkey_error = result.as_ref().err().cloned();
                    hotkey.portal_bound(shortcut, result);
                    cx.notify();
                }
            }
            #[cfg(not(target_os = "linux"))]
            Signal::PortalBound { .. } => {}
        }
    }

    /// Make the app match the quick panel settings: the shortcut and the
    /// quit mode follow `enabled`, and a changed shortcut is registered.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let wanted = self.store.read(cx).prefs.quick.clone();
        if self.applied.as_ref() == Some(&wanted) {
            return;
        }
        let was_enabled = self.applied.as_ref().is_some_and(|p| p.enabled);
        let shortcut_changed = self.applied.as_ref().map(|p| &p.shortcut) != Some(&wanted.shortcut);
        self.applied = Some(wanted.clone());
        if wanted.enabled {
            // The app lives on without windows: only Quit ends it.
            cx.set_quit_mode(QuitMode::Explicit);
            if self.hotkey.is_none() {
                self.hotkey = Some(Hotkey::start(self.signals.clone()));
            }
            if !was_enabled || shortcut_changed {
                self.register_shortcut(&wanted, cx);
            }
            if self.tray.is_none() {
                match Tray::new(self.signals.clone()) {
                    Ok(tray) => self.tray = Some(tray),
                    Err(error) => tracing::warn!(%error, "no tray icon"),
                }
            }
            if self.listening.is_none() {
                match instance::listen(&instance::Endpoint::for_user(), self.signals.clone()) {
                    Ok(listening) => self.listening = Some(listening),
                    Err(error) => tracing::warn!(%error, "not listening for `--quick`"),
                }
            }
        } else {
            cx.set_quit_mode(QuitMode::Default);
            self.hotkey = None;
            self.hotkey_error = None;
            self.tray = None;
            self.listening = None;
        }
        cx.notify();
    }

    fn register_shortcut(&mut self, prefs: &QuickPrefs, cx: &mut Context<Self>) {
        let shortcut = if prefs.shortcut.is_empty() {
            hotkey::default_shortcut()
        } else {
            prefs.shortcut.as_str()
        };
        let Some(hotkey) = &mut self.hotkey else {
            return;
        };
        self.hotkey_error = hotkey.set(shortcut, cx).err();
        if let Some(error) = &self.hotkey_error {
            tracing::warn!(%error, shortcut, "quick panel shortcut not registered");
        }
    }

    /// How the shortcut stands, for Settings.
    pub(crate) fn hotkey_status(&self) -> HotkeyStatus {
        self.hotkey
            .as_ref()
            .map_or(HotkeyStatus::Off, Hotkey::status)
    }

    /// Why the configured shortcut could not be registered; the previous
    /// shortcut, shown by [`Self::hotkey_status`], is still active.
    pub(crate) fn hotkey_error(&self) -> Option<&str> {
        self.hotkey_error.as_deref()
    }

    /// Bring the main window forward, reopening it when it was closed.
    pub(crate) fn show_main(&mut self, cx: &mut Context<Self>) {
        let window = match self.live_main(cx) {
            Some((window, _)) => window,
            None => match crate::ui::open_main_window(&self.store, cx) {
                Ok(main) => {
                    let window = main.0;
                    self.main = Some(main);
                    window
                }
                Err(error) => {
                    tracing::warn!(%error, "could not reopen the main window");
                    return;
                }
            },
        };
        let _ = window.update(cx, |_, window, _| window.activate_window());
        cx.activate(true);
    }

    /// The installed host, if the quick panel is set up.
    pub(crate) fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<QuickGlobal>().map(|g| g.0.clone())
    }

    /// Show the panel when it is closed, close it when it is open.
    pub(crate) fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.panel.is_some() {
            self.hide(cx);
        } else {
            self.show(cx);
        }
    }

    /// Open the panel (or bring the open one forward).
    pub(crate) fn show(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = &self.panel {
            let _ = open
                .window
                .update(cx, |_, window, _| window.activate_window());
            return;
        }
        let now = cx.background_executor().now();
        self.release = None;
        let resume = self.state.resume(now);
        let display = self.main_display(cx);
        let store = self.store.clone();
        let opened = panel::open_window(store, resume, display, cx);
        match opened {
            Ok((window, view)) => {
                // Take the keyboard focus, so that losing it later dismisses
                // the panel.
                let _ = window.update(cx, |_, window, _| window.activate_window());
                let events = cx.subscribe(&view, Self::on_panel_event);
                self.panel = Some(OpenPanel {
                    window,
                    view,
                    _events: events,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "could not open the quick panel");
            }
        }
    }

    /// Close the panel, keeping its session and draft for the next opening.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.panel else { return };
        let view = open.view.clone();
        let _ = open.window.update(cx, |_, window, cx| {
            view.update(cx, |panel, cx| panel.dismiss(window, cx))
        });
    }

    /// The display of the main window, to centre the panel on.
    fn main_display(&mut self, cx: &mut Context<Self>) -> Option<DisplayId> {
        let (window, _) = self.live_main(cx)?;
        window
            .update(cx, |_, window, cx| window.display(cx).map(|d| d.id()))
            .ok()
            .flatten()
    }

    /// The main window when it is still open.
    fn live_main(&mut self, cx: &mut Context<Self>) -> Option<(AnyWindowHandle, Entity<QmtApp>)> {
        let (window, app) = self.main.clone()?;
        if window.update(cx, |_, _, _| ()).is_ok() {
            Some((window, app))
        } else {
            self.main = None;
            None
        }
    }

    fn on_panel_event(
        &mut self,
        _: Entity<QuickPanel>,
        event: &QuickPanelEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            QuickPanelEvent::Closed {
                session,
                draft,
                picks,
            } => {
                self.panel = None;
                let now = cx.background_executor().now();
                self.state.hide(session.clone(), draft.clone(), now);
                self.state.picks = picks.clone();
                self.arm_release(cx);
                cx.notify();
            }
            QuickPanelEvent::OpenInMain { target, then } => {
                self.open_in_main(target.clone(), then.clone(), cx);
            }
        }
    }

    /// Show `target` in the main window (reopening it when closed), bring it
    /// to the front and close the panel (design D7). `then` is an app-level
    /// request of the panel's pane (a fork, an undo) the main window takes up.
    pub(crate) fn open_in_main(
        &mut self,
        target: SessionRef,
        then: Option<SessionPaneEvent>,
        cx: &mut Context<Self>,
    ) {
        let (window, app) = match self.live_main(cx) {
            Some(main) => main,
            None => match crate::ui::open_main_window(&self.store, cx) {
                Ok(main) => {
                    self.main = Some(main.clone());
                    main
                }
                Err(error) => {
                    tracing::warn!(%error, "could not reopen the main window");
                    return;
                }
            },
        };
        app.update(cx, |app, cx| app.show_session(target, cx));
        if let Some(event) = then {
            let _ = window.update(cx, |_, window, cx| {
                app.update(cx, |app, cx| app.apply_pane_event(event, window, cx))
            });
        }
        let _ = window.update(cx, |_, window, _| window.activate_window());
        cx.activate(true);
        if let Some(open) = &self.panel {
            let view = open.view.clone();
            let _ = open.window.update(cx, |_, window, cx| {
                view.update(cx, |panel, cx| panel.close_handed_off(window, cx))
            });
        }
    }

    /// Release the session once the panel has stayed closed long enough.
    fn arm_release(&mut self, cx: &mut Context<Self>) {
        self.release = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RELEASE_AFTER).await;
            let _ = this.update(cx, |host, cx| {
                let now = cx.background_executor().now();
                if host.state.release_if_due(now) {
                    cx.notify();
                }
            });
        }));
    }

    /// Stand in for the platform's shortcut backend.
    #[cfg(test)]
    pub(crate) fn set_hotkey_for_test(&mut self, hotkey: Hotkey) {
        self.hotkey = Some(hotkey);
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &QuickState {
        &self.state
    }

    /// The main window the host knows.
    #[cfg(test)]
    pub(crate) fn main_for_test(&self) -> Option<AnyWindowHandle> {
        self.main.as_ref().map(|(window, _)| *window)
    }

    /// The main app the host knows.
    #[cfg(test)]
    pub(crate) fn main_app_for_test(&self) -> Option<Entity<QmtApp>> {
        self.main.as_ref().map(|(_, app)| app.clone())
    }

    /// The open panel window and view.
    #[cfg(test)]
    pub(crate) fn panel_for_test(&self) -> Option<(AnyWindowHandle, Entity<QuickPanel>)> {
        self.panel.as_ref().map(|p| (p.window, p.view.clone()))
    }
}

#[cfg(test)]
mod tests;
