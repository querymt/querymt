"""Worktree isolation through the Python bindings, against a real temp Git repository.

No model is called: creation, listing, status and removal are pure Git/registry
operations. Run with ``pytest crates/py/querymt-agent-py/tests``.
"""

import asyncio
import os
import subprocess
from pathlib import Path

import pytest

import querymt_agent
from querymt_agent import Agent, WorktreeError


def run(coro):
    return asyncio.run(coro)


def git(repo: Path, *args: str) -> None:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@example.com",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@example.com",
    }
    subprocess.run(["git", *args], cwd=repo, check=True, env=env, capture_output=True)


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    path = tmp_path / "repo"
    path.mkdir()
    git(path, "init", "-q", "-b", "main")
    (path / "README.md").write_text("hello\n")
    git(path, "add", "README.md")
    git(path, "commit", "-q", "-m", "initial")
    return path


async def archive_when_released(agent: Agent, workspace_id: str, timeout: float = 10.0):
    """The owner actor releases its checkout lease asynchronously after a
    session is deleted; archival is refused until then (bounded retry)."""
    deadline = asyncio.get_running_loop().time() + timeout
    while True:
        try:
            return await agent.worktrees.archive(workspace_id)
        except WorktreeError as err:
            if err.code != "operation_refused" or asyncio.get_running_loop().time() > deadline:
                raise
            await asyncio.sleep(0.1)


async def make_agent(repo: Path, tmp_path: Path, **kwargs) -> Agent:
    return await Agent.single(
        provider="openai",
        model="gpt-4o-mini",
        api_key="unused",
        cwd=str(repo),
        db=str(tmp_path / "sessions.db"),
        **kwargs,
    )


def test_module_exports_the_worktree_surface():
    assert {"Agent", "AgentSession", "Worktrees", "WorktreeError"} <= set(
        querymt_agent.__all__
    )
    assert issubclass(WorktreeError, RuntimeError)


def test_worktrees_are_disabled_by_default(repo, tmp_path):
    async def scenario():
        agent = await make_agent(repo, tmp_path)
        assert agent.worktrees is None
        with pytest.raises(WorktreeError) as raised:
            await agent.chat_session(workspace="worktree")
        assert raised.value.code == "workspace_service_unavailable"

    run(scenario())


def test_option_less_sessions_stay_local_when_enabled(repo, tmp_path):
    async def scenario():
        root = tmp_path / "wt"
        agent = await make_agent(repo, tmp_path, worktrees={"storage_root": str(root)})
        assert agent.worktrees is not None

        for session in (
            await agent.chat_session(),
            await agent.chat_session(workspace="local"),
        ):
            assert session.workspace is None
        assert (await agent.worktrees.list())["workspaces"] == []
        assert not any(root.iterdir()), "no checkout is created for local sessions"

    run(scenario())


def test_explicit_worktree_session_and_lifecycle(repo, tmp_path):
    async def scenario():
        root = tmp_path / "wt"
        agent = await make_agent(repo, tmp_path, worktrees={"storage_root": str(root)})

        session = await agent.chat_session(workspace="worktree", name="iso")
        binding = session.workspace
        assert binding is not None
        cwd = Path(binding["sessionCwd"])
        assert cwd != repo and root in cwd.parents
        assert binding["baseline"] == "commit"
        assert (cwd / "README.md").read_text() == "hello\n"
        # The source checkout is untouched.
        assert (repo / "README.md").read_text() == "hello\n"

        listed = await agent.worktrees.list()
        assert [w["owner_session_id"] for w in listed["workspaces"]] == [session.id]

        workspace_id = binding["workspaceId"]
        status = await agent.worktrees.status(workspace_id)
        assert status["workspace_id"] == workspace_id and status["checkout_exists"]

        # An actively owned workspace is never removed; nothing is lost.
        with pytest.raises(WorktreeError) as refused:
            await agent.worktrees.remove(workspace_id)
        assert refused.value.code == "removal_refused"
        assert cwd.exists()

        with pytest.raises(WorktreeError) as unknown:
            await agent.worktrees.status("ws-unknown")
        assert unknown.value.code == "workspace_not_found"

    run(scenario())


def test_include_local_changes_captures_uncommitted_edits(repo, tmp_path):
    async def scenario():
        agent = await make_agent(
            repo, tmp_path, worktrees={"storage_root": str(tmp_path / "wt")}
        )
        (repo / "README.md").write_text("dirty\n")
        clean = await agent.chat_session(workspace="worktree")
        dirty = await agent.chat_session(workspace="worktree", include_local_changes=True)
        assert (Path(clean.workspace["sessionCwd"]) / "README.md").read_text() == "hello\n"
        assert (Path(dirty.workspace["sessionCwd"]) / "README.md").read_text() == "dirty\n"
        assert dirty.workspace["baseline"] == "current_working_tree"

    run(scenario())


