"""Training orchestration on CPU with a stub trainer.

Covers harness-adapter-training / Recorded training configuration and / Checkpoints and resume.
The stub is driven by a real `AsyncGRPOConfig` built from the resolved configuration, so the
interval and retention it applies are the ones TRL would be given.
"""

from __future__ import annotations

import hashlib
import json
import pickle
import sys
from pathlib import Path

import pytest

from querymt_harbor import train
from querymt_harbor.config import DEFAULT_CONFIG, load_reference
from querymt_harbor.guard import METRIC, BlockLog
from querymt_harbor.qualify import QualificationFailed, check, record
from querymt_harbor.reward import RolloutReward
from querymt_harbor.train import (
    BLOCK_LOG_FILE,
    CHECKPOINT_DIR,
    CONFIG_FILE,
    ConfigChanged,
    GuardMetricCallback,
    build_async_grpo_config,
    build_lora_config,
    latest_complete_checkpoint,
    overrides_from,
    resolve,
    reward_function,
    run_training,
)

PASSING_ROLLOUT = {
    "reward": 0.0,
    "n_turns": 7,
    "n_roots": 1,
    "n_trainable_tokens": 3120,
    "atif": "match",
}


class Interrupted(Exception):
    """The process died mid-run."""


class StubTrainer:
    """Emulates HF Trainer's step loop and checkpointing; trains nothing."""

    def __init__(self, args, run_dir: Path, stop_after: int | None = None):
        self.args = args
        self.run_dir = run_dir
        self.stop_after = stop_after
        self.started_from: int | None = None
        self.steps_run: list[int] = []
        self.config_present_at_first_step: bool | None = None
        self.saved_model = False

    def _save_checkpoint(self, step: int) -> None:
        path = Path(self.args.output_dir) / f"checkpoint-{step}"
        path.mkdir(parents=True)
        (path / "adapter_model.safetensors").write_bytes(b"lora")
        (path / "trainer_state.json").write_text(json.dumps({"global_step": step}))  # written last
        limit = self.args.save_total_limit
        if limit:  # what HF does when a limit is set
            kept = sorted(
                Path(self.args.output_dir).glob("checkpoint-*"), key=lambda p: int(p.name[11:])
            )
            for old in kept[:-limit]:
                for child in old.iterdir():
                    child.unlink()
                old.rmdir()

    def train(self, resume_from_checkpoint: str | None = None):
        step = int(Path(resume_from_checkpoint).name.split("-")[1]) if resume_from_checkpoint else 0
        self.started_from = step
        self.config_present_at_first_step = (self.run_dir / CONFIG_FILE).is_file()
        while step < self.args.max_steps:
            step += 1
            self.steps_run.append(step)
            if step % self.args.save_steps == 0:
                self._save_checkpoint(step)
            if self.stop_after == step:
                raise Interrupted(step)

    def save_model(self, output_dir=None):
        self.saved_model = True


@pytest.fixture
def reference():
    return load_reference(DEFAULT_CONFIG)


def qualified_run(tmp_path: Path) -> Path:
    run_dir = tmp_path / "run"
    record(run_dir, check(PASSING_ROLLOUT))
    return run_dir


def config_for(reference, run_dir: Path, **budget):
    return resolve(reference, run_dir, {"budget": budget} if budget else None)


def factory_for(run_dir: Path, holder: list, *, stop_after=None):
    def factory(config):
        args = build_async_grpo_config(config, bf16=False, report_to="none")
        trainer = StubTrainer(args, run_dir, stop_after)
        holder.append(trainer)
        return trainer

    return factory


def test_defaults_reproduce_the_reference_recipe(reference, tmp_path):
    config = resolve(reference, tmp_path / "run")

    assert config["lora"] == {
        "r": 32,
        "alpha": 64,
        "target_modules": [
            "q_proj",
            "k_proj",
            "v_proj",
            "o_proj",
            "gate_proj",
            "up_proj",
            "down_proj",
        ],
    }
    assert config["optimizer"]["learning_rate"] == 2e-5
    assert config["sampling"] == {
        "num_generations": 8,
        "temperature": 0.8,
        "top_p": 1.0,
        "thinking": False,
    }
    budget = config["budget"]
    assert budget["agent_step_limit"] == 50
    assert budget["agent_timeout_sec"] == 1800
    assert budget["max_staleness"] == 16
    assert budget["max_steps"] == 1000
    assert budget["gradient_accumulation_steps"] == 16
    assert budget["micro_batch_max_tokens"] == 65536
    assert config["reward"] == {"bonus_weight": 0.1, "tool_budget": 15}
    assert config["checkpoints"] == {"save_every": 10, "keep_all": True}
    assert config["run"]["seed"] == 0


