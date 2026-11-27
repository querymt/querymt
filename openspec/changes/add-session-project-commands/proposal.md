# Proposal

## Why

A session does not see the project commands of its own folder. Open a session in a repository that has `.qmt/commands/explain.md`, type `/`, and `explain` is not offered; typing `/explain` runs nothing from that file.

The agent builds its command registry once, at startup, from the agent config's `cwd`. A session only re-reads commands from its own folder when it runs in a managed worktree. Every other session uses the startup registry (`SessionRuntime.slash_registry` is documented as "unset for legacy local sessions, which keep the agent-level registry"). A `qmtcode` started with its embedded config has no `cwd`, so its startup registry holds only the global commands, and project commands never show up for any ordinary session.

It was found while checking `slash-command-transcript` by hand: the `/explain` command file had to be copied to `~/.qmt/commands` before the app listed it.

Command files are also read only once: global ones at agent startup, a managed session's at materialization. A new or edited file in `~/.qmt/commands` or `<root>/.qmt/commands` takes effect only after the agent restarts, and the app learns the catalog only from the session/new, load, resume and fork responses.

## What Changes

- **A session's project commands come from its own folder.** At materialization the session resolves its project root and reads `<root>/.qmt/commands` from there, the same rule managed sessions already use: the root of the Git checkout containing the session's cwd, or the cwd itself when it is not in a Git repository.
- **The catalog and the expansion agree.** A live session advertises exactly the registry it expands prompts with; an idle session (no live actor) re-resolves from its stored cwd. Both use one function.
- **Command files are read without a restart.** When a file in `~/.qmt/commands` or in a session's `<root>/.qmt/commands` is added, edited or removed, the agent rebuilds the affected registries. A live session expands from the new files on its next prompt, and the agent sends its clients a fresh `available_commands_update`, so the palette and composer list change without reopening the session. It uses the existing file watchers (the workspace index watcher), not a new one.
- **Unchanged:** global and configured paths, the scripts policy and the discovery switches follow the agent's `slash_commands` config as before. A session with no cwd, an agent without a slash config, and a session hosted on a mesh peer keep the agent-level registry (or none).
- **BREAKING (behaviour):** a session that runs outside a managed worktree no longer uses the startup registry's project commands. Its own folder's commands replace them. Today `/deploy` in such a session runs the startup directory's `deploy.md`; after this change it runs the session folder's, and a command that only exists in the startup directory is no longer offered there.

## Non-goals

- Reloading configured `slash_commands.paths` without a restart. They are read at startup and at materialization, as today.
- Live reload of a `.qmt/commands` folder the repository's `.gitignore` covers. The workspace index ignores it, so its changes are read when the session next materializes (open, load, resume).
- Watching a `~/.qmt/commands` folder that did not exist when the agent started. It is read at the next agent start.
- A "reload commands" action.
- A trust prompt for the commands of a repository the user has never run before (see design.md, Risks).
- Remote (mesh) sessions. Their commands are not visible to this host.
- Commands from folders other than the session's project root (the session's cwd may be a subfolder; ancestors above the Git root are not searched).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: adds where a session's project commands come from, and that global and project command files are read without a restart.

## Impact

- **Package `querymt-agent-runtime`:**
  - `slash_commands/registry.rs`: one function that builds a session's registry from its cwd.
  - `agent/session_materializer.rs`: the session's registry is built from its cwd, not only for managed sessions.
  - `agent/handle/core.rs`: the catalog of an idle session uses the same function (`workspace_catalog_registry`).
  - Runtime docs on `SessionRuntime.slash_registry`.
  - `agent/core.rs`: `SessionRuntime.slash_registry` becomes replaceable; the agent-level registry too.
  - The session's subscription to its project root's workspace index (`session_materializer.rs`), and the ACP translation of the new event (`acp/shared.rs`).
- **Package `querymt-agent-protocol-types`:** a new ephemeral event, `AgentEventKind::SlashCommandsChanged`. `AgentEventKind` is `#[typeshare]`, so the generated types change. It is never journaled.
- **Package `querymt-agent`:** `api/agent.rs` watches `~/.qmt/commands` for the agent's lifetime.
- **Package `querymt-agent` (tests):** `runtime_tests/handle/worktree_slash_commands.rs` asserts the old behaviour ("legacy local sessions keep the agent-level registry"); that assertion changes. New tests next to it.
- **ACP behaviour (additive):** `available_commands_update` can now arrive mid-session, whenever a command file changes. The catalog and the invocation record keep their shape. Session core already replaces its list on every update, so `querymt-gpui` needs no change.
- **querymt-desktop follow-up (not planned here):** regenerate typeshare types for the new event kind; handle `available_commands_update` at any time, if it reads it only after load.
- **Docs:** `docs/docs/agent/slash-commands.md` (where project commands are searched; "Commands are discovered at startup" and "Restart application to reload commands" are replaced), `CHANGELOG.md` Unreleased (both behaviour changes).
- **Related:** `git-worktree-workspaces` already requires that for managed sessions "the advertised command and its actual expansion/execution use the isolated workspace sources". This change extends the same rule to every local session; it does not edit that spec.
- **Archive order:** independent of the other slash changes. If `add-slash-command-records` is archived first, nothing here changes; its catalog descriptor reads `source` from whichever registry the session ended up with.
