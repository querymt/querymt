"""The querymt harness registration and its isolation from other harnesses."""

from __future__ import annotations

import copy
import os

import pytest
from openenv.harbor import seams
from openenv.harbor.rollout import build_trial_config

import querymt_harbor.seam as querymt_seam
from querymt_harbor import _delegate


def trial(harness: str, tmp_path, **kw):
    return build_trial_config(
        task_dir=tmp_path,
        harness=harness,
        sandbox="docker",
        intercept_url="http://proxy.test:8100",
        session_id="sess-1",
        model="Qwen/adapter-x",
        trial_name="t",
        trials_dir=tmp_path / "trials",
        **kw,
    )


def test_wrapper_exposes_the_querymt_harness(tmp_path):
    assert seams.get("querymt").import_path == "querymt_harbor.agent:QueryMTAgent"

    config = trial("querymt", tmp_path, agent_step_limit=30)

    assert config.agent.import_path == "querymt_harbor.agent:QueryMTAgent"
    assert config.agent.model_name == "adapter-x"  # OpenEnv's leaf-name normalisation
    assert config.agent.env == {
        "QMT_BASE_URL": "http://proxy.test:8100/v1",
        "QMT_API_KEY": "sess-1",
        "QMT_MODEL": "adapter-x",
    }
    assert config.agent.kwargs == {"step_limit": 30}


def test_wrapper_leaves_other_harnesses_unchanged(tmp_path):
    before = {name: copy.copy(seam) for name, seam in seams.SEAMS.items() if name != "querymt"}
    plain = trial("pi", tmp_path).model_dump(mode="json")

    querymt_seam.register()
    querymt_seam.register()  # idempotent

    assert trial("pi", tmp_path).model_dump(mode="json") == plain
    assert {n: s for n, s in seams.SEAMS.items() if n != "querymt"} == before
    assert list(seams.SEAMS).count("querymt") == 1


def test_verifier_keeps_its_own_credentials(tmp_path, monkeypatch):
    monkeypatch.setenv("OPENAI_API_KEY", "grader-key")
    monkeypatch.delenv("QMT_API_KEY", raising=False)

    _model, _kwargs, agent_env, proc_env = seams.get("querymt").resolve(
        base_url="http://proxy.test:8100", session="sess-1", model="adapter-x"
    )
    config = trial("querymt", tmp_path)

    assert proc_env == {}
    assert set(config.agent.env) == {"QMT_BASE_URL", "QMT_API_KEY", "QMT_MODEL"}
    assert set(agent_env) == set(config.agent.env)
    assert os.environ["OPENAI_API_KEY"] == "grader-key"
    assert "QMT_API_KEY" not in os.environ
    # Nothing the verifier could read: no OPENAI_* in the trial's agent env either.
    assert not any(key.startswith("OPENAI_") for key in config.agent.env)


@pytest.mark.parametrize("command", ["serve", "rollout"])
def test_wrappers_delegate_with_the_same_argv(monkeypatch, command):
    seen = {}

    def fake_main():
        import sys

        seen["argv"] = list(sys.argv)
        seen["registered"] = "querymt" in seams.SEAMS

    monkeypatch.setattr("openenv.cli.__main__.main", fake_main)
    monkeypatch.delitem(seams.SEAMS, "querymt", raising=False)

    _delegate.delegate(command, ["--harness", "querymt", "--llm-url", "http://x", "-n", "2"])

    assert seen["argv"] == [
        "openenv",
        "harbor",
        command,
        "--harness",
        "querymt",
        "--llm-url",
        "http://x",
        "-n",
        "2",
    ]
    assert seen["registered"]


def test_direct_exposure_advertises_the_configured_host(monkeypatch):
    from openenv.core.harness.capture.forwarding import make_forwarder

    querymt_seam.register()
    monkeypatch.setenv("QMT_DIRECT_ADVERTISE_HOST", "host.docker.internal")
    assert make_forwarder("direct").start(8100) == "http://host.docker.internal:8100"
    monkeypatch.delenv("QMT_DIRECT_ADVERTISE_HOST")
    assert make_forwarder("direct").start(8100) == "http://127.0.0.1:8100"


def test_agent_timeout_env_overrides_the_task_timeout(monkeypatch):
    import asyncio

    from openenv.harbor import runner

    seen: dict = {}

    async def fake(*_args, **kwargs):
        seen.update(kwargs)

    monkeypatch.setattr(runner, "run_rollout", fake)
    querymt_seam._register_agent_timeout()
    monkeypatch.setenv("QMT_AGENT_TIMEOUT_SEC", "45")
    asyncio.run(runner.run_rollout())
    assert seen["agent_timeout_sec"] == 45.0
    seen.clear()
    monkeypatch.delenv("QMT_AGENT_TIMEOUT_SEC")
    asyncio.run(runner.run_rollout())
    assert seen.get("agent_timeout_sec") is None
