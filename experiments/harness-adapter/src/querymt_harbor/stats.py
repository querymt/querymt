"""Paired evaluation statistics (design D8).

Scores are per-task numbers in [0, 1] (the verifier's correctness); `None` means the task was
not graded. Only tasks graded on both sides enter the comparison; the rest are listed, not
silently dropped.
"""

from __future__ import annotations

import math
from collections.abc import Mapping
from dataclasses import dataclass

Z_95 = 1.959963984540054
"""Two-sided 95% normal quantile."""

SOLVED_AT = 1.0
"""A task counts as solved at this score (the reward bonus uses the same threshold)."""


@dataclass(frozen=True)
class PairedReport:
    """Base versus adapter on the tasks graded for both."""

    n: int
    """Tasks in the pairing."""
    mean_diff_points: float
    """Adapter minus base, in percentage points."""
    ci_half_width_points: float
    """Half-width of the 95% normal interval on the paired per-task differences."""
    newly_solved: int
    newly_lost: int
    sign_test_p: float
    """Exact two-sided sign test on the discordant pairs."""
    excluded: tuple[str, ...]
    """Tasks that were ungraded (or missing) on at least one side, sorted."""


def sign_test_p(wins: int, losses: int) -> float:
    """Exact two-sided binomial sign test on `wins` and `losses` (ties are not counted)."""
    n = wins + losses
    if n == 0:
        return 1.0
    tail = sum(math.comb(n, k) for k in range(min(wins, losses) + 1))
    return min(1.0, 2 * tail / 2**n)


def paired_report(
    base: Mapping[str, float | None],
    adapter: Mapping[str, float | None],
    *,
    solved_at: float = SOLVED_AT,
) -> PairedReport:
    """Compare `adapter` with `base` task by task.

    The interval is a normal interval on the mean of the paired differences with the variance
    taken with divisor n (not n - 1), as the reference recipe reports it.
    """
    tasks = sorted(set(base) | set(adapter))
    pairs: list[tuple[str, float, float]] = []
    excluded: list[str] = []
    for task in tasks:
        b, a = base.get(task), adapter.get(task)
        if b is None or a is None:
            excluded.append(task)
        else:
            pairs.append((task, float(b), float(a)))

    n = len(pairs)
    if n == 0:
        return PairedReport(0, 0.0, 0.0, 0, 0, 1.0, tuple(excluded))

    diffs = [a - b for _, b, a in pairs]
    mean = sum(diffs) / n
    variance = sum((d - mean) ** 2 for d in diffs) / n
    half_width = Z_95 * math.sqrt(variance / n)
    newly_solved = sum(1 for _, b, a in pairs if a >= solved_at > b)
    newly_lost = sum(1 for _, b, a in pairs if b >= solved_at > a)
    return PairedReport(
        n=n,
        mean_diff_points=100 * mean,
        ci_half_width_points=100 * half_width,
        newly_solved=newly_solved,
        newly_lost=newly_lost,
        sign_test_p=sign_test_p(newly_solved, newly_lost),
        excluded=tuple(excluded),
    )
