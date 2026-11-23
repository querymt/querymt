"""Start, watch and stop the processes of a run (design D7), and the pipeline's `Services`.

A training run is four processes: vLLM on GPU 0, the Harbor env server (with its capture proxy
and public tunnel), the safeagents guard when enabled, and the trainer on GPU 1. Evaluation is
vLLM plus the Harbor server. `commands.py` builds their argv; this module owns what happens to
them:

- every process runs in its own process group, with its output in `<run>/logs/<name>.log`;
- `wait_healthy` returns when a process serves, and fails with the log's last lines if the
  process died first or never came up;
- a health supervisor watches the Harbor server and its capture proxy while the trainer runs, and
  ends the run with a clear message when they stop answering, instead of letting every rollout
  fail one by one;
- `stop_all` terminates the groups in reverse start order (so the trainer goes before vLLM) and
  kills what is left after a grace period, so no run leaves a GPU process behind.

`LocalServices` is the `pipeline.Services` for a single machine, and `hf_jobs_command` builds the
`hf jobs uv run` command that runs the same pipeline on HF Jobs with a bucket at `/data`. The
real thing needs the GPU host (task 7); the logic here is tested with tiny stand-in processes.
"""

from __future__ import annotations

import contextlib
import os
import signal
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .commands import (
    Command,
    safeagents_command,
    server_command,
    trainer_command,
    vllm_command,
)
from .evaluate import EvalTask, Runner, server_runner
from .select import SWEBENCH, TERMINAL_BENCH

LOG_DIR = "logs"
VLLM_START_TIMEOUT_SEC = 1800.0
"""Loading a 27B model with its graph capture takes minutes."""
SERVER_START_TIMEOUT_SEC = 300.0
STOP_GRACE_SEC = 30.0

# The two benchmarks, keyed as the selection set and the reports key them. Their revisions are
# in `[data].benchmarks`.
BENCHMARK_REPOS = {
    SWEBENCH: "harness-hub/swebench-pro-v2-hard51-harbor",
    TERMINAL_BENCH: "harborframework/terminal-bench-2.1",
}


class LaunchError(RuntimeError):
    """A process failed to start, died, or stopped being healthy."""


# --- health checks ----------------------------------------------------------------------------


def http_ok(url: str, timeout: float = 5.0) -> bool:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:  # noqa: S310
            return 200 <= response.status < 300
    except (urllib.error.URLError, OSError, ValueError):
        return False


