"""Rollout reward (harness-adapter-training / Rollout reward)."""

from __future__ import annotations

import pickle

import pytest
from trl.experimental.async_grpo.openenv_harness import HarnessRolloutOutcome

from querymt_harbor.reward import RewardConfig, RolloutReward, correctness, score


def outcome(env_reward, tool_calls=0, *, timed_out=False, trace=None):
    return HarnessRolloutOutcome(
        env_reward=env_reward,
        completion=[{"role": "assistant", "content": "done"}],
        trace=trace or [],
        tool_call_count=tool_calls,
        tool_failure_count=0,
        tool_calls_by_name={},
        timed_out=timed_out,
    )


def test_solved_rollout_earns_the_efficiency_bonus():
    """correctness=1.0, 6 tool calls: 1.0 + 0.1 * 0.6 = 1.06."""
    assert RolloutReward()(outcome(1.0, 6)) == pytest.approx(1.06)


def test_unsolved_rollout_earns_no_bonus():
    assert RolloutReward()(outcome(0.0, 2)) == 0.0


def test_partial_correctness_earns_no_bonus():
    assert RolloutReward()(outcome(0.5, 1)) == 0.5


def test_reward_key_falls_back():
    assert correctness({"reward": 1.0}) == 1.0
    assert correctness({"correctness": 0.25, "reward": 1.0}) == 0.25
    assert correctness({"other": 1.0}) is None
    assert correctness({}) is None
    assert correctness(None) is None


def test_ungraded_rollout_is_excluded():
    """No verifier reward means unscored (None), which is not 0."""
    assert RolloutReward()(outcome(None, 3)) is None
    assert score(None, 3) is None


def test_bonus_is_clipped_at_both_ends():
    config = RewardConfig()
    assert score(1.0, 0, config) == pytest.approx(1.1)
    assert score(1.0, 15, config) == 1.0
    assert score(1.0, 40, config) == 1.0


def test_timeout_keeps_the_verifier_score():
    assert RolloutReward()(outcome(1.0, 15, timed_out=True)) == 1.0
    assert RolloutReward()(outcome(0.0, 15, timed_out=True)) == 0.0


def test_weights_are_configurable_and_validated():
    custom = RolloutReward(RewardConfig(bonus_weight=0.3, tool_budget=10))
    assert custom(outcome(1.0, 5)) == pytest.approx(1.15)
    with pytest.raises(ValueError, match="tool_budget"):
        RewardConfig(tool_budget=0)
    with pytest.raises(ValueError, match="bonus_weight"):
        RewardConfig(bonus_weight=-0.1)


def test_reward_function_pickles_for_trls_rollout_process(tmp_path):
    reward = RolloutReward(RewardConfig(0.2, 20), guard_enabled=False, block_log=tmp_path / "b")
    clone = pickle.loads(pickle.dumps(reward))
    assert clone == reward
    assert clone(outcome(1.0, 10)) == pytest.approx(1.1)
