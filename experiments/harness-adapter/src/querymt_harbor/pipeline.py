"""The end-to-end pipeline command (spec: End-to-end pipeline command).

    python -m querymt_harbor.pipeline run [--stage NAME] [--run-dir DIR] [--config FILE]

Stages, in order: `preflight`, `qualify`, `baseline`, `train`, `evaluate`, `select`, `publish`.
A stage that finishes writes `stages/<name>.done.json` in the run directory with its outputs; a
rerun skips every stage that has a marker and resumes at the first without one. A failed stage
writes no marker. `--stage NAME` runs one stage alone (skipping it if it is done, unless
`--force`).

The stages themselves are plain functions over a `Services` object, the part that starts vLLM,
the Harbor server, the guard and the trainer and talks to them (`launcher.LocalServices`). So
the order, the markers, the preflight checks and the stage logic are tested on CPU with a fake
`Services`, and nothing here starts a process.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from .config import Reference, harness_digests, load_reference
from .evaluate import (
    EVAL_DIR,
    EvalTask,
    Runner,
    checkpoint_name,
    evaluate_checkpoints,
    evaluate_model,
    load_scores,
)
from .qualify import QUALIFY_FILE, ensure_qualified, qualify
from .select import (
    SWEBENCH,
    TERMINAL_BENCH,
    declare_selection_set,
    select_and_report,
)
from .train import CHECKPOINT_DIR, deep_merge, overrides_from, resolve

STAGES = ("preflight", "qualify", "baseline", "train", "evaluate", "select", "publish")
STAGE_DIR = "stages"

# Environment variables the sandbox provider needs, by `QMT_SANDBOX` (default `e2b`).
SANDBOX_CREDENTIALS: Mapping[str, tuple[str, ...]] = {
    "e2b": ("E2B_API_KEY",),
    "modal": ("MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"),
    "daytona": ("DAYTONA_API_KEY",),
    "docker": (),
    # Hugging Face sandboxes (HF Jobs) run on the HF token, which preflight always requires.
    "hf-sandbox": (),
}


class PipelineError(RuntimeError):
    """A stage cannot run or finish; the message says what to do."""


class PreflightFailed(PipelineError):
    """One or more prerequisites are missing. Raised before any service is started."""

    def __init__(self, problems: Sequence[str]) -> None:
        self.problems = tuple(problems)
        super().__init__("preflight failed:\n" + "\n".join(f"  - {p}" for p in self.problems))


# --- stage markers ---------------------------------------------------------------------------


def marker_path(run_dir: Path, stage: str) -> Path:
    return run_dir / STAGE_DIR / f"{stage}.done.json"


def is_done(run_dir: Path, stage: str) -> bool:
    return marker_path(run_dir, stage).is_file()


def completed_stages(run_dir: Path) -> list[str]:
    return [stage for stage in STAGES if is_done(run_dir, stage)]


def write_marker(run_dir: Path, stage: str, outputs: Mapping[str, Any]) -> Path:
    path = marker_path(run_dir, stage)
    path.parent.mkdir(parents=True, exist_ok=True)
    record = {"stage": stage, "finished_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    record["outputs"] = dict(outputs)  # type: ignore[assignment]
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", "utf-8")
    tmp.replace(path)  # a marker is either complete or absent
    return path


def read_marker(run_dir: Path, stage: str) -> dict[str, Any]:
    return json.loads(marker_path(run_dir, stage).read_text("utf-8"))


# --- preflight -------------------------------------------------------------------------------


def list_gpus() -> list[str]:
    """Names of the visible GPUs from `nvidia-smi --list-gpus`; empty if there is none."""
    exe = shutil.which("nvidia-smi")
    if exe is None:
        return []
    proc = subprocess.run([exe, "--list-gpus"], capture_output=True, text=True, check=False)
    return [line.strip() for line in proc.stdout.splitlines() if line.startswith("GPU ")]


def hub_dataset_access(token: str | None) -> Callable[[str, str], None]:
    """Check that a dataset revision is readable on the Hub with `token`."""

    def check(repo: str, revision: str) -> None:
        from huggingface_hub import HfApi

        HfApi(token=token).dataset_info(repo, revision=revision or None)

    return check


def verify_headless(reference: Reference) -> dict[str, str]:
    """Fetch (or find in the cache) and hash every pinned `qmt-headless` archive."""
    from .agent import default_cache_dir
    from .release import fetch_archive

    pin = reference.headless
    if not pin.platforms:
        from .release import ReleaseError

        raise ReleaseError("no qmt-headless archive is pinned (task 3.7 records the hashes)")
    for platform in pin.platforms:
        fetch_archive(pin, platform, default_cache_dir())
    return {name: asset.sha256 for name, asset in sorted(pin.platforms.items())}


def verify_plugin(reference: Reference) -> str:
    """Fetch (or find in the cache) and hash the pinned provider plugin."""
    from .agent import default_cache_dir
    from .plugin import fetch_plugin

    fetch_plugin(reference.plugin, default_cache_dir())
    return reference.plugin.blob_sha256


def free_disk_gb(path: Path) -> float:
    probe = path
    while not probe.exists() and probe != probe.parent:
        probe = probe.parent
    return shutil.disk_usage(probe).free / 1e9


@dataclass
class Probes:
    """What preflight looks at. The defaults touch the machine; tests replace them."""

    gpus: Callable[[], list[str]] = list_gpus
    dataset_access: Callable[[str, str], None] | None = None
    """`(repo, revision)` raising if unreadable; defaults to the Hub with `$HF_TOKEN`."""
    headless: Callable[[Reference], dict[str, str]] = verify_headless
    plugin: Callable[[Reference], str] = verify_plugin
    free_gb: Callable[[Path], float] = free_disk_gb


def sandbox_provider(env: Mapping[str, str]) -> str:
    return env.get("QMT_SANDBOX", "e2b")


def datasets(raw: Mapping[str, Any]) -> list[tuple[str, str]]:
    """Every dataset repository and revision the experiment reads."""
    data = raw["data"]
    out = [(s["repo"], s["revision"]) for k, s in data.items() if k != "benchmarks"]
    out += [(b["repo"], b["revision"]) for b in data["benchmarks"]]
    return out


def preflight(
    raw: Mapping[str, Any],
    reference: Reference,
    run_dir: Path,
    env: Mapping[str, str],
    probes: Probes | None = None,
) -> dict[str, Any]:
    """Check everything a run needs and return the marker outputs; raise with every problem.

    Starts nothing but the probes themselves. All problems are collected, so one run of
    preflight lists everything that has to be fixed.
    """
    probes = probes or Probes()
    cfg = raw["preflight"]
    problems: list[str] = []

    gpus = probes.gpus()
    if len(gpus) < cfg["min_gpus"]:
        problems.append(
            f"{len(gpus)} GPU{'s' if len(gpus) != 1 else ''} visible, "
            f"at least {cfg['min_gpus']} are required (vLLM on one, the trainer on the other)"
        )

    provider = sandbox_provider(env)
    credentials = list(SANDBOX_CREDENTIALS.get(provider, ()))
    credentials.append("HF_TOKEN")
    present = {name: bool(env.get(name)) for name in credentials}
    for name, ok in present.items():
        if not ok:
            why = f" (sandbox provider {provider})" if name != "HF_TOKEN" else ""
            problems.append(f"{name} is not set{why}")

    access = probes.dataset_access or hub_dataset_access(env.get("HF_TOKEN") or None)
    for repo, revision in datasets(raw):
        try:
            access(repo, revision)
        except Exception as exc:  # noqa: BLE001 - any failure means no access
            problems.append(f"dataset {repo}@{revision} is not readable: {exc}")

    headless: dict[str, str] = {}
    try:
        headless = probes.headless(reference)
    except Exception as exc:  # noqa: BLE001
        problems.append(f"qmt-headless archive: {exc}")
    plugin = ""
    try:
        plugin = probes.plugin(reference)
    except Exception as exc:  # noqa: BLE001
        problems.append(f"provider plugin pin: {exc}")

    free = probes.free_gb(run_dir)
    if free < cfg["min_free_disk_gb"]:
        problems.append(
            f"{free:.0f} GB free at {run_dir}, at least {cfg['min_free_disk_gb']} GB are required"
        )

    if problems:
        raise PreflightFailed(problems)
    return {
        "gpus": gpus,
        "sandbox_provider": provider,
        "credentials_present": sorted(present),
        "free_disk_gb": round(free),
        "revisions": {
            "datasets": {repo: revision for repo, revision in datasets(raw)},
            "plugin_blob_sha256": plugin,
            "headless": {"version": reference.headless.version, "sha256": headless},
            "harness": harness_digests(reference),
        },
    }


# --- services and the stages ------------------------------------------------------------------


class Services(Protocol):
    """What the stages need from the running system (`launcher.LocalServices`)."""

    def qualification_rollout(self, run_dir: Path) -> Any:
        """One rollout under the configured guard; returns the `HarborRolloutResult`."""

    def eval_tasks(self) -> Mapping[str, Sequence[EvalTask]]:
        """The tasks to evaluate on, by benchmark (`select.SWEBENCH`, `select.TERMINAL_BENCH`)."""

    def eval_runner(self) -> Runner:
        """The rollout runner for evaluation."""

    def serve_eval(self, adapters: Mapping[str, Path]) -> AbstractContextManager[Any]:
        """Serve the base model plus `adapters` (name -> directory) while the context is open."""

    def train(self, config: dict[str, Any]) -> None:
        """Run the trainer, with all its services, until it finishes or fails."""


Uploader = Callable[[Path, str], Any]


@dataclass
class Context:
    raw: dict[str, Any]
    reference: Reference
    run_dir: Path
    env: Mapping[str, str] = field(default_factory=lambda: dict(os.environ))
    services: Services | None = None
    probes: Probes | None = None
    config_overrides: Mapping[str, Any] = field(default_factory=dict)
    config_path: Path | None = None
    repo_id: str = ""
    uploader: Uploader | None = None
    concurrency: int | None = None
    """Evaluation rollouts in flight; `None` is `eval.concurrency`."""

    def need_services(self) -> Services:
        if self.services is None:
            raise PipelineError("this stage needs the launcher's services; none were provided")
        return self.services


def _all_eval_tasks(ctx: Context) -> list[EvalTask]:
    by_benchmark = ctx.need_services().eval_tasks()
    return [t for benchmark in (SWEBENCH, TERMINAL_BENCH) for t in by_benchmark.get(benchmark, ())]


def stage_preflight(ctx: Context) -> dict[str, Any]:
    return preflight(ctx.raw, ctx.reference, ctx.run_dir, ctx.env, ctx.probes)


def stage_qualify(ctx: Context) -> dict[str, Any]:
    result = ctx.need_services().qualification_rollout(ctx.run_dir)
    verdict = qualify(ctx.run_dir, result)  # records qualify.json; raises if it did not pass
    return {"file": QUALIFY_FILE, "passed": verdict.passed, "result": verdict.result}


def stage_baseline(ctx: Context) -> dict[str, Any]:
    """The base model under the profile on every task, after the selection set is declared."""
    services = ctx.need_services()
    tasks = _all_eval_tasks(ctx)
    by_benchmark = services.eval_tasks()
    declare_selection_set(
        ctx.run_dir,
        [t.task_id for t in by_benchmark.get(SWEBENCH, ())],
        [t.task_id for t in by_benchmark.get(TERMINAL_BENCH, ())],
        seed=ctx.raw["run"]["seed"],
        subset_size=ctx.raw["selection"]["terminal_bench_subset"],
    )
    with services.serve_eval({}):
        report = evaluate_model(
            services.eval_runner(),
            tasks,
            name="base",
            model=ctx.raw["model"]["base"],
            raw=ctx.raw,
            run_dir=ctx.run_dir,
            concurrency=ctx.concurrency,
        )
    return {
        "file": f"{EVAL_DIR}/base.json",
        "tasks": len(tasks),
        "ungraded": sum(1 for s in report["scores"].values() if s is None),
    }


def stage_train(ctx: Context) -> dict[str, Any]:
    ensure_qualified(ctx.run_dir)  # fail before any service starts
    config = resolve(ctx.reference, ctx.run_dir, ctx.config_overrides)
    ctx.need_services().train(config)
    done = sorted(_checkpoint_steps(ctx.run_dir))
    if not done:
        raise PipelineError(
            f"training finished but {ctx.run_dir / CHECKPOINT_DIR} has no checkpoint"
        )
    return {"checkpoints": [checkpoint_name(s) for s in done], "last_step": done[-1]}


def _checkpoint_steps(run_dir: Path) -> list[int]:
    from .evaluate import completed_checkpoints

    return list(completed_checkpoints(run_dir / CHECKPOINT_DIR))


def stage_evaluate(ctx: Context) -> dict[str, Any]:
    services = ctx.need_services()
    reports = evaluate_checkpoints(
        services.eval_runner(),
        _all_eval_tasks(ctx),
        raw=ctx.raw,
        run_dir=ctx.run_dir,
        checkpoints_dir=ctx.run_dir / CHECKPOINT_DIR,
        serve=services.serve_eval,
        concurrency=ctx.concurrency,
    )
    existing = sorted(p.stem for p in (ctx.run_dir / EVAL_DIR).glob("checkpoint-*.json"))
    return {"evaluated_now": sorted(reports), "reports": existing}


def stage_select(ctx: Context) -> dict[str, Any]:
    eval_dir = ctx.run_dir / EVAL_DIR
    base_file = eval_dir / "base.json"
    if not base_file.is_file():
        raise PipelineError(f"no {base_file}: run the baseline stage first")
    candidates = {p.stem: load_scores(p) for p in sorted(eval_dir.glob("checkpoint-*.json"))}
    if not candidates:
        raise PipelineError(
            f"no checkpoint evaluations in {eval_dir}: run the evaluate stage first"
        )
    chosen, _report = select_and_report(ctx.run_dir, load_scores(base_file), candidates)
    return {"selected": chosen.selected, "files": ["selection.json", "eval_report.json"]}


def stage_publish(ctx: Context) -> dict[str, Any]:
    from .publish import build_bundle, hub_uploader, publish

    repo_id = ctx.repo_id or ctx.raw.get("publish", {}).get("repo_id", "")
    if not repo_id:
        raise PipelineError(
            "no Hub repository: set publish.repo_id in the config or pass --repo-id"
        )
    selection_file = ctx.run_dir / "selection.json"
    if not selection_file.is_file():
        raise PipelineError(f"no {selection_file}: run the select stage first")
    selected = json.loads(selection_file.read_text("utf-8"))["selected"]
    bundle = build_bundle(
        ctx.run_dir / "bundle",
        run_dir=ctx.run_dir,
        checkpoint=ctx.run_dir / CHECKPOINT_DIR / selected,
        reference=ctx.reference,
        repo_id=repo_id,
    )
    uploader = ctx.uploader or hub_uploader(ctx.env.get("HF_TOKEN") or None)
    publish(bundle, repo_id, uploader)
    return {"repo_id": repo_id, "checkpoint": selected, "bundle": str(bundle)}


StageFn = Callable[[Context], Mapping[str, Any]]

DEFAULT_STAGES: Mapping[str, StageFn] = {
    "preflight": stage_preflight,
    "qualify": stage_qualify,
    "baseline": stage_baseline,
    "train": stage_train,
    "evaluate": stage_evaluate,
    "select": stage_select,
    "publish": stage_publish,
}


@dataclass(frozen=True)
class StageRun:
    stage: str
    skipped: bool
    outputs: Mapping[str, Any]


def run(
    ctx: Context,
    *,
    only: str | None = None,
    force: bool = False,
    stages: Mapping[str, StageFn] = DEFAULT_STAGES,
    log: Callable[[str], Any] = print,
) -> list[StageRun]:
    """Run the pipeline (or `only` one stage), skipping stages that are marked done.

    Stops at the first failing stage; the stages before it keep their markers, so rerunning
    resumes there. `force` reruns the selected stage even if it is marked done.
    """
    if only is not None and only not in STAGES:
        raise PipelineError(f"unknown stage {only!r}; stages are {', '.join(STAGES)}")
    selected = [only] if only else list(STAGES)
    ran: list[StageRun] = []
    for stage in selected:
        if is_done(ctx.run_dir, stage) and not (force and only):
            log(f"[{stage}] done, skipping")
            ran.append(StageRun(stage, True, read_marker(ctx.run_dir, stage)["outputs"]))
            continue
        log(f"[{stage}] running")
        outputs = stages[stage](ctx)
        write_marker(ctx.run_dir, stage, outputs)
        log(f"[{stage}] done")
        ran.append(StageRun(stage, False, outputs))
    return ran


# --- command line ----------------------------------------------------------------------------


def main(
    argv: Sequence[str] | None = None,
    *,
    services_factory: Callable[[Context], Services] | None = None,
) -> int:
    parser = argparse.ArgumentParser(
        prog="querymt_harbor.pipeline", description=__doc__.splitlines()[0]
    )
    sub = parser.add_subparsers(dest="command", required=True)
    run_parser = sub.add_parser("run", help="run all stages, or one with --stage")
    run_parser.add_argument("--config", default=None, help="reference.toml")
    run_parser.add_argument("--run-dir", default=None)
    run_parser.add_argument("--stage", choices=STAGES, default=None)
    run_parser.add_argument("--force", action="store_true", help="rerun --stage even if done")
    run_parser.add_argument("--repo-id", default="", help="Hub repository for the publish stage")
    run_parser.add_argument("--override", action="append", default=[], metavar="SECTION.KEY=JSON")
    status_parser = sub.add_parser("status", help="list the completed stages")
    status_parser.add_argument("--config", default=None)
    status_parser.add_argument("--run-dir", default=None)
    args = parser.parse_args(argv)

    reference = load_reference(args.config)
    overrides = overrides_from(getattr(args, "override", []))
    raw = resolve(reference, args.run_dir, overrides)
    run_dir = Path(raw["run"]["dir"])

    if args.command == "status":
        done = completed_stages(run_dir)
        for stage in STAGES:
            print(f"{stage:10} {'done' if stage in done else 'pending'}")
        return 0

    # `resolve` is the training view; the stages read the reference tables with the overrides.
    ctx = Context(
        raw=deep_merge(reference.raw, overrides),
        reference=reference,
        run_dir=run_dir,
        repo_id=args.repo_id,
        config_overrides=overrides,
        config_path=Path(args.config) if args.config else None,
    )
    if args.stage not in ("preflight", "select", "publish"):
        if services_factory is None:
            from .launcher import LocalServices

            services_factory = LocalServices.from_context
        # Constructing the services starts nothing: preflight must be able to stop a run first.
        ctx.services = services_factory(ctx)
    try:
        run(ctx, only=args.stage, force=args.force)
    except PipelineError as exc:
        print(exc, file=sys.stderr)
        return 1
    except Exception as exc:  # noqa: BLE001 - the CLI reports a failed stage, not a traceback
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