def test_every_card_value_reaches_its_trl_field(reference, tmp_path):
    """The D5 mapping, checked against the pinned `AsyncGRPOConfig` (task 4.1)."""
    config = resolve(reference, tmp_path / "run")
    trl = build_async_grpo_config(config, bf16=False, report_to="none")

    assert trl.learning_rate == 2e-5
    assert trl.num_generations == 8
    assert trl.temperature == 0.8
    assert trl.top_p == 1.0
    assert trl.chat_template_kwargs == {"enable_thinking": False}
    assert trl.max_staleness == 16
    assert trl.gradient_accumulation_steps == 16
    assert trl.token_budget == 65536  # <= 65,536 tokens per micro-batch row
    assert trl.max_steps == 1000
    assert trl.save_steps == 10
    assert trl.save_total_limit is None  # every checkpoint is kept
    assert trl.seed == 0
    assert trl.output_dir == str(tmp_path / "run" / CHECKPOINT_DIR)

    lora = build_lora_config(config)
    assert (lora.r, lora.lora_alpha) == (32, 64)
    assert set(lora.target_modules) == set(config["lora"]["target_modules"])


def test_configuration_is_written_before_training(reference, tmp_path):
    run_dir = qualified_run(tmp_path)
    holder: list[StubTrainer] = []

    run_training(config_for(reference, run_dir, max_steps=10), factory_for(run_dir, holder))

    assert holder[0].config_present_at_first_step is True
    recorded = json.loads((run_dir / CONFIG_FILE).read_text())
    profile_sha = hashlib.sha256(reference.profile.read_bytes()).hexdigest()
    assert recorded["provenance"]["profile_sha256"] == profile_sha
    assert recorded["provenance"]["plugin"]["blob_sha256"] == reference.plugin.blob_sha256
    assert recorded["provenance"]["headless"]["version"] == reference.headless.version
    assert set(recorded["provenance"]["prompts_sha256"]) == {p.name for p in reference.prompts}
    assert holder[0].saved_model


def test_checkpoints_are_kept_at_the_interval(reference, tmp_path):
    run_dir = qualified_run(tmp_path)
    holder: list[StubTrainer] = []

    run_training(config_for(reference, run_dir, max_steps=30), factory_for(run_dir, holder))

    names = sorted(p.name for p in (run_dir / CHECKPOINT_DIR).glob("checkpoint-*"))
    assert names == ["checkpoint-10", "checkpoint-20", "checkpoint-30"]
    assert all(
        (p / "adapter_model.safetensors").is_file() for p in (run_dir / CHECKPOINT_DIR).iterdir()
    )


def test_a_retention_limit_would_have_deleted_checkpoints(reference, tmp_path):
    """Guards the test above: the stub does delete when `keep_all` is off."""
    run_dir = qualified_run(tmp_path)
    config = resolve(
        reference, run_dir, {"budget": {"max_steps": 50}, "checkpoints": {"keep_all": False}}
    )
    run_training(config, factory_for(run_dir, []))
    names = sorted(p.name for p in (run_dir / CHECKPOINT_DIR).glob("checkpoint-*"))
    assert names == ["checkpoint-30", "checkpoint-40", "checkpoint-50"]
    assert config["trl"]["save_total_limit"] == 3


def test_interrupted_run_resumes(reference, tmp_path):
    run_dir = qualified_run(tmp_path)
    config = config_for(reference, run_dir, max_steps=50)

    first: list[StubTrainer] = []
    with pytest.raises(Interrupted):
        run_training(config, factory_for(run_dir, first, stop_after=34))
    assert first[0].steps_run[-1] == 34

    second: list[StubTrainer] = []
    result = run_training(config, factory_for(run_dir, second))

    assert second[0].started_from == 30
    assert second[0].steps_run[0] == 31, "steps up to 30 are not repeated"
    assert result.resumed_from == run_dir / CHECKPOINT_DIR / "checkpoint-30"
    names = sorted(
        (p.name for p in (run_dir / CHECKPOINT_DIR).glob("checkpoint-*")), key=lambda n: int(n[11:])
    )
    assert names == [f"checkpoint-{n}" for n in (10, 20, 30, 40, 50)]


