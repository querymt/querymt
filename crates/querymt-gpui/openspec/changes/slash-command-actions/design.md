# Design

## Context

- **Catalog:** the runtime advertises commands from `slash_commands/acp.rs` (registry commands) and `Builtin::entry` (`/compact`, `/worktree`). The only input signal is `AvailableCommandInput::Unstructured { hint }`, set when a command has an argument hint or its template contains `$ARGUMENTS`. `/compact`'s hint is "[optional instructions, …]"; `/worktree`'s hint is its whole subcommand grammar.
- **Native rules:** `Builtin::accepts(via)` already decides delivery per built-in. `/compact` accepts prompt and queue, not steer. `/worktree` accepts the prompt only. The client does not know this, so the palette can queue a `/worktree` that the agent then rejects.
- **Client view:** `SlashCommandView { name, description, input_hint }` in `agent-session-core` (`view.rs:163`), stored as `SessionView.slash_commands` (`src/session.rs:935`).
- **Palette:** `palette_groups` builds `PaletteTarget::SlashCommand { name, takes_input: input_hint.is_some() }` (`src/ui/keys.rs:547`). `SessionPane::run_slash_command` (`src/ui/slash.rs:170`) either fills the composer or sends. When the session is not ready, it returns silently after the palette has already closed (`slash.rs:188`).
- **Kit:** `CommandItem` implements `Disableable`, and disabled rows are skipped by selection and confirm (gpui-component `command/state.rs:259`, `443`). The `Command` context binds only Esc, Enter, Up and Down. `on_confirm` receives an `IndexPath` and no modifier.
- **Transcript:** every `UserMessage` becomes `ItemKind::User` (`src/session/projection.rs:569`) and is drawn as the accent bubble (`src/ui/session_view.rs:3229`). `/worktree` output is `AgentEventKind::WorktreeCommandOutput`, replayed as `SessionUpdate::AgentMessageChunk` (querymt `agent-protocol-types/src/replay.rs:463`) and marked `Durability::Ephemeral` (`events.rs:1384`). It therefore renders as an assistant reply and is lost on reload. A compaction already has its own row (`render_compaction`).

## Goals / Non-Goals

**Goals:**
- One source of truth for how a command behaves: the agent. The client derives nothing from hint text.
- History renders exactly like the live session.

**Non-Goals:**
- Subcommands (`slash-subcommands`).
- Structured argument forms.
- Changes to the composer's own slash list beyond reading the same metadata.

## Decisions

### D1. Command metadata in `_meta.querymt` on each `AvailableCommand`

- **Shape:** `{"input": "none"|"optional"|"required", "native": bool, "accepts": ["prompt","queue","steer"]}`.
- **Built-ins:** derive it from `Builtin` (`accepts` already exists).
- **Registry commands:**
  - `input` is `required` when the template uses `$ARGUMENTS` without a default, `optional` with an `argument-hint` and no required use, and `none` otherwise;
  - `native` is false;
  - `accepts` is all three.
  - A frontmatter key `argument-required: bool` overrides the derivation.
- **Client view:** `agent-session-core` parses this into `SlashCommandView { input: CommandInput, native: bool, accepts: Deliveries }`. Missing `_meta` gives `CommandInput::Required` when a hint exists, else `None`, plus `native: false` and all deliveries. This is the spec's "today's behaviour" fallback.
- **Alternative considered (rejected by the user):** inferring "optional" from a fully bracketed hint in the client. It is fragile and wrong for `/worktree`.

### D2. Palette targets carry the metadata; Tab is the secondary pick

- **Target:** `PaletteTarget::SlashCommand { name, input: CommandInput }` replaces `takes_input`.
- **Enter and click:** `none` and `optional` send; `required` prefills.
- **Tab:** caught with `capture_key_down` on the palette card while a slash row is selected. It reads `CommandState::selected_index()`, maps it through the same `targets` vector, and prefills. Tab on other rows does nothing.
- **Hints:** the tooltip for optional-input rows reads "Enter runs · Tab adds instructions". A `Kbd` hint in the row's second line is used only if the row stays readable at 560 px.
- **Labels:** a required-input label gets a trailing `…`, following the existing "Schedule a prompt…" convention for actions that ask for more.
- **Alternative considered:** a second palette item per command ("compact" and "compact with instructions…"). It doubles the list and splits search ranking.

