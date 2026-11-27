# Proposal

## Why

A session does not see the project commands of its own folder. Open a session in a repository that has `.qmt/commands/explain.md`, type `/`, and `explain` is not offered; typing `/explain` runs nothing from that file.

The agent builds its command registry once, at startup, from the agent config's `cwd`. A session only re-reads commands from its own folder when it runs in a managed worktree. Every other session uses the startup registry (`SessionRuntime.slash_registry` is documented as "unset for legacy local sessions, which keep the agent-level registry"). A `qmtcode` started with its embedded config has no `cwd`, so its startup registry holds only the global commands, and project commands never show up for any ordinary session.

It was found while checking `slash-command-transcript` by hand: the `/explain` command file had to be copied to `~/.qmt/commands` before the app listed it.

## What Changes

- **A session's project commands come from its own folder.** At materialization the session resolves its project root and reads `<root>/.qmt/commands` from there, the same rule managed sessions already use: the root of the Git checkout containing the session's cwd, or the cwd itself when it is not in a Git repository.
- **The catalog and the expansion agree.** A live session advertises exactly the registry it expands prompts with; an idle session (no live actor) re-resolves from its stored cwd. Both use one function.
- **Unchanged:** global and configured paths, the scripts policy and the discovery switches follow the agent's `slash_commands` config as before. A session with no cwd, an agent without a slash config, and a session hosted on a mesh peer keep the agent-level registry (or none).
- **BREAKING (behaviour):** a session that runs outside a managed worktree no longer uses the startup registry's project commands. Its own folder's commands replace them. Today `/deploy` in such a session runs the startup directory's `deploy.md`; after this change it runs the session folder's, and a command that only exists in the startup directory is no longer offered there.

## Non-goals

- Reloading commands while a live session runs. Changes to command files are read when the session next materializes (open, load, resume), as for managed sessions.
- Watching the folder, or a "reload commands" action.
- A trust prompt for the commands of a repository the user has never run before (see design.md, Risks).
- Remote (mesh) sessions. Their commands are not visible to this host.
- Commands from folders other than the session's project root (the session's cwd may be a subfolder; ancestors above the Git root are not searched).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: adds where a session's project commands come from.

## Impact

- **Package `querymt-agent-runtime`:**
  - `slash_commands/registry.rs`: one function that builds a session's registry from its cwd.
  - `agent/session_materializer.rs`: the session's registry is built from its cwd, not only for managed sessions.
  - `agent/handle/core.rs`: the catalog of an idle session uses the same function (`workspace_catalog_registry`).
  - Runtime docs on `SessionRuntime.slash_registry`.
- **Package `querymt-agent` (tests):** `runtime_tests/handle/worktree_slash_commands.rs` asserts the old behaviour ("legacy local sessions keep the agent-level registry"); that assertion changes. New tests next to it.
- **No wire changes.** The catalog and the invocation record keep their shape; only which commands they hold changes.
- **Docs:** `docs/docs/agent/slash-commands.md` (where project commands are searched), `CHANGELOG.md` Unreleased (the behaviour change).
- **Related:** `git-worktree-workspaces` already requires that for managed sessions "the advertised command and its actual expansion/execution use the isolated workspace sources". This change extends the same rule to every local session; it does not edit that spec.
- **Archive order:** independent of the other slash changes. If `add-slash-command-records` is archived first, nothing here changes; its catalog descriptor reads `source` from whichever registry the session ended up with.