def test_an_interrupted_save_is_not_resumed_from(reference, tmp_path):
    run_dir = qualified_run(tmp_path)
    ckpts = run_dir / CHECKPOINT_DIR
    for step, complete in ((10, True), (20, True), (30, False)):
        (ckpts / f"checkpoint-{step}").mkdir(parents=True)
        (ckpts / f"checkpoint-{step}" / "adapter_model.safetensors").write_bytes(b"x")
        if complete:
            (ckpts / f"checkpoint-{step}" / "trainer_state.json").write_text("{}")

    assert latest_complete_checkpoint(ckpts) == ckpts / "checkpoint-20"
    assert latest_complete_checkpoint(run_dir / "nothing") is None


def test_resuming_with_a_changed_configuration_is_refused(reference, tmp_path):
    run_dir = qualified_run(tmp_path)
    run_training(config_for(reference, run_dir, max_steps=10), factory_for(run_dir, []))

    changed = resolve(
        reference, run_dir, {"budget": {"max_steps": 10}, "optimizer": {"learning_rate": 1e-4}}
    )
    built: list[StubTrainer] = []
    with pytest.raises(ConfigChanged, match="different configuration"):
        run_training(changed, factory_for(run_dir, built))
    assert built == [], "no trainer is built for a changed recipe"


def test_training_requires_a_passing_qualification(reference, tmp_path):
    run_dir = tmp_path / "run"
    built: list[StubTrainer] = []
    with pytest.raises(QualificationFailed, match="run the qualify stage"):
        run_training(config_for(reference, run_dir, max_steps=10), factory_for(run_dir, built))
    assert built == [] and not (run_dir / CONFIG_FILE).exists()

    record(run_dir, check({**PASSING_ROLLOUT, "n_roots": 3}))
    with pytest.raises(QualificationFailed, match="history was rewritten"):
        run_training(config_for(reference, run_dir, max_steps=10), factory_for(run_dir, built))
    assert built == []


def test_overrides_reject_unknown_keys(reference, tmp_path):
    with pytest.raises(ValueError, match="unknown configuration key 'optimiser'"):
        resolve(reference, tmp_path, {"optimiser": {"learning_rate": 1}})
    with pytest.raises(ValueError, match="unknown configuration key 'learning_rte'"):
        resolve(reference, tmp_path, {"optimizer": {"learning_rte": 1}})
    assert overrides_from(
        ["optimizer.learning_rate=1e-5", "guard.enabled=false", "model.base=x/y"]
    ) == {
        "optimizer": {"learning_rate": 1e-5},
        "guard": {"enabled": False},
        "model": {"base": "x/y"},
    }


def test_reward_function_uses_the_configured_weights(reference, tmp_path):
    config = resolve(
        reference, tmp_path / "run", {"reward": {"bonus_weight": 0.3, "tool_budget": 10}}
    )
    reward = reward_function(config, tmp_path / "run")

    assert isinstance(reward, RolloutReward)
    assert reward.config.bonus_weight == 0.3 and reward.config.tool_budget == 10
    assert reward.guard_enabled is True
    assert reward.block_log == tmp_path / "run" / BLOCK_LOG_FILE
    assert pickle.loads(pickle.dumps(reward)) == reward  # TRL runs it in the rollout process


def test_guard_metric_is_logged_per_step(tmp_path):
    log = BlockLog(tmp_path / "blocked")
    logged: list[dict] = []
    callback = GuardMetricCallback(log, logged.append)

    log.record(1)
    log.record(2)
    callback.on_step_end(None, None, None)
    callback.on_step_end(None, None, None)

    assert logged == [{METRIC: 2.0}, {METRIC: 0.0}]


def test_training_needs_no_openenv_source_checkout(reference, tmp_path, monkeypatch):
    # `harbor_env` is not in the wheel; the session factory is vendored, so training must not
    # depend on it being importable.
    monkeypatch.setitem(sys.modules, "harbor_env", None)
    monkeypatch.setitem(sys.modules, "harbor_env.harness", None)
    import querymt_harbor.harbor_session as vendored

    assert vendored.HarborSessionFactory.__module__ == "querymt_harbor.harbor_session"
    assert not hasattr(train, "HARBOR_ENV_HINT")
