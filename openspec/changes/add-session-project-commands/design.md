# Design

## Context

See proposal.md for the motivation. Paths are under `crates/`.

- **The startup registry.** `agent/src/api/agent.rs` builds it once, from `search_paths(cwd, include_global, include_project, paths)` with the builder's `cwd`, and stores it as `AgentConfig.slash_command_registry`. It also keeps the discovery config as `AgentConfig.slash_commands_config`, which is `Some` only when discovery is enabled.
- **A session's registry.** `agent-runtime/src/agent/core.rs` `SessionRuntime.slash_registry` is a `OnceCell`; `effective_slash_registry(startup)` returns it when set and the startup registry otherwise. The expansion (`session_actor.rs`, the steer check and the prompt path) and the live catalog (`GetSessionSlashRegistry`) both read it, so a live session's catalog and expansion cannot diverge.
- **Who sets it.** Only `session_materializer.rs` `materialize_session_actor`, and only inside the block for a session bound to a managed workspace (`workspace_service` present, the cwd inside a registered checkout). That block calls `SlashCommandRegistry::registry_for_workspace(config, &repo.checkout.root)`.
- **An idle session.** `agent/handle/core.rs` `session_user_registry` falls back to `workspace_catalog_registry`, which returns `None` unless a workspace service exists and the session is managed (or its stored cwd is inside a registered checkout). `None` means the startup registry.
- **The decision being replaced.** The comment on `SessionRuntime.slash_registry` and the test `managed_sessions_expand_and_advertise_their_own_checkout_commands` (`agent/src/runtime_tests/handle/worktree_slash_commands.rs`) state that legacy local sessions keep the agent-level registry. That was a deliberate part of the worktree feature (kept the existing behaviour for sessions it did not touch); it was never a requirement of a spec. `git-worktree-workspaces` only requires the isolated case.
- **Project root rule.** `registry_for_workspace` takes a root; for managed sessions that root is `resolve_repository(cwd).checkout.root`, the root of the Git working tree containing the session's cwd. `resolve_repository` fails for a directory that is not in a Git repository.

## Goals / Non-Goals

**Goals:**
- Every local session sees the project commands of its own folder, advertised and expanded from the same registry.
- One function decides a session's registry; the materializer and the idle catalog both call it.

**Non-Goals:**
- See proposal.md.

## Decisions

### D1. One function builds a session's registry from its cwd

- `SlashCommandRegistry::registry_for_session_cwd(config, cwd) -> Self` in `slash_commands/registry.rs`:
  - project root = `resolve_repository(cwd).checkout.root`, or `cwd` itself when that fails (not a Git repository, bare repository, unreadable);
  - then `registry_for_workspace(config, &root)`.
- Rule for the root, and why:
  - **Git root, then cwd.** A session started in `crates/foo` of a repository finds the repository's `.qmt/commands`, as it does inside a managed worktree. A directory outside Git has no better root than itself.
  - **No upward search beyond the Git root.** A non-Git cwd does not climb to a parent that happens to hold `.qmt/commands`; that would make a command in `~/.qmt/commands`-like folders above the project apply by accident.
- Alternative considered: the cwd only, with no Git lookup. Rejected: a session in a subfolder would see no project commands, and managed sessions would follow a different rule from the rest.
- Alternative considered: the nearest ancestor that contains a `.qmt/commands` directory. Rejected: the result changes when someone creates the directory in a parent, and managed sessions would still follow another rule.

### D2. The materializer calls it for every local session

- In `materialize_session_actor`, after the managed-workspace block: when `slash_registry` is still `None`, `slash_commands_config` is `Some` and enabled, and `cwd` is `Some`, set `slash_registry` to the function's result.
- The managed branch keeps its own call (its root comes from the same `resolve_repository`, so the result is equal); it may be folded into the shared call, but its tests must stay green either way.
- A session without a cwd, or an agent whose discovery is off or unconfigured, still has no session registry, so it keeps `effective_slash_registry`'s startup fallback.
- Rewrite the doc comment on `SessionRuntime.slash_registry`: unset only when the session has no cwd or discovery is unconfigured.

### D3. The idle catalog uses the same rule

- `workspace_catalog_registry` becomes `session_catalog_registry`:
  - `None` when `slash_commands_config` is absent or disabled (unchanged);
  - a managed session (a binding from the workspace service) uses its `effective_cwd` (unchanged);
  - otherwise the session's stored cwd, from the history store, through D1. It no longer needs a workspace service or a registered checkout.
  - `None` when the session has no stored cwd.
- A live session's catalog is unchanged: it asks the actor.
- The result is the same registry the actor builds at materialization, because both call D1 with the same cwd.

### D4. What stays the same

- `SlashCommandsConfig` and its keys.
- Which sources `search_paths` adds, their priorities (configured over project over global) and the scripts policy.
- The startup registry: it still serves a session with no cwd, and it is still logged at startup ("Discovered slash commands").
- The catalog's shape and the invocation record (`add-slash-command-records`).

### D5. Cost

- Materialization does one Git discovery (already done for the mutation gate a few lines later) and a few directory reads. The idle catalog does the same on every request that needs it, as it already does for managed sessions. No cache: the cost is small and a cache would serve stale commands.

## Risks / Trade-offs

- **The behaviour change for sessions outside a managed worktree.** A command that exists only in the startup directory disappears from sessions started elsewhere; a project command that shadowed a global one now shadows it only in its own folder. → Accepted: it is the bug. CHANGELOG lists it as a behaviour change.
- **A repository's commands are offered as soon as a session opens in it.** A cloned repository can define `.qmt/commands/review.md`, which outranks the user's global `review`. → Not new: managed sessions and the startup directory already work this way. The command runs only when the user types it, scripts are off by default (`scripts.enabled`), and the app can show the expanded prompt (`slash-command-transcript`). A trust prompt is a non-goal; if wanted, it is a separate change that applies to all project commands.
- **Edits during a live session are not picked up.** → Same as managed sessions; the next open, load or resume reads the folder again. Stated in the proposal as a non-goal.
- **The old test asserts the replaced behaviour.** → Task 1.1 changes its first half to the new behaviour; the managed half stays.

## Migration Plan

1. Land the change. No stored data changes.
2. Rollback: revert it. Sessions outside managed worktrees return to the startup registry.
