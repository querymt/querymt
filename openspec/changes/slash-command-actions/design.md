# Design

## Context

See proposal.md for the motivation. Paths are relative to `crates/querymt-gpui`, which is a member of this workspace and depends on `querymt-agent-session-core` by path. The current state:

- **Catalog in the app.** `SessionView.slash_commands` (`src/session.rs`) holds `querymt_agent_session_core::view::SlashCommandView { name, description, input_hint }`. `SessionPane::slash_commands` (`src/ui/slash.rs`) returns it, or nothing for a read-only session.
- **Palette.** `App::palette_groups` (`src/ui/keys.rs`) builds `PaletteTarget::SlashCommand { name, takes_input: input_hint.is_some() }`. `App::run_palette` sets `palette_open = false` before it looks at the target. `SessionPane::run_slash_command` then fills the composer or sends; when the session is not ready it does neither.
- **Readiness.** `SessionPane::session_ready` (`src/ui/session_view.rs`) is one boolean: connected, not read-only, loaded, no load error, and a running turn can be controlled. `send_session_prompt` returns silently when it is false.
- **Delivery.** `OpenSession::send` (`src/session/open.rs`) queues any text that starts with `/` while a turn runs, and otherwise follows the caller's `Delivery`. `SessionPane.delivery` defaults to steer.
- **Kit.** gpui-component 0.11 `Command`:
  - binds Esc, Enter, Up and Down only;
  - has `on_select`, `on_confirm(IndexPath)`, `footer`, and `CommandItem::child` for custom row content;
  - skips disabled items when it picks the highlighted row (`CommandState::selected_index`), so the highlight moves to the first enabled row.
- **Agent rules** (`add-slash-command-records`): `SlashCommandView` gains `source`, `effect`, `input` (each an `Option`) and `accepts: Option<Vec<..>>`, each `None` when the agent sends no descriptor. No command is steerable. `/worktree` accepts only `session/prompt`.

## Goals / Non-Goals

**Goals:**
- One function answers "what is this command and can the session take it now", used by the palette, the composer and the send path.
- The app decides nothing from hint text or from a leading `/`.

**Non-Goals:**
- See proposal.md. In addition: no change to palette ranking or grouping.

## Decisions

### D1. One resolver turns the agent's view into what the app acts on

- New module `src/session/commands.rs` (no UI types, so `open.rs` can use it):
  - `CommandTraits::of(&SlashCommandView) -> CommandTraits { input: InputNeed, accepts_queue: bool, effect: Option<Effect>, source: Option<Source> }`, with `is_prompt()` (`effect == Some(Prompt)`) and `is_action()` (`effect == Some(Action)`);
  - `recognise(text, &[SlashCommandView]) -> Option<&SlashCommandView>`.
- `CommandTraits::of` is the only place with the no-descriptor fallback, decided per field from `None`:
  - `input` `None`: a hint means `Required`, no hint means `None`;
  - `accepts` `None`: `accepts_queue` is true; `Some(list)`: true exactly when the list holds `queue` (an empty list accepts nothing);
  - `effect` and `source` are copied, so they stay `None`.
- A command without an effect is neither a prompt nor an action. No caller may read `!is_prompt()` as "action" or `!is_action()` as "prompt"; `add-slash-command-replies` (pending command rows) relies on this for agents that send no descriptor.
- `recognise` follows the agent's own rule (querymt `Builtin::parse`, `try_parse_invocation`): leading whitespace skipped, then `/`, then a word that equals an advertised name exactly and is followed by whitespace or the end.
- Alternative considered: keeping the fallback in `agent-session-core`. Rejected: the core reports what the agent sent; what an absent field means is this app's policy.

### D2. `OpenSession::send` asks `recognise`, not `starts_with('/')`

- The queue-or-steer choice in `send` becomes: a recognised command or `Delivery::Queue` queues; everything else steers.
- A recognised command that does not accept queueing must not reach the running-turn branch. The pane blocks it (D3). `send` keeps a guard for other callers: it adds the notice "Available when the turn ends" and sends nothing.
- Callers do not change: `SessionPane::run_slash_command`, `send_session_prompt`, and the two test callers pass text and the pane's delivery as today.

### D3. Readiness returns a reason

