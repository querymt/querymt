# Design

## Context

- **Prerequisite.** `use-agent-session-core` is archived first. `SessionView` is then a `SessionCore` with a projection and an overlay (`src/session/view.rs`), so one per open session is cheap and self-contained.
- **One slot today.** `AgentsStore.session: Option<SessionView>` (`src/store.rs:1688`) is "the" open session. Roughly 31 places in `store.rs`, 7 in `src/store/{delegates,fork,mcp_notice,worktree}.rs` and about 70 in `src/ui` read it. Five notification branches in `on_incoming` (`store.rs:4397-4470`) and the 8 ms flush (`schedule_session_flush`, `:4554`) filter on it. Every flush calls `cx.notify()` on the whole store, which `QmtApp` observes (`ui/mod.rs:382`).
- **Launch takes over the main window.** `launch` sets `self.session = None` and later `Some(view)` (`:2821`, `:2928`). It keeps one `_launch_task` (a second launch drops, and so cancels, the first) and one `LaunchState { busy, error, … }`.
- **The prompt start is duplicated.** `launch` (`:2918-2944`) and `send_prompt` (`:3642-3661`) both do optimistic prompt → `prompting = true` → `sync_inputs` → `session/prompt` with `_meta.querymt.client_prompt_id` → a spawned `finish_prompt`.
- **Navigation lives in the data model.** `SessionView.parent: Option<Box<ParentSession>>`. `open_child_session`, `open_parent_session` and `reload_session` carry it across by hand (`:3462-3525`).
- **Rendering.** The row renderers in `src/ui/session_view.rs` (about lines 600-3666) are free functions over `&RenderCtx`. They are tied to `QmtApp` only through `RenderCtx.weak: WeakEntity<QmtApp>` (13 `weak.update` callbacks). The chrome, composer and `render_session` (`:3668-5694`) read about 15 `QmtApp` fields. Request cards (`ui/inbox.rs:574`) use the `QmtApp` fields `form_inputs` and `request_collapsed`.
- **GPUI 0.3.12** (gpui-kit 0.9.1) when this was written; the app now builds gpui-pre 0.3.14 with gpui-kit 0.11.0. The facts below were checked on 0.3.12.
  - Floating windows: `WindowKind::PopUp` is an `NSPanel` (non-activating, all Spaces) on macOS and `WS_EX_TOOLWINDOW | WS_EX_TOPMOST` on Windows. On X11 it is override-redirect, so the window manager doesn't give it focus. On Wayland it is a plain toplevel; `WindowKind::LayerShell(LayerShellOptions)` with `KeyboardInteractivity` exists for that case.
  - Quit and windows: `App::set_quit_mode` is runtime-settable. `on_window_closed`, `Window::display`, `activate_window` and `remove_window` exist.
  - Displays: there is no pointer-position or display-under-pointer API.
- **Build constraints.** One binary crate whose tests live in the bin target. The web build (`wasm32`) shares `main.rs::run`. There is no `anyhow`/`thiserror` dependency; errors are `String`s.

## Goals / Non-Goals

**Goals:**
- Any number of independent open sessions, each one an entity whose lifetime is owned by the surfaces that hold it.
- One session pane implementation for the main window and the panel, with no copied rendering code.
- Remove the duplicated prompt path, the repeated `filter(agent && session)` lookups and the navigation state inside `SessionView`.
- The quick panel on macOS, Windows, X11 and Wayland, with a working fallback wherever a global shortcut or floating window isn't available.

**Non-Goals:**
- Converting every `agent_id: &str` in the store to a newtype. Newtypes are introduced at the open-session boundary only.
- Turning `PendingInput.state` and `SessionView.phase` into enums. The core and projection own those strings; they are left for a later change.
- Closing or unsubscribing sessions on the agent. The ACP surface used here has no such method (see Risks).
- Several quick panels, quick panel tabs, a separate quick-chat history.

## Decisions

### D1. One entity per open session, indexed weakly by the store

```mermaid
classDiagram
    class SessionKey {
        <<Clone Eq Hash Debug>>
        +agent: AgentId
        +session: SessionId
    }
    class AgentsStore {
        configs, prefs, runtime, requests
        -hub: HashMap~SessionKey, WeakEntity~OpenSession~~
        +open(SessionRef, cx) Entity~OpenSession~
        +launch(LaunchRequest, cx) Task~Result~Entity~OpenSession~, LaunchError~~
        -route(&SessionKey, impl FnOnce(&mut OpenSession, &mut Context))
    }
    class OpenSession {
        <<Entity, EventEmitter~OpenSessionEvent~>>
        +key: SessionKey
        +view: SessionView
        -link: Link
        -flush_claimed: bool
        +send(text, attachments, Delivery, cx)
        +cancel(cx) / load_older(cx) / tool_content(..) / set_option(..) / undo(..) / redo(cx)
        -start_prompt(content, cx)
    }
    class Link {
        rpc: Option~Rc~RpcClient~~
        generation: u64
        caps: SessionCaps
    }
    class SessionPane {
        <<Entity, EventEmitter~SessionPaneEvent~>>
        -session: Option~Entity~OpenSession~~
        -back: Vec~SessionRef~
        -density: Density
        -transcript: Transcript
        -expanded, copied, composer, attachments, delivery, menus
        -requests: RequestCards
    }
    class QmtApp { main: Entity~SessionPane~ }
    class QuickPanel { pane: Entity~SessionPane~ ; picker }
    class QuickHost { <<Global>> store, quick: QuickState, hotkey, tray }

    AgentsStore "1" o.. "*" OpenSession : weak index
    SessionPane --> OpenSession : strong
    QuickHost --> OpenSession : strong while dismissed
    OpenSession *-- SessionView
    OpenSession *-- Link
    QmtApp *-- SessionPane
    QuickPanel *-- SessionPane
```

- `AgentsStore.session` is removed. `open(SessionRef)` returns the existing entity when `hub[key]` upgrades, or else creates one, inserts a `WeakEntity` and starts `session/load`. Dead entries are pruned when a lookup fails to upgrade and on each `session/list` refresh.
- The session closes when the last strong handle drops. GPUI frees the entity and the next routed input finds a dead weak handle. No reference counting by hand (`own-rc-single-thread`).
- The flush timer and `cx.notify()` move into `OpenSession`. A streamed chunk re-renders only the panes that observe that session. The store still notifies for list-level changes (phase, titles).
- *Alternative: slots in the store* (`views: HashMap<SessionKey, SessionView>`, `main`, `quick`). Rejected for three reasons: the store would encode window layout, releasing a slot would be manual, and every chunk would still notify every store observer.

### D2. Calls go one way: store → session; session → store by event

GPUI panics when an entity is updated while it is already being updated. The rule:
- The store may call into an `OpenSession` (routing, reconnect).
- An `OpenSession` never reads or updates `AgentsStore` inside its own update. It holds what it needs in `Link`: the `Rc<RpcClient>`, the connection generation, and `SessionCaps` (can control the run, tool content, undo, history window), copied from the runtime when it opens.
- Anything the store must do afterwards goes out as an `OpenSessionEvent`. GPUI delivers events after the emitting update ends. The events are `TurnFinished { error: Option<String> }` (log it and refresh the session list), `Loaded` (delegate routes and the MCP notice, both keyed per session on the runtime already), and `ListChanged`.
- On reconnect or disconnect the store calls `rebind(link)` on every live session of that agent. Today `store.rs:2704` does this only for the open one.

```mermaid
sequenceDiagram
    participant A as Agent
    participant S as AgentsStore
    participant O as Entity<OpenSession>
    participant P1 as Main SessionPane
    participant P2 as Quick SessionPane

    A->>S: session/update {sessionId}
    S->>S: key = SessionKey::from_update(agent, params)  (let-else)
    S->>S: hub.get(&key).and_then(WeakEntity::upgrade)
    alt alive
        S->>O: update: view.queue_session_update; claim flush
        Note over O: 8 ms timer owned by O
        O->>O: view.flush()
        O-->>P1: notify (observer)
        O-->>P2: notify (observer)
    else dead or absent
        S->>S: hub.remove(&key) (if dead)
    end
    S->>S: schedule_sessions_refresh (list phase, unchanged)
```

### D3. `SessionKey` and newtypes at the boundary

