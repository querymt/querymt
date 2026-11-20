"""QueryMTAgent: install, budget override, run/kill/export sequence (fake sandbox)."""

from __future__ import annotations

import asyncio
import dataclasses
import hashlib
import io
import json
import re
import tarfile
from pathlib import Path

import pytest
from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import ExecResult
from harbor.models.agent.context import AgentContext

from querymt_harbor.agent import QueryMTAgent, set_max_steps
from querymt_harbor.config import load_reference
from querymt_harbor.plugin import PluginError
from querymt_harbor.release import ArchivePin, HeadlessPin, ReleaseError
from test_plugin import REPO as PLUGIN_REPO
from test_plugin import WASM, make_registry

ENDPOINT = "http://proxy.test:8000/v1"
ARCHIVE_URL = "https://example.test/qmt-headless.tar.gz"
PROFILE = "/installed-agent/qmt/confs/harness_adapter.toml"
LOG_QMT = "/logs/agent/qmt"


def make_archive() -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        payload = b"#!/bin/sh\necho fake qmt-headless\n"
        info = tarfile.TarInfo("qmt-headless-v9-x86_64-unknown-linux-musl/qmt-headless")
        info.size = len(payload)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(payload))
    return buffer.getvalue()


class FakeEnvironment:
    """Records every exec and upload, in order; uploads keep the file content."""

    def __init__(self) -> None:
        self.events: list[tuple[str, str]] = []
        self.uploads: dict[str, bytes] = {}
        self.cwds: list[str | None] = []

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.events.append(("exec", command))
        self.cwds.append(cwd)
        stdout = "Linux\nx86_64\n" if command.startswith("uname") else ""
        return ExecResult(stdout=stdout, stderr="", return_code=0)

    async def upload_file(self, source_path, target_path):
        self.events.append(("upload", target_path))
        self.uploads[target_path] = Path(source_path).read_bytes()

    def execs(self) -> list[str]:
        return [value for kind, value in self.events if kind == "exec"]


class Setup:
    def __init__(
        self,
        tmp_path: Path,
        *,
        archive: bytes,
        served_archive: bytes | None = None,
        served_wasm: bytes = WASM,
    ):
        registry_get, plugin_pin, self.registry_calls = make_registry(served_wasm)
        served = served_archive if served_archive is not None else archive
        self.archive_calls: list[str] = []

        def http_get(url: str, headers: dict[str, str]) -> bytes:
            if url == ARCHIVE_URL:
                self.archive_calls.append(url)
                return served
            return registry_get(url, headers)

        self.http_get = http_get
        base = load_reference()
        self.reference = dataclasses.replace(
            base,
            plugin=plugin_pin,
            headless=HeadlessPin(
                version="9.9.9",
                platforms={
                    "linux-x86_64": ArchivePin(
                        sha256=hashlib.sha256(archive).hexdigest(), url=ARCHIVE_URL
                    )
                },
            ),
        )
        self.tmp_path = tmp_path
        self.logs = tmp_path / "logs"
        self.logs.mkdir()

    def agent(self, **kwargs) -> QueryMTAgent:
        agent = QueryMTAgent(
            logs_dir=self.logs,
            reference=self.reference,
            cache_dir=self.tmp_path / "cache",
            extra_env={"QMT_BASE_URL": ENDPOINT, "QMT_API_KEY": "s", "QMT_MODEL": "m"},
            **kwargs,
        )
        agent._http_get = self.http_get
        return agent


@pytest.fixture
def setup(tmp_path) -> Setup:
    return Setup(tmp_path, archive=make_archive())


def test_agent_is_an_installed_agent_with_native_atif(setup):
    agent = setup.agent()
    assert isinstance(agent, BaseInstalledAgent)
    assert QueryMTAgent.capabilities.atif is True
    assert QueryMTAgent.import_path() == "querymt_harbor.agent:QueryMTAgent"
    assert agent.version() == "9.9.9"


async def test_install_places_pinned_binary(setup):
    env = FakeEnvironment()
    await setup.agent().install(env)

    assert any(
        "install -D -m 0755" in c and "/opt/qmt/bin/qmt-headless" in c and "-name qmt-headless" in c
        for c in env.execs()
    )
    assert "/installed-agent/qmt/prompts/default_system.txt" in env.uploads