- `SessionPane::readiness(command: Option<&SlashCommandView>, cx) -> Result<(), Blocked>` replaces the body of `session_ready`, which becomes `readiness(None, cx).is_ok()`.
- Checks, in order:

  | Condition | `Blocked` | Copy |
  | --- | --- | --- |
  | agent not connected | `NotConnected` | "Agent not connected" |
  | session loading | `Loading` | "Session is loading" |
  | load error | `LoadFailed` | "Session could not be loaded" |
  | turn running and it cannot be controlled, or the command does not accept queueing | `TurnRunning` | "Available when the turn ends" |

- A read-only session keeps listing no commands (`SessionPane::slash_commands`), so it needs no reason.
- `send_session_prompt` calls `readiness(recognise(text), cx)`. When it is blocked and the text is a command, the text stays and the reason shows in the composer's hint row (`slash_hint` / `render_slash_hint`), which already sits under the composer.

### D4. Unavailable palette rows stay selectable

- `PaletteTarget::SlashCommand { name, input: InputNeed, blocked: Option<Blocked> }` replaces `takes_input`.
- `App::run_palette` returns before `palette_open = false` when the target is a blocked slash command.
- The row is drawn dimmed (label and icon in `p.dim`, the reason in `p.muted`) but is not a kit-disabled item.
- Why not `Disableable::disabled(true)`: the kit would move the highlight to the first enabled row. With `worktree` typed and its row disabled, Enter would run whatever is listed next. The main spec promises that the first row is the best match and the one Enter acts on.
- Trade-off: the highlight can rest on a row that does nothing. The reason on the row explains it.

### D5. Row content

- Every slash row uses `CommandItem::child` (rows with a description already do):
  - first line: the bare name, plus `…` when `input` is `Required`; at the row's end, `Prompt` in `text_xs` and `p.muted` when `is_prompt()` is true;
  - second line: the description, or the `Blocked` copy.
- `PaletteEntry` gains `mark: Option<SharedString>`; `description` carries the reason when blocked.
- Keywords add the source name (`builtin`, `global`, `project`, `configured`) and `prompt` for prompt commands.
- Tokens only (`p.text`, `p.muted`, `p.dim`); no new colours, and no chip or fill for the mark, to keep the row at its current height.
- Alternative considered: two groups ("Commands", "Prompt commands"). Rejected: it changes the group-order requirement and splits ranking for a one-word distinction.

### D6. Enter, Tab and the key hint

- Enter and click on an available row: `None` and `Optional` submit; `Required` prefills.
- Tab: `capture_key_down` on the palette card. For an unmodified Tab it resolves the highlighted target (`CommandState::selected_index` into the groups `filter_palette(self.palette_groups(cx), query)` gives, the same ones `palette_view` renders) and:
  - on an available slash row with `Optional` or `Required` input, prefills and closes the palette;
  - on any other row, stops the key and does nothing, so focus cannot leave the search field.
- Key hint: `Command::footer` renders "Enter to run · Tab to add arguments" with kit `Kbd` while the highlighted target is an available slash row with `Optional` input. `Command::on_select` notifies `App` so the footer follows the highlight.
- Alternative considered: a tooltip on the row. Rejected: the palette is driven from the keyboard, and a hover tooltip is not seen.

### D7. Prefill keeps the draft

- `SessionPane::run_slash_command(name, input, window, cx)` prefills through one helper:
  - an empty composer becomes `/name `;
  - a draft whose first word is `/word` has that word replaced (the existing `complete`);
  - any other draft becomes the arguments: `/name <draft>`.
- Submitting (`None`, `Optional`) leaves the composer's draft untouched, as today.

## Risks / Trade-offs

- **Enter on `compact` compacts at once.** A mis-pick replaces the context with a summary. → The footer names what Enter does. `slash-subcommands` adds the rule for destructive commands; if `compact` needs a guard, the descriptor is where it would be stated, in a later change.
- **The descriptor comes from another change.** → `add-slash-command-records` lands first in this workspace (task 1.1 checks it). With no descriptor every decision falls back to today's behaviour (D1).
- **Tab is taken from the search field.** → The palette has no other focus target.
- **A prompt-only command waits for the turn to end although the agent would take it.** → Accepted for now: starting a prompt while a turn runs would need its own run-state handling in `OpenSession`.
- **`…` and `Prompt` add text to a 560 px row.** → The name truncates before the mark; task 3.3 checks the longest advertised names.

## Migration Plan

1. Land `add-slash-command-records` in this workspace. No revision pin: `querymt-gpui` uses `querymt-agent-session-core` by path.
2. Ship this change. No stored data changes.
3. Rollback: revert this change; the agent's descriptor is ignored by the previous build.
