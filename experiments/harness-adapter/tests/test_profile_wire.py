"""Wire-level behaviour of the harness adapter profile, using the real qmt-headless."""

from __future__ import annotations

import re
from pathlib import Path

from headless_run import HeadlessResult, profile_with_max_steps, qmt_env, run_headless

REPO = Path(__file__).resolve().parents[3]
FIXED_TOOLS = {
    "edit",
    "read_tool",
    "write_file",
    "multiedit",
    "replace_symbol",
    "glob",
    "search_text",
    "ls",
    "index",
    "get_symbol",
    "get_function",
    "find_symbol_references",
    "shell",
    "todowrite",
    "todoread",
}
BIG_OUTPUT = {"name": "shell", "arguments": {"command": "head -c 60000 /dev/zero | tr '\\0' 'a'"}}


def run_profile(
    headless_bin, profile, qmt_home, server, tmp_path, *, model="m", key="k"
) -> HeadlessResult:
    return run_headless(
        headless_bin,
        profile,
        tmp_path / "run",
        env=qmt_env(qmt_home, server.base_url, model, key),
    )


def test_profile_resolves_endpoint_from_environment_wire(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    fake_chat_server.script([{"text": "ok"}])
    result = run_profile(
        headless_bin,
        profile_path,
        qmt_home,
        fake_chat_server,
        tmp_path,
        model="model-z",
        key="key-q",
    )
    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.summary["outcome"] == "end_turn", result.summary
    req = fake_chat_server.requests[0]
    assert req.path == "/v1/chat/completions"
    assert req.body["model"] == "model-z"
    assert req.headers["authorization"] == "Bearer key-q"


def test_consecutive_requests_share_the_prior_prefix(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    fake_chat_server.script(
        [{"tool_calls": [BIG_OUTPUT]}] * 3 + [{"text": "finished"}], repeat_last=True
    )
    result = run_profile(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path)
    assert result.summary["outcome"] == "end_turn", result.stderr[-1500:]

    reqs = fake_chat_server.requests
    assert len(reqs) == 4, [r.body.get("messages", [])[-1:] for r in reqs]
    for earlier, later in zip(reqs, reqs[1:], strict=False):
        before = earlier.body["messages"]
        after = later.body["messages"]
        assert len(after) > len(before)
        # Every earlier message is unchanged (tool outputs included).
        assert after[: len(before)] == before
        # ...followed by the model's own response to it.
        reply = after[len(before)]
        assert reply["role"] == "assistant"
        assert reply.get("tool_calls"), reply
    # The outputs really were large, so a pruning/compaction regression would show.
    tool_msgs = [m for m in reqs[-1].body["messages"] if m["role"] == "tool"]
    assert len(tool_msgs) == 3
    assert all(len(str(m["content"])) > 40_000 for m in tool_msgs)


def test_no_auxiliary_model_calls(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path):
    fake_chat_server.script([{"tool_calls": [BIG_OUTPUT]}, {"text": "finished"}])
    result = run_profile(headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path)
    assert result.summary["outcome"] == "end_turn", result.stderr[-1500:]

    reqs = fake_chat_server.requests
    assert len(reqs) == 2, "one request per agent-loop step and nothing else"
    for req in reqs:
        names = {t["function"]["name"] for t in req.body.get("tools", [])}
        assert names == FIXED_TOOLS, "every request is an agent-loop request"
        text = str(req.body["messages"]).lower()
        assert not re.search(
            r"summar(y|ize) (the|this) conversation|generate a (short )?title", text
        )


def test_session_stops_at_the_call_budget(
    headless_bin, profile_path, qmt_home, fake_chat_server, tmp_path
):
    copy = profile_with_max_steps(profile_path, tmp_path / "profile", 3)

    fake_chat_server.script([{"tool_calls": [{"name": "ls", "arguments": {"path": "."}}]}])
    result = run_profile(headless_bin, copy, qmt_home, fake_chat_server, tmp_path)

    assert len(fake_chat_server.requests) == 3, result.stderr[-1500:]
    assert result.run_returncode == 0, result.stderr[-1500:]
    assert result.export_returncode == 0, result.export_stderr[-1500:]
    assert result.summary["outcome"] == "stopped", result.summary
    assert result.summary["stop_type"] == "step_limit", result.summary
