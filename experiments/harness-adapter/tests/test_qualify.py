"""The qualification gate (harness-adapter-training / Qualification gate before training).

Fixtures are `HarborRolloutResult` models built from a recorded real rollout
(`fixtures/live_eval_rollout.json`: the live 3.8 run, `roots=1`, `atif=match`).
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from openenv.harbor.models import HarborRolloutResult

from querymt_harbor.qualify import (
    QUALIFY_FILE,
    QualificationFailed,
    check,
    ensure_qualified,
    qualify,
)

RECORDED = json.loads((Path(__file__).parent / "fixtures" / "live_eval_rollout.json").read_text())


def result(**overrides) -> HarborRolloutResult:
    """The spec's passing rollout (reward present, 7 turns, 1 root, 3120 tokens, atif=match)."""
    base = {**RECORDED, "rollout_type": "train", "n_turns": 7, "n_trainable_tokens": 3120}
    return HarborRolloutResult(**{**base, **overrides})


def test_passing_qualification_unlocks_training(tmp_path):
    qualification = qualify(tmp_path, result(reward=0.0))

    assert qualification.passed and qualification.failures == ()
    recorded = json.loads((tmp_path / QUALIFY_FILE).read_text())
    assert recorded["passed"] is True
    assert recorded["result"]["n_turns"] == 7
    assert recorded["result"]["n_trainable_tokens"] == 3120
    assert ensure_qualified(tmp_path).passed  # what the train stage calls


def test_forked_history_blocks_training(tmp_path):
    with pytest.raises(QualificationFailed, match="history was rewritten"):
        qualify(tmp_path, result(n_roots=3))
    assert json.loads((tmp_path / QUALIFY_FILE).read_text())["passed"] is False
    with pytest.raises(QualificationFailed, match="history was rewritten"):
        ensure_qualified(tmp_path)


def test_missing_tokens_block_training(tmp_path):
    with pytest.raises(QualificationFailed, match="did not return token ids and logprobs"):
        qualify(tmp_path, result(n_trainable_tokens=0))


@pytest.mark.parametrize("atif", ["MISMATCH", "none"])
def test_trace_mismatch_blocks_training(tmp_path, atif):
    with pytest.raises(QualificationFailed, match="trajectory"):
        qualify(tmp_path, result(atif=atif))


def test_trace_mismatch_and_missing_trajectory_are_told_apart():
    assert "disagree" in check(result(atif="MISMATCH")).failures[0]
    assert "missing" in check(result(atif="none")).failures[0]


def test_the_recorded_live_eval_rollout_is_not_trainable():
    """The real llama-server rollout has roots=1 and atif=match but no token ids."""
    qualification = check(HarborRolloutResult(**RECORDED))
    assert not qualification.passed
    assert len(qualification.failures) == 1
    assert "token ids" in qualification.failures[0]


def test_ungraded_rollout_blocks_training():
    qualification = check(result(reward=None, ok=False, error="verifier crashed"))
    assert not qualification.passed
    assert "no reward" in qualification.failures[0]
    assert "verifier crashed" in qualification.failures[0]


def test_no_turns_is_not_reported_as_a_forked_history():
    qualification = check(result(n_turns=0, n_roots=0, n_trainable_tokens=0))
    assert any("never reached the model endpoint" in f for f in qualification.failures)
    assert not any("rewritten" in f for f in qualification.failures)


def test_all_failures_are_reported_together():
    qualification = check(result(n_roots=3, n_trainable_tokens=0, atif="MISMATCH"))
    assert len(qualification.failures) == 3


def test_a_dict_result_is_accepted():
    assert check(result().model_dump()).passed


def test_training_is_blocked_when_qualification_never_ran(tmp_path):
    with pytest.raises(QualificationFailed, match="run the qualify stage"):
        ensure_qualified(tmp_path)
