"""Evaluation of the base model and checkpoints (design D8; spec: Paired evaluation,
Infrastructure failures are rerun).

Each candidate is served by one multi-LoRA vLLM (`commands.vllm_command(..., trainer=False)`)
and every task is run once through the `querymt` seam with the evaluation policy: 100 agent
steps, the per-benchmark time limit, temperature 0.8, top-p 1.0, thinking off.

An attempt that failed because of the infrastructure, not the model, says nothing about the
model and is not scored: the model endpoint was unreachable from the sandbox (zero captured
turns plus a connection error), the sandbox did not start, or the agent did not install. It is
rerun up to `eval.infra_reruns` times; only the last attempt is reported, and if that one is
still an infrastructure failure the task is *ungraded* (`None`), never zero. Every rerun is
logged in `eval/infra_reruns.jsonl`.

The rollout itself is a `Runner`, a callable taking a `RolloutRequest`. The real one
(`server_runner`) talks to the Harbor env server and needs the full GPU setup (task 7.3); the
tests drive the classification, rerun and reporting logic with a fake runner.
"""

from __future__ import annotations

import json
import re
from collections import Counter
from collections.abc import Callable, Iterable, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .select import qualify_id

EVAL_DIR = "eval"
INFRA_LOG_FILE = "infra_reruns.jsonl"
SHELL_TOOL = "shell"

CONNECTION = "endpoint_unreachable"
SANDBOX = "sandbox_start_failed"
INSTALL = "agent_install_failed"
CAPACITY = "server_at_capacity"