### D3. Disabled rows instead of silent no-ops

- **Readiness:** `palette_groups` asks `SessionPane::command_readiness(&SlashCommandView, cx) -> Result<(), Blocked>`.
  - It combines `session_ready` (connected, loaded, not read-only) with the delivery the send would need: prompt when idle; queue or steer, per `Delivery`, while running.
  - It checks that delivery against `accepts`.
- **Blocked rows:** use `.disabled(true)` through `Disableable`. The reason is the second line, in `muted`, not `dim` (design guides: a disabled state needs an explanation, not only lower contrast).
- **Reasons:**

  | Blocked | Copy |
  | --- | --- |
  | `NotConnected` | "Agent not connected" |
  | `Loading` | "Session is loading" |
  | `ReadOnly` | "Session is read-only" |
  | `NotWhileRunning` | "Available when the agent is idle" |

- `run_slash_command` keeps its guard as a second line of defence, but now logs at debug level.

### D4. Command invocations are recorded by the core, not guessed by the client

- **Core:** `UserMessage` gains `command: Option<CommandRun { name, args, native }>`, set by the runtime when `Builtin::parse` or registry expansion matched. For registry commands, the expanded prompt the model sees stays out of the user row; the row shows what the user typed.
- **Native output:** it becomes a durable event replayed as a new update kind. A `querymt/command_output` extension `session/update` carries `{ user_message_id, text }`, so ACP clients that do not know it ignore it instead of showing it as a model reply. `agent-session-core` attaches it to the `UserMessage` as `command.output`.
- **Client projection:**
  - `UserMessage` with a `command` becomes a new `ItemKind::Command { name, args, output, status }`;
  - `/compact` is skipped when the turn's next entry is a compaction, and its args are passed into that compaction row's details.
- **Fallback for older agents:** a user message whose first word is an advertised command name is projected as `ItemKind::Command` without output. Older native output still arrives as assistant text; that is accepted until the agent upgrades.
- **Alternative considered:** client-only classification. It cannot fix the ephemeral output or the history of sessions whose catalog has not arrived yet.

### D5. The command row's look

- **Row:** full-width, left-aligned like the agent's column, not right-aligned like prompts, because it is an action taken on the session, not a message to the model.
  - `SquareSlash` icon in `muted`;
  - `/name` in `text` with `MONO`; args in `muted` `MONO`, truncated with the full text in a tooltip;
  - a status mark on the right: spinner while running, check, or `CircleAlert` in `danger`;
  - no fill, and a 1 px `border` outline at the theme radius, so it reads as a control record, not a bubble.
- **Output:** below the row, in a collapsible mono block using the existing tool-output block and its line caps (`Density::output_lines`), collapsed by default in compact density. Copy is on hover, as for prompts.
- **Tokens only:** `p.text`, `p.muted`, `p.border`, `p.danger`, `kit::MONO`; no raw colours (design guides, "Tokens before values").
- **Both densities:** the same row; compact density uses the 26 px chip height already used for tool rows.

## Risks / Trade-offs

- [The core change is in another repository and gates D1 and D4] → The client parts are written against the new `SlashCommandView` behind defaults. Tasks 1.x land the core first; client tasks start after the pinned revision is bumped.
- [New `session/update` kind ignored by other ACP clients] → That is intended: they currently show it as a model reply, which is the bug.
- [Registry arity derivation could misclassify a template] → The `argument-required` frontmatter override; the default for a hint without `$ARGUMENTS` is `optional`, the safer side, because sending without arguments is visible and reversible.
- [Tab in the palette may surprise users who expect focus traversal] → The palette's search input has no other focus target; the tooltip names the key.

## Migration Plan

1. Land the querymt change. Old clients ignore `_meta` and the new update kind.
2. Bump `querymt-agent-session-core` here.
3. Ship the client.

Rollback is reverting the client; the agent's additions are inert for old clients.

## Open Questions

- Whether `/compact`'s instructions should also be visible in the collapsed compaction row or only in its details. This is a visual choice that can be settled in review of task 4.3.
