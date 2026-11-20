"""Register the `querymt` harness with OpenEnv's seam table (design D3).

Only `agent_env` is used. Harbor injects it into agent-phase commands alone, so the
verifier's own `OPENAI_API_KEY` is never touched, and no `env`/`proc_env` entry means
OpenEnv takes no process-global lock.
"""

from __future__ import annotations

import os

from openenv.core.harness.capture import forwarding
from openenv.harbor import seams

ADVERTISE_ENV = "QMT_DIRECT_ADVERTISE_HOST"
AGENT_TIMEOUT_ENV = "QMT_AGENT_TIMEOUT_SEC"
NAME = "querymt"
IMPORT_PATH = "querymt_harbor.agent:QueryMTAgent"


def build_seam() -> seams.Seam:
    return seams.Seam(
        name=NAME,
        dialect="openai_chat",
        model_fmt="{model}",
        import_path=IMPORT_PATH,
        agent_env={
            "QMT_BASE_URL": "{base_url}/v1",
            "QMT_API_KEY": "{session}",
            "QMT_MODEL": "{model}",
        },
        step_limit=lambda n: {"step_limit": n},
        status="untested",
        notes="qmt-headless, frozen harness adapter profile, native ATIF (querymt_harbor).",
    )


class _AdvertisedDirectExposure(forwarding.DirectExposure):
    """`--expose direct` advertising `$QMT_DIRECT_ADVERTISE_HOST` instead of 127.0.0.1.

    Inside a Docker sandbox `127.0.0.1` is the container itself, so the capture proxy (which
    listens on 0.0.0.0) is unreachable at the advertised address. OpenEnv's CLI has no option
    for the advertised host, so the wrapper reads it from the environment. Unset, behaviour is
    OpenEnv's own.
    """

    def __init__(self, advertise_host: str | None = None, scheme: str = "http") -> None:
        super().__init__(
            advertise_host or os.environ.get(ADVERTISE_ENV) or "127.0.0.1", scheme=scheme
        )


def _register_direct_exposure() -> None:
    if forwarding._FORWARDERS.get("direct") is forwarding.DirectExposure:
        forwarding._FORWARDERS["direct"] = _AdvertisedDirectExposure


def _register_agent_timeout() -> None:
    """Let `$QMT_AGENT_TIMEOUT_SEC` override the task's agent timeout.

    `openenv harbor rollout` has no option for it, but `run_rollout` takes
    `agent_timeout_sec`; this is how the live test exercises Harbor's timeout path in
    seconds instead of the task's 900. Unset, behaviour is OpenEnv's own.
    """
    from openenv.harbor import runner

    if getattr(runner.run_rollout, "_qmt_agent_timeout", False):
        return
    original = runner.run_rollout

    async def run_rollout(*args, **kwargs):
        value = os.environ.get(AGENT_TIMEOUT_ENV)
        if value and kwargs.get("agent_timeout_sec") is None:
            kwargs["agent_timeout_sec"] = float(value)
        return await original(*args, **kwargs)

    run_rollout._qmt_agent_timeout = True  # type: ignore[attr-defined]
    runner.run_rollout = run_rollout


def register() -> seams.Seam:
    """Add the seam to OpenEnv's table. Idempotent; never replaces another harness."""
    _register_direct_exposure()
    _register_agent_timeout()
    existing = seams.SEAMS.get(NAME)
    if existing is not None:
        return existing
    seam = build_seam()
    seams.SEAMS[NAME] = seam
    return seam


register()