_CONNECTION = re.compile(
    r"connect(ion)?\s*(error|refused|reset|timed?\s*out|failed)|connecterror|"
    r"cannot connect|could not connect|name or service not known|"
    r"temporary failure in name resolution|could not resolve|econnrefused|"
    r"network is unreachable|tunnel|bad gateway|remote protocol error",
    re.IGNORECASE,
)
_CAPACITY = re.compile(
    r"server at capacity|capacity[_ ]reached|sessioncapacityerror|sessions? active",
    re.IGNORECASE,
)
_SANDBOX = re.compile(
    r"(failed|unable|error)[^.\n]{0,40}(start|create|build|provision)[^.\n]{0,40}"
    r"(sandbox|container|environment)|"
    r"sandbox[^.\n]{0,40}(failed|did not start|not start|could not|timed?\s*out|error)|"
    r"environment (start|build) (failed|error)",
    re.IGNORECASE,
)
_INSTALL = re.compile(
    r"(failed|error)[^.\n]{0,30}install|install(ation)?[^.\n]{0,30}(failed|error)",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class EvalTask:
    benchmark: str
    task_id: str
    split: str
    """The dataset spec the Harbor server serves this task from."""
    index: int
    """Index of the task in that split."""

    @property
    def key(self) -> str:
        """The benchmark-qualified id the scores, the selection set and the reports use."""
        return qualify_id(self.benchmark, self.task_id)


@dataclass(frozen=True)
class RolloutRequest:
    task: EvalTask
    model: str
    """The served model name: the base model or one LoRA module."""
    step_limit: int
    timeout_sec: float
    sampling: Mapping[str, float]
    attempt: int
    """1 for the first attempt."""


Runner = Callable[[RolloutRequest], Any]


def _field(result: Any, name: str, default: Any = None) -> Any:
    if isinstance(result, Mapping):
        return result.get(name, default)
    return getattr(result, name, default)


def classify_infrastructure(result: Any) -> str | None:
    """Why `result` is an infrastructure failure, or `None` if it tells us about the model.

    A rollout the model really ran (turns captured) is never one, whatever its log says about
    connections: the model's own commands fail to connect all the time.
    """
    if _field(result, "ok", True) and not _field(result, "error"):
        return None
    turns = int(_field(result, "n_turns", 0) or 0)
    text = " ".join(
        str(_field(result, name) or "") for name in ("error", "exception_type", "agent_log_tail")
    )
    if turns == 0 and _CAPACITY.search(text):
        return CAPACITY
    if turns == 0 and _CONNECTION.search(text):
        return CONNECTION
    if turns == 0 and _SANDBOX.search(text):
        return SANDBOX
    if turns == 0 and _INSTALL.search(text):
        return INSTALL
    return None


def tool_counts(result: Any) -> Counter[str]:
    """Tool calls by name, from the captured turns (or the conversations as a fallback)."""
    counts: Counter[str] = Counter()
    for turn in _field(result, "turns", ()) or ():
        for call in _field(turn, "tool_calls", ()) or ():
            if name := _field(call, "name"):
                counts[str(name)] += 1
    if counts:
        return counts
    for conversation in _field(result, "conversations", ()) or ():
        for message in _field(conversation, "messages", ()) or ():
            if _field(message, "role") != "assistant":
                continue
            for call in _field(message, "tool_calls", ()) or ():
                function = _field(call, "function") or {}
                if name := _field(function, "name") or _field(call, "name"):
                    counts[str(name)] += 1
    return counts


def shell_share(counts: Mapping[str, int]) -> float | None:
    """The share of `shell` among all tool calls; `None` when there were none."""
    total = sum(counts.values())
    return counts.get(SHELL_TOOL, 0) / total if total else None


@dataclass(frozen=True)
class Attempt:
    number: int
    infrastructure: str | None
    reward: float | None
    n_turns: int
    error: str | None

    def to_json(self) -> dict[str, Any]:
        return {
            "attempt": self.number,
            "infrastructure_failure": self.infrastructure,
            "reward": self.reward,
            "n_turns": self.n_turns,
            "error": self.error,
        }


@dataclass(frozen=True)
class TaskOutcome:
    task: EvalTask
    score: float | None
    """The verifier's score of the last attempt, `None` when ungraded."""
    attempts: tuple[Attempt, ...]
    tools: Mapping[str, int] = field(default_factory=dict)
    """Tool calls of the reported attempt only."""

    @property
    def infrastructure_failed(self) -> bool:
        return self.attempts[-1].infrastructure is not None


def _crash(exc: BaseException) -> dict[str, Any]:
    """A runner that raised is a failed attempt, not a failed evaluation."""
    return {
        "ok": False,
        "error": f"{type(exc).__name__}: {exc}",
        "exception_type": type(exc).__name__,
        "n_turns": 0,
        "reward": None,
    }


def evaluate_task(
    runner: Runner,
    task: EvalTask,
    *,
    model: str,
    raw: Mapping[str, Any],
) -> TaskOutcome:
    """Run `task` once, rerunning infrastructure failures up to `eval.infra_reruns` times."""
    cfg = raw["eval"]
    step_limit = int(cfg["agent_step_limit"])
    timeout = float(cfg["timeout_sec"][task.benchmark])
    sampling = {"temperature": cfg["temperature"], "top_p": cfg["top_p"]}
    attempts: list[Attempt] = []
    result: Any = None
    for number in range(1, int(cfg["infra_reruns"]) + 2):
        request = RolloutRequest(task, model, step_limit, timeout, sampling, number)
        try:
            result = runner(request)
        except Exception as exc:  # noqa: BLE001 - one task must not stop the evaluation
            result = _crash(exc)
        reason = classify_infrastructure(result)
        reward = _field(result, "reward")
        attempts.append(
            Attempt(
                number,
                reason,
                None if reward is None else float(reward),
                int(_field(result, "n_turns", 0) or 0),
                _field(result, "error"),
            )
        )
        if reason is None:
            break
    last = attempts[-1]
    score = None if last.infrastructure is not None else last.reward
    return TaskOutcome(task, score, tuple(attempts), dict(tool_counts(result)))


def _report(name: str, model: str, outcomes: Sequence[TaskOutcome]) -> dict[str, Any]:
    tools: Counter[str] = Counter()
    for outcome in outcomes:
        tools.update(outcome.tools)
    reruns = [
        {"task": o.task.key, **a.to_json()}
        for o in outcomes
        for a in o.attempts
        if a.infrastructure is not None
    ]
    return {
        "name": name,
        "model": model,
        "scores": {o.task.key: o.score for o in outcomes},
        "tasks": {
            o.task.key: {
                "score": o.score,
                "attempts": [a.to_json() for a in o.attempts],
                "ungraded_after_infrastructure_failure": o.infrastructure_failed,
                "tool_calls": dict(o.tools),
            }
            for o in outcomes
        },
        "tool_calls": dict(tools),
        "shell_share": shell_share(tools),
        "infrastructure_failures": reruns,
    }


def eval_path(run_dir: Path, name: str) -> Path:
    return run_dir / EVAL_DIR / f"{name}.json"


def eval_concurrency(raw: Mapping[str, Any], requested: int | None = None) -> int:
    """Rollouts in flight: `requested`, else `eval.concurrency`.

    The Harbor server refuses a rollout beyond `serving.max_concurrent_rollouts`, so a larger
    number is a configuration error, reported before any rollout starts.
    """
    value = int(raw["eval"]["concurrency"] if requested is None else requested)
    limit = int(raw["serving"]["max_concurrent_rollouts"])
    if value < 1:
        raise ValueError(f"evaluation concurrency must be at least 1, got {value}")
    if value > limit:
        raise ValueError(
            f"evaluation concurrency {value} exceeds serving.max_concurrent_rollouts {limit}: "
            "the Harbor server would refuse the excess rollouts"
        )
    return value


def evaluate_model(
    runner: Runner,
    tasks: Iterable[EvalTask],
    *,
    name: str,
    model: str,
    raw: Mapping[str, Any],
    run_dir: Path | None = None,
    concurrency: int | None = None,
) -> dict[str, Any]:
    """Evaluate one served model on `tasks`; write `eval/<name>.json` when `run_dir` is given."""
    task_list = list(tasks)
    with ThreadPoolExecutor(max_workers=eval_concurrency(raw, concurrency)) as pool:
        outcomes = list(
            pool.map(lambda t: evaluate_task(runner, t, model=model, raw=raw), task_list)
        )
    report = _report(name, model, outcomes)
    if run_dir is not None:
        path = eval_path(run_dir, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, indent=2) + "\n", "utf-8")
        failures = report["infrastructure_failures"]
        if failures:
            with (path.parent / INFRA_LOG_FILE).open("a", encoding="utf-8") as log:
                for entry in failures:
                    log.write(json.dumps({"model": name, **entry}) + "\n")
    return report


