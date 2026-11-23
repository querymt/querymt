"""Rollout reward (design D5, spec: Rollout reward / Guarded rollouts score zero).

    reward = correctness                                   if correctness < 1
           = correctness + bonus_weight * clip(1 - tool_calls / tool_budget, 0, 1)   otherwise

`None` means unscored: the verifier did not run. That is not 0, because TRL leaves an unscored
rollout out of its group's baseline. A timeout keeps the verifier's score. A rollout in which
safeagents blocked any tool call is 0 whatever the verifier said (a block is a policy
violation, not an infrastructure failure, so it is scored even when the verifier did not run).
"""

from __future__ import annotations

import logging
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .guard import BlockLog, count_blocked

logger = logging.getLogger(__name__)

SOLVED_AT = 1.0


@dataclass(frozen=True)
class RewardConfig:
    bonus_weight: float = 0.1
    tool_budget: float = 15.0

    def __post_init__(self) -> None:
        if self.bonus_weight < 0:
            raise ValueError(f"bonus_weight must be >= 0, got {self.bonus_weight!r}")
        if self.tool_budget <= 0:
            raise ValueError(f"tool_budget must be > 0, got {self.tool_budget!r}")


def correctness(rewards: Mapping[str, float] | None) -> float | None:
    """The verifier's `correctness`, or `reward` when `correctness` is absent.

    `None` when the verifier produced neither (it did not run).
    """
    if not rewards:
        return None
    for key in ("correctness", "reward"):
        if key in rewards:
            return float(rewards[key])
    return None


def efficiency(tool_calls: int, tool_budget: float) -> float:
    """`clip(1 - tool_calls / tool_budget, 0, 1)`."""
    return max(0.0, min(1.0, 1.0 - tool_calls / tool_budget))


def score(
    correctness_value: float | None, tool_calls: int, config: RewardConfig = RewardConfig()
) -> float | None:
    """The reward for a graded rollout, or `None` when it is unscored."""
    if correctness_value is None:
        return None
    reward = float(correctness_value)
    if reward >= SOLVED_AT:
        reward += config.bonus_weight * efficiency(tool_calls, config.tool_budget)
    return reward


@dataclass(frozen=True)
class RolloutReward:
    """TRL's `rollout_reward_fn`: `HarnessRolloutOutcome -> float | None`.

    A frozen dataclass of plain values, so it pickles into TRL's rollout process.
    """

    config: RewardConfig = RewardConfig()
    guard_enabled: bool = True
    block_log: Path | None = None
    """Where blocked rollouts are counted for the `guard_blocked_rollouts` metric."""

    def __call__(self, outcome: Any) -> float | None:
        if self.guard_enabled:
            blocked = count_blocked(getattr(outcome, "trace", None), outcome.completion)
            if blocked:
                if self.block_log is not None:
                    BlockLog(self.block_log).record(blocked)
                return 0.0
        value = score(outcome.env_reward, outcome.tool_call_count, self.config)
        if value is None:
            logger.warning(
                "verifier did not run (tool_calls=%d); rollout unscored", outcome.tool_call_count
            )
        elif outcome.timed_out:
            logger.warning("agent timed out; keeping the verifier's score of %.3f", value)
        return value
