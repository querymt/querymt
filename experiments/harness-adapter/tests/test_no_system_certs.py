"""`qmt-headless` must run in an image that has no CA certificates (design D2c)."""

from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest

from fake_server import FakeChatServer
from headless_run import export_args, run_args
from querymt_harbor.plugin import EMPTY_MODELS_REGISTRY

REPO = Path(__file__).resolve().parents[3]
EXAMPLES = REPO / "crates/agent/examples"
PROFILE_IN_CONTAINER = "/profile/confs/harness_adapter.toml"
IMAGE = "alpine:3"


def _musl_binary() -> Path:
    override = os.environ.get("QMT_HEADLESS_MUSL_BIN")
    if override:
        return Path(override)
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}[
        platform.machine().lower()
    ]
    return REPO / f"target/{arch}-unknown-linux-musl/release/examples/qmt-headless"


@pytest.mark.skipif(not os.environ.get("QMT_RUN_DOCKER"), reason="set QMT_RUN_DOCKER=1 (Docker)")
def test_rollout_runs_without_system_certificates(openai_plugin):
    """musl qmt-headless in alpine with no CA bundle: `run` exits 0 and the request arrives."""
    binary = _musl_binary()
    assert binary.is_file(), (
        f"build it first (cargo zigbuild --release ... --example qmt-headless): {binary}"
    )
    assert shutil.which("docker"), "docker is required"

    # Under the repo, not pytest's tmp_path: Docker VMs (Colima, Docker Desktop) share $HOME only.
    work = Path(tempfile.mkdtemp(prefix="docker-nocerts-", dir=REPO / "target"))
    home = work / "home"
    home.mkdir()
    shutil.copy(binary, work / "qmt-headless")
    shutil.copy(openai_plugin, work / "openai.wasm")
    (work / "instruction.txt").write_text("hi")
    (work / "cwd").mkdir()

    server = FakeChatServer(responder=lambda _i, _b: {"text": "ok"}, bind_host="0.0.0.0").start()
    try:
        base_url = f"http://host.docker.internal:{server.port}/v1"
        (home / "providers.toml").write_text(
            '[[providers]]\nname = "openai"\npath = "/work/openai.wasm"\n'
            '[providers.config]\nallowed_hosts = ["host.docker.internal"]\n'
        )
        (home / "models.dev.json").write_text(EMPTY_MODELS_REGISTRY)

        run = run_args(
            Path("/work/qmt-headless"),
            Path(PROFILE_IN_CONTAINER),
            Path("/work/cwd"),
            Path("/work/instruction.txt"),
            Path("/work/sessions.db"),
        )
        export = export_args(
            Path("/work/qmt-headless"),
            Path("/work/sessions.db"),
            Path("/work/trajectory.json"),
            Path("/work/summary.json"),
        )
        script = (
            # Strip every CA bundle from the image and prove it is gone.
            "rm -rf /etc/ssl /usr/share/ca-certificates /etc/ca-certificates* ; "
            "if [ -e /etc/ssl/certs/ca-certificates.crt ]; then echo CA_PRESENT; exit 99; fi ; "
            f"{' '.join(run)} ; rc=$? ; echo run_rc=$rc ; "
            f"{' '.join(export)} ; erc=$? ; echo export_rc=$erc ; "
            "exit $rc"
        )
        proc = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--add-host",
                "host.docker.internal:host-gateway",
                "-v",
                f"{work}:/work",
                "-v",
                f"{EXAMPLES}:/profile:ro",
                "-e",
                "QMT_HOME=/work/home",
                "-e",
                "QMT_CONFIG_DIR=/work/home",
                "-e",
                f"QMT_BASE_URL={base_url}",
                "-e",
                "QMT_MODEL=m",
                "-e",
                "QMT_API_KEY=k",
                "-e",
                "HOME=/tmp",
                IMAGE,
                "sh",
                "-c",
                script,
            ],
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        tail = (proc.stdout + proc.stderr)[-2000:]
        assert "CA_PRESENT" not in proc.stdout, tail
        assert "run_rc=0" in proc.stdout, tail
        assert proc.returncode == 0, tail
        assert len(server.requests) == 1, tail
        summary = json.loads((work / "summary.json").read_text())
        assert summary["outcome"] == "end_turn", summary
    finally:
        server.stop()
        shutil.rmtree(work, ignore_errors=True)
