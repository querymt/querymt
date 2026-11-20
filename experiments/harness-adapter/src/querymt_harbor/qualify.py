"""The qualification gate (spec: Qualification gate before training).

Training does not start until one rollout of the querymt harness under the configured guard has
shown that what the trainer needs is really captured:

- the verifier produced a reward (otherwise nothing was graded);
- at least one turn was captured;
- all turns form one root, i.e. the harness history was never rewritten, which would fork
  the capture into separate rows;
- trainable tokens are above zero, i.e. the engine and proxy returned token ids and logprobs
  (this is what fails if safeagents hides them, design D6);
- the trace check is `match`, i.e. QueryMT's own trajectory and the token capture agree.

The result is recorded in `qualify.json` whether it passes or not, and `ensure_qualified`
is what the train stage calls before it spawns anything.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

QUALIFY_FILE = "qualify.json"

# Result fields the gate reads, recorded with the verdict.
RECORDED = (
    "task_name",
    "dataset",
    "harness",
    "sandbox",
    "reward",
    "rollout_type",
    "n_turns",
    "n_roots",
    "n_trainable_tokens",
    "atif",
    "ok",
    "error",
)


class QualificationFailed(RuntimeError):
    """Training must not start: the qualification rollout did not pass (or never ran)."""


@dataclass(frozen=True)
class Qualification:
    passed: bool
    failures: tuple[str, ...]
    """One human-readable line per failed check; empty when `passed`."""
    result: dict[str, Any]
    """The recorded fields of the rollout result."""

    def to_json(self) -> dict[str, Any]:
        return {"passed": self.passed, "failures": list(self.failures), "result": self.result}


def _field(result: Any, name: str, default: Any = None) -> Any:
    if isinstance(result, Mapping):
        return result.get(name, default)
    return getattr(result, name, default)


def check(result: Any) -> Qualification:
    """Apply the gate to one rollout result (a `HarborRolloutResult` or its dict)."""
    reward = _field(result, "reward")
    turns = int(_field(result, "n_turns", 0) or 0)
    roots = int(_field(result, "n_roots", 0) or 0)
    tokens = int(_field(result, "n_trainable_tokens", 0) or 0)
    atif = _field(result, "atif", "none")

    failures: list[str] = []
    if reward is None:
        failures.append(
            "the verifier produced no reward: nothing was graded"
            + (f" ({_field(result, 'error')})" if _field(result, "error") else "")
        )
    if turns < 1:
        failures.append("no turns were captured: the agent never reached the model endpoint")
    elif roots != 1:
        failures.append(
            f"the capture has {roots} roots, not 1: the harness history was rewritten "
            "(a rewritten prefix forks the capture into separate training rows)"
        )
    if turns >= 1 and tokens <= 0:
        failures.append(
            "no trainable tokens: the engine or proxy did not return token ids and logprobs "
            "(behind safeagents, rerun with guard.enabled=false, design D6)"
        )
    if atif != "match":
        failures.append(
            "the trajectory and the token capture disagree (atif=MISMATCH)"
            if atif == "MISMATCH"
            else f"the trajectory is missing or unchecked (atif={atif}): "
            "there is nothing to cross-check the token capture against"
        )

    recorded = {name: _field(result, name) for name in RECORDED}
    return Qualification(not failures, tuple(failures), recorded)


def record(run_dir: Path, qualification: Qualification) -> Path:
    """Write `qualify.json` in the run directory (pass or fail) and return its path."""
    run_dir.mkdir(parents=True, exist_ok=True)
    path = run_dir / QUALIFY_FILE
    path.write_text(json.dumps(qualification.to_json(), indent=2) + "\n", "utf-8")
    return path


def qualify(run_dir: Path, result: Any) -> Qualification:
    """Check `result`, record the verdict and raise `QualificationFailed` unless it passed."""
    qualification = check(result)
    record(run_dir, qualification)
    if not qualification.passed:
        raise QualificationFailed("qualification failed: " + "; ".join(qualification.failures))
    return qualification


def ensure_qualified(run_dir: Path) -> Qualification:
    """The train stage's guard: a passing `qualify.json` must exist in `run_dir`."""
    path = run_dir / QUALIFY_FILE
    if not path.is_file():
        raise QualificationFailed(
            f"no {QUALIFY_FILE} in {run_dir}: run the qualify stage before training"
        )
    data = json.loads(path.read_text("utf-8"))
    qualification = Qualification(
        bool(data["passed"]), tuple(data.get("failures", ())), dict(data.get("result", {}))
    )
    if not qualification.passed:
        raise QualificationFailed(
            "qualification did not pass, training is blocked: " + "; ".join(qualification.failures)
        )
    return qualification
