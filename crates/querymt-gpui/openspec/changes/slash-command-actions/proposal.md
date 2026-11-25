# Proposal

## Why

Slash commands are actions, but the app treats them as text. Picking `compact` in the command palette only writes `/compact ` into the composer, because the agent's only signal is a free-text input hint, and an optional hint looks the same as a required one. Once a command is sent, the conversation shows it as an ordinary user prompt in an accent bubble. The output of a native command such as `/worktree list` arrives as an assistant message, so it reads like a model reply, and it is not stored, so it is gone after a reload. Users cannot tell a deterministic command from an LLM request, either before or after running it.

## What Changes

- **Command metadata from the agent (querymt core).** Each advertised command carries, under `_meta.querymt`:
  - whether its input is `none`, `optional` or `required`;
  - whether the agent runs it itself (native) or expands it into a prompt;
  - how it may be delivered (prompt, queue, steer).
  - Agents that send none of this keep today's behaviour.
- **Palette runs optional-input commands.** Enter on a command whose input is optional sends it at once. Tab puts it in the composer to add arguments. A command whose input is required is still put in the composer, and its row ends in `…` so the difference shows before picking. **BREAKING** for the spec scenario "Command with input", which today requires `/compact` to fill the composer.
- **Palette respects session state.** A command offered while the session cannot take it (not connected, loading, read-only, or a native command that cannot be queued during a run) is shown disabled with the reason, instead of closing the palette and doing nothing.
- **Commands look like commands in the conversation.** A sent command is drawn as a command row (mono name and arguments, a command icon, no bubble), not as a user prompt. A native command's output is drawn under it as command output, not as an assistant reply, and survives a reload. `/compact` folds into the compaction row it produces.
- **Core: command runs are recorded.** querymt marks a user message that invoked a command and stores native command output durably, so history renders the same as the live session.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`:
  - running from the palette depends on the command's input arity;
  - commands are disabled when the session cannot run them;
  - commands and native command output have their own presentation in the conversation;
  - the app reads the agent's command metadata.

## Impact

- **querymt (separate repository; must land first):**
  - `agent-runtime/src/slash_commands/{acp.rs,builtin.rs}` advertise the `_meta.querymt` fields.
  - `agent-session-core` exposes them on `SlashCommandView` and adds a command marker to `UserMessage` (or a command entry kind).
  - `agent-protocol-types` makes `WorktreeCommandOutput` durable and replays it as command output instead of `AgentMessageChunk` (`replay.rs:463`).
- **This repository:**
  - `Cargo.toml` bumps the `querymt-agent-session-core` revision.
  - `src/ui/keys.rs`: palette targets, arity and disabled state.
  - `src/ui/slash.rs`: `run_slash_command`, and the composer list markers.
  - `src/session/projection.rs` and `src/session.rs`: a command item kind.
  - `src/ui/session_view.rs`: rendering the command and output rows.
  - Tests in `src/ui/keys.rs`, `src/ui/slash.rs` and the session view.
- **Compatibility:** agents without the metadata behave exactly as today. History written before the core change shows commands as plain prompts.
- **Related:** `slash-subcommands` builds on the same metadata.