`AgentId(String)` and `SessionId(String)` (`#[derive(Clone, Debug, PartialEq, Eq, Hash)]`, `as_str()`, `Display`) and `SessionKey { agent, session }`. `SessionRef { key, cwd, title }` is what a surface asks to open and what the back stack stores. The hub, `OpenSession`, panes and events use them. The rest of the store keeps `&str` and converts at those calls (`type-newtype-ids`, `anti-stringly-typed`, scoped per Non-Goals). Constructors that read wire JSON return `Option` (`api-parse-dont-validate`). The about 12 `.as_mut().filter(|v| v.agent_id == … && v.session_id == …)` chains become `hub` lookups with `let … else` (`pat-let-else`).

### D4. One prompt path

`OpenSession::start_prompt(content, cx)` does the optimistic prompt, `prompting`, `sync_inputs`, sends `session/prompt` with `client_prompt_id`, and spawns the completion through a `WeakEntity<OpenSession>`. That completion is today's `finish_prompt`, minus the store work, which goes out as `TurnFinished`. `send` calls it for an idle session and `launch` calls it for the first prompt. `Delivery { Prompt, Steer, Queue }` replaces the `&'static str` in `QmtApp.delivery` (`ui/mod.rs:306`) and in `send_prompt`. It maps to the wire method in one exhaustive `match` (`type-enum-states`, `pat-exhaustive-enum`).

### D5. Launch returns the session; the caller owns the progress

```mermaid
sequenceDiagram
    participant C as Caller (start page / quick panel)
    participant S as AgentsStore
    participant R as RpcClient
    participant O as Entity<OpenSession>

    C->>S: launch(LaunchRequest{agent, cwd, prompt, attachments, placement})
    S-->>C: Task<Result<Entity<OpenSession>, LaunchError>> (#[must_use])
    S->>R: session/new | _querymt/remote/createSession, config options
    S->>O: cx.new(OpenSession::launched(..)); hub.insert(key, weak)
    S->>O: start_prompt(first prompt)
    S-->>C: Ok(entity)
    C->>C: pane.show(entity) and clear its own busy/error
```

- `LaunchState` is split. The placement choices (`target`, `worktree`, `remote_profile`) stay in the store as start-page preferences and are copied into `LaunchRequest`. Progress (`busy`, `error`, `creating_on`, `last_started`) moves to the caller, which holds the `Task`. `_launch_task` is removed, so two launches no longer cancel each other.
- `LaunchError` is a small enum with `Display`: `NotConnected`, `Create(String)`, `PeerProfileGone`. It is hand-written because the crate has no `thiserror` (`err-custom-type`).
- The quick panel launches with `placement: Placement::Local` and the primary agent, so the start page's peer and worktree choice doesn't leak into quick chats.

### D6. `SessionPane`: one pane, two densities

- The pane takes `QmtApp`'s session fields: `transcript`, `expanded_items`, `copied`, `session_prompt`, `session_attachments`, `session_attach_error`, `delivery`, `session_details_open`, `session_menu_open`, `session_options_open`, `plan_open`, `waiting_open`, `usage_hovered`, `usage_pinned`, `smooth_scroll` and the turn diff state. Its methods are `chrome`, `composer`, `options_panel` and `render`.
- The free row renderers stay where they are. `RenderCtx.weak` becomes `WeakEntity<SessionPane>` and `RenderCtx` gains `density: Density`. Compact density changes defaults and limits (tool and reasoning rows collapsed, lower `OUTPUT_LINES`/`READ_LINES`/`DIFF_LINES`, narrower column, actions on hover, no usage ring or details panel). It does not add separate row code. No trait or generic over the host (`anti-over-abstraction`).
- `Density::composer_lines` is `(2, 6)` for Compact: two lines at rest, growing to six. The model chip is part of the compact composer (D12); the options panel still is not.
- What belongs to the app leaves as `SessionPaneEvent { OpenChild(SessionRef), Fork(..), OpenImage(..), OpenInMain(SessionKey), Back }`. `QmtApp` and `QuickPanel` each handle it (`api-common-traits`: `Debug`, `Clone`).
- **Back stack.** `back: Vec<SessionRef>` replaces `SessionView.parent`. `navigate` pushes and `back` pops (`mem::take` when swapping). Reload reopens the same key and doesn't touch the stack.
- **Request cards.** `form_inputs` and `request_collapsed` move into a `RequestCards` value, so the Inbox page and each pane own their own. The card renderer takes it plus a filter (all requests for the Inbox, one `SessionKey` for a pane).

### D7. Quick panel window and state

```mermaid
stateDiagram-v2
    [*] --> Hidden
    Hidden --> Shown: hotkey / tray / --quick
    Shown --> Hidden: Esc / hotkey / focus lost / OpenInMain
    Hidden --> Hidden: release timer (15 min) drops session + draft
    Shown --> Shown: pick session / new quick chat
```

- `QuickHost` (a GPUI `Global`, native only) owns `Entity<AgentsStore>`, so the store outlives the main window. It also owns the hotkey and tray handles and `QuickState { session: Option<Entity<OpenSession>>, draft: String, hidden_at: Option<Instant> }`.
- Each time the panel is shown, a new window and a new `SessionPane` are created from `QuickState`. On close, the draft and the session handle are written back. Text inputs are created per window (`InputState::new(window, …)`), so the pane isn't reused across windows. The strong handle in `QuickState` keeps the session open while the panel is hidden. A 15-minute timer clears it (see the `open-sessions` lifetime requirement).
- **Window kind per platform.**

  | Platform | Kind | Notes |
  |---|---|---|
  | macOS | `PopUp` | |
  | Windows | `PopUp` | |
  | X11 | `Floating` | not `PopUp`: override-redirect windows don't get keyboard focus from the window manager; confirmed by spike task 6.1 |
  | Wayland | `LayerShell` | layer `Overlay`, `KeyboardInteractivity::Exclusive`, when the compositor has wlr-layer-shell |
  | Wayland (GNOME) | `Normal` | |

  `titlebar: None`, `Transparent` background, not resizable by the user (the panel resizes itself, D13). Width 680. A new quick chat opens as a 64 px bar and a conversation at 260; both grow (D13, D15).
- `WindowBackgroundAppearance` stays `Transparent` on macOS, so GPUI does not add its own blur view next to the one `chrome` installs (D11). On Windows and Linux `chrome` picks the appearance.
- Position: `Bounds::centered(main_window.display(cx).map(|d| d.id()), …)`, else the primary display.
- Dismissal: `observe_window_activation`, then `remove_window` when the window becomes inactive. Esc is handled with `capture_key_down` so the composer can't swallow it.
- **Open in main.** If the main window isn't open, create it with `ui::open_main_window(store, cx)`, which is extracted from `main.rs`. Then call `main.pane.navigate(SessionRef)`; the same entity comes from the hub, so there is no reload. Then activate the window and remove the panel.

```mermaid
sequenceDiagram
    participant U as User
    participant H as QuickHost
    participant W as Panel window
    participant P as Quick SessionPane
    participant S as AgentsStore
    participant O as Entity<OpenSession>
    participant M as Main SessionPane

    U->>H: hotkey
    H->>W: open_window(kind per platform)
    W->>P: new(QuickState.session, draft)
    U->>P: type + Enter (no session yet)
    P->>S: launch(LaunchRequest{primary agent, defaults})
    S-->>P: Ok(O)
    P->>O: observe
    U->>P: Cmd/Ctrl+Enter
    P-->>W: SessionPaneEvent::OpenInMain(key)
    W->>M: navigate(SessionRef) -> S.open(key) = same O
    W->>W: remove_window()
    W->>H: QuickState { session: None, draft: "" }
```

### D8. Global shortcut backends

```mermaid
flowchart TD
    start["QuickHost::start (native)"] --> os{platform}
    os -- macOS / Windows --> gh["global-hotkey GlobalHotKeyManager<br/>created on main thread in App::run"]
    os -- Linux --> sess{"WAYLAND_DISPLAY set?"}
    sess -- no (X11) --> gh
    sess -- yes --> portal["ashpd GlobalShortcuts portal<br/>(feature global_shortcuts)"]
    portal -- unavailable / denied --> none["Hotkey::Unavailable(reason)<br/>Settings shows `querymt-gpui --quick`"]
    gh -- register error --> none
    gh --> chan["async_channel::bounded(8)"]
    portal --> chan
    chan --> loop["cx.spawn loop on the foreground:<br/>Pressed -> QuickHost::toggle"]
```

- `enum Hotkey { Native(GlobalHotKeyManager, HotKey), Portal(Task<()>), Unavailable(String) }`. It is an enum, not a trait object, because there are three closed cases (`anti-type-erasure`).
- Events are pushed from the crate's event handler, or from the portal task on the background executor, into a bounded channel (`async-bounded-channel`). A foreground loop awaits it, so there's no polling. Only `Pressed` acts.
- Changing the shortcut registers the new one first and unregisters the old one only on success, so a taken shortcut leaves the previous one working.
- Default bindings come from the spec. They are stored as a string in `Prefs.quick` (`#[serde(default)]`, `serde-default-compat`), parsed with `HotKey::from_str` at the boundary, and recorded from GPUI keystrokes through one mapping function with unit tests for each modifier and key.

### D9. Single instance, `--quick` and background mode

- The first instance listens on a local socket named per user (the `interprocess` crate's local sockets: a Unix socket in the runtime dir on Unix, a named pipe on Windows). A later instance connects, sends `quick` (with `--quick`) or `show`, and exits. A failed connect means no instance is running, so this one starts. With `--quick` it starts with only the panel shown. Accepting connections runs on the background executor and forwards over the same channel as the hotkey.
- **Panel enabled:**
  - `cx.set_quit_mode(QuitMode::Explicit)`.
  - A tray icon with Open QueryMT, Quick panel and Quit. It uses `tray-icon` with `default-features = false, features = ["ksni"]`, so Linux needs no GTK3 or libappindicator; it is created on the main thread inside `App::run`.
  - `Quit` calls `cx.quit()`.
- **Panel disabled:** `QuitMode::Default`, no tray, no socket listener (so `--quick` starts a normal instance).
- Everything in this decision is `#[cfg(not(target_family = "wasm"))]` in `src/quick/`. `main.rs::run` calls `quick::start(store, cx)` on native only.

### D10. Module layout and visibility

- `src/session/open.rs`: `SessionKey`, `SessionRef`, `OpenSession`, `Link`, `OpenSessionEvent`.
- `src/ui/session/`: `pane.rs`, `rows.rs` (today's free renderers), `composer.rs`.
- `src/quick/`: `mod.rs` (`QuickHost`), `panel.rs`, `hotkey.rs`, `tray.rs`, `instance.rs`.
- New APIs are `pub(crate)` (`proj-pub-crate-internal`), with doc comments on the public items (`doc-all-public`). Methods returning an `Entity` or a `Task` are `#[must_use]` (`api-must-use`). Ignored `this.update` results on a dropped entity log at `debug` with structured fields (`obs-structured-fields`) instead of `let _ =`.

### D11. Panel chrome: the platform's own material, in the app's appearance

*Revised 2026-11-27: the panel follows the app's light or dark theme (task 7.39). Everything below that says "dark whatever the theme", "set to dark aqua once", "a black tint" or "a white backdrop" describes the dark panel and now has a mirror image for the light one: aqua instead of dark aqua, a white tint, a black backdrop as the worst case, and the `observe_global::<Palette>` hook is back, calling `chrome::follow_theme`. The decision was made after the first version put dark text, taken from the light app theme, on the always-dark panel.*

```mermaid
flowchart TD
    open["QuickPanel window opened"] --> os{platform}
    os -- macOS --> ver{"AppKit >= 26?"}
    ver -- yes --> glass["NSGlassEffectView<br/>cornerRadius, tintColor"]
    ver -- no --> vfx["NSVisualEffectView<br/>material HUDWindow<br/>state Active<br/>cornerRadius"]
    os -- Windows --> mica["WindowBackgroundAppearance::<br/>MicaBackdrop, else Blurred"]
    os -- Wayland --> wl{"compositor blur?"}
    wl -- yes --> blur["WindowBackgroundAppearance::Blurred"]
    wl -- no --> solid
    os -- X11 --> solid["Opaque surface"]
    glass --> cap["Chrome { glass: true }"]
    vfx --> cap
    mica --> cap
    blur --> cap
    solid --> cap2["Chrome { glass: false }"]
    cap --> paint["Panel paints see-through surfaces"]
    cap2 --> paint2["Panel paints solid surfaces"]
```

- `src/quick/chrome/` with `mod.rs` (the `Chrome` value and the platform call), `macos.rs`, `windows.rs`, `linux.rs`, all `#[cfg(not(target_family = "wasm"))]`. `Chrome { glass: bool, radius: Pixels }` is the only thing the panel's render code reads: with `glass`, surfaces use `p.bg.opacity(..)` and a light hairline border; without it, today's opaque `p.bg`, `p.surface` and `p.card`.
- **macOS, written against `objc2-app-kit` directly.** `Window` implements `HasWindowHandle`, and the handle is GPUI's drawing view (`gpui-pre-macos/src/window.rs:2465`). The material view must not be a subview of it (it would cover GPUI's content): it goes into the window's `contentView`, added `NSWindowOrderingMode::Below` GPUI's view, which is what GPUI's own `Blurred` path does (`window.rs:1908-1917`).
  - `NSVisualEffectView` with `material: HUDWindow`, `blendingMode: BehindWindow`, `cornerRadius: 14`, autoresizing in both axes, and **`state: Active`**. The default, `FollowsWindowActiveState`, would grey the material permanently, because a non-activating `NSPanel` never becomes the active window.
  - `appearance` is set to dark aqua for a dark app theme and to aqua for a light one (`macos::set_appearance`), at install and again whenever the palette global changes (`chrome::follow_theme`). The desktop's own setting is not read.
  - **A black tint of our own over it** (revised 2026-11-25, from a screenshot over a white window). Set to dark aqua, Liquid Glass still comes out light grey over a bright window (about 0.66 grey, brighter where it bends the light), and our text, drawn by GPUI, cannot adapt to it as AppKit's text does. So the root paints black at 60 % over both macOS materials (`hud::LIQUID_GLASS_TINT`, `hud::VFX_TINT`), the least that holds the contract below; it was 0, which read at about 1.7:1. Our tint rather than the glass view's `tintColor`: Apple's tint is not a plain layer, so its effect cannot be computed in a test. Liquid Glass draws its own edge, so the panel paints no hairline over it; the visual-effect view has none and keeps the hairline. `Chrome` carries both (`tint`, `rimmed`).
  - **The window's root is transparent.** `gpui_kit::open_window` wraps the view in `gpui_base::Root`, and gpui-component's root plugin paints that surface with the app theme's opaque background (`gpui-component-0.9.1/src/root.rs:725`). In the panel that covered the material on every desktop, Mica and compositor blur included, until the spike of task 7.14 found it. The panel opens its window with its own `Root` given a transparent background, which is applied after the plugin's style; a test guards it.
  - macOS 26 and later (`NSAppKitVersionNumber >= 2685`): `NSGlassEffectView` with `cornerRadius` and an optional `tintColor` instead. One runtime check, one fallback.
  - **No window shadow** (revised 2026-11-25, found by hand over a white window). GPUI makes every window titled (`Titled | FullSizeContentView`, `gpui-pre-macos-0.3.14/src/window.rs:1087`) and gives a transparent one a background of black at 0.0001 alpha so it keeps a shadow (`window.rs:1937`). The system then shadows the whole window in its own rounded outline, not the panel: during the opening a fixed light shape showed that the panel grew into, and at rest a sliver stayed at each corner, where the system's corner is squarer than ours. Neither material changed it (Liquid Glass and `QMT_MATERIAL=vfx` looked the same). `chrome::install` turns the shadow off (`setHasShadow(false)`) on every macOS window, with or without a material, so nothing is drawn outside what the panel paints; `invalidateShadow` is no longer called. If a shadow is wanted back it must follow the panel's shape: a borderless window with a clear background, its shadow invalidated each step of the opening (not done).
- **Why not `window-vibrancy`.** It is a good reference for the AppKit calls, but its entry points attach the effect to the view behind the window handle, which for GPUI is the drawing view, so the effect would cover the UI. Working around that means handing it a fake handle, which is more code than calling AppKit directly. `objc2`, `objc2-app-kit`, `objc2-foundation` and `raw-window-handle` are already in the lock file through GPUI, so using them adds no new builds, and they are needed anyway for D13 and the window shadow.
- **Windows and Linux** cannot force the backdrop dark: the system picks it. So on these two the panel's root carries a dark tint (`hud::TINT`, 58 % black) that keeps text legible over a light backdrop, and a desktop with no material gets a near-opaque `#14161a`. They need no new crate: `window.set_background_appearance(..)` covers Mica, Mica Alt and blur. Windows picks Mica from build 22621 and blur before it. GPUI 0.3.12 has no query for whether a compositor blurs, so Linux cannot probe `Blurred`: it reports `glass: true` only on Wayland KDE (read from `XDG_CURRENT_DESKTOP`), and X11, GNOME and other desktops get `glass: false`.
- **Legibility is a measured contract (review, 2026-11-25).** By hand, the panel over a bright window was barely readable on macOS: the root paints no tint there (`DARK_MATERIAL_TINT` = 0), and the test that holds the tint under 0.2 (`hud.rs:220-225`) protects that. The legibility test therefore changes from "the tint stays small" to "text passes over the worst case": every text token the panel uses for essential content (`text`, `muted`, and the accent of user messages) keeps 4.5:1 over the panel's surface composited on a white backdrop, using the strongest backdrop the material lets through. How much of a white window each material lets through is a constant per material (`hud::LIQUID_GLASS_WHITE_THROUGH`, `hud::VFX_WHITE_THROUGH`, 0.75): first guessed at 0.2, which let the test pass while the panel was unreadable; now from the screenshot, with margin. The knob is the root's tint above. Essential labels (model, folder, mode, the header title) move from `dim` to `muted`, and the values in the bar's chips (folder, model, agent, reasoning level) to `text` (since D19's revision the bar's labels are `muted`, still held to 4.5:1); `dim` stays for decoration only. The text fields' placeholder, selection and caret take the panel's palette too (`kit::input_colours`, see task 7.29).
- **Reduce Transparency.** When the system asks for it (macOS `accessibilityDisplayShouldReduceTransparency`, Windows "Transparency effects" off), `chrome::install` returns `Chrome { glass: false }` and the panel takes the opaque path. Nothing reads this setting today.
- *Alternative: GPUI's `WindowBackgroundAppearance::Blurred` on macOS too.* Rejected: its blur view is a full-window rectangle with no corner radius (blurred square corners outside the rounded card), no material choice, and no way to set the state or the appearance.

### D15. The panel's own look

The panel is a HUD (Spotlight, Raycast), not a small main window.

- **Two fixed palettes, picked by the app's theme.** `hud_palette(user, glass)` in `src/quick/hud.rs` returns a dark `Palette` for a dark app theme and a light one for a light theme, each with tokens of its own (neither derived from the app's colours) and the app's theme id, so code fences keep the app's syntax colours. The light panel lays less over the macOS materials than the dark one (white 0.35 against black 0.6, tuned by eye): white over an already light blur flattens it, and the light palette's text is darker, so the contract still holds. The accent is the user's: lightened on dark glass, darkened (lightness at most 0.34) on light glass so links and white-on-accent text hold 4.5:1. `Chrome::surfaces` tints black for the dark palette and white for the light one, with a hairline of the opposite colour.
- **It reaches the transcript.** The transcript takes its colours from the global `Palette`, read once per frame (`session_view.rs:5348`, `session_pane.rs:592`) and passed down. `SessionPane` gets an optional palette that replaces the global one there; `md.rs` (5 reads) and `turn_diff.rs:152` take the palette as an argument instead of reading the global. Popovers and tooltips outside the pane stay on the global.
- **Markdown too (task 7.40).** gpui-kit's `TextView` wrapper derives the text, link, code and table colours from the app theme and ignores the surrounding text colour once a style is set, so markdown stayed in the app's colours however the pane was painted. `md.rs` now builds `gpui_kit::base::TextView` with a `TextViewStyle` made from the `Palette` it is given (`md::view_style`), in both windows. Fence colours come from the highlighter `highlight::install` puts in `TextViewDefaults` with the theme, which is the app's syntax theme for the panel too, so no highlighter of our own is needed. Permission cards (`RequestCards::request_card`) take the palette too. Kit widgets drawn in the panel (inputs, buttons) read the app theme, which now agrees with the panel's appearance.
- **Shape.** Corner radius 20. No rule under the header, which becomes a small dim title row. The composer is a floating pill with a 1 px highlight along its top edge instead of a border (in a conversation whose agent offers modes, a 1 px outline in the mode's colour, D20), and no shadow over a material.
- **A new quick chat is one bar**, like Raycast's: the input at 18 px straight on the panel's surface, and under it one line of labels: the agent and folder, the mode, the model (which opens the model picker), the reasoning effort and the session-picker button (D19). No header and no box around the input. The header, with new chat, the session picker and "open in main window", appears with a conversation; its buttons are drawn in the panel's colours. The input rests at one line in the bar and at two in a conversation (`SessionPane::set_composer_bar`).
- **See-through tokens over a material.** `hud_palette(user, glass)`: wells (`bg`: code blocks, tool cards) are 24 % black, raised bits (`card`, `border`) a little white, so the material shows through them instead of being covered by slabs. `surface` stays 86 % opaque, because popovers over the conversation are painted with it. Without a material every token is opaque. In the pane, the fades and halos that blend into a page colour are left out over a material.
- **Transcript.** Assistant text sits directly on the glass, without a card. Your messages are a right-aligned pill tinted with the accent. Tool calls are one-line chips of low contrast (about 8 % white), not bordered cards. These are `Density::Compact` styling, so the main window is untouched.
- **Type.** The system UI font at 14 to 15 px for the conversation; monospace only for tool output and code.
- **Accent use.** Only for the edge light, the focus ring and your messages.
- *Alternative (the first version): always dark, whatever the theme.* Replaced on 2026-11-27. A dark panel over a light desktop looked out of place, and every widget the kit draws took the light theme's colours (dark text on the dark panel, see 7.16, 7.29 and 7.40). The panel's shape (glass, pills, no cards, no header rule, no shadow) is what keeps it from looking like a small main window, not its colour.

### D12. One model picker, two windows

- The picker's state and rendering live in `QmtApp` today (`ui/start.rs:566-684`, `:1377-1730`, plus `delegates.rs`). They move into a `ModelPicker` entity (`src/ui/model_picker.rs`) holding the query input and the open flag, rendering the search, the sections and the reasoning footer, and emitting `ModelPicked { key }`.
- Its target is an enum: `Target::Session(Entity<OpenSession>)` sets the session's `model` option; `Target::Launch { agent }` reports the choice to the caller instead of writing a preference. The start page keeps writing `prefs.launch_models` on `ModelPicked`; the quick panel keeps the key in its own `QuickState.model: Option<String>` and puts it in `LaunchRequest.model`, so a model picked in the panel does not change what the start page starts new sessions with (spec: *Change the model from the panel*).
- `LaunchRequest` gains `model: Option<String>`, applied as a config option at session creation the way the launch default is today. `None` keeps today's behaviour.
- In the panel the picker opens from the bar's model label, from the model name in a conversation's header (D20), and from Cmd/Ctrl+M, as in the main window (`ui/keys.rs:60`). It renders inside the panel like the session picker, so it is not a second window.
- `SessionPaneEvent::ModelPicker` is then handled by the panel itself instead of falling through to "open in the main window" (`quick/panel.rs:234`).

### D13. The window grows with the conversation

```
  new quick chat        first replies           past the cap
  +--------------+      +--------------+        +--------------+
  | header       |      | header       |        | header       |
  | composer     |      | turn 1       |        | ... scrolls  |
  +--------------+      | composer     |        | composer     |
   680 x 260 (bar: 64)  +--------------+        +--------------+
                         grows upward,           stops at
                         bottom edge fixed       min(760, 70% of display)
```

- **A new quick chat is its bar** (D15): 64 px: one line of text over the line of labels (D19). It grows with the lines typed and with an error shown above it (the bar's laid-out height is the floor), and returns to 64. The window is placed where a centred 260 px panel has its bottom edge, so the bar leaves the conversation room to grow upward. Showing a conversation raises the panel to 260 at once, before it is laid out.
- The pane reports its content height; the panel asks for `clamp(content + chrome, 260, min(760, 0.7 * display height))`, in steps of 8 px to avoid a resize on every streamed chunk, and never smaller while a conversation is shown. `new_chat` and the release timer reset it to 260.
- Overlays set a floor: while the session picker or the model picker is open, the requested height is at least what the overlay needs, so it is never clipped (spec: *Picker in a small panel*).
  - **The floor is measured, not estimated (review, 2026-11-25).** `picker_height` adds `PICKER_CHROME + rows * PICKER_ROW` in fixed pixels. It leaves out the gaps between rows (seven 4 px gaps for eight rows) and does not follow the rem scale the rows are laid out in, so the last row was cut off. Overlays report their laid-out height through a `canvas` measure, as the bar does (`bar_h`, `panel.rs:1721`), and the floor is the measured height plus the header. The estimate stays only for the first frame, before anything is laid out.
  - The list layout scrolls under its search row past the panel's limit, as the grid already does, so a short display never clips a row.
  - The model sheet's floor is its search row and first five models (D20).
- **macOS**: `setFrame:display:animate:` through `objc2-app-kit`, with the origin recomputed so the **bottom edge stays fixed** and the composer does not move under the cursor. The window has no shadow to recompute (D11).
- **Windows and X11**: `Window::resize(size)`. GPUI has no reposition API in 0.3.12, so the top-left stays fixed and the panel grows downward. Accepted divergence (D11's stance: the panel is native per desktop, not identical).
- **Wayland**: the compositor places the panel; with `LayerShell` the panel is centred by the compositor and the resize is applied where it sits.
- GPUI applies a macOS resize one frame later, so the growth is driven from the pane's measured height after layout, never during paint, and is rate-limited to one resize per frame.

### D16. Keyboard and the panel's own choices

- `Picks { model, cwd, reasoning }` replaces the lone model the panel kept: all three are the panel's own and survive a close like the model did. `LaunchRequest` gained `reasoning: Option<String>`; `None` is the launch default. A launch still remembers its folder as the last used one, for any surface.
- The folder is chosen in an overlay that reuses `WorkspaceField` (arrows, Enter, Tab, completion), which now emits `Picked`. Enter takes the typed path unless an arrow key moved to a suggestion; Tab completes into the first suggestion. Before, any suggestion that merely matched what was typed was taken, so a typed path could not be confirmed. The agent is named in the bar only when several are configured, to leave the input its width; the folder chip's tooltip names it always.
- `ModelPicker` has a keyboard cursor over the pickable rows (headers and "N more" rows are skipped, the list scrolls to it); it rests on the chosen model, or the first while searching. Enter picks the cursor row, no longer always the first.
- Cmd/Ctrl+1…8 reuse the app's `OpenSidebarSession` action: with the session list open they show that row; closed they do nothing in the panel.
- The compact composer rests at one line (it grew from two) so the placeholder and the buttons share a centre line; the empty bar's height is no longer rounded to steps, which left a gap above it.

### D17. Overlays are part of the panel (review, 2026-11-25)

The session list, the model picker and the folder chooser each drew a card painted with `surface` (`0x1b1e23` at 86 %, `hud.rs:51`), with a border and `shadow_lg` (`panel.rs:1192`, `1310`; `model_picker.rs:771`). Over a material with no tint on the root, that card was the only coloured slab in the panel and read as a dark blue box. The composer, by contrast, already drops its shadow over a material (`session_view.rs:5293`).

- **The session list opens in place.** With the list open, the panel's body shows the list instead of the pane, under the header (a new quick chat shows it above the bar). It is not an absolutely placed card: no surface, border or shadow of its own, rows straight on the panel's surface like the transcript. The pane is kept alive, not rebuilt, so the conversation returns at its scroll position. Escape and Cmd/Ctrl+K close it.
- **Model picker and folder chooser open in place too** (revised 2026-11-25, user decision). First drawn as sheets over the panel on a `popover` token, they read as a dark box: a dimming layer (`black` at 30 %) covered the panel and the card was a near-opaque grey, and a see-through card would have let the transcript show through the list. So they take the conversation's place like the session list: no dimming, card, border or shadow, measured by the same floor (D13). The folder chooser's suggestions are laid out in it (a `WorkspaceField` option), not in a `deferred` dropdown, which took no room in the layout and so fell below the window's edge once the floor was measured. In the panel the model picker is a search field over the rows only (D20).
- **The composer's command list** (`slash.rs:309-312`) still floats, over the transcript inside the composer, so it keeps a box of its own that the transcript must not show through: the neutral `popover` token (`0x161616`, 72 % over a material, opaque without one) and the no-shadow rule. It is now the only overlay in the panel that uses `popover`.
- **`hud.rs` gets one neutral `popover`** in place of the bluish `surface` for every overlay. `surface` stays for the few places that must be opaque over text: tooltips, and the edge cases listed in D15.
- *Alternative: keep the cards and only neutralise their colour.* Rejected (user decision): a floating card inside a floating panel is a second layer of chrome, and the list reads more directly when it takes the conversation's place.

### D18. The panel can be moved, and remembers where (2026-11-25)

- **Drag areas.** The header's empty space in a conversation, and in a new quick chat the bar's background and its row of agent, folder and model. Not the text field or a button: the drag handler sits on dedicated background elements, and controls stop the mouse-down. A press calls `Window::start_window_move()` (AppKit's `performWindowDragWithEvent` on macOS, `_NET_WM_MOVERESIZE` on X11, `xdg_toplevel.move` on Wayland toplevels); on Windows the same elements are a `WindowControlArea::Drag` (`HTCAPTION`). The window is made movable (`is_movable: true`), which AppKit and Windows require.
- **Growing keeps working.** On macOS the growth reads the window's frame when it runs (`chrome/macos.rs`), so a moved panel grows upward from where it was left; Windows and X11 keep the top-left, as before.
- **Where it was left.** On close, the panel's bottom-left corner (the edge D13 keeps fixed) is stored with the display's id and size in `Prefs.quick_positions`, so it survives a restart. On open, a stored corner for that display is used, moved inside the display's visible area if it no longer fits; else the panel is centred as before. Kept per display, so a laptop's position is not applied on an external screen.
- **Wayland layer-shell** surfaces are placed by the compositor: no drag there and nothing is stored. GNOME's normal window moves.
- *Alternative: move with a modifier from anywhere.* Rejected: not discoverable, and the composer takes most of a new quick chat.

### D14. Motion: opening, and a light along the edge while the agent works

- **Opening.** 280 ms, shaped by two curves of our own over a linear `with_animation` (`open_opacity`, `open_scale` in `quick/panel.rs`). Opacity eases in and out (smoothstep) and is full after 64 % of the time, about 180 ms. Scale starts at 0.94 and gathers speed into the full size, which it reaches at the same moment; the remaining 100 ms are a **bump**: it springs back by 1.4 % and settles slowly. The scale never passes 1, because the window is the panel's final size and would clip an overshoot. GPUI has no transform for a `div`, so the scale is an inset of the absolutely placed root. On macOS the material is a view of ours that GPUI's animation does not reach, so each frame a `canvas` in the root hands the same values to AppKit (`chrome::open_step`): the window's `alphaValue`, which fades material and content as one (the root then does not fade on its own), and the material view's frame, scaled around its centre. The window has no system shadow (D11): it was cut to the window's final outline and showed as a fixed shape the panel grew into. The window starts at alpha 0 so the material does not show ahead of the animation. `cx.reduce_motion()` is respected for free, since GPUI renders a single final frame then (`gpui-pre/src/elements/animation.rs:301`), and the window is not made invisible first. The window itself is shown at its final bounds, and the composer is focused before the first frame, so typing during the animation is not lost. **Dismissal is not animated**: the window is removed at once.
- **Working indication: a light along the edge, the same on every desktop.** A 1.5 pt line on the panel's rounded edge with a soft glow inward of at most 14 pt at 12 % or less, so it stays in the 20 px either side of the 640 px transcript column and never runs under text. Only the renderer differs per desktop (below).

  | Activity (below) | What the edge shows |
  |---|---|
  | Running | a highlight with a fading tail travels round the edge; a lap takes 2.4 s for a slow stream, down to 1.2 s for a fast one |
  | a tool call starts or ends (a spark) | the highlight flares once, 250 ms |
  | Waiting | the highlight stops; the whole edge breathes slowly (2.4 s) in the `warn` colour |
  | Error | one soft pulse in `danger`, 0.6 s, then off; no flicker |
  | Done | the highlight finishes its lap, the whole edge lights once and fades, 0.7 s |
  | Idle | nothing, and no frames |

  Changes cross-fade over 200 ms. A run shorter than 150 ms shows nothing (the light waits that long before it appears), and a run shorter than 1 s ends with a fade, without the finishing lap. Colours come from the HUD palette (D15): the accent, `warn` and `danger`. Every number here is a constant in `EdgeMotion`, to tune by eye.
  - *Rejected: the plasma band of the first plan.* A 24 px band of flowing noise runs under the text at the panel's sides (the column leaves 20 px), noise behind text costs legibility, and a red flicker reads as a glitch. The flat test band of the spike (task 7.14) confirmed that a band that wide looks heavy.
  - **Activity: what the edge follows.** The panel derives one `Activity` value from the session it shows, each time that session or its requests change (pure logic in `src/quick/activity.rs`, built on every platform so its tests run everywhere):

    | What the session shows | Phase |
    |---|---|
    | phase `starting`, `model` or `tools`, or a prompt in flight | Running |
    | a tool call starts or ends | Running, plus a spark |
    | phase `waiting`, or a pending request for this session | Waiting |
    | a run ends and the session has an error it did not have before | Error |
    | a run ends without one | Done |
    | idle, no session shown, or another session was just shown | Idle |

    The session has no "done" or "error" phase (`session/projection.rs:829` maps the run state to `idle`, `starting`, `model`, `tools`, `waiting`), so Done and Error are transitions: they are only raised by a run that ends while its session is shown. Showing another session, or a new quick chat, takes that session's state as it is, with no finishing lap and no pulse for an error it already had. The bar of a new quick chat (D15) shows no session, so the edge is off there; a quick chat being created keeps the bar's spinner.
  - **Stream rate.** The client is not told tokens per second. The rate is an estimate: characters appended per second to the reply and the reasoning being streamed, smoothed over about half a second and scaled so that a fast stream is 1. It only sets the lap time.
  - **Speed changes keep the position.** The lap time follows the rate in steps, at most four times a second. The position is anchored, `position(t) = anchor + (t - anchor_t) / lap`, and a new lap time re-anchors at the current position, so the highlight never jumps. A renderer that multiplied time by speed would make it jump at every change.
- **One model, renderers behind a trait.** `src/quick/edge.rs`, pure and built on every platform:
  - `EdgeMotion`: every constant above, in one place.
  - `Edge`: turns `Activity`, the rate and the clock into an `EdgeState` (what is shown, since when, the anchor and the lap time, the colour role, and whether motion is allowed), and says when a flare, a pulse or a finish is over, which then settles the `Tracker` (7.13). It replaces `activity::link`, whose display-link table no renderer needs any more.
  - `frame_at(&EdgeState, t) -> EdgeFrame`: the head's position along the perimeter (0 to 1), the tail's length, the strength of the line and of the glow, and the colour role at time `t`. This is the definition of the motion; its tests are the motion's tests.
  - `trait EdgeLight`: `show(&EdgeState, &EdgeColours) -> Result<(), EdgeError>` (a state that shows nothing hides it) and `resize(width, height) -> Result<(), EdgeError>`. A renderer that draws outside GPUI implements it. The panel holds `Option<Box<dyn EdgeLight>>`: when it is `None`, or a call fails (the panel then drops it for the rest of the panel's life), the panel draws the ring below. Tests put fakes in its place: one that fails, to check the switch, and one that works, to check that the ring is not drawn as well.
- **macOS: Core Animation (`CoreAnimationRim`, `src/quick/chrome/rim.rs`).** A view of our own hosting plain layers, between the material (D11) and GPUI's view: the position the spike proved on both materials (task 7.14). `show` turns the state into layer animations whose timing is `frame_at`'s (the same anchor, lap and durations, with `beginTime` taken from `CACurrentMediaTime`), and the system compositor runs them. There is no frame callback and no work on our main thread between changes, so a busy main thread (renders of 16 to 25 ms were logged in a debug build) does not stall the light. The conversion from state to animation parameters is a pure function, tested against `frame_at` at sample times so the two cannot drift.
  - Layers: the edge's path is a rounded rectangle at the panel's corner radius, inset by half the line width. The highlight is a `CAShapeLayer` stroking that path traced twice, so `strokeStart`/`strokeEnd` can run across the start, with a round cap; the tail is two shorter, fainter segments behind it; the glow is the highlight's shadow (`shadowRadius`, no offset) clipped by a mask of the panel's shape, so it falls inward only. Breath, pulse, flare and finish animate `opacity`, `lineWidth` and `shadowOpacity`.
  - Resizing: `resize` rebuilds the paths inside a `CATransaction` with actions disabled, so Core Animation does not animate the change by itself. It is called where the spike drew: in `chrome::open_step`, which scales the view with the material, and after the deferred resize of D13.
  - Reduced motion and Low Power Mode (`NSProcessInfo.isLowPowerModeEnabled`, read at each change of state): Running and Waiting show a still, lit edge with no animation; Error and Done go straight off.
  - Where it runs: over the material the panel installs (`Chrome::dark_material`), on every macOS version the panel supports. Without the material the panel's root is opaque and would cover it, so the ring is drawn.
- **Windows, Linux and the fallback: the ring, drawn by GPUI from `frame_at`.** A non-interactive `canvas` inside the panel's rounded root strokes the same line and the travelling highlight as a path along the rounded rectangle (`PathBuilder::stroke`, `Window::paint_path`), with the same constants. It replaces the pulsing ring of task 7.5. It requests frames only while the edge is not Idle, and draws one still frame under reduced motion. It runs on the main thread, so a long layout can stall it there. Its cost is path work on the main thread at every frame while the light moves: on a 680 × 260 panel the outline is about 700 points, and up to ten strokes are triangulated per frame, rebuilt each time; task 7.21 builds the points once per size and measures what is left. It must draw below the pickers and overlays: as the root's last child (`panel.rs:1434`) it now draws over the model picker where that reaches the edge (task 7.21).
- **Later, if wanted: Metal.** A procedural effect (noise, a glow that follows the text) would be a `MetalRim` implementing `EdgeLight`: the same place, the same `frame_at`, and a display link on a thread of its own so the main thread cannot stall it. The spike (task 7.14) settled what was unknown: a `CAMetalLayer` composites there on Liquid Glass and on the visual-effect material, and a shader compiled at runtime takes about 0.4 ms. It is not built now, and its code is removed with the spike.
- **Dependencies.** `objc2-quartz-core` (layers, shape layers, animations, transactions) and `objc2-core-graphics` (paths, colours) 0.3.2 are direct macOS dependencies, the versions GPUI builds, so `Cargo.lock` only gains edges. `objc2-metal`, added for the spike, is removed with it.
- **What it cannot do.** Nothing of ours can see or blur the desktop behind the window: that stays the material's job (D11).
- **Why not off the main thread inside GPUI (checked November 2026).** GPUI builds, paints and presents every frame on the main thread, on every platform; nothing in it draws elsewhere. What exists:
  - *Custom shaders: none.* Zed #42905 (custom fragment shaders) was closed unmerged in December 2025 for want of a Zed use case; its continuation in the community fork, gpui-ce#4, was closed unmerged in March 2026; Zed issue #43273 stays open.
  - *`TimedTransformation`* (Zed #66896, merged 13 November 2026): GPUI advances an SVG sprite's scale, translation, rotation and alpha just before presenting a frame, without re-rendering the view (37 to 73 times fewer main-thread instructions per animated frame in its benchmark). It still runs on the main thread and only for SVG sprites, so it could carry the breath and the pulse but not a highlight travelling along the edge. It is not in the GPUI this app builds (`gpui-pre` 0.3.12, a snapshot of `zed@d2a4fb6`) until gpui-kit moves to a newer snapshot.
  - *GPUI Fast* (longbridge's fork; gpui-kit 0.9.1 has an opt-in `gpui-fast` feature): retained mode cuts main-thread time per frame (in its benchmark a spinner goes from 5.91 ms to 0.73 ms), still on the main thread; and window composition (a port of Zed #62379, closed upstream) that puts native surfaces between GPUI's layers on every desktop: AppKit views, DirectComposition visuals on Windows, Wayland subsurfaces, X11 child windows.
  So on macOS the light leaves GPUI for Core Animation (task 7.20); on Windows and Linux the ring stays in GPUI and is made cheap (task 7.21), and a renderer of its own there is a spike (task 7.22).
- **Windows and Linux, later: a renderer outside GPUI.** The same `EdgeLight` and `frame_at`, in a surface of our own that the system composites. On Windows a DirectComposition visual whose animations the composition engine runs, as Core Animation does on macOS; GPUI already presents through DirectComposition (a target for the window with `topmost = false`, `gpui-pre-windows-0.3.12/src/directx_renderer.rs:1017`). On Wayland a subsurface above GPUI's surface, drawn from a thread of our own; GPUI's Linux backend has no subsurfaces. X11 keeps the ring. Whether either can be placed and kept in step with GPUI's window is unknown: task 7.22 decides, as task 7.14 did for macOS, and a working path becomes a change of its own.

### D19. The bar's controls: a line of labels under the prompt (review 2026-11-25, revised 2026-11-26)

The bar of a new quick chat had grown to the input plus up to six controls in one row: the agent's name, the folder, the mode (`quick-panel-status-and-keys` D4), the model, the reasoning effort and the session-picker button (`quick/panel.rs:1944-2035`). It read as overloaded, and on a long prompt the controls stayed on the input's centre line while the text wrapped beside them in a narrow column. The first answer (tags on the model chip, kept below as a rejected alternative) took as much width from the prompt and added colour; the prompt is what the bar is for.

```
+----------------------------------------------------------------------+
|  Reply to the agent...                                               |  18 px, full width
|  [] querymt-gpui   * build   claude-opus-5-5 v   (o) auto         ## |  12 px, one line
+----------------------------------------------------------------------+
```

- **The prompt owns the width.** The input is one row across the bar (648 pt of the 680 pt panel). Under it is one row of labels whose text starts on the input's leading edge; each item's padding hangs outside that line. The session-picker button sits at the row's trailing end; the launch spinner at the trailing end of the input row.
- **Height.** The bar stays 64 px (`BAR_HEIGHT`): 8 px above the 26 px input line, 2 px between, the 22 px row of labels, 6 px below. The visible air above the prompt's text and below the labels' text is then about the same (12 and 11 px).
- **Labels, not chips.** One text size for the row, 12 px, set once on the row so no label can fall back to the window's default. Labels in `p.muted`, glyphs in `p.dim` (decoration). Items are 22 px high with a 6 px radius, no fill at rest; on hover a `p.chip` fill and the label in `p.text`. The folder with its glyph (the agent's name before it when several are configured, D16); the mode as a 6 px dot in `mode_tone` and its name, so the colour is a hint and the name the signal; the model with a 10 px chevron, the one label that opens a list; the reasoning effort as `Brain` and its name, also at `auto`. Widths are capped (agent 90, folder 160, model 220) and truncate.
- **Each label is a button.** `quick-folder` opens the folder chooser, `quick-mode` moves to the next mode (Shift+click the previous one) as Tab and Shift+Tab do, `quick-model` opens the model picker, `quick-reasoning` moves to the next effort as Cmd/Ctrl+T does. Each keeps its tooltip with its key and stops the mouse-down (D18); the gaps between them stay drag area. The `quick-mode:{label}` selector stays.
- **A long prompt** wraps at the full width; the bar grows with it through `bar_h` as before (D13), with the labels under it. Nothing stacks, so the input's height is not measured and there is no hysteresis.
- **Only the bar.** A conversation shows the same things differently: the model and effort in the header and the mode in the composer, with no chips (D20).
- **`kit::badged` stays in `ui/kit.rs`** (moved there by the first answer) for the main window's model pill and mode button, with its one right tag; the HUD size, the left tag and `hud::tag_fill` go.
- *Alternative: mode and reasoning as tags on the model chip, the controls stacking beside the input past two lines (the first answer, 2026-11-25, implemented).* Rejected on use: the tags read as notification badges on the chip's edge; the chip's text fell back to the window's default size; the tags were opaque slabs on glass; two tones competed with the model for attention; and the row still took about 330 pt from the prompt. Its details follow, for the record.
- *Alternative: one segmented capsule (mode | model | reasoning) beside the input.* Rejected: about 420 pt of the row, less room for the prompt than before.
- *Alternative: chrome-less labels on the prompt's row.* Rejected: still about 290 pt, or less only by hiding the labels behind dots and icons.
- *Alternative: hide the controls while typing.* Rejected: it hides what the chat runs with at the moment Enter is pressed.

The first answer, superseded:

- **Mode and reasoning are tags on the model chip,** as the main window's model pill carries the reasoning (`start.rs:588`, `session_view.rs:4270`): a small tag on the chip's top edge, the mode at the left, the reasoning effort at the right. Both are always shown, the reasoning also at `auto`, as in the main window. The bar's own `quick-mode` and `quick-reasoning` chips go away; the chips left are the folder (with the agent's name before it when several are configured, D16) and the model.
- **Tags are buttons.** Clicking the mode tag moves to the next mode (Shift+click the previous one), as Tab and Shift+Tab do; clicking the reasoning tag moves to the next effort, as Cmd/Ctrl+T does. Each keeps its tooltip with that key. They stop the mouse-down, so they are not drag areas (D18). The model chip's own click still opens the model picker.
- **A tag the HUD can carry.** `quick-panel-status-and-keys` D3 kept the mode off the notched badge for two reasons, and both are answered here rather than ignored:
  - *The cut-out.* The badge cuts itself out of the pill with a `p.surface` spread shadow (`start.rs:449-455`), a dark ring on glass. In the panel over a material the tag has no cut-out; it is a filled tag that overlaps the chip's edge. Without a material the cut-out is painted in the bar's own opaque root colour (`Chrome::surfaces`), not `surface`.
  - *Colour as the only signal.* The tag holds its label as text (the mode's name, the effort's name), so the tone is a hint. The label is drawn in `p.text` on a tint of the tone, because mode is an essential label under the legibility contract (D11, task 7.27); the tone's own colour as text is not held to 4.5:1 on a material.
  - *Size.* The main window's tag is 14 px high with 8.25 px text, made for its 12 px toolbar. Next to the bar's 18 px input the panel's tag is 16 px high with 10 px text.
- **One helper.** `badged` and `badged_tone` move from `ui/start.rs` into `ui/kit.rs`, taking a left and a right tag (either optional) and an optional cut-out colour. The main window passes its `surface` and one tag at the right, and looks as before.
- **The model chip has room for both tags.** It gets a minimum width so the two tags do not overlap; each tag truncates at half the chip's width.
- **Layout follows the input's height.** While the input is one or two lines, the controls sit in one row to the right of it, on its centre line, as before. Past two lines they become a column to the right of the input, aligned to the bottom (the edge D13 keeps fixed): the agent and folder, then the model chip with its tags, then the session-picker button.
  - The input's height is measured after layout through a `canvas`, as the bar's is (`bar_h`), not counted from the text, because wrapping counts.
  - **Hysteresis.** A column is narrower than a row, so the input gets wider when the controls stack and can fall back to two lines, which would unstack them again. So the bar stacks when the input is taller than two lines and returns to one row only when it is one line again or the draft is empty.
- **Only the bar.** The compact composer of a conversation keeps its separate mode chip and model chip (`quick-panel-status-and-keys` D3); its layout is already one row beside a pill that grows upward.
- *Alternative: let the row wrap (`flex_wrap`).* Rejected: it wraps by width, not by the input's height, so a long prompt would still keep every control on one line beside a narrow column of text.
- *Alternative: the tags as a second line inside the model chip.* Rejected: it makes the chip taller than the input's line, so at rest the controls would no longer share the input's centre line.

### D20. A conversation shows what it runs with, and does not set it (2026-11-26)

The panel is for a quick look and a quick reply, like Spotlight. It is not the main window made small. In a conversation:
- The composer carried a mode chip and a model chip beside the input (`quick-panel-status-and-keys` D3).
- The mode chip was as wide as its name, so Tab moved the input and the model chip.
- The reasoning effort was shown only inside the model sheet.
- The sheet's title and Done rows took about 100 px of a window that grows to fit them.

```
  Session title              claude-opus-5 · High    72%  [+] [=] [↗]
  ...conversation...
+------------------------------------------------------------------+
| [+] Reply to the agent...                            Build  [^]  |  <- outline in the mode's tone
+------------------------------------------------------------------+
```

- **Rule.** In a conversation the panel shows the model, the reasoning effort and the mode, as text. It has no controls of its own for them. They change by key (Tab/Shift+Tab, Cmd/Ctrl+T, Cmd/Ctrl+M), by clicking the text that shows them, or in the main window (Cmd/Ctrl+Enter). The bar of a new quick chat is unchanged (D19): it is where the user chooses what a chat runs with.
- **Header: model and reasoning effort.**
  - Placed between the drag area and the context meter (`panel.rs:1374-1384`): the model's name, `·`, and the effort's name.
  - Styled as `text_xs` in `p.muted`; the model is capped at 220 and truncates. At rest there is no fill, border, chevron or badge; on hover it takes the bar's label hover.
  - Clicking the model opens the model sheet; clicking the effort moves to the next effort, as Cmd/Ctrl+T does. Tooltips name the value and the key.
  - The effort is left out when the session has no reasoning option. While a change is pending, the text is at 60 % and inert.
  - The effort's box is as wide as the widest of the agent's effort names (the mode's technique), so moving from Low to Medium to High does not move the model (revised 2026-11-26, after testing by hand: the header group is anchored on its right, so a box as wide as its text pushed the model sideways).
  - Both occlude the drag area (D18).
- **Composer: the mode's name.**
  - The compact composer becomes plus, input, mode, send; the model chip goes.
  - The mode is its name in `text_xs` `p.muted`, at the input row's trailing end.
  - Its box is as wide as the widest of the agent's mode names (the bar's technique, `panel.rs:1998-2015`), so changing the mode moves nothing.
  - Click moves to the next mode, Shift+click to the previous; the tooltip shows Tab.
  - It is always shown while the agent offers modes.
- **Composer: the mode's colour.** The pill's 1 px top highlight (D15) becomes a 1 px outline all round in `mode_tone`, at reduced strength. It starts at 45 % and is tuned by hand over a bright and a dark window. Without modes the white highlight stays. The colour is a hint; the name is the signal.
- **Composer: one row, centred.** The compact pill has 6 px above and below its row, so the attach button, the input, the mode and the send button sit on the pill's centre line. It had the main window's 4 px above and 8 px below, which left the row 2 px high; the outline all round made it visible (revised 2026-11-26). The main window's composer keeps its padding.
- **Model sheet: search and list.**
  - In the panel (`PickerFrame::Sheet`) the picker is the search row and the rows only: no title row, reasoning footer, close button or Done.
  - It closes on a pick, Esc, or Cmd/Ctrl+M.
  - The panel makes room for the first five models (`SHEET_MODEL_ROWS`, headings not counted); the rest scroll under the search row.
  - The main window's modal keeps its frame.
- *Alternative: a second row of labels under the composer's input, as in the bar.* Rejected: it puts a control row in the place that should be the emptiest, and adds 22 px.
- *Alternative: labels beside the input.* Rejected in D19 (about 300 px from the prompt).
- *Alternative: the effort as a tag on a model chip.* Rejected in D19 (read as notification badges).
- *Alternative: the mode by colour alone.* Rejected: the spec and the design guides forbid it.

## Risks / Trade-offs

- **The agent keeps streaming every session loaded on the connection.** There is no `session/close`, so this already happens today and grows with more open sessions. → Accepted: inputs for unknown keys cost one hash lookup. A follow-up can use an agent-side unsubscribe if QueryMT adds one.
- **Re-entrancy panics if an `OpenSession` method reaches into the store.** → D2 rule, plus a test that opens, prompts, finishes and reconnects with two open sessions under the GPUI test app; a double lease panics there.
- **X11 floating focus and stacking differ between window managers.** → Spike task before building the panel. The fallback is `Normal` kind with always-on-top requested where the window manager supports it.
- **GNOME has no wlr-layer-shell.** → `Normal` window; the panel may open behind fullscreen apps. Accepted and noted in the release notes.
- **Portal shortcuts need user approval and a portal backend.** → `Hotkey::Unavailable` with the `--quick` hint; the spec covers it.
- **A global shortcut can collide with other apps** (Option+Space is Alfred's default). → Configurable; registration errors are shown; the previous shortcut stays.
- **The split is large**: about 70 UI reads of `store.session` and a 6,300-line `session_view.rs`. → Done in no-behaviour-change steps (Migration Plan). Each step keeps the existing session tests and the `perf`/`split_tests` harnesses green, rebuilt around `SessionPane`.
- **More windows mean more render work when both show one streaming session.** → Each pane syncs its own `Transcript`; rows are cached per pane. Watch the existing transcript telemetry (`transcript_frame_cost`) for the main pane, which must not regress.
- **Closing the main window drops its pane, so its session may close.** → Intended. The sidebar lists it, and reopening the main window starts on the start page as it does on launch.
- **The AppKit material view is attached to a view GPUI owns.** GPUI drops the content view and its subviews when the window closes (`gpui-pre-macos/src/window.rs:1400-1412`), which is what we want, but a future GPUI version could rebuild the view hierarchy under us. → `chrome::macos` installs the view once after the window is open, holds no strong reference beyond the window, and every call is a no-op when the view it expects is missing; the panel still works, without glass.
- **The panel looks different on each desktop** (material, corner radius, growth direction). → Chosen, not accidental (D11, D13): one look faked everywhere would be worse on every desktop. The spec says so, and `Chrome { glass }` keeps the difference to one flag in the UI code.
- **Resizing on every streamed chunk would thrash the compositor.** → 8 px steps, grow-only, one resize per frame at most, and a cap (D13).
- **The edge light costs work while an agent runs.** → On macOS the compositor runs the animations, with no frame callback and nothing on our main thread between changes of state. The GPUI ring requests frames only while the edge is not Idle. Nothing runs while idle, and reduced motion or Low Power Mode give one still frame. Measured in task 7.20: the main thread's time per change, and the CPU of the app and of WindowServer while an agent streams, with and without the light.
- **On Windows and Linux the ring draws on the main thread at display rate while the edge moves, and stalls with it** (GPUI offers no other path, D14). → The outline's points are built once per size, frames are requested only while the light moves, and its cost is measured in task 7.21, with and without GPUI Fast; a renderer outside GPUI is a spike (task 7.22).
- **Two descriptions of the same motion could drift**: the Core Animation timing and `frame_at`. → Both come from one `EdgeState` and one `EdgeMotion`; the conversion to animation parameters is a pure function tested against `frame_at` at sample times.
- **A layer between the material and GPUI's view must follow the window.** → Autoresizing mask, as the material has, and paths rebuilt with actions disabled at each step of the opening and after each resize; checked by hand while the panel grows and opens.
- **The rim may not be created, or GPUI may rebuild its views under it.** → Every `EdgeLight` call returns an error that switches the panel to the ring for the rest of its life; tests cover the switch.
- **Something can paint over the whole window.** The component library's root painted the app theme's background over the material on every desktop, unnoticed until the spike. → The panel's root is transparent, and a test guards it (D11).
- **Recolouring the transcript touches code shared with the main window.** → The palette is passed in, with the global as its default, so the main window's colours are unchanged; the existing tests stay green (task 7.11).
- **Mode colours are positional** (`start::mode_tone`, 8 tones), and the 6th and 7th are close to the panel's `danger` and `running`. → Agents so far offer two to four modes. If one offers more, give the HUD its own mode tones, kept clear of the status colours (D20).
- **In a conversation no chip says the model or effort can be changed** (D20). → Hover state and tooltips with the keys; the bar still shows them as labels.

## Migration Plan

1. Archive `use-agent-session-core`, so the `session-transcript` spec exists.
2. `SessionKey`/`SessionRef`, `Delivery`, and the shared `start_prompt` inside today's single-slot store. No behaviour change.
3. `OpenSession` entity and the weak hub; `store.session` becomes the main pane's handle; `launch` returns the entity. No behaviour change.
4. `SessionPane` extraction and the back stack; `QmtApp` holds one pane. No behaviour change.
5. Compact density and `RequestCards`.
6. `QuickHost`, panel window, hotkey backends, tray, single instance, settings.
7. Chrome, motion, sizing and the shared model picker.
8. End-to-end checks on each platform.

Rollback: steps 2–5 are refactors that can stay. Step 6 is behind the "Quick panel" setting (off by default until step 7 passes), and reverting `src/quick/` plus its `main.rs` call removes it.

## Open Questions

- Whether the 15-minute release time, the 680 px width, the 64 px bar and the 260 px conversation minimum and the 70 %-of-display cap feel right. All are constants to tune after use.
- Which macOS material reads best behind the transcript (`HUDWindow`, `Popover` or `Menu`), and how dark Liquid Glass must be tinted for text to stay legible over a bright window (task 7.19).
- Whether the edge marking a session that is waiting for the user, in a different colour (D14), replaces the request card's own attention cue or only adds to it.
- Whether to build the app on GPUI Fast (gpui-kit's `gpui-fast` feature) for its retained mode: it would make the ring's frames and the main window's cheaper alike, so it is a change of its own, informed by the figures of task 7.21.
- Whether a renderer outside GPUI on Windows and Wayland is worth its cost (task 7.22).
- The edge's constants (D14): the line's width, the glow's width and strength, the lap times, how long the flare, the breath, the pulse and the finish last, the delay before the light appears, and the stream rate that counts as fast.
