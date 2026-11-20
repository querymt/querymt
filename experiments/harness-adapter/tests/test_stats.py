"""Paired evaluation statistics (harness-adapter-training / Paired evaluation)."""

from __future__ import annotations

import pytest

from querymt_harbor.stats import paired_report, sign_test_p


def _tasks(n: int) -> list[str]:
    return [f"task-{i:02d}" for i in range(n)]


def test_paired_report_statistics():
    """51 tasks, 15 newly solved and 2 lost: +25.5 points, +-14.2, 15 / 2, p = 0.0023."""
    tasks = _tasks(51)
    base = dict.fromkeys(tasks, 0.0)
    adapter = dict.fromkeys(tasks, 0.0)
    for t in tasks[:15]:
        adapter[t] = 1.0
    for t in tasks[15:17]:
        base[t] = 1.0

    report = paired_report(base, adapter)

    assert report.n == 51
    assert report.mean_diff_points == pytest.approx(25.5, abs=0.05)
    assert report.ci_half_width_points == pytest.approx(14.2, abs=0.05)
    assert (report.newly_solved, report.newly_lost) == (15, 2)
    assert round(report.sign_test_p, 4) == 0.0023
    assert report.excluded == ()


def test_ungraded_tasks_are_excluded_from_pairs():
    tasks = _tasks(10)
    base: dict[str, float | None] = dict.fromkeys(tasks, 0.0)
    adapter: dict[str, float | None] = dict.fromkeys(tasks, 1.0)
    for t in tasks[:3]:
        base[t] = None

    report = paired_report(base, adapter)

    assert report.n == 7
    assert report.excluded == tuple(tasks[:3])
    assert report.newly_solved == 7
    assert report.mean_diff_points == pytest.approx(100.0)


def test_a_task_missing_on_one_side_is_excluded():
    report = paired_report({"a": 0.0, "b": 1.0}, {"a": 1.0})
    assert report.n == 1
    assert report.excluded == ("b",)


def test_no_difference_is_not_significant():
    tasks = _tasks(4)
    report = paired_report(dict.fromkeys(tasks, 1.0), dict.fromkeys(tasks, 1.0))
    assert (report.mean_diff_points, report.ci_half_width_points) == (0.0, 0.0)
    assert (report.newly_solved, report.newly_lost, report.sign_test_p) == (0, 0, 1.0)


def test_empty_pairing_is_defined():
    report = paired_report({"a": None}, {"a": 1.0})
    assert report.n == 0
    assert report.sign_test_p == 1.0
    assert report.excluded == ("a",)


def test_sign_test_is_exact_and_symmetric():
    # 17 discordant pairs, 2 on the losing side: 2 * (1 + 17 + 136) / 2**17
    assert sign_test_p(15, 2) == pytest.approx(308 / 131072)
    assert sign_test_p(2, 15) == sign_test_p(15, 2)
    assert sign_test_p(5, 5) == 1.0
