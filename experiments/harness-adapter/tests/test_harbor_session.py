"""The vendored `HarborSessionFactory`: the interface training relies on, on a fake Harbor server.

The file is a copy of OpenEnv v0.7.0's `harbor_env/harness.py` (the wheel does not ship it), so
these tests pin what this experiment uses of it: prompts map back to tasks, a session runs one
rollout and reports the verifier's score, and the factory survives the pickling TRL does.
"""

from __future__ import annotations

import pickle

import pytest
from openenv.harbor.models import HarborRolloutResult, HarborTaskRef, HarborTurn

from querymt_harbor.harbor_session import (
    HarborSession,
    HarborSessionFactory,
    instruction_id,
)

TASKS = [
    HarborTaskRef(index=0, task_id="a", task_name="alpha", instruction="Fix the alpha bug."),
    HarborTaskRef(index=1, task_id="b", task_name="beta", instruction="Fix the beta bug."),
]


class FakeEnv:
    """Stands in for `HarborEnv`: records `run_rollout` calls and returns a scripted result."""

    def __init__(self, result=None, error: Exception | None = None):
        self.result = result or HarborRolloutResult(reward=1.0, n_turns=1, ok=True)
        self.error = error
        self.calls: list[dict] = []
        self.closed = False

    def splits(self):
        return [{"name": "train-split", "num_tasks": len(TASKS)}]

    def num_tasks(self, split):
        return len(TASKS)

    def get_task_range(self, split, start, stop):
        return [t.model_dump() for t in TASKS[start:stop]]

    def get_task(self, split, index):
        return TASKS[index]

    def run_rollout(self, **kwargs):
        self.calls.append(kwargs)
        if self.error:
            raise self.error
        return self.result

    def close(self):
        self.closed = True


def make_factory(env: FakeEnv, **kwargs) -> HarborSessionFactory:
    factory = HarborSessionFactory(
        "http://server:8001",
        split="train-split",
        llm_url="http://127.0.0.1:8787",
        model="Qwen/Qwen3.8-27B",
        harness="querymt",
        sandbox="e2b",
        reward_key="correctness,reward",
        agent_timeout_sec=1800,
        agent_step_limit=50,
        **kwargs,
    )
    factory._env = env
    factory.new_client = lambda: env  # type: ignore[method-assign]
    return factory


def test_prompt_rows_carry_the_instruction_and_the_task_index():
    rows = make_factory(FakeEnv()).prompt_rows()
    assert [r["task_index"] for r in rows] == [0, 1]
    assert rows[1]["prompt"] == [{"role": "user", "content": "Fix the beta bug."}]


def test_a_prompt_maps_back_to_its_task_and_one_rollout_runs_it():
    env = FakeEnv()
    factory = make_factory(env)
    session = factory.create([{"role": "user", "content": "Fix the beta bug."}])

    assert session.wait_for_completion() == 0
    (call,) = env.calls
    assert call["task_index"] == 1
    assert (call["harness"], call["sandbox"]) == ("querymt", "e2b")
    assert (call["llm_url"], call["model"]) == ("http://127.0.0.1:8787", "Qwen/Qwen3.8-27B")
    assert (call["agent_timeout_sec"], call["agent_step_limit"]) == (1800, 50)
    assert call["reward_key"] == "correctness,reward"
    assert session.verify([]).env_reward == 1.0


def test_the_sampling_policy_is_sent_with_every_rollout():
    env = FakeEnv()
    factory = make_factory(env, sampling={"temperature": 0.8, "top_p": 1.0, "top_k": -1})
    factory.create("Fix the alpha bug.").wait_for_completion()
    assert env.calls[0]["sampling"]["temperature"] == 0.8


def test_an_unknown_prompt_is_refused():
    with pytest.raises(KeyError, match="does not match any task"):
        make_factory(FakeEnv()).create("something else entirely")


def test_a_failed_rollout_is_unscored_not_zero():
    # Server error: the call raises, the session reports failure and has no grade.
    session = make_factory(FakeEnv(error=RuntimeError("tunnel down"))).create("Fix the alpha bug.")
    assert session.wait_for_completion() == 1
    assert session.verify([]).env_reward is None

    # Server reports the rollout as not ok: the exit code says so and the reward stays unset.
    failed = HarborRolloutResult(ok=False, error="sandbox failed to start", reward=None)
    session = make_factory(FakeEnv(result=failed)).create("Fix the alpha bug.")
    assert session.wait_for_completion() == 1
    assert session.verify([]).env_reward is None


def test_each_session_gets_a_client_of_its_own_and_closes_it():
    env = FakeEnv()
    factory = make_factory(env)
    session = factory.create("Fix the alpha bug.")
    assert isinstance(session, HarborSession)
    session.close()
    assert env.closed


def test_an_eval_tier_rollout_has_no_trainable_trace():
    result = HarborRolloutResult(
        reward=1.0, rollout_type="eval", capture_level="text", turns=[HarborTurn(turn=0)]
    )
    session = make_factory(FakeEnv(result=result)).create("Fix the alpha bug.")
    session.wait_for_completion()
    assert session.fetch_proxy_trace() == []


def test_the_factory_survives_the_pickling_trl_does_for_its_rollout_process():
    factory = make_factory(FakeEnv())
    factory.prompt_rows()  # the parent builds the dataset first, which opens a client
    del factory.__dict__["new_client"]  # the test's lambda; a real factory has a method
    clone = pickle.loads(pickle.dumps(factory, protocol=pickle.HIGHEST_PROTOCOL))
    assert clone._env is None
    assert clone._by_instruction == factory._by_instruction


def test_instruction_ids_ignore_surrounding_whitespace():
    assert instruction_id("  x\n") == instruction_id("x")
