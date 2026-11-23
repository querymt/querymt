"""Training: the thin fork of TRL's `async_grpo_harbor.py` (design D5).

What differs from the upstream script, and nothing else:

- the reward is `querymt_harbor.reward.RolloutReward` (correctness with `reward` fallback, a
  configurable efficiency bonus, guard zeroing, unscored stays unscored);
- rollouts go through `--rollout-llm-url` (the safeagents proxy) while weights are synced to
  `--vllm-url` (vLLM itself);
- every checkpoint is kept (`save_total_limit=None`) and a restart resumes from the latest
  complete `checkpoint-*` in the run directory;
- every default comes from `configs/reference.toml`;
- the resolved configuration, with the profile SHA-256, is written before the first step.

`resolve` and `run_training` are pure orchestration and are tested on CPU with a stub trainer.
`build_harbor_trainer` is the part that needs a GPU host, a Harbor server and vLLM; it follows
the upstream script line for line and is verified by task 7.4, not by the CPU tests.
"""

from __future__ import annotations

import argparse
import copy
import json
import os
import re
import sys
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from transformers import TrainerCallback

from .config import Reference, harness_digests, load_reference
from .guard import METRIC, BlockLog
from .qualify import ensure_qualified
from .reward import RewardConfig, RolloutReward

CONFIG_FILE = "training_config.json"
CHECKPOINT_DIR = "checkpoints"
BLOCK_LOG_FILE = "guard_blocked.log"
_CHECKPOINT = re.compile(r"^checkpoint-(\d+)$")


class ConfigChanged(RuntimeError):
    """The run directory was started with a different configuration."""


def deep_merge(base: Mapping[str, Any], overrides: Mapping[str, Any]) -> dict[str, Any]:
    """`overrides` applied over `base`; an unknown key is an error, not a silent no-op."""
    merged = copy.deepcopy(dict(base))
    for key, value in overrides.items():
        if key not in merged:
            raise ValueError(f"unknown configuration key {key!r}; known: {sorted(merged)}")
        if isinstance(merged[key], Mapping):
            if not isinstance(value, Mapping):
                raise ValueError(f"{key!r} is a table, got {value!r}")
            merged[key] = deep_merge(merged[key], value)
        else:
            merged[key] = value
    return merged


