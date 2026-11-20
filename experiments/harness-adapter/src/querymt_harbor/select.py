"""Checkpoint selection on a declared subset (design D8, spec: Checkpoint selection).

The released checkpoint is the one with the highest solve rate on a *selection set* that is
declared, and written to `selection_set.json`, before any evaluation runs: all of SWE-bench Pro
hard51 plus a fixed 18-task subset of Terminal-Bench 2.1 chosen by seed. The remaining
Terminal-Bench tasks are held out: they never influence the choice and are reported separately,
as a paired comparison with the base model.

Task ids are qualified with their benchmark (`terminal_bench_2_1/<id>`) so two benchmarks can
never collide in a score table.
"""

from __future__ import annotations

import json
import random
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .stats import SOLVED_AT, PairedReport, paired_report

SWEBENCH = "swebench_pro_hard51"
TERMINAL_BENCH = "terminal_bench_2_1"
SELECTION_SET_FILE = "selection_set.json"
SELECTION_FILE = "selection.json"
EVAL_REPORT_FILE = "eval_report.json"

Scores = Mapping[str, float | None]
"""Qualified task id -> verifier score, `None` for ungraded."""


class SelectionSetChanged(RuntimeError):
    """A selection set was already declared and this is a different one."""


def qualify_id(benchmark: str, task_id: str) -> str:
    return f"{benchmark}/{task_id}"


@dataclass(frozen=True)
class SelectionSet:
    seed: int
    swebench: tuple[str, ...]
    terminal_bench: tuple[str, ...]
    """The fixed subset of Terminal-Bench tasks that is part of the selection set."""
    held_out: tuple[str, ...]
    """The Terminal-Bench tasks outside the subset, reported but never used to choose."""

    @property
    def task_ids(self) -> tuple[str, ...]:
        return self.swebench + self.terminal_bench

    def to_json(self) -> dict[str, Any]:
        return {
            "seed": self.seed,
            SWEBENCH: list(self.swebench),
            TERMINAL_BENCH: list(self.terminal_bench),
            f"{TERMINAL_BENCH}_held_out": list(self.held_out),
        }

    @classmethod
    def from_json(cls, data: Mapping[str, Any]) -> SelectionSet:
        return cls(
            seed=int(data["seed"]),
            swebench=tuple(data[SWEBENCH]),
            terminal_bench=tuple(data[TERMINAL_BENCH]),
            held_out=tuple(data[f"{TERMINAL_BENCH}_held_out"]),
        )


def declare_selection_set(
    run_dir: Path,
    swebench_ids: Sequence[str],
    terminal_bench_ids: Sequence[str],
    *,
    seed: int,
    subset_size: int = 18,
) -> SelectionSet:
    """Fix the selection set and record it in `selection_set.json`.

    Call this before any evaluation. Calling it again with the same inputs returns the recorded
    set; with different inputs it raises, because a set that moves after results exist is how
    a checkpoint gets selected on the data it was tuned on.
    """
    if len(terminal_bench_ids) < subset_size:
        raise ValueError(
            f"Terminal-Bench has {len(terminal_bench_ids)} tasks, "
            f"fewer than the {subset_size}-task subset"
        )
    tb_sorted = sorted(terminal_bench_ids)
    chosen = set(random.Random(f"{seed}/{TERMINAL_BENCH}-selection").sample(tb_sorted, subset_size))
    selection = SelectionSet(
        seed=seed,
        swebench=tuple(qualify_id(SWEBENCH, t) for t in sorted(swebench_ids)),
        terminal_bench=tuple(qualify_id(TERMINAL_BENCH, t) for t in tb_sorted if t in chosen),
        held_out=tuple(qualify_id(TERMINAL_BENCH, t) for t in tb_sorted if t not in chosen),
    )
    path = run_dir / SELECTION_SET_FILE
    if path.is_file():
        recorded = SelectionSet.from_json(json.loads(path.read_text("utf-8")))
        if recorded != selection:
            raise SelectionSetChanged(
                f"{path} already declares a different selection set; it must not change once "
                "evaluation has started. Use a new run directory for a different set."
            )
        return recorded
    run_dir.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(selection.to_json(), indent=2) + "\n", "utf-8")
    return selection


def load_selection_set(run_dir: Path) -> SelectionSet:
    path = run_dir / SELECTION_SET_FILE
    if not path.is_file():
        raise FileNotFoundError(
            f"no {SELECTION_SET_FILE} in {run_dir}: declare the selection set before evaluating"
        )
    return SelectionSet.from_json(json.loads(path.read_text("utf-8")))


