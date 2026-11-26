//! The global shortcut (design D8).
//!
//! macOS, Windows and X11 use `global-hotkey`; Wayland uses the
//! GlobalShortcuts portal. Presses reach the app through one bounded channel
//! ([`Signal`]) that a foreground loop awaits. Shortcuts are stored as the
//! strings `global-hotkey` parses (`control+alt+Space`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_channel::Sender;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui_kit::Keystroke;

use super::Signal;

/// The shortcut a fresh install uses: Option+Space on macOS, Ctrl+Alt+Space
/// elsewhere.
pub(crate) fn default_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "alt+Space"
    } else {
        "control+alt+Space"
    }
}

/// `global-hotkey` name of a GPUI key, or `None` for keys a shortcut cannot use.
fn key_name(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return match c {
            'a'..='z' => Some(format!("Key{}", c.to_ascii_uppercase())),
            'A'..='Z' => Some(format!("Key{c}")),
            '0'..='9' => Some(format!("Digit{c}")),
            '-' => Some("Minus".into()),
            '=' => Some("Equal".into()),
            '[' => Some("BracketLeft".into()),
            ']' => Some("BracketRight".into()),
            '\\' => Some("Backslash".into()),
            ';' => Some("Semicolon".into()),
            '\'' => Some("Quote".into()),
            ',' => Some("Comma".into()),
            '.' => Some("Period".into()),
            '/' => Some("Slash".into()),
            '`' => Some("Backquote".into()),
            _ => None,
        };
    }
    let named = match key {
        "space" => "Space",
        "enter" => "Enter",
        "tab" => "Tab",
        "escape" => "Escape",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        _ => {
            let n: u8 = key.strip_prefix('f')?.parse().ok()?;
            return (1..=24).contains(&n).then(|| format!("F{n}"));
        }
    };
    Some(named.into())
}

/// The shortcut a keystroke records, in `global-hotkey` form. A global
/// shortcut needs at least one of Control, Alt, Shift and Command/Super, so
/// anything else is `None`.
pub(crate) fn shortcut_from_keystroke(keystroke: &Keystroke) -> Option<String> {
    let m = &keystroke.modifiers;
    if !(m.control || m.alt || m.shift || m.platform) {
        return None;
    }
    let key = key_name(&keystroke.key)?;
    let mut out = String::new();
    for (on, name) in [
        (m.control, "control"),
        (m.alt, "alt"),
        (m.shift, "shift"),
        (m.platform, "meta"),
    ] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key);
    Some(out)
}

/// A shortcut as people read it: `⌥ Space` on macOS, `Ctrl+Alt+Space` elsewhere.
pub(crate) fn describe(shortcut: &str) -> String {
    let mac = cfg!(target_os = "macos");
    let parts: Vec<String> = shortcut
        .split('+')
        .map(|token| match token.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => if mac { "⌃" } else { "Ctrl" }.into(),
            "alt" | "option" => if mac { "⌥" } else { "Alt" }.into(),
            "shift" => if mac { "⇧" } else { "Shift" }.into(),
            "meta" | "cmd" | "command" | "super" => {
                if mac {
                    "⌘".into()
                } else if cfg!(target_os = "windows") {
                    "Win".into()
                } else {
                    "Super".into()
                }
            }
            _ => {
                let key = token
                    .strip_prefix("Key")
                    .or_else(|| token.strip_prefix("Digit"))
                    .unwrap_or(token);
                key.to_owned()
            }
        })
        .collect();
    if mac {
        parts.join(" ")
    } else {
        parts.join("+")
    }
}

