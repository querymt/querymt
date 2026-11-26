# Design

## Context

- **Bindings:** all bindings are global (no key context) and are registered in `bind_keys` (`src/ui/keys.rs:50`).
  - The panel handles `OpenModelPicker`, `CycleReasoning`, `ChooseFolder` and `OpenSidebarSession` on its root (`src/quick/panel.rs:2090-2100`).
  - `NewSession` (`cmd-n`) is bound but only `QmtApp` handles it, so in the panel's window it does nothing.
- **Session list triggers:** `toggle_picker` (`panel.rs:636`) is reached only from the header title (`panel.rs:1079-1100`) and the bar's icon button (`panel.rs:1711-1718`).
- **Tooltip text:** written by hand, for example "Model for this chat (Cmd/Ctrl+M)" (`panel.rs:1646`).
- **Kit support:** `Kbd::global_binding_for_action(&action, window)` and `Kbd::format(&Keystroke)` (gpui-component `kbd.rs:92`, `107`) give the platform notation from the real binding.
- **Mode in sessions:** `SessionView::mode_option()` is the agent's mode option.
  - The comfortable composer shows it as a notched badge on Options (`session_view.rs:5368-5375`, colour from `start::mode_tone`).
  - Compact density has no Options control (`session_view.rs:5216`), but Tab and Shift+Tab still call `cycle_session_mode` (`session_view.rs:5250-5257`).
- **Mode in a quick chat:** a quick chat launches through `LaunchRequest` (`panel.rs:807`), which carries model and reasoning but no mode, so the store uses `prefs.mode()`. The bar's chips are built in `render_empty` (`panel.rs:1579-1669`).
- **Context usage:**
  - `session_chrome` computes `usage = view.usage.or_else(|| view.last_used.zip(model_context))` (`session_view.rs:4318`).
  - It draws `kit::ring(frac, 18., colour, p.border)` with the 70% and 90% thresholds and a tooltip (`session_view.rs:4427-4457`).
  - Compact density has no title bar chrome, and the quick header (`panel.rs:1041`) does not use any of it.

## Goals / Non-Goals

**Goals:**
- Every panel control reachable from the keyboard, with its shortcut discoverable in its tooltip.
- One implementation of the context indicator and its thresholds for both windows.

**Non-Goals:**
- The session list's surface and sizing (in `add-quick-session-panel`, the in-place list).
- A command palette inside the panel. Cmd/Ctrl+P stays unbound there, reserved for that.

## Decisions

### D1. `ToggleSessionList` action on Cmd/Ctrl+K; `NewSession` handled by the panel

- **New action:** `ToggleSessionList` in `keys.rs`'s `actions!`, bound with `k("cmd-k", "ctrl-k")`. The panel root handles it with `toggle_picker`; the main window has no handler, so the key does nothing there.
- **`NewSession`:** the panel root handles it with `new_chat`.
- **Text fields:** the kit's input bindings do not use `cmd-k` / `ctrl-k`. The global binding reaches the root from the composer and from the list's search field, so the same key closes the list.
- **Alternatives considered:**
  - Cmd/Ctrl+P, to mirror the main palette. It would conflict with a future palette in the panel.
  - Cmd/Ctrl+L. On Linux desktops it commonly means "location".

### D2. Shortcut labels come from the keymap

- **Helper:** `kit::shortcut_label(action: &dyn Action, window) -> Option<SharedString>` wraps `Kbd::global_binding_for_action` and `Kbd::format`.
- **Tooltips:** a `kit::tip_with_key(text, action)` tooltip builder renders the text and a `Kbd` element. It replaces every hand-written "(Cmd/Ctrl+…)" in `src/quick/panel.rs` and the compact model chip (`session_view.rs:4269`).
- **Digit hints:** the `Cmd/Ctrl+1…8` row hints (`panel.rs:1350`) use the same formatter.
- **Rationale:** the guides require keyboard discoverability, and the binding is the single source of truth. A rebinding cannot leave a stale hint.

### D3. Mode chip shared by both densities' compact layouts

- **Builder:** `SessionPane::mode_chip(option: &SelectOption, p, cx)` returns a 26 px chip in the shape of `compact_model_chip`:
  - a 7 px dot in `mode_tone(...)`;
  - the label in `p.text`;
  - a tooltip "Agent mode: Plan" plus the Tab hint.
  - Clicking it calls `cycle_session_mode(false)`. Shift+click goes back.
- **Placement:** the compact composer row becomes `plus, input, mode_chip, model_chip, actions`.
- **Why a dot and a label, not the notched badge:** the badge's cut-out is a `p.surface` shadow (`start.rs:449-455`), which shows as a dark ring on glass. Colour as the only signal also fails the guides' accessibility checklist, so the label carries the meaning and the tone is a hint.
- **Pending option:** while the option change is pending (`pending_options`), the chip is shown at 60% opacity and is inert, as the model chip already is.
- *Superseded in a conversation by `add-quick-session-panel` D20 (2026-11-26): the mode is plain text in a reserved width with the composer's outline in its tone; the chip and `compact_model_chip` go.*

### D4. Mode for the next quick chat

- **Data:** `Picks.mode: Option<String>` (`src/quick/mod.rs:63`) and `LaunchRequest.mode: Option<String>`. `AgentsStore::launch` uses `request.mode.or(prefs.mode())`.
- **Bar:** the chip is drawn from `LAUNCH_MODES` and `picks.mode`. Tab and Shift+Tab are captured in the bar's input (`IndentInline` / `OutdentInline`, as `session_composer` does) and cycle `picks.mode`.
- **Lifetime:** `Picks` already survives panel reopenings for the quick chat it belongs to, and `new_chat` clears it, so the mode follows the same lifetime as the panel's model choice.
- **Space in the bar:** at 680 px, adding a ~70 px chip leaves the input at least ~240 px with several agents shown. The folder chip's text cap drops from 100 px to 90 px; nothing else changes.

### D5. One context indicator for both windows

- **Extraction:** the main window's ring, colour thresholds and tooltip text move from `session_chrome` into `kit::context_meter(used, size, p) -> Option<Stateful<Div>>` (`None` when `size == 0`). `session_chrome` calls it.
- **Quick panel:** the header calls it between the title and the open-in-main button. From 70% it adds the percentage in `MONO`, `text_xs`, in the ring's colour.
- **Data:** `SessionPane::context_usage(cx) -> Option<(u64, u64)>` exposes the same `usage` expression, so the panel does not re-derive it.
- **Click:** the meter is not clickable in the panel, which has no details panel; the tooltip carries the numbers.

## Risks / Trade-offs

- [Global `cmd-k` may later collide with a main-window binding] → The binding is declared next to the others in `bind_keys`; a future main-window use would need a key context on one side. Noted in a comment there.
- [Tab in the bar used to insert nothing useful] → No behaviour is lost. The bar is one line, and Shift+Enter breaks lines.
- [`Kbd::global_binding_for_action` returns one binding where two exist (`ctrl-b` and `cmd-b` for the sidebar)] → The panel's actions have a single binding each. The helper documents that it shows the highest-precedence one.
