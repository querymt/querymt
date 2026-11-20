"""Argv and environment for the processes the experiment starts (design D7).

Only builders live here, so the exact commands can be asserted without starting anything:
the launcher spawns them for training and evaluation, `evaluate` uses the vLLM one for the
multi-LoRA serving of checkpoints.

vLLM has two roles. The *trainer* role serves the base model on GPU 0 with the flags TRL needs
(runtime LoRA updates, token ids and processed logprobs, NCCL weight transfer). The *eval* role
serves the base model plus a set of checkpoints with `--lora-modules` and only the inference
flags of the model card, so the numbers are measured on the configuration that is published.
"""

from __future__ import annotations

import json
import sys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Inference flags shared by training and evaluation: what the adapter is trained and served with.
_INFERENCE = (
    "--enable-auto-tool-choice",
    "--tool-call-parser",
    "qwen3_xml",
    "--reasoning-parser",
    "qwen3",
    "--default-chat-template-kwargs",
    json.dumps({"enable_thinking": False}),
    "--language-model-only",
    "--enable-prefix-caching",
    "--mamba-cache-mode",
    "align",
)

# Only the trainer role: capture of token ids and logprobs, and weight sync from TRL.
_TRAINER_ONLY = (
    "--logprobs-mode",
    "processed_logprobs",
    "--return-tokens-as-token-ids",
    "--weight-transfer-config",
    json.dumps({"backend": "nccl"}, separators=(",", ":")),
)


def flatten_overrides(overrides: Mapping[str, Any], prefix: str = "") -> list[str]:
    """`{"optimizer": {"learning_rate": 1e-5}}` -> `["optimizer.learning_rate=1e-05"]`."""
    out: list[str] = []
    for key, value in overrides.items():
        path = f"{prefix}{key}"
        if isinstance(value, Mapping):
            out += flatten_overrides(value, f"{path}.")
        else:
            out.append(f"{path}={json.dumps(value)}")
    return out


SAFEAGENTS_MODULE = "safeagents.cli"
"""`safeagents` is a package without a `__main__`, so `python -m safeagents` fails; its CLI
module is the runnable one (the same code as the `safeagents` console script)."""


def safeagents_argv() -> list[str]:
    """The command that runs safeagents from this interpreter, whatever the venv layout."""
    return [sys.executable, "-m", SAFEAGENTS_MODULE]


@dataclass(frozen=True)
class Command:
    """One process: its argv and the environment it adds to the inherited one."""

    argv: tuple[str, ...]
    env: Mapping[str, str] = field(default_factory=dict)

    def shell(self) -> str:
        """A copy-pasteable rendering for logs and the runbook."""
        import shlex

        prefix = [f"{k}={shlex.quote(v)}" for k, v in sorted(self.env.items())]
        return " ".join([*prefix, *(shlex.quote(a) for a in self.argv)])


def vllm_command(
    raw: Mapping[str, Any],
    *,
    trainer: bool,
    adapters: Mapping[str, Path | str] | None = None,
    port: int | None = None,
) -> Command:
    """`vllm serve` for the trainer role, or for the eval role with `adapters` as LoRA modules.

    `adapters` maps served names to adapter directories (the eval role); the trainer role
    receives its adapter through weight sync and takes none.
    """
    serving, lora = raw["serving"], raw["lora"]
    if trainer and adapters:
        raise ValueError("the trainer role syncs its adapter at runtime and serves no modules")
    if adapters and len(adapters) > serving["max_loras"]:
        raise ValueError(
            f"{len(adapters)} adapters do not fit --max-loras {serving['max_loras']}; "
            "serve them in batches"
        )
    argv: list[str] = [
        "vllm",
        "serve",
        raw["model"]["base"],
        "--port",
        str(port if port is not None else serving["vllm_port"]),
        "--enable-lora",
        "--max-lora-rank",
        str(lora["r"]),
        "--max-loras",
        str(serving["max_loras"]),
    ]
    if adapters:
        argv += ["--lora-modules", *(f"{name}={path}" for name, path in adapters.items())]
    argv += [*_INFERENCE, "--max-model-len", str(serving["max_model_len"])]
    env = {"CUDA_VISIBLE_DEVICES": str(serving["vllm_gpu"])}
    if trainer:
        argv += _TRAINER_ONLY
        env |= {"VLLM_ALLOW_RUNTIME_LORA_UPDATING": "1", "VLLM_SERVER_DEV_MODE": "1"}
    return Command(tuple(argv), env)


def safeagents_command(raw: Mapping[str, Any], run_dir: Path) -> Command:
    """The guard proxy in the foreground, listening in front of vLLM.

    The journal stays in the run directory so a run's blocked calls are kept with it.
    """
    guard, serving = raw["guard"], raw["serving"]
    return Command(
        (
            *safeagents_argv(),
            "--journal",
            str(run_dir / "safeagents_calls.jsonl"),
            f"http://127.0.0.1:{serving['vllm_port']}",
            str(guard["port"]),
        )
    )


def server_command(
    raw: Mapping[str, Any],
    *,
    llm_url: str,
    datasets: Sequence[str],
) -> Command:
    """The Harbor env server with the querymt seam registered (`querymt_harbor.serve`)."""
    serving = raw["serving"]
    argv = [
        sys.executable,
        "-m",
        "querymt_harbor.serve",
        "--llm-url",
        llm_url,
        "--model",
        raw["model"]["base"],
        "--port",
        str(serving["server_port"]),
        "--capture-port",
        str(serving["capture_port"]),
        "--expose",
        serving["expose"],
    ]
    for dataset in datasets:
        argv += ["--dataset", dataset]
    # OpenEnv reads this at start-up; its default of 4 refuses the fifth concurrent rollout.
    env = {"MAX_CONCURRENT_ENVS": str(serving["max_concurrent_rollouts"])}
    return Command(tuple(argv), env)


def trainer_command(
    raw: Mapping[str, Any],
    run_dir: Path,
    *,
    split: str,
    config_path: str | Path | None = None,
    overrides: Mapping[str, Any] | None = None,
) -> Command:
    """`python -m querymt_harbor.train` on the trainer GPU, pointed at the other processes.

    `overrides` are the pipeline's `--override` values, handed on so the trainer resolves the
    same configuration the pipeline did.
    """
    serving, guard = raw["serving"], raw["guard"]
    vllm_url = f"http://127.0.0.1:{serving['vllm_port']}"
    rollout_url = f"http://127.0.0.1:{guard['port']}" if guard["enabled"] else vllm_url
    argv = [
        sys.executable,
        "-m",
        "querymt_harbor.train",
        "--run-dir",
        str(run_dir),
        "--server",
        f"http://127.0.0.1:{serving['server_port']}",
        "--vllm-url",
        vllm_url,
        "--rollout-llm-url",
        rollout_url,
        "--split",
        split,
    ]
    if config_path is not None:
        argv += ["--config", str(config_path)]
    for item in flatten_overrides(overrides or {}):
        argv += ["--override", item]
    return Command(tuple(argv), {"CUDA_VISIBLE_DEVICES": str(serving["trainer_gpu"])})
