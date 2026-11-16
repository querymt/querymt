"""Change an agent's team between turns.

One agent, one session, three turns. Between the turns the subagents the root
can delegate to are added and removed while the agent keeps running:

1. Turn 1: a plain agent with no subagents does the work itself.
2. Add ``reviewer`` and ``doc_writer``; turn 2 can delegate to them.
3. Remove ``reviewer``; turn 3 can no longer delegate to it.

The root's ``delegate`` tool lists the current subagents on every turn, so no
restart, rebuild or new session is needed. Changes last until the agent stops.

    ANTHROPIC_API_KEY=... python dynamic_subagents.py

Other provider: QMT_PROVIDER=openai QMT_MODEL=gpt-4o QMT_SUBAGENT_MODEL=gpt-4o-mini
"""

import asyncio
import os

from querymt_agent import Agent, SubagentError

PROVIDER = os.environ.get("QMT_PROVIDER", "anthropic")
MODEL = os.environ.get("QMT_MODEL", "claude-sonnet-4-5")
SUB_MODEL = os.environ.get("QMT_SUBAGENT_MODEL", "claude-haiku-4-5")


def show_team(agent: Agent) -> None:
    print(f"   team (generation {agent.roster_generation}):")
    for s in agent.subagents or []:
        print(f"     - {s['id']:<11} {s['description']}")
    if not agent.subagents:
        print("     <empty>")


async def main() -> None:
    # A plain agent. "delegate" must be in its tools, otherwise subagents
    # cannot be added to it later (SubagentError.code == "delegation_disabled").
    agent = await Agent.create(
        provider=PROVIDER,
        model=MODEL,
        cwd=".",
        system=(
            "You coordinate a small team. Delegate review work and documentation "
            "work to a fitting subagent when one exists; otherwise do it yourself. "
            "Keep answers short."
        ),
        tools=["delegate", "read_tool", "glob", "search_text"],
    )

    agent.on_delegation(lambda to, objective: print(f"   [delegate -> {to}] {objective}"))
    agent.on_subagents_changed(
        lambda c: print(f"   [team] {c['kind']} {c['id']} ({c['origin']}, generation {c['generation']})")
    )
    agent.on_tool_call(
        lambda name, _args: print(f"   [tool] {name}") if name != "delegate" else None
    )

    print("== Turn 1: no subagents ==")
    show_team(agent)
    print(await agent.chat("What does Cargo.toml in this directory define?"), "\n")

    print("== Adding subagents ==")
    await agent.add_subagent(
        "reviewer",
        provider=PROVIDER,
        model=SUB_MODEL,
        description="Reviews code for bugs and unclear design",
        capabilities=["review"],
        system="You are a strict, concise code reviewer.",
        tools=["read_tool", "glob", "search_text"],
    )
    await agent.add_subagent(
        "doc_writer",
        provider=PROVIDER,
        model=SUB_MODEL,
        description="Writes short user-facing documentation",
        capabilities=["docs"],
        system="You write clear, brief documentation.",
        tools=["read_tool"],
    )
    show_team(agent)

    print("\n== Turn 2: the root can delegate ==")
    print(
        await agent.chat(
            "Get crates/agent/src/prelude.rs reviewed and ask for a "
            "two-line doc summary of it."
        ),
        "\n",
    )

    print("== Removing the reviewer ==")
    await agent.remove_subagent("reviewer")
    show_team(agent)

    print("\n== Turn 3: no reviewer any more ==")
    print(await agent.chat("Review crates/agent/src/prelude.rs again."), "\n")

    # Errors are typed, carry a stable code, and leave the team unchanged.
    try:
        await agent.remove_subagent("reviewer")
    except SubagentError as err:
        print(f"(as expected: {err.code}: {err})")


asyncio.run(main())
