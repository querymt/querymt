"""Checkpoint selection (harness-adapter-training / Checkpoint selection on a declared subset)."""

from __future__ import annotations

import json

import pytest

from querymt_harbor.select import (
    EVAL_REPORT_FILE,
    SELECTION_FILE,
    SELECTION_SET_FILE,
    TERMINAL_BENCH,
    SelectionSetChanged,
    declare_selection_set,
    load_selection_set,
    qualify_id,
    select_and_report,
    select_checkpoint,
)

SWE = [f"swe-{i:03d}" for i in range(82)]
TB = [f"tb-{i:02d}" for i in range(89)]


def declared(tmp_path, seed=0):
    return declare_selection_set(tmp_path, SWE, TB, seed=seed)


def scores_with(selection, solved_selection: int, *, held_out_solved: int = 0, ungraded=()):
    """A score table solving the first `solved_selection` selection tasks and some held-out ones."""
    table: dict[str, float | None] = dict.fromkeys(selection.task_ids, 0.0)
    for task in selection.task_ids[:solved_selection]:
        table[task] = 1.0
    table.update(dict.fromkeys(selection.held_out, 0.0))
    for task in selection.held_out[:held_out_solved]:
        table[task] = 1.0
    for task in ungraded:
        table[task] = None
    return table


def test_selection_set_is_declared_and_recorded(tmp_path):
    selection = declared(tmp_path)

    assert len(selection.swebench) == 82 and len(selection.terminal_bench) == 18
    assert len(selection.held_out) == 89 - 18
    assert not set(selection.terminal_bench) & set(selection.held_out)
    recorded = json.loads((tmp_path / SELECTION_SET_FILE).read_text())
    assert recorded["seed"] == 0
    assert len(recorded[TERMINAL_BENCH]) == 18
    assert load_selection_set(tmp_path) == selection


def test_subset_is_fixed_by_seed(tmp_path):
    a = declare_selection_set(tmp_path / "a", SWE, TB, seed=5)
    b = declare_selection_set(tmp_path / "b", SWE, list(reversed(TB)), seed=5)
    c = declare_selection_set(tmp_path / "c", SWE, TB, seed=6)
    assert a == b
    assert a.terminal_bench != c.terminal_bench


def test_the_declared_set_cannot_move_once_recorded(tmp_path):
    first = declared(tmp_path, seed=1)
    assert declared(tmp_path, seed=1) == first  # idempotent
    with pytest.raises(SelectionSetChanged, match="different selection set"):
        declared(tmp_path, seed=2)


def test_selection_uses_only_the_declared_set(tmp_path):
    """Checkpoints 140, 160, 180 score 0.52, 0.58, 0.55 on the 100-task selection set."""
    selection = declared(tmp_path)
    assert len(selection.task_ids) == 100
    # Held-out tasks are solved the other way round: if they leaked in, 140 would win.
    candidates = {
        "checkpoint-140": scores_with(selection, 52, held_out_solved=60),
        "checkpoint-160": scores_with(selection, 58, held_out_solved=0),
        "checkpoint-180": scores_with(selection, 55, held_out_solved=30),
    }

    chosen = select_checkpoint(selection, candidates)

    assert chosen.selected == "checkpoint-160"
    rates = {s.checkpoint: s.solve_rate for s in chosen.scores}
    assert rates == {"checkpoint-140": 0.52, "checkpoint-160": 0.58, "checkpoint-180": 0.55}
    assert chosen.selection_task_ids == selection.task_ids


def test_decision_record_lists_the_task_ids_and_scores(tmp_path):
    selection = declared(tmp_path)
    base = scores_with(selection, 30)
    candidates = {
        "checkpoint-80": scores_with(selection, 40),
        "checkpoint-100": scores_with(selection, 45),
    }

    select_and_report(tmp_path, base, candidates)

    record = json.loads((tmp_path / SELECTION_FILE).read_text())
    assert record["selected"] == "checkpoint-100"
    assert record["selection_task_ids"] == list(selection.task_ids)
    assert record["scores"]["checkpoint-80"]["solve_rate"] == 0.4


def test_held_out_result_is_reported(tmp_path):
    selection = declared(tmp_path)
    base = scores_with(selection, 30, held_out_solved=10)
    adapter = scores_with(selection, 60, held_out_solved=20)

    chosen, report = select_and_report(tmp_path, base, {"checkpoint-160": adapter})

    held = report["held_out_terminal_bench_vs_base"]
    assert chosen.selected == "checkpoint-160"
    assert held["n"] == 89 - 18, "only tasks outside the 18-task subset"
    assert set(held["task_ids"]) == set(selection.held_out)
    assert held["newly_solved"] == 10 and held["newly_lost"] == 0
    assert held["mean_diff_points"] == pytest.approx(100 * 10 / 71)
    assert json.loads((tmp_path / EVAL_REPORT_FILE).read_text()) == report
    # The selection-set comparison is a separate number, not the held-out one.
    assert report["selection_set_vs_base"]["n"] == 100


def test_ungraded_selection_tasks_count_as_unsolved_for_every_checkpoint(tmp_path):
    selection = declared(tmp_path)
    missing = selection.task_ids[:5]
    candidates = {
        "checkpoint-100": scores_with(selection, 50, ungraded=missing),  # first 5 were solved
        "checkpoint-120": scores_with(selection, 50),
    }
    chosen = select_checkpoint(selection, candidates)
    assert chosen.selected == "checkpoint-120"
    by_name = {s.checkpoint: s for s in chosen.scores}
    assert by_name["checkpoint-100"].ungraded == 5
    assert by_name["checkpoint-100"].solve_rate == pytest.approx(0.45)


def test_ties_go_to_the_earlier_checkpoint(tmp_path):
    selection = declared(tmp_path)
    candidates = {
        "checkpoint-160": scores_with(selection, 50),
        "checkpoint-140": scores_with(selection, 50),
    }
    assert select_checkpoint(selection, candidates).selected == "checkpoint-140"


def test_selection_needs_a_declared_set_and_candidates(tmp_path):
    with pytest.raises(FileNotFoundError, match="declare the selection set"):
        select_and_report(tmp_path, {}, {"checkpoint-1": {}})
    selection = declared(tmp_path)
    with pytest.raises(ValueError, match="no candidate"):
        select_checkpoint(selection, {})


def test_too_small_terminal_bench_is_rejected(tmp_path):
    with pytest.raises(ValueError, match="fewer than the 18-task subset"):
        declare_selection_set(tmp_path, SWE, TB[:10], seed=0)


def test_ids_are_qualified_by_benchmark():
    assert qualify_id(TERMINAL_BENCH, "x") == "terminal_bench_2_1/x"