@pytest.mark.parametrize("limit", [100, 7])
async def test_per_run_budget_overrides_the_default(setup, limit):
    env = FakeEnvironment()
    await setup.agent(step_limit=limit).install(env)

    uploaded = env.uploads[PROFILE].decode()
    original = setup.reference.profile.read_text()
    assert f"max_steps = {limit}" in uploaded
    # Nothing else differs: undoing the one rewrite reproduces the repository file.
    assert set_max_steps(uploaded, 50) == original
    assert uploaded.splitlines() != original.splitlines() or limit == 50


async def test_default_budget_leaves_profile_untouched(setup):
    env = FakeEnvironment()
    await setup.agent().install(env)
    assert env.uploads[PROFILE].decode() == setup.reference.profile.read_text()


@pytest.mark.parametrize("bad", [0, -3, True, 2.5, "10"])
def test_invalid_budget_is_rejected(setup, bad):
    env = FakeEnvironment()
    with pytest.raises(ValueError, match="step_limit"):
        setup.agent(step_limit=bad)
    assert env.events == []  # no sandbox was ever touched


@pytest.mark.parametrize("bad", [0, -1, True, "5"])
def test_invalid_export_timeout_is_rejected(setup, bad):
    with pytest.raises(ValueError, match="export_timeout_sec"):
        setup.agent(export_timeout_sec=bad)


async def test_install_uploads_pinned_plugin_and_local_providers_toml(setup):
    env = FakeEnvironment()
    await setup.agent().install(env)

    plugin_path = f"/installed-agent/qmt/plugins/{setup.reference.plugin.filename}"
    assert (
        hashlib.sha256(env.uploads[plugin_path]).hexdigest() == setup.reference.plugin.blob_sha256
    )

    providers = env.uploads["/installed-agent/qmt/providers.toml"].decode()
    assert f'path = "{plugin_path}"' in providers
    assert "oci://" not in providers
    assert 'allowed_hosts = ["proxy.test"]' in providers

    assert env.uploads["/installed-agent/qmt/models.dev.json"] == b'{"providers":{}}\n'
    assert "/installed-agent/qmt/prompts/harness_adapter_meta.jinja2" in env.uploads


async def test_plugin_hash_mismatch_fails_install(tmp_path):
    setup = Setup(tmp_path, archive=make_archive(), served_wasm=b"tampered")
    env = FakeEnvironment()

    with pytest.raises(PluginError, match="sha256 mismatch"):
        await setup.agent().install(env)

    # Nothing ran in the sandbox: the harness binary never starts.
    assert env.events == []


async def test_checksum_mismatch_fails_install(tmp_path):
    setup = Setup(tmp_path, archive=make_archive(), served_archive=make_archive() + b"x")
    env = FakeEnvironment()

    with pytest.raises(ReleaseError, match="sha256 mismatch"):
        await setup.agent().install(env)

    # Only the platform probe ran: nothing was uploaded or installed, so no binary ran.
    assert env.execs() == ["uname -s && uname -m"]
    assert env.uploads == {}


async def test_unpinned_platform_fails_install(tmp_path):
    setup = Setup(tmp_path, archive=make_archive())

    class Arm(FakeEnvironment):
        async def exec(self, command, **kwargs):
            self.events.append(("exec", command))
            return ExecResult(stdout="Linux\naarch64\n", stderr="", return_code=0)

    env = Arm()
    with pytest.raises(ReleaseError, match="linux-aarch64"):
        await setup.agent().install(env)
    assert env.uploads == {}


async def test_install_downloads_nothing_in_the_sandbox(setup):
    env = FakeEnvironment()
    await setup.agent().install(env)
    forbidden = re.compile(r"\b(apt|apt-get|pip|pip3|uv|uvx|curl|wget|npm)\b|https?://|ghcr\.io")
    for command in env.execs():
        assert not forbidden.search(command), command
        assert "models.dev" not in command
    # The archive and the plugin were fetched on the host.
    assert setup.archive_calls == [ARCHIVE_URL]
    assert any(PLUGIN_REPO in url for url in setup.registry_calls)