def test_configured_default_baseline_fills_only_unmade_choices(repo, tmp_path):
    async def scenario():
        agent = await make_agent(
            repo,
            tmp_path,
            worktrees={
                "storage_root": str(tmp_path / "wt"),
                "baseline": "current_working_tree",
            },
        )
        (repo / "README.md").write_text("dirty\n")
        unpinned = await agent.chat_session(workspace="worktree")
        pinned = await agent.chat_session(workspace="worktree", include_local_changes=False)
        by_ref = await agent.chat_session(workspace="worktree", base_ref="HEAD")
        assert unpinned.workspace["baseline"] == "current_working_tree"
        assert pinned.workspace["baseline"] == "commit"
        assert by_ref.workspace["baseline"] == "commit"

    run(scenario())


def test_worktrees_facade_create_archive_remove(repo, tmp_path):
    async def scenario():
        agent = await make_agent(
            repo, tmp_path, worktrees={"storage_root": str(tmp_path / "wt")}
        )
        created = await agent.worktrees.create(name="child")
        workspace_id = created["workspace_id"]
        assert Path(created["session_cwd"]).is_dir()
        assert created["baseline_mode"] == "commit"

        # A live owner session keeps the workspace active: neither removal nor
        # archival may pull the checkout out from under it, and nothing is lost.
        for refused_call, code in (
            (agent.worktrees.remove, "removal_refused"),
            (agent.worktrees.archive, "operation_refused"),
        ):
            with pytest.raises(WorktreeError) as refused:
                await refused_call(workspace_id)
            assert refused.value.code == code
        assert Path(created["checkout_path"]).is_dir()

        # Once the owner is gone the workspace can be archived and removed;
        # removal still never force-deletes.
        await agent.delete_session(created["session_id"])
        archived = await archive_when_released(agent, workspace_id)
        assert archived["lifecycle"] == "archived"
        removed = await agent.worktrees.remove(workspace_id)
        assert removed["checkout_removed"] is True
        assert not Path(created["checkout_path"]).exists()

    run(scenario())


def test_handoff_moves_ownership_to_a_new_session(repo, tmp_path):
    async def scenario():
        agent = await make_agent(
            repo, tmp_path, worktrees={"storage_root": str(tmp_path / "wt")}
        )
        created = await agent.worktrees.create(name="owner")
        checkout = Path(created["checkout_path"])
        (Path(created["session_cwd"]) / "README.md").write_text("owner edits\n")

        handed = await agent.worktrees.handoff(session_id=created["session_id"])
        assert handed["previous_session_id"] == created["session_id"]
        assert handed["session_id"] != created["session_id"]
        assert handed["workspace_id"] == created["workspace_id"]
        assert handed["checkout_path"] == str(checkout)
        assert handed["summary_seeded"] is False
        # Files are untouched and the successor is the single owner.
        assert (Path(handed["session_cwd"]) / "README.md").read_text() == "owner edits\n"
        listed = await agent.worktrees.list()
        assert [w["owner_session_id"] for w in listed["workspaces"]] == [handed["session_id"]]

        # The previous owner is no longer an owner: stable code, nothing created.
        with pytest.raises(WorktreeError) as refused:
            await agent.worktrees.handoff(session_id=created["session_id"])
        assert refused.value.code == "owner_transferred"

        with pytest.raises(WorktreeError) as bad:
            await agent.worktrees.handoff()
        assert bad.value.code == "invalid_options"

    run(scenario())


def test_invalid_arguments_raise_clear_errors(repo, tmp_path):
    async def scenario():
        with pytest.raises(ValueError, match="unknown worktrees option"):
            await make_agent(repo, tmp_path, worktrees={"mode": "always"})
        with pytest.raises(ValueError, match="baseline"):
            await make_agent(repo, tmp_path, worktrees={"baseline": "nope"})
        agent = await make_agent(repo, tmp_path, worktrees=True)
        with pytest.raises(ValueError, match="invalid workspace"):
            await agent.chat_session(workspace="sandbox")
        with pytest.raises(WorktreeError) as bad:
            await agent.chat_session(workspace="local", base_ref="main")
        assert bad.value.code == "invalid_options"

    run(scenario())