def load_scores(path: Path) -> dict[str, float | None]:
    """The qualified-id -> score mapping of an `eval/<name>.json`, as `select` consumes it."""
    return dict(json.loads(path.read_text("utf-8"))["scores"])


# --- checkpoints -------------------------------------------------------------------------


def checkpoint_name(step: int) -> str:
    return f"checkpoint-{step}"


def evaluation_steps(available: Iterable[int], *, first: int, every: int) -> list[int]:
    """The saved steps to evaluate: `first`, `first + every`, ... that exist."""
    return sorted(s for s in set(available) if s >= first and (s - first) % every == 0)


def batches(items: Sequence[Any], size: int) -> list[Sequence[Any]]:
    """Consecutive batches of at most `size` (vLLM serves at most `--max-loras` at once)."""
    if size < 1:
        raise ValueError("batch size must be at least 1")
    return [items[i : i + size] for i in range(0, len(items), size)]


def completed_checkpoints(checkpoints_dir: Path) -> dict[int, Path]:
    """Fully written `checkpoint-N` directories by step (see `train.latest_complete_checkpoint`)."""
    found: dict[int, Path] = {}
    if not checkpoints_dir.is_dir():
        return found
    for path in checkpoints_dir.iterdir():
        match = re.fullmatch(r"checkpoint-(\d+)", path.name)
        if (
            match
            and (path / "trainer_state.json").is_file()
            and (path / "adapter_model.safetensors").is_file()
        ):
            found[int(match.group(1))] = path
    return found


Serve = Callable[[Mapping[str, Path]], Any]
"""Start a vLLM for these adapters (name -> directory); returns a context manager."""


def evaluate_checkpoints(
    runner: Runner,
    tasks: Sequence[EvalTask],
    *,
    raw: Mapping[str, Any],
    run_dir: Path,
    checkpoints_dir: Path,
    serve: Serve,
    concurrency: int | None = None,
) -> dict[str, dict[str, Any]]:
    """Evaluate every due checkpoint that has no report yet, in batches of `--max-loras`.

    Reports that exist are kept (a restarted stage continues instead of repeating hours of
    rollouts). Returns the reports of the checkpoints evaluated now.
    """
    cfg = raw["eval"]
    eval_concurrency(raw, concurrency)  # fail before a vLLM is started
    available = completed_checkpoints(checkpoints_dir)
    due = [
        step
        for step in evaluation_steps(available, first=cfg["first_checkpoint"], every=cfg["every"])
        if not eval_path(run_dir, checkpoint_name(step)).is_file()
    ]
    reports: dict[str, dict[str, Any]] = {}
    for batch in batches(due, int(raw["serving"]["max_loras"])):
        adapters = {checkpoint_name(step): available[step] for step in batch}
        with serve(adapters):
            for step in batch:
                name = checkpoint_name(step)
                reports[name] = evaluate_model(
                    runner,
                    tasks,
                    name=name,
                    model=name,
                    raw=raw,
                    run_dir=run_dir,
                    concurrency=concurrency,
                )
    return reports


# --- the real runner ---------------------------------------------------------------------


def server_runner(
    server: str,
    *,
    llm_url: str,
    sandbox: str = "e2b",
    harness: str = "querymt",
    api_key: str = "",
) -> Runner:
    """Rollouts through a running `querymt_harbor.serve`: the eval tier of the capture proxy.

    Needs the Harbor env server, vLLM and a sandbox provider (verified by tasks 7.3 and 7.5).
    One client per call: the websocket is not shared between worker threads.
    """

    def run(request: RolloutRequest) -> Any:
        from openenv.harbor.client import HarborEnv

        with HarborEnv(base_url=server) as env:
            return env.run_rollout(
                split=request.task.split,
                task_index=request.task.index,
                harness=harness,
                sandbox=sandbox,
                llm_url=llm_url,
                model=request.model,
                api_key=api_key,
                purpose="eval",
                eval_sampling=dict(request.sampling),
                agent_step_limit=request.step_limit,
                agent_timeout_sec=request.timeout_sec,
            )

    return run