# --- run: one headless run, then kill check and export -----------------------------


def _index(execs: list[str], needle: str) -> list[int]:
    return [i for i, c in enumerate(execs) if needle in c]


async def test_rollout_runs_one_headless_session(setup):
    env = FakeEnvironment()
    agent = setup.agent()
    await agent.run("fix the bug", env, AgentContext())

    execs = env.execs()
    runs = _index(execs, "/opt/qmt/bin/qmt-headless run ")
    assert len(runs) == 1
    run = execs[runs[0]]
    assert f"{LOG_QMT}/run.pid" in run and "exec " in run
    assert run.index("echo $$") < run.index("qmt-headless run")
    assert f"run {PROFILE} " in run
    assert '--cwd "$PWD"' in run  # the task workspace is the process's working directory
    assert f"--db {LOG_QMT}/sessions.db" in run
    assert "--instruction-file /installed-agent/qmt/instruction.txt" in run
    assert env.uploads["/installed-agent/qmt/instruction.txt"] == b"fix the bug"

    kills = _index(execs, "kill -TERM")
    exports = _index(execs, "qmt-headless export ")
    assert len(kills) == 1 and len(exports) == 1
    assert runs[0] < kills[0] < exports[0]
    export = execs[exports[0]]
    assert f"--db {LOG_QMT}/sessions.db" in export
    assert "--atif /logs/agent/trajectory.json" in export
    assert f"--summary {LOG_QMT}/summary.json" in export
    # The kill sequence is TERM first, KILL only if still alive.
    kill = execs[kills[0]]
    assert kill.index("kill -TERM") < kill.index("kill -KILL")


async def test_timeout_still_exports(setup):
    started = asyncio.Event()
    order: list[str] = []

    class Blocking(FakeEnvironment):
        async def exec(self, command, **kwargs):
            if "/opt/qmt/bin/qmt-headless run " in command:
                order.append("run")
                started.set()
                await asyncio.Event().wait()  # blocks until cancelled
            if "kill -TERM" in command:
                order.append("kill")
            if "qmt-headless export " in command:
                order.append("export")
            return await super().exec(command, **kwargs)

    agent = setup.agent()
    task = asyncio.create_task(agent.run("slow task", Blocking(), AgentContext()))
    await started.wait()
    task.cancel()  # what asyncio.wait_for does at Harbor's agent timeout
    with pytest.raises(asyncio.CancelledError):
        await task

    assert order == ["run", "kill", "export"]


async def test_original_error_survives_a_failed_export(setup):
    class Failing(FakeEnvironment):
        async def exec(self, command, **kwargs):
            if "qmt-headless run " in command:
                return ExecResult(stdout="", stderr="provider down", return_code=3)
            if "qmt-headless export " in command:
                raise OSError("sandbox gone")
            return await super().exec(command, **kwargs)

    with pytest.raises(RuntimeError, match="provider down|exit"):
        await setup.agent().run("task", Failing(), AgentContext())


async def test_hung_export_is_bounded(setup):
    class Hung(FakeEnvironment):
        async def exec(self, command, **kwargs):
            if "qmt-headless export " in command:
                await asyncio.Event().wait()
            return await super().exec(command, **kwargs)

    await asyncio.wait_for(
        setup.agent(export_timeout_sec=0.2).run("task", Hung(), AgentContext()), timeout=10
    )


def test_context_is_populated_from_final_metrics(setup):
    agent = setup.agent()
    (setup.logs / "trajectory.json").write_text(
        json.dumps(
            {
                "final_metrics": {
                    "total_prompt_tokens": 600,
                    "total_completion_tokens": 66,
                    "total_cached_tokens": 5,
                    "total_cost_usd": 0.5,
                }
            }
        )
    )
    context = AgentContext()
    agent.populate_context_post_run(context)
    assert (context.n_input_tokens, context.n_output_tokens) == (600, 66)
    assert (context.n_cache_tokens, context.cost_usd) == (5, 0.5)


def test_missing_trajectory_leaves_context_empty(setup):
    context = AgentContext()
    setup.agent().populate_context_post_run(context)
    assert context.n_input_tokens is None
