# Tasks

Runtime commands use the CI feature set `remote,oauth,plugin-loaders,api` (`cargo t-runtime`, `cargo cl-runtime`). Facade commands (`cargo t-agent`, `cargo cl-agent`) never build the wasm UI and need no `QMT_UI_DIST` (see `.cargo/config.toml`). Paths are under `crates/`.

## 1. Session registry from the session's folder

- [ ] 1.1 Add `SlashCommandRegistry::registry_for_session_cwd(config, cwd)` in `agent-runtime/src/slash_commands/registry.rs` (design D1): project root is the Git checkout root of `cwd`, else `cwd` itself. Covers `slash-commands` / Session in a subfolder of a repository, Folder outside Git does not search parents. Unit tests in that file next to `registry_for_workspace_resolves_prompt_sources_from_the_checkout`: `session_registry_uses_the_git_root_of_a_subfolder`, `session_registry_uses_a_non_git_folder_itself`, `session_registry_does_not_search_parents_outside_git`. Verify: `cargo t-runtime -E 'test(/session_registry_/)'`.
- [ ] 1.2 In `agent-runtime/src/agent/session_materializer.rs` `materialize_session_actor`, set `SessionRuntime.slash_registry` from `registry_for_session_cwd` for every session with a cwd when discovery is enabled and configured; keep the managed branch's result (design D2). Rewrite the doc comment on `SessionRuntime.slash_registry` (`agent/core.rs`). Covers `slash-commands` / Session in a plain folder sees its project commands, Sessions in different folders differ, Agent-level commands do not leak into other folders, Session without a working directory keeps the agent-level commands, Discovery switched off. Verify: tests in task 2.1.
- [ ] 1.3 Replace `workspace_catalog_registry` in `agent-runtime/src/agent/handle/core.rs` by `session_catalog_registry` (design D3): config check first, the managed binding's `effective_cwd` when bound, otherwise the stored cwd through `registry_for_session_cwd`; no workspace service required. Covers `slash-commands` / Idle session reports the same commands as a live one. Verify: tests in task 2.1.

## 2. Tests through the real handle

- [ ] 2.1 New file `agent/src/runtime_tests/handle/session_project_commands.rs` (registered in `handle/mod.rs`) with a fixture without a workspace service that shares its builder code with `slash_worktree_fixture` in `worktree_slash_commands.rs`. Tests named after the scenarios, each through `new_session`, `slash_command_catalog` and a prompt, as the existing tests do: `session_in_a_plain_folder_sees_its_project_commands`, `session_in_a_subfolder_of_a_repository`, `sessions_in_different_folders_differ`, `idle_session_reports_the_same_commands_as_a_live_one`, `agent_level_commands_do_not_leak_into_other_folders`, `session_without_a_working_directory_keeps_the_agent_level_commands`, `discovery_switched_off`. Verify: `cargo t-agent -E 'test(/session_in_a_plain_folder|session_in_a_subfolder|sessions_in_different_folders|idle_session_reports|agent_level_commands_do_not_leak|session_without_a_working_directory|discovery_switched_off/)'`.
- [ ] 2.2 Update `managed_sessions_expand_and_advertise_their_own_checkout_commands` in `agent/src/runtime_tests/handle/worktree_slash_commands.rs`: the first half ("Legacy local session: the agent-level registry still drives expansion", assertion `AGENTLEVEL-DEPLOY`) now expects the session folder's `ROOT-DEPLOY`; the managed half is unchanged. Fix the module doc comment ("legacy local sessions keep their existing behavior"). Covers `slash-commands` / Managed worktree session unchanged. Verify: `cargo t-agent -E 'test(/managed_sessions_expand_and_advertise|shared_child_in_a_managed_checkout|active_session_catalog_and_invocation/)'` passes.
- [ ] 2.3 Package checks: `cargo t-runtime`, `cargo fmt -p querymt-agent-runtime -p querymt-agent -- --check`, `cargo cl-runtime`, `cargo t-agent -E 'test(/slash|worktree|compact/)'`, `cargo cl-agent`. Record pass, fail and skip counts.

## 3. Docs and release notes

- [ ] 3.1 `docs/docs/agent/slash-commands.md`: where project commands are searched (a session's own project root: the Git root of its working directory, else the folder), and that edits are read when a session next opens. Verify: the text matches design D1 and the proposal's non-goals.
- [ ] 3.2 `CHANGELOG.md` Unreleased: a **BREAKING (behaviour)** entry for sessions outside managed worktrees now using their own folder's commands instead of the startup directory's. Verify: the entry names both the fix and the removed behaviour.

## 4. Check by hand

- [ ] 4.1 With a qmtcode built from this change, started with its embedded config and an empty `~/.qmt/commands`, open a session in a folder that has `.qmt/commands/explain.md` (a plain folder, then a subfolder of a Git repository whose root has the file). Verify: typing `/` lists `explain`, and `/explain closures` shows the expanded prompt under "Show prompt" in the desktop app; a session in a folder without the file does not list it. Record each as pass or fail here.
- [ ] 4.2 Run `openspec validate add-session-project-commands --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- Independent of the other slash changes; archive in any order.
- If a trust prompt for repository commands is wanted, open a separate change (design.md, Risks).
