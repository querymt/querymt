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
    agent = await Agent.single(
        provider="openai",
        model="gpt-4o-mini",
        tools=["read_tool", "glob", "search_text"],
    )
    print(await agent.chat("Say hello briefly."))


asyncio.run(main())
```

## Git worktree isolation

Managed Git worktrees give a session its own checkout of a repository. They are
opt-in twice: the agent must enable the service, **and** each session must ask
for isolation. Enabling the service alone never changes a session.

```python
agent = await Agent.single(
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
exposes `create`, `handoff`, `list`, `status`, `result`, `publish`, `archive`,
`remove` and `recover`, each returning a dict with the same shape as the ACP
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

Failures raise `querymt_agent.WorktreeError` (a `RuntimeError`) with a stable
`.code` such as `workspace_service_unavailable`, `invalid_options`,
`owner_active`, `workspace_not_archived`, `unpreserved_content`,
`unpreserved_commits`, `workspace_archived`, `removal_refused` or
`workspace_not_found`. Unknown keys in the `worktrees`
dict raise `ValueError`. Isolation is a separate checkout, not a sandbox.

`Agent.from_config(path)` reads `[agent.workspace]` / `[quorum.workspace]` from
TOML. See the repository's `docs/docs/agent/worktrees.md` for the full guide.

### Tests

```bash
pip install pytest
maturin develop --manifest-path crates/py/querymt-agent-py/Cargo.toml
QMT_HOME="$(mktemp -d)" python -m pytest crates/py/querymt-agent-py/tests
```
