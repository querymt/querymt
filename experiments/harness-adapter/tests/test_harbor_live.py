"""Live Harbor + Docker rollout of the querymt harness (needs Docker and an endpoint).

Gated by QMT_RUN_LIVE_HARBOR=1. Required environment:

  QMT_LIVE_LLM_URL   OpenAI-compatible endpoint, e.g. a local vLLM serving Qwen/Qwen3.5-2B
  QMT_LIVE_MODEL     the served model id
  QMT_LIVE_DATASET   a Terminal-Bench 2.1 Harbor dataset (path or Hub id)

Optional:

  QMT_LIVE_TIER      `local` (default; llama-server, text capture, no token ids) or `vllm`
                     (also asserts trainable tokens)
  QMT_HARNESS_CONFIG a config whose [headless_binary] points at the archives to test
                     (`scripts/package-headless.sh` writes configs/local.toml)
  QMT_DIRECT_ADVERTISE_HOST  host the sandbox reaches the capture proxy on
                     (default `host.docker.internal`)
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

pytestmark = [
    pytest.mark.live,
    pytest.mark.skipif(
        os.environ.get("QMT_RUN_LIVE_HARBOR") != "1", reason="set QMT_RUN_LIVE_HARBOR=1"
    ),
]

REFERENCE = Path(
    os.environ.get("QMT_HARNESS_CONFIG")
    or Path(__file__).resolve().parents[1] / "configs" / "reference.toml"
).resolve()  # QMT_HARNESS_CONFIG may be relative, as the README's smoke command has it


def _env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        pytest.fail(f"{name} must be set for the live Harbor test")
    return value


def rollout(
    out: Path,
    *,
    config: Path | None = None,
    agent_timeout_sec: float | None = None,
    keep_trials: Path | None = None,
) -> list[dict]:
    env = dict(os.environ)
    env.setdefault("QMT_DIRECT_ADVERTISE_HOST", "host.docker.internal")
    if agent_timeout_sec is not None:
        env["QMT_AGENT_TIMEOUT_SEC"] = str(agent_timeout_sec)
    if config is not None:
        env["QMT_HARNESS_CONFIG"] = str(config)
    subprocess.run(
        [
            sys.executable,
            "-m",
            "querymt_harbor.rollout",
            "--harness",
            "querymt",
            "--sandbox",
            "docker",
            "--expose",
            "direct",
            "--llm-url",
            _env("QMT_LIVE_LLM_URL"),
            "--model",
            _env("QMT_LIVE_MODEL"),
            "--dataset",
            _env("QMT_LIVE_DATASET"),
            "-n",
            "1",
            "--out",
            str(out),
            *(["--trials-dir", str(keep_trials)] if keep_trials else []),
        ],
        env=env,
        check=False,
        timeout=3600,
    )
    return json.loads(out.read_text())


def test_harbor_docker_rollout_end_to_end(tmp_path):
    (result,) = rollout(tmp_path / "ok.json")

    assert result["ok"], result.get("error")
    assert result["reward"] is not None
    assert result["n_turns"] > 0
    assert result["n_roots"] == 1
    assert result["atif"] == "match"
    if os.environ.get("QMT_LIVE_TIER", "local") == "vllm":
        assert result["n_trainable_tokens"] > 0


class _ProcessWatch:
    """Samples `qmt-headless run` processes in the running task container until stopped."""

    def __init__(self) -> None:
        self.samples: list[tuple[float, int]] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _count(self, container: str) -> int | None:
        proc = subprocess.run(
            ["docker", "exec", container, "sh", "-c",
             "ps -eo args | grep 'qmt-headless run' | grep -v grep | grep -vc 'sh -c'"],
            capture_output=True, text=True, check=False,
        )  # fmt: skip
        out = proc.stdout.strip()
        return int(out) if out.isdigit() else None

    def _run(self) -> None:
        while not self._stop.is_set():
            ids = subprocess.run(
                ["docker", "ps", "-q"], capture_output=True, text=True, check=False
            ).stdout.split()
            if ids:
                count = self._count(ids[0])
                if count is not None:
                    self.samples.append((time.monotonic(), count))
            self._stop.wait(2)

    def __enter__(self) -> _ProcessWatch:
        self._thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self._stop.set()
        self._thread.join()


def test_agent_timeout_keeps_the_trajectory_and_leaves_no_process(tmp_path):
    trials = tmp_path / "trials"
    with _ProcessWatch() as watch:
        (result,) = rollout(tmp_path / "timeout.json", agent_timeout_sec=25, keep_trials=trials)

    assert not result["ok"]
    assert result["exception_type"] == "AgentTimeoutError"
    assert result["n_roots"] == 1
    (trial,) = list(trials.glob("oe-*"))
    summary = json.loads((trial / "agent/qmt/summary.json").read_text())
    assert summary["outcome"] == "incomplete", summary
    assert (trial / "agent/trajectory.json").is_file()
    # The agent ran, and after Harbor cancelled it the container (kept for the verifier) has no
    # `qmt-headless run` left.
    counts = [n for _, n in watch.samples]
    assert 1 in counts, counts
    last_running = max(i for i, n in enumerate(counts) if n >= 1)
    assert len(counts) > last_running + 1 and set(counts[last_running + 1 :]) == {0}, counts


def test_tampered_checksum_records_no_model_call(tmp_path):
    tampered = tmp_path / "reference.toml"
    text = REFERENCE.read_text()
    # Only the qmt-headless archive checksums: the plugin pin above them stays valid, so the
    # failure under test is the binary check and not an earlier plugin one.
    head, marker, binary = text.partition("[headless_binary")
    assert marker, "config has no [headless_binary] table"
    binary = re.sub(r'(sha256 = ")[0-9a-f]{64}(")', r"\g<1>" + "0" * 64 + r"\g<2>", binary)
    assert "0" * 64 in binary
    text = head + marker + binary
    # Paths in the config are relative to its own directory.
    text = text.replace('"../../../', f'"{REFERENCE.parents[3]}/')
    tampered.write_text(text)

    (result,) = rollout(tmp_path / "bad.json", config=tampered)

    assert not result["ok"]
    assert "sha256 mismatch" in (result.get("error") or "") + result.get("agent_log_tail", "")
    assert result["n_turns"] == 0
