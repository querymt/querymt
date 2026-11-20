"""Guarded rollouts score zero (harness-adapter-training / Guarded rollouts score zero).

The block notice is produced by the pinned safeagents itself, run against a response that a
fake upstream would have returned, so a change of the notice text in a safeagents bump fails
here instead of silently turning the guard off.
"""

from __future__ import annotations

import asyncio
import json

from safeagents.guard import Blocked, Config, Guard
from safeagents.rewrite import guard_response
from trl.experimental.async_grpo.openenv_harness import HarnessRolloutOutcome

from querymt_harbor.guard import BLOCK_MARKER, METRIC, BlockLog, count_blocked
from querymt_harbor.reward import RolloutReward

REQUEST = {
    "model": "qmt-adapter",
    "messages": [{"role": "user", "content": "fix the bug in foo.py"}],
    "tools": [
        {"type": "function", "function": {"name": "shell", "parameters": {"type": "object"}}}
    ],
}


def upstream_response(command: str) -> dict:
    """What vLLM returns for a model that wants to run `command`."""
    return {
        "choices": [
            {
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": json.dumps({"command": command}),
                            },
                        }
                    ],
                },
            }
        ]
    }


def through_safeagents(command: str) -> dict:
    """The response after the pinned safeagents guard, as the capture proxy records it."""
    response = upstream_response(command)
    asyncio.run(guard_response(response, Guard(REQUEST, Config())))
    return response


def trace_of(*responses: dict) -> list[dict]:
    return [{"request": REQUEST, "response": r} for r in responses]


def outcome(trace: list[dict], env_reward: float | None, tool_calls: int = 0, completion=None):
    return HarnessRolloutOutcome(
        env_reward=env_reward,
        completion=completion or [{"role": "assistant", "content": ""}],
        trace=trace,
        tool_call_count=tool_calls,
        tool_failure_count=0,
        tool_calls_by_name={},
        timed_out=False,
    )


def test_marker_is_the_pinned_safeagents_notice():
    notice = Blocked("shell", "some reason").notice()
    assert notice.startswith(BLOCK_MARKER)
    # ...and it is what the real rewrite puts into the response.
    blocked = through_safeagents("pkill -f safeagents")
    content = blocked["choices"][0]["message"]["content"]
    assert content.startswith(BLOCK_MARKER)
    assert "tool_calls" not in blocked["choices"][0]["message"]


def test_blocked_tool_call_zeroes_reward(tmp_path):
    log = tmp_path / "guard" / "blocked.log"
    reward = RolloutReward(block_log=log)
    trace = trace_of(through_safeagents("ls"), through_safeagents("pkill -f safeagents"))

    assert reward(outcome(trace, env_reward=1.0, tool_calls=2)) == 0.0
    assert BlockLog(log).drain() == 1  # the guard_blocked_rollouts metric counts it


def test_clean_guarded_rollout_is_scored_normally(tmp_path):
    log = tmp_path / "blocked.log"
    reward = RolloutReward(block_log=log)
    trace = trace_of(*[through_safeagents("ls") for _ in range(3)])

    assert reward(outcome(trace, env_reward=1.0, tool_calls=15)) == 1.0
    assert not log.exists()


def test_blocked_rollout_is_zero_even_when_the_verifier_did_not_run():
    trace = trace_of(through_safeagents("pkill -f safeagents"))
    assert RolloutReward()(outcome(trace, env_reward=None)) == 0.0


def test_guard_off_ignores_notices():
    trace = trace_of(through_safeagents("pkill -f safeagents"))
    assert RolloutReward(guard_enabled=False)(outcome(trace, env_reward=1.0, tool_calls=15)) == 1.0


def test_trace_is_the_source_not_the_last_requests_history():
    """An early block counts even when the harness no longer shows it in the final request."""
    trace = trace_of(through_safeagents("pkill -f safeagents"), through_safeagents("ls"))
    clean_completion = [{"role": "assistant", "content": "all done"}]
    assert count_blocked(trace, clean_completion) == 1


def test_completion_is_the_fallback_without_a_trace():
    notice = through_safeagents("pkill -f safeagents")["choices"][0]["message"]["content"]
    completion = [
        {"role": "user", "content": "task"},
        {"role": "assistant", "content": notice},
        {"role": "assistant", "content": "ok"},
    ]
    assert count_blocked([], completion) == 1


def test_tool_output_containing_the_marker_is_not_a_block():
    """An agent that prints the marker (cat of a file) must not zero its own rollout."""
    completion = [
        {"role": "tool", "content": f"{BLOCK_MARKER}shell`: copied from a file"},
        {"role": "assistant", "content": "ok"},
    ]
    assert count_blocked([], completion) == 0


def test_block_log_drains_only_new_lines(tmp_path):
    log = BlockLog(tmp_path / "log")
    assert log.drain() == 0
    log.record(1)
    log.record(3)
    assert log.drain() == 2
    assert log.drain() == 0
    log.record(1)
    reader = BlockLog(tmp_path / "log")  # a fresh reader starts from the beginning
    assert reader.drain() == 3
    assert log.drain() == 1


def test_block_log_ignores_a_half_written_line(tmp_path):
    path = tmp_path / "log"
    path.write_text("1\n2")
    log = BlockLog(path)
    assert log.drain() == 1
    path.write_text("1\n2\n")
    assert log.drain() == 1


def test_metric_name():
    assert METRIC == "guard_blocked_rollouts"