def resolve(
    reference: Reference,
    run_dir: Path | None = None,
    overrides: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """The full resolved training configuration (JSON-able), as recorded in `training_config.json`.

    Unset values are the reference recipe's; `overrides` is a nested mapping over the same
    tables as `reference.toml` (for example `{"optimizer": {"learning_rate": 1e-5}}`).
    """
    raw = deep_merge(reference.raw, overrides or {})
    run = raw["run"]
    run_dir = Path(run_dir if run_dir is not None else run["dir"])
    lora, opt, sampling, budget = raw["lora"], raw["optimizer"], raw["sampling"], raw["budget"]
    checkpoints = raw["checkpoints"]

    # Card value -> TRL `AsyncGRPOConfig` field (design D5, confirmed against the pinned commit).
    trl = {
        "learning_rate": opt["learning_rate"],
        "num_generations": sampling["num_generations"],
        "temperature": sampling["temperature"],
        "top_p": sampling["top_p"],
        "top_k": -1,  # full vocabulary: the agent-owned loop records no candidate sets
        "chat_template_kwargs": {"enable_thinking": bool(sampling["thinking"])},
        "max_staleness": budget["max_staleness"],
        "gradient_accumulation_steps": budget["gradient_accumulation_steps"],
        "token_budget": budget["micro_batch_max_tokens"],
        "max_steps": budget["max_steps"],
        "save_strategy": "steps",
        "save_steps": checkpoints["save_every"],
        "save_total_limit": None if checkpoints["keep_all"] else 3,
        "seed": run["seed"],
        # Not on the card; as in the upstream example.
        "dtype": "bfloat16",
        "bf16": True,
        "optim": "adamw_torch",
        "activation_checkpointing": True,
        "activation_checkpointing_kwargs": {"use_reentrant": False},
        "logging_steps": 1,
        "log_completions": True,
        "num_completions_to_print": 2,
        "report_to": "trackio",
        "project": "qmt-harness-adapter",
        "run_name": run["name"],
    }
    headless = reference.headless
    return {
        "run": {"name": run["name"], "dir": str(run_dir), "seed": run["seed"]},
        "model": dict(raw["model"]),
        "lora": {
            "r": lora["r"],
            "alpha": lora["alpha"],
            "target_modules": list(lora["target_modules"]),
        },
        "optimizer": {"learning_rate": opt["learning_rate"]},
        "sampling": dict(sampling),
        "budget": dict(budget),
        "checkpoints": dict(checkpoints),
        "reward": dict(raw["reward"]),
        "guard": dict(raw["guard"]),
        "trl": trl,
        "provenance": {
            **harness_digests(reference),
            "plugin": {
                "repository": reference.plugin.repository,
                "manifest_digest": reference.plugin.manifest_digest,
                "blob_sha256": reference.plugin.blob_sha256,
            },
            "headless": {
                "version": headless.version,
                "sha256": {name: pin.sha256 for name, pin in sorted(headless.platforms.items())},
            },
            "data": {
                name: {"repo": spec["repo"], "revision": spec["revision"], "tasks": spec["tasks"]}
                for name, spec in raw["data"].items()
                if name != "benchmarks"
            },
            "benchmarks": [dict(b) for b in raw["data"]["benchmarks"]],
        },
    }


def build_async_grpo_config(config: Mapping[str, Any], **extra: Any):
    """The real TRL `AsyncGRPOConfig` for a resolved configuration.

    `extra` adds runtime values (`vllm_server_base_url`) or overrides for a CPU host
    (`bf16=False, report_to="none"`).
    """
    from trl import AsyncGRPOConfig

    run_dir = Path(config["run"]["dir"])
    kwargs = {**config["trl"], "output_dir": str(run_dir / CHECKPOINT_DIR), **extra}
    return AsyncGRPOConfig(**kwargs)


def build_lora_config(config: Mapping[str, Any]):
    from peft import LoraConfig

    lora = config["lora"]
    return LoraConfig(
        r=lora["r"], lora_alpha=lora["alpha"], target_modules=list(lora["target_modules"])
    )


def reward_function(config: Mapping[str, Any], run_dir: Path) -> RolloutReward:
    reward = config["reward"]
    guard = config["guard"]
    return RolloutReward(
        RewardConfig(bonus_weight=reward["bonus_weight"], tool_budget=reward["tool_budget"]),
        guard_enabled=bool(guard["enabled"]),
        block_log=run_dir / BLOCK_LOG_FILE,
    )


def latest_complete_checkpoint(checkpoints_dir: Path) -> Path | None:
    """The highest-step `checkpoint-N` that was fully written, or `None`.

    HF's Trainer writes `trainer_state.json` last, so a directory without it (or without the
    adapter weights) is a save that was interrupted and is skipped.
    """
    if not checkpoints_dir.is_dir():
        return None
    best: tuple[int, Path] | None = None
    for path in checkpoints_dir.iterdir():
        match = _CHECKPOINT.match(path.name)
        if not match or not path.is_dir():
            continue
        if not (path / "trainer_state.json").is_file():
            continue
        if not (path / "adapter_model.safetensors").is_file():
            continue
        step = int(match.group(1))
        if best is None or step > best[0]:
            best = (step, path)
    return best[1] if best else None


def checkpoint_step(path: Path) -> int:
    match = _CHECKPOINT.match(path.name)
    if not match:
        raise ValueError(f"not a checkpoint directory: {path}")
    return int(match.group(1))


class TrainerLike(Protocol):
    def train(self, resume_from_checkpoint: str | None = None) -> Any: ...

    def save_model(self, output_dir: str | None = None) -> Any: ...


TrainerFactory = Callable[[dict[str, Any]], TrainerLike]


@dataclass(frozen=True)
class TrainResult:
    config_path: Path
    resumed_from: Path | None


def write_config(run_dir: Path, config: Mapping[str, Any]) -> Path:
    """Write `training_config.json`, or verify it matches the one the run started with."""
    path = run_dir / CONFIG_FILE
    text = json.dumps(config, indent=2, sort_keys=True) + "\n"
    if path.is_file():
        if json.loads(path.read_text("utf-8")) != json.loads(text):
            raise ConfigChanged(
                f"{path} was written by a run with a different configuration; resuming with "
                "changed settings would mix two recipes in one run directory. Revert the change "
                "or start a new run directory."
            )
        return path
    run_dir.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(text, "utf-8")
    tmp.replace(path)
    return path


def run_training(
    config: dict[str, Any],
    trainer_factory: TrainerFactory,
    *,
    require_qualified: bool = True,
) -> TrainResult:
    """Qualify-check, record the configuration, resume if possible, train, save.

    The configuration file is written before the trainer is even built, so a run that dies at
    start-up still leaves its recipe behind, and it exists before the first optimiser step.
    """
    run_dir = Path(config["run"]["dir"])
    if require_qualified:
        ensure_qualified(run_dir)
    config_path = write_config(run_dir, config)
    resume = latest_complete_checkpoint(run_dir / CHECKPOINT_DIR)
    trainer = trainer_factory(config)
    trainer.train(resume_from_checkpoint=str(resume) if resume else None)
    trainer.save_model()
    return TrainResult(config_path, resume)


class GuardMetricCallback(TrainerCallback):
    """Logs `guard_blocked_rollouts`: blocked rollouts since the previous step.

    The reward function runs in TRL's rollout process, so it reports through a log file
    (`guard.BlockLog`) that this callback drains in the trainer process.
    """

    def __init__(self, block_log: BlockLog, log: Callable[[dict[str, float]], Any]) -> None:
        self.block_log = block_log
        self.log = log

    def on_step_end(self, args, state, control, **kwargs):  # noqa: ANN001
        self.log({METRIC: float(self.block_log.drain())})
        return control


def build_harbor_trainer(
    config: dict[str, Any],
    *,
    server: str,
    vllm_url: str,
    rollout_llm_url: str,
    split: str,
    sandbox: str = "e2b",
    harness: str = "querymt",
    reward_key: str = "correctness,reward",
):
    """The real trainer. Needs a GPU host, a Harbor server and vLLM (verified by task 7.4)."""
    from functools import partial

    from datasets import Dataset
    from transformers import AutoTokenizer
    from trl.experimental.async_grpo.openenv_harness import HarnessAsyncGRPOTrainer

    from .harbor_session import HarborSessionFactory  # vendored: the wheel lacks harbor_env

    run_dir = Path(config["run"]["dir"])
    budget = config["budget"]
    model = config["model"]["base"]
    factory = partial(
        HarborSessionFactory,
        server,
        split=split,
        harness=harness,
        sandbox=sandbox,
        llm_url=rollout_llm_url,  # safeagents (or vLLM when the guard is off)
        model=model,
        agent_timeout_sec=budget["agent_timeout_sec"],
        agent_step_limit=budget["agent_step_limit"],
        reward_key=reward_key,
    )
    probe = factory()
    try:
        dataset = Dataset.from_list(probe.prompt_rows())
    finally:
        if probe._env is not None:  # the pinned factory has no public close
            probe._env.close()

    trainer = HarnessAsyncGRPOTrainer(
        model=model,
        harness_session_factory=factory,
        args=build_async_grpo_config(config, vllm_server_base_url=vllm_url),
        train_dataset=dataset,
        processing_class=AutoTokenizer.from_pretrained(model),
        rollout_reward_fn=reward_function(config, run_dir),
        peft_config=build_lora_config(config),
    )
    if config["guard"]["enabled"]:
        trainer.add_callback(GuardMetricCallback(BlockLog(run_dir / BLOCK_LOG_FILE), trainer.log))
    return trainer


def _parse_override(text: str) -> tuple[list[str], Any]:
    key, _, value = text.partition("=")
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError:
        parsed = value
    return key.split("."), parsed


def overrides_from(items: Sequence[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for item in items:
        path, value = _parse_override(item)
        node = out
        for part in path[:-1]:
            node = node.setdefault(part, {})
        node[path[-1]] = value
    return out


def main(argv: Sequence[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", default=None, help="reference.toml (default: the packaged one)")
    parser.add_argument("--run-dir", default=None)
    parser.add_argument("--server", required=True, help="a running `openenv harbor serve`")
    parser.add_argument("--vllm-url", required=True, help="vLLM: generation and weight sync")
    parser.add_argument("--rollout-llm-url", default=None, help="the safeagents proxy")
    parser.add_argument("--split", required=True, help="the dataset the server serves")
    parser.add_argument("--sandbox", default=os.environ.get("QMT_SANDBOX", "e2b"))
    parser.add_argument("--override", action="append", default=[], metavar="SECTION.KEY=JSON")
    args = parser.parse_args(argv)

    reference = load_reference(args.config)
    config = resolve(reference, args.run_dir, overrides_from(args.override))
    guard = config["guard"]
    rollout_url = args.rollout_llm_url or (
        f"http://127.0.0.1:{guard['port']}" if guard["enabled"] else args.vllm_url
    )
    run_training(
        config,
        lambda cfg: build_harbor_trainer(
            cfg,
            server=args.server,
            vllm_url=args.vllm_url,
            rollout_llm_url=rollout_url,
            split=args.split,
            sandbox=args.sandbox,
        ),
    )


if __name__ == "__main__":
    sys.exit(main())