/// XDG shortcuts-spec trigger (`CTRL+ALT+space`) of a shortcut, for the
/// portal's `preferred_trigger`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn portal_trigger(shortcut: &str) -> String {
    shortcut
        .split('+')
        .map(|token| match token.to_ascii_lowercase().as_str() {
            "control" | "ctrl" => "CTRL".to_owned(),
            "alt" | "option" => "ALT".to_owned(),
            "shift" => "SHIFT".to_owned(),
            "meta" | "cmd" | "command" | "super" => "LOGO".to_owned(),
            _ => match token {
                "Space" => "space".into(),
                "Enter" => "Return".into(),
                "Backspace" => "BackSpace".into(),
                "PageUp" => "Page_Up".into(),
                "PageDown" => "Page_Down".into(),
                "ArrowUp" => "Up".into(),
                "ArrowDown" => "Down".into(),
                "ArrowLeft" => "Left".into(),
                "ArrowRight" => "Right".into(),
                "Minus" => "minus".into(),
                "Equal" => "equal".into(),
                "Comma" => "comma".into(),
                "Period" => "period".into(),
                "Slash" => "slash".into(),
                "Semicolon" => "semicolon".into(),
                "Quote" => "apostrophe".into(),
                "Backquote" => "grave".into(),
                "BracketLeft" => "bracketleft".into(),
                "BracketRight" => "bracketright".into(),
                "Backslash" => "backslash".into(),
                other => match other
                    .strip_prefix("Key")
                    .or_else(|| other.strip_prefix("Digit"))
                {
                    Some(rest) => rest.to_ascii_lowercase(),
                    None => other.to_owned(),
                },
            },
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Something that can hold one registered shortcut.
pub(crate) trait Backend {
    type Key;

    fn register(&mut self, shortcut: &str) -> Result<Self::Key, String>;
    fn unregister(&mut self, key: Self::Key);
}

/// The registered shortcut of a backend. A new shortcut is registered before
/// the old one is let go, so one that cannot be registered (taken by another
/// application) leaves the previous one working.
pub(crate) struct Registry<B: Backend> {
    backend: B,
    active: Option<(String, B::Key)>,
}

impl<B: Backend> Registry<B> {
    pub(crate) fn new(backend: B) -> Self {
        Self {
            backend,
            active: None,
        }
    }

    pub(crate) fn set(&mut self, shortcut: &str) -> Result<(), String> {
        if self.active.as_ref().is_some_and(|(s, _)| s == shortcut) {
            return Ok(());
        }
        let key = self.backend.register(shortcut)?;
        if let Some((_, old)) = self.active.replace((shortcut.to_owned(), key)) {
            self.backend.unregister(old);
        }
        Ok(())
    }

    pub(crate) fn active(&self) -> Option<&str> {
        self.active.as_ref().map(|(s, _)| s.as_str())
    }

    #[cfg(test)]
    pub(crate) fn backend(&self) -> &B {
        &self.backend
    }
}

/// `global-hotkey` on the main thread.
pub(crate) struct Native {
    manager: GlobalHotKeyManager,
    /// Id of the registered shortcut, which the event handler compares.
    id: Arc<AtomicU32>,
}

impl Native {
    /// Create the manager. Must run on the main thread.
    pub(crate) fn new(signals: Sender<Signal>) -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let id = Arc::new(AtomicU32::new(0));
        let watched = id.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state() == HotKeyState::Pressed && event.id() == watched.load(Ordering::SeqCst)
            {
                // A full channel means presses are already queued.
                let _ = signals.try_send(Signal::TogglePanel);
            }
        }));
        Ok(Self { manager, id })
    }
}

impl Backend for Native {
    type Key = HotKey;

    fn register(&mut self, shortcut: &str) -> Result<HotKey, String> {
        let hotkey: HotKey = shortcut
            .parse()
            .map_err(|e: global_hotkey::hotkey::HotKeyParseError| e.to_string())?;
        self.manager
            .register(hotkey)
            .map_err(|e| format!("could not register {}: {e}", describe(shortcut)))?;
        self.id.store(hotkey.id(), Ordering::SeqCst);
        Ok(hotkey)
    }

    fn unregister(&mut self, key: HotKey) {
        if let Err(error) = self.manager.unregister(key) {
            tracing::debug!(%error, "could not release the previous quick panel shortcut");
        }
    }
}

/// How the shortcut stands, for Settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HotkeyStatus {
    /// Nothing asked for it yet.
    Off,
    /// This shortcut is registered.
    Active(String),
    /// The desktop offers no way to register one; the reason says why.
    Unavailable(String),
}

/// The command that toggles the panel of the running instance, for
/// desktops where the shortcut has to be bound by hand.
pub(crate) const CLI_HINT: &str = "querymt-gpui --quick";

