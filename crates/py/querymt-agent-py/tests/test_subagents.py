"""Subagent changes through the Python bindings. No model is called.

Run with ``pytest crates/py/querymt-agent-py/tests``.
"""

import asyncio
import tempfile
import warnings

import pytest
from querymt_agent import Agent, SubagentError


def run(coro):
    return asyncio.run(coro)


async def make(**kwargs):
    kwargs.setdefault("tools", ["delegate", "read_tool"])
    return await Agent.create(
        provider="anthropic",
        model="m",
        api_key="x",
        cwd=tempfile.gettempdir(),
        db=tempfile.mktemp(suffix=".db"),
        **kwargs,
    )


def test_add_list_remove_and_generation():
    async def go():
        agent = await make()
        assert not agent.has_subagents()
        assert agent.roster_generation == 0
        assert await agent.add_subagent(
            "reviewer", provider="anthropic", model="m", api_key="x",
            description="Reviews", capabilities=["review"],
        ) == 1
        assert agent.has_subagents()
        (info,) = agent.subagents
        assert info["id"] == "reviewer" and info["origin"] == "runtime"
        assert await agent.remove_subagent("reviewer") == 2
        assert agent.subagents == []

    run(go())


def test_errors_carry_codes():
    async def go():
        agent = await make()
        await agent.add_subagent("a", provider="anthropic", model="m", api_key="x")
        with pytest.raises(SubagentError) as dup:
            await agent.add_subagent("a", provider="anthropic", model="m", api_key="x")
        assert dup.value.code == "duplicate_id"
        with pytest.raises(SubagentError) as unknown:
            await agent.remove_subagent("nope")
        assert unknown.value.code == "unknown_agent"
        plain = await make(tools=["read_tool"])
        with pytest.raises(SubagentError) as disabled:
            await plain.add_subagent("x", provider="anthropic", model="m", api_key="x")
        assert disabled.value.code == "delegation_disabled"

    run(go())


def test_on_subagents_changed_reports_successful_changes_only():
    async def go():
        agent = await make()
        seen = []
        agent.on_subagents_changed(seen.append)
        await agent.add_subagent("a", provider="anthropic", model="m", api_key="x")
        with pytest.raises(SubagentError):
            await agent.add_subagent("a", provider="anthropic", model="m", api_key="x")
        await agent.remove_subagent("a")
        for _ in range(100):
            if len(seen) >= 2:
                break
            await asyncio.sleep(0.05)
        assert [(c["kind"], c["id"], c["generation"]) for c in seen] == [
            ("added", "a", 1),
            ("removed", "a", 2),
        ]
        assert seen[0]["origin"] == "runtime" and seen[0]["location"] == "local"

    run(go())


def test_startup_subagents_and_validation():
    async def go():
        agent = await make(
            subagents=[{"id": "coder", "provider": "anthropic", "model": "m", "api_key": "x"}]
        )
        assert [s["origin"] for s in agent.subagents] == ["config"]
        with pytest.raises(ValueError):
            await make(subagents=[{"id": "x", "provider": "p"}])
        with pytest.raises(ValueError):
            await make(subagents=[{"id": "x", "provider": "p", "model": "m", "bogus": 1}])

    run(go())


def test_deprecated_names_still_work():
    async def go():
        with pytest.warns(DeprecationWarning):
            agent = await Agent.single(
                provider="anthropic", model="m", api_key="x",
                cwd=tempfile.gettempdir(), db=tempfile.mktemp(suffix=".db"),
            )
        with pytest.warns(DeprecationWarning):
            assert agent.is_multi() is False
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            assert agent.has_subagents() is False

    run(go())