def tcp_open(port: int, host: str = "127.0.0.1", timeout: float = 2.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


# --- process groups ---------------------------------------------------------------------------


Spawn = Callable[[str, Command, Path], "subprocess.Popen[bytes]"]


def default_spawn(name: str, command: Command, log: Path) -> subprocess.Popen[bytes]:
    """Start `command` in its own process group (session), output to `log`."""
    log.parent.mkdir(parents=True, exist_ok=True)
    handle = log.open("ab")
    try:
        return subprocess.Popen(
            list(command.argv),
            env={**os.environ, **command.env},
            stdout=handle,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    finally:
        handle.close()  # the child holds its own descriptor


def _tail(path: Path, lines: int = 15) -> str:
    try:
        return "\n".join(path.read_text("utf-8", errors="replace").splitlines()[-lines:])
    except OSError:
        return "(no log)"


@dataclass
class Managed:
    name: str
    command: Command
    process: subprocess.Popen[bytes]
    log: Path


class ProcessGroup:
    """The processes of one stack. Use as a context manager: leaving it stops everything."""

    def __init__(self, log_dir: Path, spawn: Spawn = default_spawn) -> None:
        self.log_dir = log_dir
        self._spawn = spawn
        self.processes: list[Managed] = []

    def start(self, name: str, command: Command) -> Managed:
        log = self.log_dir / f"{name}.log"
        try:
            process = self._spawn(name, command, log)
        except OSError as exc:
            raise LaunchError(f"cannot start {name} ({command.argv[0]}): {exc}") from exc
        managed = Managed(name, command, process, log)
        self.processes.append(managed)
        return managed

    def wait_healthy(
        self,
        managed: Managed,
        check: Callable[[], bool],
        *,
        timeout: float,
        interval: float = 1.0,
        sleep: Callable[[float], Any] = time.sleep,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        deadline = clock() + timeout
        while True:
            code = managed.process.poll()
            if code is not None:
                raise LaunchError(
                    f"{managed.name} exited with code {code} before it was healthy; "
                    f"last lines of {managed.log}:\n{_tail(managed.log)}"
                )
            if check():
                return
            if clock() >= deadline:
                raise LaunchError(
                    f"{managed.name} was not healthy after {timeout:.0f} s; "
                    f"last lines of {managed.log}:\n{_tail(managed.log)}"
                )
            sleep(interval)

    def stop_all(self, grace: float = STOP_GRACE_SEC) -> None:
        """Terminate every process group, last started first; kill what ignores SIGTERM."""
        for managed in reversed(self.processes):
            _signal_group(managed.process, signal.SIGTERM)
        deadline = time.monotonic() + grace
        for managed in reversed(self.processes):
            remaining = max(0.0, deadline - time.monotonic())
            try:
                managed.process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                _signal_group(managed.process, signal.SIGKILL)
                managed.process.wait()
            else:
                # The leader is gone; anything it left behind in the group goes too.
                _signal_group(managed.process, signal.SIGKILL)
        self.processes.clear()

    def __enter__(self) -> ProcessGroup:
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop_all()


def _signal_group(process: subprocess.Popen[bytes], sig: signal.Signals) -> None:
    try:
        os.killpg(os.getpgid(process.pid), sig)
    except (ProcessLookupError, PermissionError):
        with contextlib.suppress(ProcessLookupError):
            process.send_signal(sig)


class HealthSupervisor:
    """Polls `check` in a thread; `failed` is set after `max_failures` failures in a row."""

    def __init__(
        self,
        check: Callable[[], bool],
        *,
        interval: float = 15.0,
        max_failures: int = 4,
        what: str = "the Harbor server",
    ) -> None:
        self._check = check
        self._interval = interval
        self._max_failures = max_failures
        self.what = what
        self.failed = threading.Event()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, name="health-supervisor", daemon=True)

    def _loop(self) -> None:
        failures = 0
        while not self._stop.wait(self._interval):
            failures = 0 if self._check() else failures + 1
            if failures >= self._max_failures:
                self.failed.set()
                return

    def __enter__(self) -> HealthSupervisor:
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def message(self) -> str:
        return (
            f"{self.what} stopped answering ({self._max_failures} health checks in a row "
            f"{self._interval:.0f} s apart failed): its public tunnel or capture proxy is "
            "down, so every further rollout would fail. Rerun the stage to resume."
        )


# --- the stacks -------------------------------------------------------------------------------


@dataclass(frozen=True)
class Member:
    """One process of a stack and how to tell it is up (`None`: it is the main job)."""

    name: str
    command: Command
    ready: Callable[[], bool] | None
    timeout: float = SERVER_START_TIMEOUT_SEC


def _urls(raw: Mapping[str, Any]) -> dict[str, str]:
    serving = raw["serving"]
    return {
        "vllm": f"http://127.0.0.1:{serving['vllm_port']}",
        "server": f"http://127.0.0.1:{serving['server_port']}",
        "guard": f"http://127.0.0.1:{raw['guard']['port']}",
    }


def _vllm_member(raw: Mapping[str, Any], command: Command) -> Member:
    url = _urls(raw)["vllm"]
    return Member("vllm", command, lambda: http_ok(f"{url}/health"), VLLM_START_TIMEOUT_SEC)


def _server_member(raw: Mapping[str, Any], datasets: Sequence[str]) -> Member:
    urls = _urls(raw)
    command = server_command(raw, llm_url=urls["vllm"], datasets=datasets)
    return Member("harbor-server", command, lambda: http_ok(f"{urls['server']}/health"))


def _guard_member(raw: Mapping[str, Any], run_dir: Path) -> Member:
    port = raw["guard"]["port"]
    return Member("safeagents", safeagents_command(raw, run_dir), lambda: tcp_open(port))


def training_stack(
    raw: Mapping[str, Any],
    run_dir: Path,
    *,
    split: Path | str,
    config_path: Path | str | None = None,
    overrides: Mapping[str, Any] | None = None,
    with_trainer: bool = True,
) -> list[Member]:
    """vLLM (trainer role, GPU 0), the Harbor server, the guard if enabled, and the trainer."""
    members = [
        _vllm_member(raw, vllm_command(raw, trainer=True)),
        _server_member(raw, [str(split)]),
    ]
    if raw["guard"]["enabled"]:
        members.append(_guard_member(raw, run_dir))
    if with_trainer:
        members.append(
            Member(
                "trainer",
                trainer_command(
                    raw, run_dir, split=str(split), config_path=config_path, overrides=overrides
                ),
                None,
            )
        )
    return members


def eval_stack(
    raw: Mapping[str, Any], adapters: Mapping[str, Path | str], datasets: Sequence[Path | str]
) -> list[Member]:
    """vLLM serving the base model and `adapters`, and the Harbor server over `datasets`.

    `datasets` are local directories (the benchmarks as downloaded at their pinned revisions),
    never Hub ids: the server downloads a Hub id itself, without a revision, and its task
    numbering could then differ from the one the scores are filed under.

    Evaluation runs without the guard: the published adapter is served without it.
    """
    return [
        _vllm_member(raw, vllm_command(raw, trainer=False, adapters=adapters)),
        _server_member(raw, [str(d) for d in datasets]),
    ]


@contextlib.contextmanager
def running(
    members: Sequence[Member], log_dir: Path, spawn: Spawn = default_spawn
) -> Iterator[ProcessGroup]:
    """Start `members` in order, waiting for each to be healthy; stop all on exit or failure."""
    group = ProcessGroup(log_dir, spawn)
    try:
        for member in members:
            managed = group.start(member.name, member.command)
            if member.ready is not None:
                group.wait_healthy(managed, member.ready, timeout=member.timeout)
        yield group
    finally:
        group.stop_all()


def wait_for_job(
    group: ProcessGroup,
    name: str,
    supervisor: HealthSupervisor | None = None,
    *,
    sleep: Callable[[float], Any] = time.sleep,
    interval: float = 2.0,
) -> int:
    """Wait for the main process; fail if another process dies or the supervisor trips."""
    job = next(m for m in group.processes if m.name == name)
    while True:
        code = job.process.poll()
        if code is not None:
            return code
        for other in group.processes:
            if other is not job and other.process.poll() is not None:
                raise LaunchError(
                    f"{other.name} exited with code {other.process.returncode} while {name} "
                    f"was running; last lines of {other.log}:\n{_tail(other.log)}"
                )
        if supervisor is not None and supervisor.failed.is_set():
            raise LaunchError(supervisor.message())
        sleep(interval)


# --- HF Jobs ----------------------------------------------------------------------------------

JOB_SECRETS = ("HF_TOKEN", "E2B_API_KEY")


def hf_jobs_command(
    raw: Mapping[str, Any],
    *,
    bucket: str,
    script: str,
    revision: str,
    flavor: str = "h200x2",
    timeout: str = "48h",
    secrets: Sequence[str] = JOB_SECRETS,
    script_args: Sequence[str] = ("run",),
) -> list[str]:
    """`hf jobs uv run` for the pipeline with `bucket` mounted read-write at `/data`.

    Checkpoints and every stage marker live in the bucket, so a job that ends (or is
    restarted with the same command) resumes where the markers say. `revision` is the commit
    or tag of this repository the job runs (`scripts/hf_job.py` checks it out).
    """
    run_dir = f"/data/runs/{raw['run']['name']}"
    argv = ["hf", "jobs", "uv", "run", "--flavor", flavor, "--timeout", timeout, "--detach"]
    argv += ["--env", f"QMT_REPO_REV={revision}"]
    for secret in secrets:
        argv += ["--secrets", secret]
    argv += ["--volume", f"hf://buckets/{bucket}:/data"]
    argv += [script, *script_args, "--run-dir", run_dir]
    return argv


# --- the services of a single machine ---------------------------------------------------------


@dataclass
class LocalServices:
    """`pipeline.Services` on this machine. Constructing it starts nothing."""

    raw: Mapping[str, Any]
    reference: Any
    run_dir: Path
    config_path: Path | None = None
    overrides: Mapping[str, Any] = field(default_factory=dict)
    env: Mapping[str, str] = field(default_factory=lambda: dict(os.environ))
    spawn: Spawn = default_spawn
    downloader: Callable[[str, str], Path] | None = None
    client_factory: Callable[[str], Any] | None = None
    sleep: Callable[[float], Any] = time.sleep
    _eval_roots: dict[str, Path] | None = field(default=None, init=False, repr=False)

    @classmethod
    def from_context(cls, ctx: Any) -> LocalServices:
        return cls(
            raw=ctx.raw,
            reference=ctx.reference,
            run_dir=ctx.run_dir,
            config_path=ctx.config_path,
            overrides=ctx.config_overrides,
            env=ctx.env,
        )

    # -- data --

    @property
    def log_dir(self) -> Path:
        return self.run_dir / LOG_DIR

    def _download(self) -> Callable[[str, str], Path]:
        if self.downloader is not None:
            return self.downloader
        from .dataset import hub_download

        return hub_download(self.run_dir / "downloads")

    def ensure_split(self) -> Path:
        """The merged training dataset (design D4), built once under the run directory."""
        from .dataset import build_from_config

        out = self.run_dir / "data" / "split"
        if (out / "manifest.json").is_file():
            return out
        build_from_config(dict(self.raw), out, self._download())
        return out

    def eval_roots(self) -> dict[str, Path]:
        """Each benchmark's local root, downloaded once at its configured revision.

        The same directories number the tasks (`eval_tasks`) and are what the Harbor server
        serves (`serve_eval`), so a task index means the same task in both.
        """
        if self._eval_roots is None:
            download = self._download()
            roots: dict[str, Path] = {}
            for benchmark, repo in BENCHMARK_REPOS.items():
                revision = next(
                    b["revision"] for b in self.raw["data"]["benchmarks"] if b["repo"] == repo
                )
                roots[benchmark] = download(repo, revision)
            self._eval_roots = roots
        return self._eval_roots

    def eval_tasks(self) -> Mapping[str, Sequence[EvalTask]]:
        """Every task of both benchmarks, numbered by the Harbor server's own listing code.

        `EvalTask.split` is the local root: for the server a split IS the dataset spec it was
        started with.
        """
        from openenv.harbor.tasks import resolve_task_dirs

        by_benchmark: dict[str, list[EvalTask]] = {}
        for benchmark, root in self.eval_roots().items():
            dirs = resolve_task_dirs(str(root), refresh=True)
            if not dirs:
                raise LaunchError(f"no Harbor tasks under {root} ({benchmark})")
            by_benchmark[benchmark] = [
                EvalTask(benchmark, path.name, str(root), index) for index, path in enumerate(dirs)
            ]
        return by_benchmark

    def _verify_served(self, expected: Mapping[str, Sequence[EvalTask]]) -> None:
        """Ask the running server how many tasks it serves per split and compare.

        A server that lists different tasks than `eval_tasks` would file scores under the wrong
        task ids without any error, so a count mismatch stops the evaluation here.
        """
        client = self._client(_urls(self.raw)["server"])
        try:
            for tasks in expected.values():
                split = tasks[0].split
                served = client.num_tasks(split)
                if served != len(tasks):
                    raise LaunchError(
                        f"the Harbor server serves {served} tasks for {split} but {len(tasks)} "
                        "were expected: task indices would not match the scores' task ids"
                    )
        finally:
            close = getattr(client, "close", None)
            if close is not None:
                close()

    # -- the Services protocol --

    def _client(self, url: str) -> Any:
        if self.client_factory is not None:
            return self.client_factory(url)
        from openenv.harbor.client import HarborEnv

        return HarborEnv(base_url=url)

    def qualification_rollout(self, run_dir: Path) -> Any:
        """One training-tier rollout of the first task under the configured guard."""
        raw, urls = self.raw, _urls(self.raw)
        split = self.ensure_split()
        llm_url = urls["guard"] if raw["guard"]["enabled"] else urls["vllm"]
        budget, sampling = raw["budget"], raw["sampling"]
        members = training_stack(raw, run_dir, split=split, with_trainer=False)
        with running(members, self.log_dir, self.spawn):
            client = self._client(urls["server"])
            try:
                return client.run_rollout(
                    split=str(split),
                    task_index=0,
                    harness="querymt",
                    sandbox=self.env.get("QMT_SANDBOX", "e2b"),
                    reward_key="correctness,reward",
                    llm_url=llm_url,
                    model=raw["model"]["base"],
                    agent_timeout_sec=float(budget["agent_timeout_sec"]),
                    agent_step_limit=int(budget["agent_step_limit"]),
                    sampling={
                        "temperature": sampling["temperature"],
                        "top_p": sampling["top_p"],
                        "top_k": -1,
                    },
                )
            finally:
                close = getattr(client, "close", None)
                if close is not None:
                    close()

    def eval_runner(self) -> Runner:
        return server_runner(
            _urls(self.raw)["server"],
            llm_url=_urls(self.raw)["vllm"],
            sandbox=self.env.get("QMT_SANDBOX", "e2b"),
        )

    @contextlib.contextmanager
    def serve_eval(self, adapters: Mapping[str, Path]) -> Iterator[ProcessGroup]:
        expected = self.eval_tasks()
        members = eval_stack(self.raw, adapters, [t[0].split for t in expected.values()])
        with running(members, self.log_dir, self.spawn) as group:
            self._verify_served(expected)
            yield group

    def train(self, config: dict[str, Any]) -> None:
        run_dir = Path(config["run"]["dir"])
        split = self.ensure_split()
        members = training_stack(
            self.raw,
            run_dir,
            split=split,
            config_path=self.config_path,
            overrides=self.overrides,
        )
        serving = self.raw["serving"]
        server = _urls(self.raw)["server"]
        supervisor = HealthSupervisor(
            lambda: http_ok(f"{server}/health") and tcp_open(serving["capture_port"])
        )
        with running(members, self.log_dir, self.spawn) as group, supervisor:
            code = wait_for_job(group, "trainer", supervisor, sleep=self.sleep)
        if code != 0:
            trainer_log = self.log_dir / "trainer.log"
            raise LaunchError(
                f"the trainer exited with code {code}; last lines of {trainer_log}:\n"
                f"{_tail(trainer_log)}"
            )
