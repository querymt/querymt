# querymt-agent

Python bindings for QueryMT Agent.

## Install

From the repository root:

```bash
python -m venv .venv
source .venv/bin/activate
pip install maturin
maturin develop --manifest-path crates/py/querymt-py/Cargo.toml
maturin develop --manifest-path crates/py/querymt-agent-py/Cargo.toml
```

## Quick Start

```python
import asyncio
from querymt_agent import Agent


async def main() -> None:
    agent = await Agent.create(
        provider="openai",
        model="gpt-4o-mini",
        tools=["read_tool", "glob", "search_text"],
    )
    print(await agent.chat("Say hello briefly."))


asyncio.run(main())
```

## Subagents: change the team between turns

An agent can delegate to subagents, and the set can change while it runs. The
root's `delegate` tool lists the current subagents on every turn, so a change
is visible from the next turn. Changes last until the agent stops. The root's
`tools` must include `delegate`.

```python
agent = await Agent.create(
    provider="anthropic", model="claude-sonnet-4-5",
    tools=["delegate", "read_tool"],
    subagents=[{"id": "coder", "provider": "anthropic", "model": "claude-haiku-4-5",
                "description": "Writes code", "tools": ["edit", "read_tool"]}],  # optional
)
agent.on_delegation(lambda to, objective: print(to, objective))
agent.on_subagents_changed(lambda c: print(c["kind"], c["id"], c["generation"]))

await agent.chat("...")                                     # turn 1
await agent.add_subagent("reviewer", provider="anthropic", model="claude-haiku-4-5",
                         description="Reviews code", tools=["read_tool"])
await agent.chat("...")                                     # turn 2 can use `reviewer`
await agent.remove_subagent("reviewer")
agent.subagents          # [{"id": ..., "origin": "config"|"runtime"|"protocol", ...}]
agent.roster_generation  # +1 per add/remove
```

`on_subagents_changed` fires for every successful add or remove, whichever way it
was made (including ACP clients), with `kind`, `id`, `name`, `description`,
`capabilities`, `origin`, `location` and `generation`. Refused changes are not
reported. `Agent.single(...)` and `is_multi()` still work but emit a
`DeprecationWarning`; use `Agent.create(...)` and `has_subagents()`.

Failures raise `SubagentError` with a stable `.code` (`duplicate_id`,
`unknown_agent`, `delegation_disabled`, ...) and leave the team unchanged. See
`examples/dynamic_subagents.py`.

## Git worktree isolation

Managed Git worktrees give a session its own checkout of a repository. They are
opt-in twice: the agent must enable the service, **and** each session must ask
for isolation. Enabling the service alone never changes a session.

```python
agent = await Agent.create(
    provider="anthropic",
    model="claude-sonnet-4-5-20250929",
    cwd="/path/to/repo",
    worktrees=True,  # or {"storage_root": "...", "baseline": "commit"}
)

local = await agent.chat_session()                      # unchanged: local
assert local.workspace is None

isolated = await agent.chat_session(
    workspace="worktree",
    name="refactor",
    include_local_changes=True,   # also capture uncommitted edits
)
print(isolated.workspace["sessionCwd"])   # the directory it actually runs in
```

`agent.worktrees` is `None` when worktrees are not enabled; otherwise it
exposes `create`, `handoff`, `list`, `status`, `result`, `publish`, `merge`,
`archive`, `remove` and `recover`, each returning a dict with the same shape as the ACP
`querymt/worktree/*` extensions. Removal is conservative and never forces.

`archive` and `remove` work from any session and take exactly one of
`workspace_id` or `session_id` (the owner). An idle owner is unloaded; a busy one
raises `owner_active`. An owned workspace must be archived before removal:

```python
workspace_id = isolated.workspace["workspaceId"]
await agent.worktrees.archive(workspace_id=workspace_id)   # owner becomes read-only
await agent.worktrees.remove(workspace_id=workspace_id)    # after archive

# Alternatively, archive and remove in one call:
# await agent.worktrees.remove(workspace_id=workspace_id, archive=True)
```

If `remove(..., archive=True)` archives the workspace but removal is then refused
(for example `unpreserved_content`), the workspace stays archived. If archival
itself is refused (for example `owner_active`), nothing changes and the workspace
is not archived.

`merge` brings a worktree's result back to the checkout it was created from as
one squashed commit that contains only the worktree's own changes (not the
uncommitted files its baseline inherited). It takes exactly one of
`workspace_id` or `session_id`. By default it snapshots the working state (the
owner must be idle) and merges that; `commit=` merges a retained result or a
commit on the worktree branch instead. The source checkout's commit hooks run.
`archive=True` or `remove=True` (not both) archive, or archive and remove, the
workspace once the source contains the result; a failure there is reported
under `follow_up["error"]` and never undoes the merge:

```python
merged = await agent.worktrees.merge(session_id=isolated.id, archive=True)
print(merged["outcome"])   # "merged", "already_merged" or "no_changes"
```

Failures raise `querymt_agent.WorktreeError` (a `RuntimeError`) with a stable
`.code` such as `workspace_service_unavailable`, `invalid_options`,
`owner_active`, `workspace_not_archived`, `unpreserved_content`,
`unpreserved_commits`, `workspace_archived`, `removal_refused` or
`workspace_not_found`. Merge refusals add `merge_conflict`,
`touches_local_changes`, `source_missing`, `source_active` and
`commit_not_on_workspace_branch`, and carry the offending files in `.paths`
(an empty list for errors that name none). Unknown keys in the `worktrees`
dict raise `ValueError`. Isolation is a separate checkout, not a sandbox.

`Agent.from_config(path)` reads `[agent.workspace]` (and `[quorum.workspace]` of the
deprecated quorum form) from TOML. See the repository's `docs/docs/agent/worktrees.md` for the full guide.

### Tests

```bash
pip install pytest
maturin develop --manifest-path crates/py/querymt-agent-py/Cargo.toml
QMT_HOME="$(mktemp -d)" python -m pytest crates/py/querymt-agent-py/tests
```
