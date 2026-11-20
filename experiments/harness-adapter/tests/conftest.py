"""Shared fixtures: the real qmt-headless binary, the openai wasm plugin, a fake endpoint."""

from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

os.environ.setdefault("TRL_EXPERIMENTAL_SILENCE", "1")
sys.path.insert(0, str(Path(__file__).parent))

from fake_server import FakeChatServer  # noqa: E402

REPO = Path(__file__).resolve().parents[3]
PROFILE = REPO / "crates/agent/examples/confs/harness_adapter.toml"


def _cargo(*args: str) -> None:
    env = dict(os.environ)
    # Building the facade with `remote` needs no dashboard, but a fixture dir keeps
    # build.rs from downloading a UI if the feature set ever pulls it in.
    env.setdefault("QMT_UI_DIST", str(_ui_fixture()))
    subprocess.run(["cargo", *args], cwd=REPO, check=True, env=env)


def _ui_fixture() -> Path:
    path = Path(os.environ.get("TMPDIR", "/tmp")) / "qmt-ui-dist"
    path.mkdir(parents=True, exist_ok=True)
    (path / "index.html").write_text("<!doctype html><title>QueryMT</title>\n")
    (path / "querymt-ui.json").write_text(
        '{"revision":"ci","target":"embedded","acpWebSocketPath":"/acp/ws"}\n'
    )
    return path


_PROVIDER_KEY_VARS = (
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "XAI_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "DEEPSEEK_API_KEY",
    "HF_TOKEN",
)


@pytest.fixture(autouse=True)
def _no_real_credentials(monkeypatch: pytest.MonkeyPatch) -> None:
    """Tests talk to a fake server on localhost; no real credential may ever reach it.

    Spawned processes inherit `os.environ`, so removing the variables here also
    removes them from every `qmt-headless` the tests start.
    """
    for name in _PROVIDER_KEY_VARS:
        monkeypatch.delenv(name, raising=False)


@pytest.fixture(scope="session")
def profile_path() -> Path:
    return PROFILE


@pytest.fixture(scope="session")
def headless_bin() -> Path:
    """Path to qmt-headless: `QMT_HEADLESS_BIN`, else built once with cargo."""
    override = os.environ.get("QMT_HEADLESS_BIN")
    if override:
        path = Path(override)
        assert path.is_file(), f"QMT_HEADLESS_BIN does not exist: {path}"
        return path
    _cargo("build", "-p", "querymt-agent", "--example", "qmt-headless")
    return REPO / "target/debug/examples/qmt-headless"


@pytest.fixture(scope="session")
def openai_plugin() -> Path:
    """The openai provider wasm, built locally so tests need no network."""
    override = os.environ.get("QMT_OPENAI_WASM")
    if override:
        return Path(override)
    _cargo("build", "-p", "qmt-openai", "--release", "--target", "wasm32-wasip1")
    return REPO / "target/wasm32-wasip1/release/qmt_openai.wasm"


@pytest.fixture
def qmt_home(tmp_path: Path, openai_plugin: Path) -> Path:
    """A QMT_HOME/QMT_CONFIG_DIR whose providers.toml points at the local plugin."""
    home = tmp_path / "qmt-home"
    home.mkdir()
    # The plugin manifest only allows its default host; the fake endpoint is on 127.0.0.1.
    (home / "providers.toml").write_text(
        f'[[providers]]\nname = "openai"\npath = "{openai_plugin}"\n'
        '[providers.config]\nallowed_hosts = ["127.0.0.1"]\n'
    )
    return home


@pytest.fixture
def fake_chat_server() -> Iterator[FakeChatServer]:
    server = FakeChatServer(responder=lambda _i, _b: {"text": "done"}).start()
    try:
        yield server
    finally:
        server.stop()