@dataclass(frozen=True)
class CheckpointScore:
    checkpoint: str
    step: int
    solve_rate: float
    solved: int
    ungraded: int
    """Selection tasks without a grade; they count as unsolved, for every checkpoint alike."""


def step_of(checkpoint: str) -> int:
    return int(checkpoint.rsplit("-", 1)[1])


def score_on(selection: SelectionSet, checkpoint: str, scores: Scores) -> CheckpointScore:
    """Solve rate on the declared set only: scores of any other task are ignored."""
    ids = selection.task_ids
    graded = [scores.get(t) for t in ids]
    solved = sum(1 for s in graded if s is not None and s >= SOLVED_AT)
    return CheckpointScore(
        checkpoint=checkpoint,
        step=step_of(checkpoint),
        solve_rate=solved / len(ids),
        solved=solved,
        ungraded=sum(1 for s in graded if s is None),
    )


@dataclass(frozen=True)
class Selection:
    selected: str
    scores: tuple[CheckpointScore, ...]
    selection_task_ids: tuple[str, ...]

    def to_json(self) -> dict[str, Any]:
        return {
            "selected": self.selected,
            "scores": {
                s.checkpoint: {
                    "solve_rate": s.solve_rate,
                    "solved": s.solved,
                    "ungraded": s.ungraded,
                }
                for s in self.scores
            },
            "selection_task_ids": list(self.selection_task_ids),
        }


def select_checkpoint(selection: SelectionSet, candidates: Mapping[str, Scores]) -> Selection:
    """The candidate with the highest solve rate on the declared set.

    Ties go to the earlier step: the same score with less training is the safer release.
    """
    if not candidates:
        raise ValueError("no candidate checkpoints to select from")
    scored = [score_on(selection, name, scores) for name, scores in candidates.items()]
    scored.sort(key=lambda s: s.step)
    best = max(scored, key=lambda s: (s.solve_rate, -s.step))
    return Selection(best.checkpoint, tuple(scored), selection.task_ids)


def held_out_report(selection: SelectionSet, base: Scores, adapter: Scores) -> PairedReport:
    """Paired Terminal-Bench 2.1 comparison over only the tasks outside the selection subset."""
    held = set(selection.held_out)
    return paired_report(
        {t: s for t, s in base.items() if t in held},
        {t: s for t, s in adapter.items() if t in held},
    )


def _report_json(report: PairedReport) -> dict[str, Any]:
    return {
        "n": report.n,
        "mean_diff_points": report.mean_diff_points,
        "ci95_half_width_points": report.ci_half_width_points,
        "newly_solved": report.newly_solved,
        "newly_lost": report.newly_lost,
        "sign_test_p": report.sign_test_p,
        "excluded": list(report.excluded),
    }


def select_and_report(
    run_dir: Path,
    base: Scores,
    candidates: Mapping[str, Scores],
) -> tuple[Selection, dict[str, Any]]:
    """The select stage: write `selection.json` and `eval_report.json`.

    The report has the base model's and every checkpoint's rate on the selection set, the
    selected checkpoint's paired comparison with the base on the selection set, and the
    held-out Terminal-Bench comparison.
    """
    selection_set = load_selection_set(run_dir)
    chosen = select_checkpoint(selection_set, candidates)
    adapter = candidates[chosen.selected]

    selection_ids = set(selection_set.task_ids)
    selection_report = paired_report(
        {t: s for t, s in base.items() if t in selection_ids},
        {t: s for t, s in adapter.items() if t in selection_ids},
    )
    report = {
        "selected": chosen.selected,
        "base_selection_solve_rate": score_on(selection_set, "base-0", base).solve_rate,
        "checkpoints": chosen.to_json()["scores"],
        "selection_set_vs_base": _report_json(selection_report),
        "held_out_terminal_bench_vs_base": {
            "task_ids": list(selection_set.held_out),
            **_report_json(held_out_report(selection_set, base, adapter)),
        },
    }
    (run_dir / SELECTION_FILE).write_text(json.dumps(chosen.to_json(), indent=2) + "\n", "utf-8")
    (run_dir / EVAL_REPORT_FILE).write_text(json.dumps(report, indent=2) + "\n", "utf-8")
    return chosen, report
