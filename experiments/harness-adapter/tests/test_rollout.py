"""The agent's own run/kill/export commands, executed locally against the real qmt-headless."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
from pathlib import Path, PurePosixPath

from harbor.environments.base import ExecResult
from harbor.models.agent.context import AgentContext
from harbor.models.trajectories import Trajectory

from querymt_harbor.agent import QueryMTAgent
from querymt_harbor.config import load_reference


class LocalEnvironment:
    """Runs the agent's shell commands on this machine; uploads are file copies."""

    def __init__(self, env: dict[str, str]) -> None:
        self.env = env
        self.commands: list[str] = []

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.commands.append(command)
        proc = await asyncio.to_thread(
            subprocess.run,
            ["bash", "-c", command],
            cwd=cwd,
            env={**os.environ, **self.env, **(env or {})},
            capture_output=True,
            text=True,
            timeout=timeout_sec or 180,
            check=False,
        )
        return ExecResult(stdout=proc.stdout, stderr=proc.stderr, return_code=proc.returncode)

    async def upload_file(self, source_path, target_path):
        Path(target_path).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source_path, target_path)


def test_rollout_runs_one_headless_session_for_real(
    headless_bin, openai_plugin, fake_chat_server, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    install = tmp_path / "install"
    agent_dir = tmp_path / "logs" / "agent"
    work = tmp_path / "workspace"
    work.mkdir()
    agent_dir.mkdir(parents=True)

    agent = QueryMTAgent(
        logs_dir=agent_dir,
        reference=load_reference(),
        cache_dir=tmp_path / "cache",
        extra_env={
            "QMT_BASE_URL": fake_chat_server.base_url,
            "QMT_API_KEY": "key-q",
            "QMT_MODEL": "m",
        },
    )
    # Point the sandbox layout at local directories.
    agent.INSTALL_DIR = str(install)
    agent.BIN_PATH = str(headless_bin)
    agent.environment_logs_dir = PurePosixPath(agent_dir)

    for remote, content in agent.stage_files().items():
        Path(remote).parent.mkdir(parents=True, exist_ok=True)
        Path(remote).write_bytes(content)
    plugin = Path(agent.plugin_remote_path)
    plugin.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(openai_plugin, plugin)

    fake_chat_server.script(
        [
            {"tool_calls": [{"name": "ls", "arguments": {"path": "."}}]},
            {"text": "done", "usage": {"prompt_tokens": 40, "completion_tokens": 4}},
        ]
    )
    env = LocalEnvironment(
        {**agent._state_env(), **agent._extra_env, "HOME": str(tmp_path / "home")}
    )
    # cwd is the workspace, as in a sandbox whose WORKDIR is the task directory.
    original_exec = env.exec

    async def exec_in_workspace(command, **kwargs):
        return await original_exec(command, **{**kwargs, "cwd": str(work)})

    env.exec = exec_in_workspace  # type: ignore[method-assign]

    asyncio.run(agent.run("Fix the bug in main.py", env, AgentContext()))

    # One run, one export, and the PID was recorded.
    assert sum("qmt-headless" in c and " run " in c for c in env.commands) == 1
    assert sum(" export " in c for c in env.commands) == 1
    assert (agent_dir / "qmt" / "run.pid").read_text().strip().isdigit()
    assert (agent_dir / "qmt" / "sessions.db").is_file()

    summary = json.loads((agent_dir / "qmt" / "summary.json").read_text())
    atif = json.loads((agent_dir / "trajectory.json").read_text())
    assert summary["outcome"] == "end_turn", summary
    assert summary["session_id"] == atif["session_id"]  # one session, no subagent trajectories
    Trajectory.model_validate(atif)
    assert [s["source"] for s in atif["steps"]].count("agent") == 2

    first = fake_chat_server.requests[0].body["messages"]
    assert any(m["role"] == "user" and "Fix the bug in main.py" in str(m["content"]) for m in first)
    assert fake_chat_server.requests[0].headers["authorization"] == "Bearer key-q"

    context = AgentContext()
    agent.populate_context_post_run(context)
    assert context.n_input_tokens and context.n_output_tokens