/// What Settings says about the shortcut: whether it is registered, why a
/// change failed (the previous shortcut stays active), and the command to
/// bind in the desktop's own shortcut settings when none can be registered.
pub(crate) fn status_lines(status: &HotkeyStatus, error: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    match status {
        HotkeyStatus::Off => lines.push("Not registered yet.".to_owned()),
        HotkeyStatus::Active(shortcut) => {
            lines.push(format!("{} is registered.", describe(shortcut)));
        }
        HotkeyStatus::Unavailable(reason) => {
            lines.push(format!("The shortcut is unavailable: {reason}."));
            lines.push(format!(
                "Bind this command in your desktop's shortcut settings instead: {CLI_HINT}"
            ));
        }
    }
    if let Some(error) = error {
        let kept = matches!(status, HotkeyStatus::Active(_));
        lines.push(if kept {
            format!(
                "The new shortcut could not be registered ({error}); the previous one stays active."
            )
        } else {
            format!("The shortcut could not be registered ({error}).")
        });
        if !kept && !matches!(status, HotkeyStatus::Unavailable(_)) {
            lines.push(format!(
                "You can run {CLI_HINT} from your desktop's shortcut settings."
            ));
        }
    }
    lines
}

/// The backend this platform uses.
pub(crate) enum Hotkey {
    Native(Registry<Native>),
    #[cfg(target_os = "linux")]
    Portal(portal::Portal),
    Unavailable(String),
}

impl Hotkey {
    /// Pick the backend of this platform. Must run on the main thread.
    pub(crate) fn start(signals: Sender<Signal>) -> Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return Self::Portal(portal::Portal::new(signals));
        }
        match Native::new(signals) {
            Ok(native) => Self::Native(Registry::new(native)),
            Err(reason) => Self::Unavailable(reason),
        }
    }

    /// Register `shortcut`, keeping the previous one when that fails.
    /// `Ok(())` from the portal only means the request was sent: its answer
    /// arrives as [`Signal::PortalBound`].
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    pub(crate) fn set(&mut self, shortcut: &str, cx: &mut gpui_kit::App) -> Result<(), String> {
        match self {
            Self::Native(registry) => registry.set(shortcut),
            #[cfg(target_os = "linux")]
            Self::Portal(portal) => {
                portal.bind(shortcut, cx);
                Ok(())
            }
            Self::Unavailable(reason) => Err(reason.clone()),
        }
    }

    pub(crate) fn status(&self) -> HotkeyStatus {
        match self {
            Self::Native(registry) => registry
                .active()
                .map_or(HotkeyStatus::Off, |s| HotkeyStatus::Active(s.to_owned())),
            #[cfg(target_os = "linux")]
            Self::Portal(portal) => portal.status(),
            Self::Unavailable(reason) => HotkeyStatus::Unavailable(reason.clone()),
        }
    }

    /// The portal answered a bind request.
    #[cfg(target_os = "linux")]
    pub(crate) fn portal_bound(&mut self, shortcut: String, result: Result<(), String>) {
        if let Self::Portal(portal) = self {
            portal.bound(shortcut, result);
        }
    }
}

#[cfg(target_os = "linux")]
mod portal {
    use async_channel::Sender;
    use futures::StreamExt as _;
    use gpui_kit::{App, AppContext as _, Task};

    use super::{HotkeyStatus, Signal, portal_trigger};

    const SHORTCUT_ID: &str = "toggle-quick-panel";

    /// The GlobalShortcuts portal. Each bind opens a session; the previous
    /// session is closed once the new one is bound.
    pub(crate) struct Portal {
        signals: Sender<Signal>,
        active: Option<(String, Task<()>)>,
        pending: Option<(String, Task<()>)>,
        error: Option<String>,
    }

    impl Portal {
        pub(crate) fn new(signals: Sender<Signal>) -> Self {
            Self {
                signals,
                active: None,
                pending: None,
                error: None,
            }
        }

        pub(crate) fn bind(&mut self, shortcut: &str, cx: &mut App) {
            if self.active.as_ref().is_some_and(|(s, _)| s == shortcut) {
                return;
            }
            let signals = self.signals.clone();
            let trigger = portal_trigger(shortcut);
            let name = shortcut.to_owned();
            let task = cx.background_spawn(async move {
                run(name, trigger, signals).await;
            });
            self.pending = Some((shortcut.to_owned(), task));
        }

        pub(crate) fn bound(&mut self, shortcut: String, result: Result<(), String>) {
            let Some((pending, task)) = self.pending.take() else {
                return;
            };
            if pending != shortcut {
                self.pending = Some((pending, task));
                return;
            }
            match result {
                Ok(()) => {
                    self.error = None;
                    // Dropping the old task closes its session.
                    self.active = Some((pending, task));
                }
                Err(reason) => {
                    self.error = Some(reason);
                    // The previous session, if any, stays.
                }
            }
        }

        pub(crate) fn status(&self) -> HotkeyStatus {
            match (&self.active, &self.error) {
                (Some((s, _)), _) => HotkeyStatus::Active(s.clone()),
                (None, Some(reason)) => HotkeyStatus::Unavailable(reason.clone()),
                (None, None) => HotkeyStatus::Off,
            }
        }
    }

    /// One portal session: bind, report, then forward activations until
    /// the task is dropped.
    async fn run(shortcut: String, trigger: String, signals: Sender<Signal>) {
        use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};

        let bound = async {
            let proxy = GlobalShortcuts::new().await.map_err(|e| e.to_string())?;
            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(|e| e.to_string())?;
            let new = NewShortcut::new(SHORTCUT_ID, "Toggle the QueryMT quick panel")
                .preferred_trigger(trigger.as_str());
            proxy
                .bind_shortcuts(&session, &[new], None, Default::default())
                .await
                .and_then(|request| request.response())
                .map_err(|e| e.to_string())?;
            let activated = proxy.receive_activated().await.map_err(|e| e.to_string())?;
            Ok::<_, String>((proxy, session, activated))
        }
        .await;
        match bound {
            Err(reason) => {
                let _ = signals
                    .send(Signal::PortalBound {
                        shortcut,
                        result: Err(reason),
                    })
                    .await;
            }
            Ok((_proxy, _session, mut activated)) => {
                let _ = signals
                    .send(Signal::PortalBound {
                        shortcut,
                        result: Ok(()),
                    })
                    .await;
                while let Some(event) = activated.next().await {
                    if event.shortcut_id() == SHORTCUT_ID {
                        let _ = signals.try_send(Signal::TogglePanel);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    use gpui_kit::{Keystroke, Modifiers};

    use super::{Backend, Registry, describe, portal_trigger, shortcut_from_keystroke};

    fn stroke(modifiers: Modifiers, key: &str) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: None,
        }
    }

    fn mods(control: bool, alt: bool, shift: bool, platform: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            shift,
            platform,
            function: false,
        }
    }

    #[test]
    fn each_modifier_maps_to_its_name() {
        let case = |m: Modifiers| shortcut_from_keystroke(&stroke(m, "space"));
        assert_eq!(
            case(mods(true, false, false, false)).as_deref(),
            Some("control+Space")
        );
        assert_eq!(
            case(mods(false, true, false, false)).as_deref(),
            Some("alt+Space")
        );
        assert_eq!(
            case(mods(false, false, true, false)).as_deref(),
            Some("shift+Space")
        );
        assert_eq!(
            case(mods(false, false, false, true)).as_deref(),
            Some("meta+Space")
        );
        assert_eq!(
            case(mods(true, true, true, true)).as_deref(),
            Some("control+alt+shift+meta+Space")
        );
    }

    #[test]
    fn named_and_plain_keys_map_to_global_hotkey_names() {
        let alt = mods(false, true, false, false);
        let key = |k: &str| shortcut_from_keystroke(&stroke(alt, k));
        assert_eq!(key("a").as_deref(), Some("alt+KeyA"));
        assert_eq!(key("7").as_deref(), Some("alt+Digit7"));
        assert_eq!(key("f5").as_deref(), Some("alt+F5"));
        assert_eq!(key("f24").as_deref(), Some("alt+F24"));
        assert_eq!(key("enter").as_deref(), Some("alt+Enter"));
        assert_eq!(key("up").as_deref(), Some("alt+ArrowUp"));
        assert_eq!(key("pagedown").as_deref(), Some("alt+PageDown"));
        assert_eq!(key("-").as_deref(), Some("alt+Minus"));
        assert_eq!(key("`").as_deref(), Some("alt+Backquote"));
        // Not a key a shortcut can use.
        assert_eq!(key("f25"), None);
        assert_eq!(key("ß"), None);
        assert_eq!(key("capslock"), None);
    }

    #[test]
    fn a_shortcut_needs_a_modifier() {
        assert_eq!(
            shortcut_from_keystroke(&stroke(Modifiers::default(), "space")),
            None
        );
        let function = Modifiers {
            function: true,
            ..Default::default()
        };
        assert_eq!(shortcut_from_keystroke(&stroke(function, "f5")), None);
    }

    #[test]
    fn recorded_shortcuts_parse_with_global_hotkey() {
        for (m, key) in [
            (mods(true, true, false, false), "space"),
            (mods(false, true, false, false), "space"),
            (mods(true, false, true, false), "k"),
            (mods(false, false, false, true), "f12"),
            (mods(true, true, true, true), "pageup"),
        ] {
            let text = shortcut_from_keystroke(&stroke(m, key)).unwrap();
            let parsed: Result<global_hotkey::hotkey::HotKey, _> = text.parse();
            assert!(parsed.is_ok(), "{text}: {:?}", parsed.err());
        }
        let default: global_hotkey::hotkey::HotKey = super::default_shortcut().parse().unwrap();
        assert_eq!(default.id(), default.id());
    }

    #[test]
    fn shortcuts_read_naturally_and_become_portal_triggers() {
        let text = describe("control+alt+KeyK");
        assert!(text.contains('K') && !text.contains("Key"), "{text}");
        assert_eq!(portal_trigger("control+alt+Space"), "CTRL+ALT+space");
        assert_eq!(portal_trigger("meta+shift+KeyA"), "LOGO+SHIFT+a");
        assert_eq!(portal_trigger("alt+F5"), "ALT+F5");
        assert_eq!(portal_trigger("alt+ArrowUp"), "ALT+Up");
    }

    /// Registers what it is told to, except shortcuts in `taken`.
    #[derive(Clone, Default)]
    struct Fake {
        taken: Rc<RefCell<HashSet<String>>>,
        held: Rc<RefCell<Vec<String>>>,
    }

    impl Backend for Fake {
        type Key = String;

        fn register(&mut self, shortcut: &str) -> Result<String, String> {
            if self.taken.borrow().contains(shortcut) {
                return Err(format!("{shortcut} is taken"));
            }
            self.held.borrow_mut().push(shortcut.to_owned());
            Ok(shortcut.to_owned())
        }

        fn unregister(&mut self, key: String) {
            self.held.borrow_mut().retain(|k| *k != key);
        }
    }

    #[test]
    fn status_lines_explain_the_shortcut_and_name_the_command() {
        use super::{HotkeyStatus, status_lines};

        let text = |status, error| status_lines(&status, error).join(" ");
        let active = text(HotkeyStatus::Active("alt+Space".into()), None);
        assert!(
            active.contains("registered") && !active.contains("--quick"),
            "{active}"
        );

        let none = text(HotkeyStatus::Unavailable("no portal".into()), None);
        assert!(
            none.contains("unavailable") && none.contains("querymt-gpui --quick"),
            "{none}"
        );

        let taken = text(
            HotkeyStatus::Active("alt+Space".into()),
            Some("alt+KeyT is taken"),
        );
        assert!(taken.contains("previous one stays active"), "{taken}");

        let failed = text(HotkeyStatus::Off, Some("alt+KeyT is taken"));
        assert!(failed.contains("querymt-gpui --quick"), "{failed}");
    }

    #[test]
    fn failed_register_keeps_previous_shortcut() {
        let fake = Fake::default();
        fake.taken.borrow_mut().insert("alt+KeyT".into());
        let mut registry = Registry::new(fake.clone());

        registry.set("alt+Space").unwrap();
        assert_eq!(registry.active(), Some("alt+Space"));

        // Taken by another application: the old one stays registered.
        let error = registry.set("alt+KeyT").unwrap_err();
        assert!(error.contains("taken"), "{error}");
        assert_eq!(registry.active(), Some("alt+Space"));
        assert_eq!(*registry.backend().held.borrow(), ["alt+Space"]);

        // A free one replaces it, and the old one is released.
        registry.set("alt+KeyY").unwrap();
        assert_eq!(registry.active(), Some("alt+KeyY"));
        assert_eq!(*registry.backend().held.borrow(), ["alt+KeyY"]);

        // Setting the same shortcut again registers nothing twice.
        registry.set("alt+KeyY").unwrap();
        assert_eq!(*registry.backend().held.borrow(), ["alt+KeyY"]);
    }
}
